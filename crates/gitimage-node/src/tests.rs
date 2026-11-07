//! End-to-end tests of `serve` on real sockets.

use std::sync::Arc;
use std::time::Duration;

use gitimage_oci::{
    ByteStream, DynStorage, Manifest, MemoryStorage, OCI_MANIFEST, OciError, Reference, Repo,
    RepoIndex, S3Storage, Storage, UploadId,
};

use super::*;

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

async fn wait_ready(url: &str) {
    for _ in 0..100 {
        if reqwest::get(url).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{url} never came up");
}

#[tokio::test]
async fn serves_pulls_on_localhost_and_probes() {
    let storage: DynStorage = Arc::new(S3Storage::from_store(Arc::new(
        object_store::memory::InMemory::new(),
    )));
    let repo = Repo::new("ns", "app");
    let body = format!(r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}"}}"#);
    let m = Manifest {
        media_type: OCI_MANIFEST.into(),
        body: body.clone().into(),
    };
    storage
        .put_manifest(&repo, &Reference::parse("v1").unwrap(), m)
        .await
        .unwrap();

    let (port, health_port) = (free_port().await, free_port().await);
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(storage, port, health_port, async {
        let _ = rx.await;
    }));
    let base = format!("http://127.0.0.1:{port}");
    wait_ready(&format!("{base}/v2/")).await;

    let resp = reqwest::get(format!("{base}/v2/ns/app/manifests/v1"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], OCI_MANIFEST);
    assert_eq!(resp.text().await.unwrap(), body);
    let resp = reqwest::Client::new()
        .post(format!("{base}/v2/ns/app/blobs/uploads/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);

    for probe in ["healthz", "readyz"] {
        let resp = reqwest::get(format!("http://127.0.0.1:{health_port}/{probe}"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{probe}");
    }

    tx.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn reports_busy_port() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = taken.local_addr().unwrap().port();
    let storage: DynStorage = Arc::new(MemoryStorage::new());
    let err = serve(
        storage,
        port,
        free_port().await,
        std::future::pending::<()>(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains(&format!("port {port} in use on 127.0.0.1")),
        "{err:#}"
    );
}

/// Storage whose every call fails, for `/readyz`.
struct Broken;

#[async_trait::async_trait]
impl Storage for Broken {
    async fn head_blob(&self, _: &Repo, _: &Digest) -> Result<Option<u64>, OciError> {
        Err(OciError::Backend("down".into()))
    }
    async fn get_blob(
        &self,
        _: &Repo,
        _: &Digest,
        _: Option<std::ops::Range<u64>>,
    ) -> Result<ByteStream, OciError> {
        Err(OciError::Backend("down".into()))
    }
    async fn begin_upload(&self, _: &Repo) -> Result<UploadId, OciError> {
        Err(OciError::Unsupported)
    }
    async fn append(&self, _: &UploadId, _: ByteStream) -> Result<u64, OciError> {
        Err(OciError::Unsupported)
    }
    async fn commit(&self, _: &UploadId, _: &Digest) -> Result<(), OciError> {
        Err(OciError::Unsupported)
    }
    async fn abort(&self, _: &UploadId) -> Result<(), OciError> {
        Err(OciError::Unsupported)
    }
    async fn mount_blob(&self, _: &Repo, _: &Repo, _: &Digest) -> Result<bool, OciError> {
        Ok(false)
    }
    async fn delete_blob(&self, _: &Repo, _: &Digest) -> Result<(), OciError> {
        Err(OciError::Unsupported)
    }
    async fn get_manifest(&self, _: &Repo, _: &Reference) -> Result<Option<Manifest>, OciError> {
        Err(OciError::Backend("down".into()))
    }
    async fn head_manifest(&self, _: &Repo, _: &Reference) -> Result<Option<Digest>, OciError> {
        Err(OciError::Backend("down".into()))
    }
    async fn put_manifest(&self, _: &Repo, _: &Reference, _: Manifest) -> Result<Digest, OciError> {
        Err(OciError::Unsupported)
    }
    async fn delete_manifest(&self, _: &Repo, _: &Digest) -> Result<(), OciError> {
        Err(OciError::Unsupported)
    }
    async fn delete_tag(&self, _: &Repo, _: &str) -> Result<(), OciError> {
        Err(OciError::Unsupported)
    }
    async fn list(&self, _: &Repo) -> Result<RepoIndex, OciError> {
        Err(OciError::Backend("down".into()))
    }
    async fn delete_repo(&self, _: &Repo) -> Result<(), OciError> {
        Err(OciError::Unsupported)
    }
    fn deletes_blobs(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn readyz_fails_when_storage_is_down() {
    let (port, health_port) = (free_port().await, free_port().await);
    let server = tokio::spawn(serve(
        Arc::new(Broken),
        port,
        health_port,
        std::future::pending::<()>(),
    ));
    let health = format!("http://127.0.0.1:{health_port}");
    wait_ready(&format!("{health}/healthz")).await;
    assert_eq!(
        reqwest::get(format!("{health}/healthz"))
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        reqwest::get(format!("{health}/readyz"))
            .await
            .unwrap()
            .status(),
        503
    );
    server.abort();
}
