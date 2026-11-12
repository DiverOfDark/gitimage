//! The `gitimage controller` workload: reconcilers, push endpoint, hooks receiver, storage GC,
//! isolation check and TLS bootstrap (design section 5).

pub mod bootstrap;
pub mod context;
pub mod gates;
pub mod gc;
pub mod hooks;
pub mod isolation;
pub mod job;
pub mod metrics;
pub mod namespace_setup;
pub mod progress;
pub mod push_auth;
pub mod reconcile;
pub mod roller;
pub mod storage;
pub mod ui;
pub mod util;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use futures::StreamExt;
use gitimage_api::{Config, GitImage, keys};
use k8s_openapi::api::batch::v1::Job;
use kube::Client;
use kube::api::Api;
use kube::runtime::controller::Controller;
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use tokio_util::sync::CancellationToken;

use crate::context::Ctx;
use crate::metrics::Metrics;

/// Process-level settings from flags and the downward API.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config_path: PathBuf,
    /// Namespace the controller runs in.
    pub namespace: String,
    pub pod_name: String,
    pub pod_ip: String,
    pub push_cert_dir: PathBuf,
    pub push_addr: SocketAddr,
    pub hooks_addr: SocketAddr,
    pub ops_addr: SocketAddr,
    pub ui_addr: SocketAddr,
}

fn reflect<K>(api: Api<K>, cfg: watcher::Config) -> Store<K>
where
    K: kube::Resource<DynamicType = ()>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + Send
        + Sync
        + 'static,
{
    let (store, writer) = reflector::store();
    let stream = watcher(api, cfg)
        .default_backoff()
        .reflect(writer)
        .applied_objects()
        .for_each(|_| futures::future::ready(()));
    tokio::spawn(stream);
    store
}

/// Runs the controller until SIGTERM. Exits the process with code 3 if the isolation check fails.
pub async fn run(settings: Settings) -> anyhow::Result<()> {
    gitimage_tls::install_crypto_provider();
    let raw = std::fs::read_to_string(&settings.config_path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", settings.config_path.display()))?;
    let config = Config::from_yaml(&raw)?;
    let client = Client::try_default().await?;
    let ns = settings.namespace.clone();
    let metrics = Arc::new(Metrics::default());
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown = CancellationToken::new();
    {
        let s = shutdown.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            s.cancel();
        });
    }
    // Health first: the isolation check can take minutes on a cold node.
    let ops = {
        let s = shutdown.clone();
        let m = metrics.clone();
        let r = ready.clone();
        let router = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route(
                "/readyz",
                get(move || {
                    let ok = r.load(std::sync::atomic::Ordering::SeqCst);
                    async move {
                        if ok {
                            (StatusCode::OK, "ok")
                        } else {
                            (StatusCode::SERVICE_UNAVAILABLE, "starting")
                        }
                    }
                }),
            )
            .route(
                "/metrics",
                get(move || {
                    let body = m.encode();
                    async move {
                        (
                            [(
                                "content-type",
                                "application/openmetrics-text; version=1.0.0; charset=utf-8",
                            )],
                            body,
                        )
                    }
                }),
            );
        tokio::spawn(gitimage_tls::serve_plain(
            settings.ops_addr,
            router,
            async move { s.cancelled().await },
        ))
    };

    let keys = bootstrap::ensure(&client, &ns, &config.push_endpoint.host, false).await?;

    if let Err(e) = isolation::check(&client, &config, &ns).await {
        tracing::error!(reason = "IsolationUnavailable", error = %e, "isolation check failed; no builds will run");
        std::process::exit(isolation::EXIT_ISOLATION_UNAVAILABLE);
    }

    let storage = storage::build(&config, &client, &ns).await?;
    let gitimages = reflect(
        Api::<GitImage>::all(client.clone()),
        watcher::Config::default().any_semantic(),
    );
    let build_jobs = reflect(
        Api::<Job>::all(client.clone()),
        watcher::Config::default().labels(&format!("{}={}", keys::ROLE_LABEL, keys::ROLE_BUILD)),
    );

    // HTTP servers on every replica.
    let push_state = gitimage_oci::PushState {
        storage: storage.clone(),
        auth: Arc::new(push_auth::JobTokenAuth {
            key: keys.push_token_key.clone(),
            jobs: build_jobs.clone(),
        }),
        upload_key: Arc::new(keys.upload_key.clone()),
        pod_ip: settings.pod_ip.clone(),
        forward_host: config.push_endpoint.host.clone(),
        forward_port: config.push_endpoint.port,
        forward_ca_pem: Some(keys.ca.cert_pem.clone().into_bytes()),
    };
    let push = {
        let s = shutdown.clone();
        tokio::spawn(gitimage_tls::serve_tls(
            settings.push_addr,
            gitimage_oci::push_router(push_state),
            gitimage_tls::CertFiles::in_dir(&settings.push_cert_dir),
            async move { s.cancelled().await },
        ))
    };
    let hooks = {
        let s = shutdown.clone();
        let state = hooks::HookState {
            client: client.clone(),
            gitimages: gitimages.clone(),
            metrics: metrics.clone(),
        };
        tokio::spawn(gitimage_tls::serve_plain(
            settings.hooks_addr,
            hooks::router(state),
            async move { s.cancelled().await },
        ))
    };
    let ui = if config.ui.enabled {
        // Its own watch: the gate releaser's store exists only on the leader.
        let gated_pods = reflect(
            Api::<k8s_openapi::api::core::v1::Pod>::all(client.clone()),
            watcher::Config::default().labels(&format!("{}=true", keys::GATED_LABEL)),
        );
        let state = ui::UiState::new(
            &config.ui,
            client.clone(),
            gitimages.clone(),
            gated_pods,
            &keys.push_token_key,
        )
        .await?;
        let s = shutdown.clone();
        tracing::info!(addr = %settings.ui_addr, auth = ?config.ui.auth.mode, "serving the UI");
        Some(tokio::spawn(gitimage_tls::serve_plain(
            settings.ui_addr,
            ui::router(state),
            async move { s.cancelled().await },
        )))
    } else {
        None
    };
    {
        let (g, j, r) = (gitimages.clone(), build_jobs.clone(), ready.clone());
        let cert_file = settings.push_cert_dir.join("tls.crt");
        tokio::spawn(async move {
            if g.wait_until_ready().await.is_ok() && j.wait_until_ready().await.is_ok() {
                tracing::info!("caches synced");
                // The push endpoint serves only once its certificate is mounted.
                while !cert_file.exists() {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                r.store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::info!("push certificate loaded; ready");
            }
        });
    }

    // Leader-only loops.
    let (trigger_tx, trigger_rx) = tokio::sync::mpsc::unbounded_channel();
    let recorder = Recorder::new(
        client.clone(),
        Reporter {
            controller: "gitimage-controller".into(),
            instance: Some(settings.pod_name.clone()),
        },
    );
    let ctx = Arc::new(Ctx {
        client: client.clone(),
        config: config.clone(),
        namespace: ns.clone(),
        gitimage_image: config
            .controller
            .image
            .clone()
            .unwrap_or_else(|| "ghcr.io/diverofdark/gitimage:latest".into()),
        git: gitimage_git::GixClient,
        storage,
        push_key: keys.push_token_key.clone(),
        ca_pem: keys.ca.cert_pem.clone(),
        recorder,
        gitimages: gitimages.clone(),
        gated_pods: Mutex::new(None),
        resolve_cache: Mutex::new(Default::default()),
        progress: Mutex::new(Default::default()),
        followers: Mutex::new(Default::default()),
        trigger: trigger_tx,
        metrics: metrics.clone(),
    });

    let lease = kube_lease_manager::LeaseManagerBuilder::new(client.clone(), "gitimage-controller")
        .with_namespace(&ns)
        .with_identity(&settings.pod_name)
        .with_duration(15)
        .with_grace(5)
        .build()
        .await?;
    let (mut leader_rx, lease_task) = lease.watch().await;
    tracing::info!(identity = %settings.pod_name, "waiting for leadership");
    let mut trigger_rx = Some(trigger_rx);
    let leader_loops = loop {
        tokio::select! {
            _ = shutdown.cancelled() => break None,
            changed = leader_rx.changed() => {
                if changed.is_err() {
                    anyhow::bail!("lease manager stopped");
                }
                if *leader_rx.borrow_and_update() {
                    metrics.leader.set(1);
                    tracing::info!("acquired leadership; starting reconcilers");
                    break Some(start_leader_loops(ctx.clone(), trigger_rx.take().expect("started once"), settings.clone()));
                }
            }
        }
    };
    if let Some(loops) = leader_loops {
        tokio::select! {
            _ = shutdown.cancelled() => {}
            changed = leader_rx.changed() => {
                if changed.is_err() || !*leader_rx.borrow() {
                    tracing::error!("lost leadership; exiting so a fresh process can stand by");
                    std::process::exit(1);
                }
            }
        }
        loops.abort();
    }
    drop(leader_rx);
    if let Ok(Ok(manager)) = lease_task.await {
        let _ = manager.release().await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = push.await;
        let _ = hooks.await;
        let _ = ops.await;
        if let Some(ui) = ui {
            let _ = ui.await;
        }
    })
    .await;
    Ok(())
}

fn start_leader_loops(
    ctx: Arc<Ctx>,
    trigger_rx: tokio::sync::mpsc::UnboundedReceiver<kube::runtime::reflector::ObjectRef<GitImage>>,
    settings: Settings,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let gis: Api<GitImage> = Api::all(ctx.client.clone());
        let jobs: Api<Job> = Api::all(ctx.client.clone());
        let main = Controller::new(gis, watcher::Config::default().any_semantic())
            .owns(
                jobs,
                watcher::Config::default().labels(&format!(
                    "{}={}",
                    keys::ROLE_LABEL,
                    keys::ROLE_BUILD
                )),
            )
            .reconcile_on(tokio_stream_from(trigger_rx))
            .run(reconcile::reconcile, reconcile::error_policy, ctx.clone())
            .for_each(|res| {
                if let Err(e) = res {
                    tracing::debug!(error = %e, "gitimage controller event");
                }
                futures::future::ready(())
            });
        let renew = {
            let ctx = ctx.clone();
            async move {
                let mut tick = tokio::time::interval(Duration::from_secs(12 * 3600));
                loop {
                    tick.tick().await;
                    if let Err(e) = bootstrap::ensure(
                        &ctx.client,
                        &settings.namespace,
                        &ctx.config.push_endpoint.host,
                        true,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "certificate renewal failed");
                    }
                }
            }
        };
        tokio::join!(main, gates::run(ctx.clone()), gc::run(ctx.clone()), renew);
    })
}

fn tokio_stream_from<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<T>,
) -> impl futures::Stream<Item = T> + Send + 'static {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}
