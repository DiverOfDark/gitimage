//! gitimage-node: the read-only OCI server containerd pulls from in s3 mode.
//!
//! It listens on `127.0.0.1` only (containerd's registry mirror for the push endpoint host
//! points there), serves `gitimage_oci::read_only_router` over the storage, and exposes
//! `/healthz` and `/readyz` on all interfaces for the kubelet.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};

use anyhow::Context as _;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use gitimage_oci::{Digest, DynStorage, Repo, read_only_router};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Serves pulls on `127.0.0.1:<port>` and probes on `0.0.0.0:<health_port>` until `shutdown`
/// resolves, then drains both servers.
pub async fn serve(
    storage: DynStorage,
    port: u16,
    health_port: u16,
    shutdown: impl Future,
) -> anyhow::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("port {port} in use on 127.0.0.1"))?;
    let health_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, health_port));
    let health_listener = TcpListener::bind(health_addr)
        .await
        .with_context(|| format!("binding health port {health_port}"))?;
    tracing::info!(%addr, %health_addr, "gitimage node serving");

    let (stop_tx, stop_rx) = watch::channel(false);
    let stopped = |mut rx: watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    let registry = axum::serve(listener, read_only_router(storage.clone()))
        .with_graceful_shutdown(stopped(stop_rx.clone()));
    let health = axum::serve(health_listener, health_router(storage))
        .with_graceful_shutdown(stopped(stop_rx));
    let servers = async {
        tokio::try_join!(async { registry.await.context("registry server") }, async {
            health.await.context("health server")
        },)
    };
    tokio::pin!(servers);
    tokio::select! {
        result = &mut servers => return result.map(|_| ()),
        _ = shutdown => {
            tracing::info!("shutting down");
            let _ = stop_tx.send(true);
        }
    }
    servers.await.map(|_| ())
}

fn health_router(storage: DynStorage) -> Router {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(ready))
        .with_state(storage)
}

/// Ready when the storage answers a cheap lookup (a blob that never exists).
async fn ready(State(storage): State<DynStorage>) -> StatusCode {
    let probe = Repo::new("gitimage-readyz", "probe");
    match storage.head_blob(&probe, &Digest::of(b"")).await {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::warn!(error = %e, "storage not ready");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

#[cfg(test)]
mod tests;
