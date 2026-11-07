//! Serves `push_router` over `MemoryStorage` with no auth, for the OCI distribution-spec
//! conformance suite (`hack/oci-conformance.sh`). Usage: `conformance_server [addr]`.

use std::sync::Arc;

use gitimage_oci::{AuthCheck, MemoryStorage, OciError, PushState, Scope, push_router};

struct AllowAll;

#[async_trait::async_trait]
impl AuthCheck for AllowAll {
    async fn check(&self, _: Option<&str>, _: Option<&Scope>) -> Result<(), OciError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5000".into());
    let router = push_router(PushState {
        storage: Arc::new(MemoryStorage::new()),
        auth: Arc::new(AllowAll),
        upload_key: Arc::new(b"conformance".to_vec()),
        pod_ip: "127.0.0.1".into(),
        forward_host: "localhost".into(),
        forward_port: 5000,
        forward_ca_pem: None,
    });
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "conformance server listening");
    axum::serve(listener, router).await
}
