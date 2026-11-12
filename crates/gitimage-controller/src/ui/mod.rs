//! The status UI: a read-only JSON and Server-Sent-Events API plus the embedded web app, served
//! by every controller replica on its own port. Requests are made with the caller's identity
//! (see [`auth`]); the list comes from the controller's cache, filtered to the namespaces the
//! caller may list GitImages in.

pub mod assets;
pub mod auth;
pub mod buildlog;
pub mod logs;
pub mod view;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use gitimage_api::config::{Ui, UiAuthMode};
use gitimage_api::{GitImage, keys};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, PostParams};
use kube::runtime::reflector::Store;
use kube::{Client, ResourceExt};

use self::auth::{Identity, Oidc, Sealer, Session};
use self::view::{Detail, GitImageView};

/// How long a "may this user list GitImages in this namespace" answer is reused.
const ACCESS_TTL: Duration = Duration::from_secs(60);
/// Key of the all-namespaces access decision.
const ALL_NAMESPACES: &str = "*";

pub struct UiState {
    /// The controller's own client: mode `none`, and never used for a signed-in user.
    pub client: Client,
    kube_config: kube::Config,
    pub gitimages: Store<GitImage>,
    pub gated_pods: Store<Pod>,
    pub sealer: Sealer,
    pub oidc: Option<Oidc>,
    pub secure_cookies: bool,
    /// Token hash -> client acting as that user, with the token's expiry.
    clients: Mutex<HashMap<String, (Client, i64)>>,
    /// (identity key, namespace) -> may list GitImages there.
    access: Mutex<HashMap<(String, String), (bool, Instant)>>,
}

impl UiState {
    pub async fn new(
        cfg: &Ui,
        client: Client,
        gitimages: Store<GitImage>,
        gated_pods: Store<Pod>,
        session_secret: &[u8],
    ) -> anyhow::Result<Arc<Self>> {
        let base_url = cfg.base_url.clone().unwrap_or_default();
        let oidc = match (cfg.auth.mode, &cfg.auth.oidc) {
            (UiAuthMode::Oidc, Some(o)) => {
                let client_id = std::env::var("GITIMAGE_UI_OIDC_CLIENT_ID")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .or_else(|| o.client_id.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "ui.auth.oidc.clientID or GITIMAGE_UI_OIDC_CLIENT_ID is required"
                        )
                    })?;
                let secret = std::env::var("GITIMAGE_UI_OIDC_CLIENT_SECRET")
                    .ok()
                    .filter(|v| !v.is_empty());
                Some(Oidc::new(o, &base_url, client_id, secret)?)
            }
            _ => None,
        };
        Ok(Arc::new(Self {
            client,
            kube_config: kube::Config::infer().await?,
            gitimages,
            gated_pods,
            sealer: Sealer::new(session_secret),
            oidc,
            secure_cookies: base_url.starts_with("https://"),
            clients: Mutex::new(HashMap::new()),
            access: Mutex::new(HashMap::new()),
        }))
    }

    /// A Kubernetes client whose only credential is the user's ID token.
    fn client_for(&self, key: &str, session: &Session) -> Result<Client, kube::Error> {
        let now = auth::now();
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|_, (_, exp)| *exp > now);
        if let Some((c, _)) = clients.get(key) {
            return Ok(c.clone());
        }
        let mut cfg = self.kube_config.clone();
        cfg.auth_info = kube::config::AuthInfo {
            token: Some(session.token.clone().into()),
            ..Default::default()
        };
        let client = Client::try_from(cfg)?;
        clients.insert(key.to_string(), (client.clone(), session.exp));
        Ok(client)
    }
}

/// An API failure as `{"error": "..."}`.
pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<kube::Error> for ApiError {
    fn from(e: kube::Error) -> Self {
        match e {
            kube::Error::Api(s) => match s.code {
                401 => ApiError(
                    StatusCode::FORBIDDEN,
                    format!(
                        "The Kubernetes API server rejected your token: {}. It must trust this \
                         issuer and client id (audience).",
                        s.message
                    ),
                ),
                403 | 404 => ApiError(
                    StatusCode::from_u16(s.code).unwrap_or(StatusCode::BAD_GATEWAY),
                    s.message.clone(),
                ),
                _ => ApiError(StatusCode::BAD_GATEWAY, s.message.clone()),
            },
            other => ApiError(StatusCode::BAD_GATEWAY, other.to_string()),
        }
    }
}

/// Asks the API server, as the caller, whether they may list GitImages in `ns`.
async fn may_list(ui: &UiState, id: &Identity, ns: &str) -> Result<bool, kube::Error> {
    if id.unrestricted {
        return Ok(true);
    }
    let key = (id.key.clone(), ns.to_string());
    if let Some((allowed, at)) = ui.access.lock().unwrap().get(&key)
        && at.elapsed() < ACCESS_TTL
    {
        return Ok(*allowed);
    }
    let review = SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                group: Some("gitimage.dev".into()),
                resource: Some("gitimages".into()),
                verb: Some("list".into()),
                namespace: (ns != ALL_NAMESPACES).then(|| ns.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let api: Api<SelfSubjectAccessReview> = Api::all(id.client.clone());
    let allowed = api
        .create(&PostParams::default(), &review)
        .await?
        .status
        .is_some_and(|s| s.allowed);
    let mut access = ui.access.lock().unwrap();
    access.retain(|_, (_, at)| at.elapsed() < ACCESS_TTL);
    access.insert(key, (allowed, Instant::now()));
    Ok(allowed)
}

/// Every GitImage the caller may see, sorted by namespace and name.
async fn visible(ui: &UiState, id: &Identity) -> Result<Vec<GitImageView>, kube::Error> {
    let everything = may_list(ui, id, ALL_NAMESPACES).await?;
    let pods = ui.gated_pods.state();
    let mut by_ns: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    for gi in ui.gitimages.state() {
        let ns = gi.namespace().unwrap_or_default();
        let allowed = match by_ns.get(&ns) {
            Some(a) => *a,
            None => {
                let a = everything || may_list(ui, id, &ns).await?;
                by_ns.insert(ns.clone(), a);
                a
            }
        };
        if allowed {
            let gated = view::count_gated(&pods, &ns, &gi.name_any());
            out.push(view::gitimage_view(&gi, gated, false));
        }
    }
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(out)
}

/// `GET /api/me`
async fn me(State(ui): State<Arc<UiState>>, Extension(id): Extension<Identity>) -> Response {
    Json(serde_json::json!({
        "user": id.user,
        "authMode": if ui.oidc.is_some() { "oidc" } else { "none" },
    }))
    .into_response()
}

/// `GET /api/gitimages`
async fn list(
    State(ui): State<Arc<UiState>>,
    Extension(id): Extension<Identity>,
) -> Result<Json<Vec<GitImageView>>, ApiError> {
    Ok(Json(visible(&ui, &id).await?))
}

/// `GET /api/events`: the list again, every time it changes.
async fn events(State(ui): State<Arc<UiState>>, Extension(id): Extension<Identity>) -> Response {
    // Fail before the stream starts, so the browser sees a status code and not a dead stream.
    if let Err(e) = may_list(&ui, &id, ALL_NAMESPACES).await {
        return ApiError::from(e).into_response();
    }
    let tick = tokio::time::interval(Duration::from_secs(1));
    let stream = futures::stream::unfold(
        (ui, id, String::new(), tick),
        |(ui, id, mut last, mut tick)| async move {
            loop {
                tick.tick().await;
                if id.expired() {
                    return None;
                }
                let Ok(list) = visible(&ui, &id).await else {
                    return None;
                };
                let json = serde_json::to_string(&list).unwrap_or_default();
                if json != last {
                    last.clone_from(&json);
                    let event = Event::default().event("gitimages").data(json);
                    return Some((Ok::<_, Infallible>(event), (ui, id, last, tick)));
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `GET /api/gitimages/{ns}/{name}`: read entirely with the caller's own permissions.
async fn detail(
    Extension(id): Extension<Identity>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<Json<Detail>, ApiError> {
    let gi = Api::<GitImage>::namespaced(id.client.clone(), &ns)
        .get(&name)
        .await?;
    let revision = gi.status.as_ref().and_then(|s| s.revision.clone());
    let rev = revision.as_deref();
    let mut warnings = Vec::new();
    let mut workloads = Vec::new();
    let lp = ListParams::default();

    match Api::<Deployment>::namespaced(id.client.clone(), &ns)
        .list(&lp)
        .await
    {
        Ok(l) => workloads.extend(
            l.iter()
                .filter_map(|w| view::deployment_view(w, &name, rev)),
        ),
        Err(e) => warnings.push(format!("Deployments could not be listed: {e}")),
    }
    match Api::<StatefulSet>::namespaced(id.client.clone(), &ns)
        .list(&lp)
        .await
    {
        Ok(l) => workloads.extend(
            l.iter()
                .filter_map(|w| view::statefulset_view(w, &name, rev)),
        ),
        Err(e) => warnings.push(format!("StatefulSets could not be listed: {e}")),
    }
    match Api::<DaemonSet>::namespaced(id.client.clone(), &ns)
        .list(&lp)
        .await
    {
        Ok(l) => workloads.extend(l.iter().filter_map(|w| view::daemonset_view(w, &name, rev))),
        Err(e) => warnings.push(format!("DaemonSets could not be listed: {e}")),
    }

    let mut gated_pods = Vec::new();
    let gated = ListParams::default().labels(&format!("{}=true", keys::GATED_LABEL));
    match Api::<Pod>::namespaced(id.client.clone(), &ns)
        .list(&gated)
        .await
    {
        Ok(l) => gated_pods.extend(l.iter().filter_map(|p| view::gated_pod_view(p, &name))),
        Err(e) => warnings.push(format!("Pods could not be listed: {e}")),
    }
    gated_pods.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    Ok(Json(Detail {
        gitimage: view::gitimage_view(&gi, gated_pods.len(), true),
        workloads,
        gated_pods,
        warnings,
    }))
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             font-src 'self' data:; frame-ancestors 'none'; base-uri 'self'; form-action 'self'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    resp
}

pub fn router(state: Arc<UiState>) -> Router {
    let api = Router::new()
        .route("/api/me", get(me))
        .route("/api/gitimages", get(list))
        .route("/api/events", get(events))
        .route("/api/gitimages/{ns}/{name}", get(detail))
        .route("/api/gitimages/{ns}/{name}/logs", get(logs::stream))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require));
    Router::new()
        .merge(api)
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", get(auth::logout))
        .fallback(assets::serve)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}
