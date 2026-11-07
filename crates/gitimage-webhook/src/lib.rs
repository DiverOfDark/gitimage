//! The `gitimage webhook` workload: a pure [`mutate`] function behind an axum handler that
//! reads GitImages from a reflector cache. It never calls the API server on the request path.

pub mod mutate;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get, routing::post};
use futures::StreamExt;
use gitimage_api::GitImage;
use k8s_openapi::api::core::v1::Pod;
use kube::api::Api;
use kube::core::DynamicObject;
use kube::core::admission::{AdmissionRequest, AdmissionResponse, AdmissionReview};
use kube::runtime::reflector::{self, ObjectRef, Store};
use kube::runtime::{WatchStreamExt, watcher};

pub use mutate::{
    Decision, MutateConfig, Slot, bindings, find_container, release_patch, resolve_digest,
};

/// Looks a GitImage up by namespace and name.
pub trait GitImageLookup: Send + Sync + 'static {
    fn get(&self, namespace: &str, name: &str) -> Option<GitImage>;
}

impl GitImageLookup for Store<GitImage> {
    fn get(&self, namespace: &str, name: &str) -> Option<GitImage> {
        Store::get(self, &ObjectRef::new(name).within(namespace)).map(|g| (*g).clone())
    }
}

#[derive(Clone)]
pub struct AppState {
    pub lookup: Arc<dyn GitImageLookup>,
    pub config: Arc<MutateConfig>,
}

/// Router with `POST /mutate` (both webhook entries point here) and `/healthz`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/mutate", post(admit))
        .route("/guard", post(guard))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

/// The admission response for one review. Exposed for tests and for the e2e harness.
pub fn review(state: &AppState, req: AdmissionRequest<Pod>) -> AdmissionResponse {
    let resp = AdmissionResponse::from(&req);
    let Some(pod) = req.object.as_ref() else {
        return resp;
    };
    let namespace = req
        .namespace
        .clone()
        .or_else(|| pod.metadata.namespace.clone())
        .unwrap_or_default();
    let lookup = |name: &str| state.lookup.get(&namespace, name);
    match mutate::mutate(pod, &namespace, lookup, &state.config) {
        Decision::Allow(ops) if ops.is_empty() => resp,
        Decision::Allow(ops) => {
            let gated = ops
                .iter()
                .any(|op| op.path().to_string().contains("schedulingGates"));
            tracing::info!(
                namespace = %namespace,
                pod = pod.metadata.name.as_deref().or(pod.metadata.generate_name.as_deref()).unwrap_or(""),
                gated,
                "mutated pod"
            );
            match resp.clone().with_patch(json_patch::Patch(ops)) {
                Ok(r) => r,
                Err(e) => resp.deny(format!("gitimage webhook: cannot serialize patch: {e}")),
            }
        }
        Decision::Deny(reason) => {
            tracing::info!(namespace = %namespace, %reason, "denied pod");
            resp.deny(reason)
        }
    }
}

async fn admit(
    State(state): State<AppState>,
    Json(body): Json<AdmissionReview<Pod>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let req: AdmissionRequest<Pod> = body.try_into().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid AdmissionReview: {e}"),
        )
    })?;
    let out: AdmissionReview<DynamicObject> = review(&state, req).into_review();
    let mut value = serde_json::to_value(out)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    base64_patch(&mut value);
    Ok(Json(value))
}

/// Check-only entry for the cross-tenant guard: never patches.
async fn guard(
    State(state): State<AppState>,
    Json(body): Json<AdmissionReview<Pod>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let req: AdmissionRequest<Pod> = body.try_into().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid AdmissionReview: {e}"),
        )
    })?;
    let mut resp = AdmissionResponse::from(&req);
    if let Some(pod) = req.object.as_ref() {
        let ns = req
            .namespace
            .clone()
            .or_else(|| pod.metadata.namespace.clone())
            .unwrap_or_default();
        if let Some(reason) = mutate::cross_tenant_violation(pod, &ns, &state.config) {
            tracing::info!(namespace = %ns, %reason, "guard denied pod");
            resp = resp.deny(reason);
        }
    }
    serde_json::to_value(resp.into_review())
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// The API server documents `response.patch` as a base64 string (Go `[]byte`); kube
/// serializes it as a number array. Rewrite it to the documented form.
fn base64_patch(review: &mut serde_json::Value) {
    use base64::Engine as _;
    let Some(patch) = review.pointer_mut("/response/patch") else {
        return;
    };
    if let Some(arr) = patch.as_array() {
        let bytes: Vec<u8> = arr
            .iter()
            .filter_map(|v| v.as_u64())
            .map(|b| b as u8)
            .collect();
        *patch = serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
}

/// Runs the webhook: GitImage reflector, HTTPS admission server, plain health server.
pub async fn run(
    client: kube::Client,
    config: MutateConfig,
    addr: SocketAddr,
    health_addr: SocketAddr,
    cert_dir: std::path::PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let api: Api<GitImage> = Api::all(client);
    let (store, writer) = reflector::store();
    let watch = watcher(api, watcher::Config::default().any_semantic())
        .default_backoff()
        .reflect(writer)
        .applied_objects()
        .for_each(|_| futures::future::ready(()));
    tokio::spawn(watch);
    let wait = store.clone();
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready_flag = ready.clone();
    tokio::spawn(async move {
        if wait.wait_until_ready().await.is_ok() {
            ready_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::info!("GitImage cache synced");
        }
    });

    let state = AppState {
        lookup: Arc::new(store),
        config: Arc::new(config),
    };
    let (tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let health = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || {
                let ready = ready.load(std::sync::atomic::Ordering::SeqCst);
                async move {
                    if ready {
                        (StatusCode::OK, "ok")
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "cache not synced")
                    }
                }
            }),
        );
    let mut rx1 = tx.subscribe();
    let mut rx2 = tx.subscribe();
    let tls = gitimage_tls::serve_tls(
        addr,
        router(state),
        gitimage_tls::CertFiles::in_dir(cert_dir),
        async move {
            let _ = rx1.recv().await;
        },
    );
    let plain = gitimage_tls::serve_plain(health_addr, health, async move {
        let _ = rx2.recv().await;
    });
    tokio::spawn(async move {
        shutdown.await;
        let _ = tx.send(());
    });
    tokio::try_join!(tls, plain)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct Empty;
    impl GitImageLookup for Empty {
        fn get(&self, _: &str, _: &str) -> Option<GitImage> {
            None
        }
    }

    fn state() -> AppState {
        AppState {
            lookup: Arc::new(Empty),
            config: Arc::new(MutateConfig {
                pull_host: "localhost:25050".into(),
                push_authority: "gitimage-push.gitimage-system.svc:5000".into(),
                pull_secret: None,
            }),
        }
    }

    async fn post(review: serde_json::Value) -> serde_json::Value {
        let resp = router(state())
            .oneshot(
                Request::post("/mutate")
                    .header("content-type", "application/json")
                    .body(Body::from(review.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn review_for(pod: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": "705ab4f5-6393-11e8-b7cc-42010a800002",
                "kind": {"group": "", "version": "v1", "kind": "Pod"},
                "resource": {"group": "", "version": "v1", "resource": "pods"},
                "requestKind": {"group": "", "version": "v1", "kind": "Pod"},
                "requestResource": {"group": "", "version": "v1", "resource": "pods"},
                "namespace": "team-a",
                "operation": "CREATE",
                "userInfo": {"username": "system:serviceaccount:kube-system:replicaset-controller"},
                "object": pod,
                "dryRun": false
            }
        })
    }

    #[tokio::test]
    async fn gated_pod_gets_a_base64_json_patch() {
        let out = post(review_for(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"generateName": "web-", "annotations": {"gitimage.dev/image.web": "myapp"}},
            "spec": {"containers": [{"name": "web", "image": "placeholder"}]}
        })))
        .await;
        let r = &out["response"];
        assert_eq!(r["uid"], "705ab4f5-6393-11e8-b7cc-42010a800002");
        assert_eq!(r["allowed"], true);
        assert_eq!(r["patchType"], "JSONPatch");
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(r["patch"].as_str().expect("base64 string"))
            .unwrap();
        let ops: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(ops[0]["path"], "/spec/schedulingGates");
    }

    #[tokio::test]
    async fn cross_tenant_pod_is_denied() {
        let out = post(review_for(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "x"},
            "spec": {"containers": [{"name": "x", "image": "localhost:25050/team-b/app@sha256:1"}]}
        })))
        .await;
        assert_eq!(out["response"]["allowed"], false);
        assert!(
            out["response"]["status"]["message"]
                .as_str()
                .unwrap()
                .contains("team-b")
        );
    }
}
