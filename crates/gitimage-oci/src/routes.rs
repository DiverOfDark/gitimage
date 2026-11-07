//! axum routes for `/v2/`. Read-write for the push endpoint, read-only for gitimage-node.
//!
//! Everything goes through one handler that parses the path (repos are exactly
//! `<namespace>/<name>`), checks auth, forwards uploads owned by other replicas, and
//! dispatches to the blob, upload and manifest modules.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, Method, header, response::Builder};
use axum::response::{IntoResponse, Response};
use tracing::Instrument;

use crate::{DynStorage, OciError, Repo};

mod blobs;
mod error;
mod forward;
mod manifests;
mod path;
#[cfg(test)]
mod tests;
mod upload_id;
mod uploads;

use error::ApiError;
use forward::Forwarder;
use path::Route;
use upload_id::{Owner, UploadIds};

const DOCKER_CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");
const API_VERSION: HeaderName = HeaderName::from_static("docker-distribution-api-version");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Pull,
    Push,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub repo: Repo,
    pub access: Access,
}

/// Pull for reads (`GET`/`HEAD`), push for everything else.
impl Access {
    fn of(method: &Method) -> Self {
        if method == Method::GET || method == Method::HEAD {
            Self::Pull
        } else {
            Self::Push
        }
    }
}

/// Decides whether a request may touch a repo. The controller implements it with push-token
/// verification plus its Job reflector; `gitimage node` allows every pull on localhost.
#[async_trait::async_trait]
pub trait AuthCheck: Send + Sync + 'static {
    /// `authorization` is the raw `Authorization` header, if any. `GET /v2/` is checked with
    /// `scope: None`. Return `Err(OciError::Unauthorized)` to send the Basic challenge.
    async fn check(
        &self,
        authorization: Option<&str>,
        scope: Option<&Scope>,
    ) -> Result<(), OciError>;
}

#[derive(Clone)]
pub struct PushState {
    pub storage: DynStorage,
    pub auth: Arc<dyn AuthCheck>,
    /// HMAC key that signs upload ids `<pod-ip>.<uuid>.<mac>` (shared by all replicas).
    pub upload_key: Arc<Vec<u8>>,
    /// This replica's pod IP; a PATCH/PUT for an upload owned by another IP is forwarded there.
    pub pod_ip: String,
    /// Name in the serving certificate, e.g. `gitimage-push.gitimage-system.svc`.
    pub forward_host: String,
    pub forward_port: u16,
    /// CA that signed the serving certificate, for forwarding over TLS.
    pub forward_ca_pem: Option<Vec<u8>>,
}

/// Read-write `/v2/` for the controller's push endpoint. Every request is checked with
/// `state.auth`; uploads owned by another replica are forwarded to it.
///
/// # Panics
///
/// If `forward_ca_pem` is set but is not a PEM certificate.
pub fn push_router(state: PushState) -> Router {
    let forwarder = Forwarder::new(
        state.forward_host,
        state.forward_port,
        state.forward_ca_pem.as_deref(),
    )
    .expect("forward_ca_pem must be a PEM certificate");
    router(Ctx {
        storage: state.storage,
        push: Some(Push {
            auth: state.auth,
            ids: UploadIds::new(state.upload_key.to_vec(), state.pod_ip),
            forwarder,
        }),
    })
}

/// Read-only `/v2/` for `gitimage node`: pulls only, no auth; writes get 405 `UNSUPPORTED`.
pub fn read_only_router(storage: DynStorage) -> Router {
    router(Ctx {
        storage,
        push: None,
    })
}

fn router(ctx: Ctx) -> Router {
    Router::new().fallback(handle).with_state(Arc::new(ctx))
}

struct Ctx {
    storage: DynStorage,
    /// `None` for the read-only router.
    push: Option<Push>,
}

struct Push {
    auth: Arc<dyn AuthCheck>,
    ids: UploadIds,
    forwarder: Forwarder,
}

/// Finishes a response builder; builder errors only come from invalid header values.
fn build(resp: Builder, body: Body) -> Result<Response, ApiError> {
    resp.body(body)
        .map_err(|e| OciError::Backend(format!("building response: {e}")).into())
}

async fn handle(State(ctx): State<Arc<Ctx>>, req: Request) -> Response {
    let route = Route::parse(req.uri().path());
    let span = tracing::info_span!(
        "oci",
        method = %req.method(),
        route = route.as_ref().map(Route::name).unwrap_or("unknown"),
        repo = route.as_ref().ok().and_then(Route::repo).map(|r| r.path()),
        status = tracing::field::Empty,
    );
    async move {
        let result = match route {
            Ok(route) => ctx.serve(route, req).await,
            Err(e) => Err(e.into()),
        };
        let mut resp = result.unwrap_or_else(IntoResponse::into_response);
        resp.headers_mut()
            .insert(API_VERSION, HeaderValue::from_static("registry/2.0"));
        let status = resp.status().as_u16();
        tracing::Span::current().record("status", status);
        tracing::debug!(status, "request served");
        resp
    }
    .instrument(span)
    .await
}

impl Ctx {
    async fn serve(&self, route: Route, req: Request) -> Result<Response, ApiError> {
        let method = req.method().clone();
        let authorization = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let authorization = authorization.as_deref();
        if let Some(push) = &self.push {
            let scope = route.repo().map(|repo| Scope {
                repo: repo.clone(),
                access: Access::of(&method),
            });
            push.auth.check(authorization, scope.as_ref()).await?;
        } else if Access::of(&method) == Access::Push {
            return Err(OciError::Unsupported.into());
        }

        let storage = &self.storage;
        let head = method == Method::HEAD;
        let (parts, body) = req.into_parts();
        let (uri, headers) = (&parts.uri, &parts.headers);
        match (route, method) {
            (Route::Base, Method::GET | Method::HEAD) => build(
                Response::builder().header(header::CONTENT_TYPE, "application/json"),
                Body::from("{}"),
            ),
            (Route::Blob(repo, d), Method::GET | Method::HEAD) => {
                blobs::get(storage, &repo, &d, head, headers).await
            }
            (Route::Blob(repo, d), Method::DELETE) => blobs::delete(storage, &repo, &d).await,
            (Route::Manifest(repo, r), Method::GET | Method::HEAD) => {
                manifests::get(storage, &repo, &r, head).await
            }
            (Route::Manifest(repo, r), Method::PUT) => {
                manifests::put(storage, &repo, &r, headers, body).await
            }
            (Route::Manifest(repo, r), Method::DELETE) => {
                manifests::delete(storage, &repo, &r).await
            }
            (Route::Tags(repo), Method::GET | Method::HEAD) => {
                manifests::tags(storage, &repo, uri).await
            }
            (Route::Referrers(repo, d), Method::GET | Method::HEAD) => {
                manifests::referrers(storage, &repo, &d, uri).await
            }
            (Route::UploadStart(repo), Method::POST) => {
                let push = self.push.as_ref().ok_or(OciError::Unsupported)?;
                uploads::start(storage, push, &repo, authorization, uri, body).await
            }
            (Route::Upload(repo, public_id), method) => {
                let push = self.push.as_ref().ok_or(OciError::Unsupported)?;
                let id = match push.ids.decode(&repo, &public_id) {
                    Some(Owner::Local(id)) => id,
                    Some(Owner::Remote(owner)) => {
                        let req = Request::from_parts(parts, body);
                        return push.forwarder.forward(&owner, req).await;
                    }
                    None => return Err(OciError::BlobUploadUnknown.into()),
                };
                match method {
                    Method::PATCH => {
                        uploads::patch(storage, &repo, &public_id, &id, headers, body).await
                    }
                    Method::PUT => uploads::put(storage, &repo, &id, uri, body).await,
                    Method::GET => uploads::status(storage, &repo, &public_id, &id).await,
                    Method::DELETE => uploads::cancel(storage, &id).await,
                    _ => Err(OciError::Unsupported.into()),
                }
            }
            _ => Err(OciError::Unsupported.into()),
        }
    }
}
