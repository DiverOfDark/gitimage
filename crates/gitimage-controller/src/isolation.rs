//! Isolation startup check (design section 8, PRD N1): never fall back to a weaker mode. The
//! check runs a Job with exactly the build pod's isolation overlay and BuildKit image; if it
//! fails the controller exits with code 3 (`IsolationUnavailable`) and stays in CrashLoopBackOff.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use gitimage_api::Config;
use gitimage_api::config::IsolationMode;
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{ConfigMap, Container, Pod, PodSpec, PodTemplateSpec};
use k8s_openapi::api::node::v1::RuntimeClass;
use kube::api::{Api, DeleteParams, ListParams, LogParams, PostParams, PropagationPolicy};
use kube::{Client, ResourceExt};
use sha2::Digest as _;

pub const EXIT_ISOLATION_UNAVAILABLE: i32 = 3;
const CACHE_CONFIGMAP: &str = "gitimage-isolation-check";
const CACHE_TTL_SECS: i64 = 24 * 3600;
const TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, thiserror::Error)]
#[error("IsolationUnavailable: {0}")]
pub struct IsolationUnavailable(pub String);

/// Hash of everything the check depends on.
pub fn check_key(cfg: &Config) -> String {
    let iso = serde_json::to_string(&cfg.build_policy.isolation).unwrap_or_default();
    let image = crate::job::BuildkitLayout::for_config(cfg).image;
    let state = serde_json::to_string(&cfg.build.state_volume).unwrap_or_default();
    hex::encode(&sha2::Sha256::digest(format!("{iso}|{image}|{state}").as_bytes())[..8])
}

/// The check Job: the build overlay plus `buildctl-daemonless.sh debug workers`.
pub fn check_job(cfg: &Config, ns: &str) -> Job {
    let mut pod = PodSpec {
        restart_policy: Some("Never".into()),
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        ..Default::default()
    };
    let layout = crate::job::BuildkitLayout::for_config(cfg);
    let mut c = Container {
        name: "buildkit".into(),
        image: Some(layout.image.clone()),
        command: Some(vec!["buildctl-daemonless.sh".into()]),
        args: Some(vec!["debug".into(), "workers".into()]),
        termination_message_policy: Some("FallbackToLogsOnError".into()),
        ..Default::default()
    };
    crate::job::pod_overlay(cfg, &mut pod, &mut c);
    crate::job::attach_state_volume(cfg, &mut pod, &mut c);
    pod.containers = vec![c];
    let labels = BTreeMap::from([
        (
            "app.kubernetes.io/managed-by".to_string(),
            "gitimage".to_string(),
        ),
        (
            "app.kubernetes.io/component".to_string(),
            "isolation-check".to_string(),
        ),
    ]);
    Job {
        metadata: kube::core::ObjectMeta {
            name: Some(format!("gitimage-isolation-check-{}", check_key(cfg))),
            namespace: Some(ns.into()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            active_deadline_seconds: Some(TIMEOUT.as_secs() as i64),
            ttl_seconds_after_finished: Some(600),
            template: PodTemplateSpec {
                metadata: Some(kube::core::ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(pod),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn cached_ok(client: &Client, ns: &str, key: &str) -> bool {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let Ok(Some(cm)) = api.get_opt(CACHE_CONFIGMAP).await else {
        return false;
    };
    let d = cm.data.unwrap_or_default();
    let at: i64 = d.get("checkedAt").and_then(|s| s.parse().ok()).unwrap_or(0);
    d.get("key").map(String::as_str) == Some(key)
        && chrono::Utc::now().timestamp() - at < CACHE_TTL_SECS
}

async fn store_ok(client: &Client, ns: &str, key: &str) {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), ns);
    let cm = ConfigMap {
        metadata: kube::core::ObjectMeta {
            name: Some(CACHE_CONFIGMAP.into()),
            ..Default::default()
        },
        data: Some(BTreeMap::from([
            ("key".into(), key.into()),
            (
                "checkedAt".into(),
                chrono::Utc::now().timestamp().to_string(),
            ),
        ])),
        ..Default::default()
    };
    let _ = crate::util::apply(&api, CACHE_CONFIGMAP, &cm).await;
}

/// Runs the check. `Ok` means builds can run under the configured mode.
pub async fn check(client: &Client, cfg: &Config, ns: &str) -> Result<(), IsolationUnavailable> {
    let iso = &cfg.build_policy.isolation;
    if iso.mode == IsolationMode::RuntimeClass {
        let name = iso.runtime_class_name.clone().unwrap_or_default();
        let api: Api<RuntimeClass> = Api::all(client.clone());
        match api.get_opt(&name).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(IsolationUnavailable(format!(
                    "RuntimeClass {name} not found"
                )));
            }
            Err(e) => return Err(IsolationUnavailable(format!("RuntimeClass {name}: {e}"))),
        }
    }
    let key = check_key(cfg);
    if cached_ok(client, ns, &key).await {
        tracing::info!(key, "isolation check: cached result is fresh");
        return Ok(());
    }
    let jobs: Api<Job> = Api::namespaced(client.clone(), ns);
    let job = check_job(cfg, ns);
    let name = job.name_any();
    let start = Instant::now();
    // Replicas share one check Job (its name is keyed on the config): wait on a running one,
    // reuse a passed one, replace a failed one.
    loop {
        match jobs.get_opt(&name).await {
            Ok(Some(existing)) => {
                let st = existing.status.clone().unwrap_or_default();
                let failed = st.failed.unwrap_or(0) > 0;
                if failed && existing.metadata.deletion_timestamp.is_none() {
                    let dp = DeleteParams {
                        propagation_policy: Some(PropagationPolicy::Background),
                        ..Default::default()
                    };
                    let _ = jobs.delete(&name, &dp).await;
                }
                if !failed {
                    break;
                }
            }
            Ok(None) => match jobs.create(&PostParams::default(), &job).await {
                Ok(_) => break,
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                Err(e) => return Err(IsolationUnavailable(format!("creating check job: {e}"))),
            },
            Err(e) => return Err(IsolationUnavailable(format!("reading check job: {e}"))),
        }
        if start.elapsed() > Duration::from_secs(60) {
            return Err(IsolationUnavailable(format!(
                "could not start check job {name}"
            )));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    tracing::info!(mode = ?iso.mode, job = %name, "isolation check started");
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let j = jobs
            .get(&name)
            .await
            .map_err(|e| IsolationUnavailable(e.to_string()))?;
        let st = j.status.unwrap_or_default();
        if st.succeeded.unwrap_or(0) > 0 {
            store_ok(client, ns, &key).await;
            tracing::info!(mode = ?iso.mode, seconds = start.elapsed().as_secs(), "isolation check passed");
            return Ok(());
        }
        let failed = st.failed.unwrap_or(0) > 0
            || st
                .conditions
                .unwrap_or_default()
                .iter()
                .any(|c| c.type_ == "Failed" && c.status == "True");
        if failed || start.elapsed() > TIMEOUT + Duration::from_secs(30) {
            let lp = ListParams::default().labels(&format!("batch.kubernetes.io/job-name={name}"));
            let mut detail = String::new();
            if let Ok(list) = pods.list(&lp).await
                && let Some(p) = list.items.first()
            {
                let logs = pods
                    .logs(
                        &p.name_any(),
                        &LogParams {
                            tail_lines: Some(20),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap_or_default();
                let waiting = p
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|c| c.first())
                    .and_then(|c| c.state.as_ref())
                    .and_then(|s| s.waiting.as_ref())
                    .map(|w| {
                        format!(
                            "{}: {}",
                            w.reason.clone().unwrap_or_default(),
                            w.message.clone().unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                detail = format!("{waiting} {logs}");
            }
            return Err(IsolationUnavailable(format!(
                "mode {:?} failed its check job {name}: {}",
                iso.mode,
                detail.trim()
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_job_uses_the_build_overlay() {
        let cfg = Config::from_yaml(
            "buildPolicy: { isolation: { mode: runtimeClass, runtimeClassName: kata } }\nstorage: { type: registry, registry: { url: r, pushSecret: p } }\npushEndpoint: { host: h }",
        )
        .unwrap();
        let j = check_job(&cfg, "gitimage-system");
        let pod = j.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.runtime_class_name.as_deref(), Some("kata"));
        assert_eq!(
            pod.containers[0].args.as_ref().unwrap(),
            &vec!["debug".to_string(), "workers".to_string()]
        );
        assert_eq!(check_key(&cfg).len(), 16);
    }

    #[test]
    fn sysbox_check_job_matches_the_build_pod() {
        let cfg = Config::from_yaml(
            "buildPolicy: { isolation: { mode: runtimeClass, runtimeClassName: sysbox-runc, rootfulInSandbox: true } }\nstorage: { type: registry, registry: { url: r, pushSecret: p } }\npushEndpoint: { host: h }",
        )
        .unwrap();
        let j = check_job(&cfg, "gitimage-system");
        let pod = j.spec.unwrap().template.spec.unwrap();
        let mut build_pod = crate::job::build_job(
            &crate::job::tests::gitimage(),
            &cfg,
            &crate::job::tests::params(),
        )
        .spec
        .unwrap()
        .template
        .spec
        .unwrap();
        assert_eq!(pod.runtime_class_name, build_pod.runtime_class_name);
        assert_eq!(pod.host_users, Some(false));
        assert_eq!(pod.host_users, build_pod.host_users);
        let (c, b) = (&pod.containers[0], build_pod.containers.remove(0));
        assert_eq!(c.image, b.image);
        assert_eq!(c.image.as_deref(), Some("moby/buildkit:v0.34.2"));
        assert_eq!(c.security_context, b.security_context);
        assert_eq!(
            c.volume_mounts.as_ref().unwrap()[0].mount_path,
            "/var/lib/buildkit"
        );
        // A different isolation setting must not reuse a cached check result.
        let mut other = cfg.clone();
        other.build_policy.isolation.rootful_in_sandbox = false;
        assert_ne!(check_key(&cfg), check_key(&other));
    }
}
