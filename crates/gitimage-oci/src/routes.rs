//! axum routes for `/v2/`. Read-write for the push endpoint, read-only for gitimage-node.

use std::sync::Arc;

use crate::{DynStorage, OciError, Repo};

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

pub fn push_router(_state: PushState) -> axum::Router {
    todo!("gitimage-oci agent")
}

pub fn read_only_router(_storage: DynStorage) -> axum::Router {
    todo!("gitimage-oci agent")
}
