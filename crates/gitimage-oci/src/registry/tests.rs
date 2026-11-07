//! `RegistryStorage` against fake upstreams built from this crate's own `push_router`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::*;
use crate::routes::{AuthCheck, PushState, Scope, push_router};
use crate::{DynStorage, MemoryStorage, OCI_MANIFEST};

/// Accepts exactly one `Authorization` value.
struct Exact(String);

#[async_trait::async_trait]
impl AuthCheck for Exact {
    async fn check(&self, authorization: Option<&str>, _: Option<&Scope>) -> Result<(), OciError> {
        if authorization == Some(self.0.as_str()) {
            Ok(())
        } else {
            Err(OciError::Unauthorized)
        }
    }
}

fn upstream_router(storage: DynStorage, accepted: &str) -> axum::Router {
    push_router(PushState {
        storage,
        auth: Arc::new(Exact(accepted.into())),
        upload_key: Arc::new(b"k".to_vec()),
        pod_ip: "127.0.0.1".into(),
        forward_host: "localhost".into(),
        forward_port: 1,
        forward_ca_pem: None,
    })
}

async fn bind() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn spawn(listener: tokio::net::TcpListener, router: axum::Router) {
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
}

const BASIC: &str = "Basic dXNlcjpwYXNz"; // user:pass

async fn round_trip(s: &RegistryStorage, upstream: &DynStorage) {
    let repo = Repo::new("ns", "app");
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    let d = Digest::of(&data);

    let id = s.begin_upload(&repo).await.unwrap();
    for part in data.chunks(70_000) {
        let body = futures::stream::iter(vec![Ok(Bytes::copy_from_slice(part))]).boxed();
        s.append(&id, body).await.unwrap();
    }
    assert_eq!(s.upload_size(&id).await.unwrap(), data.len() as u64);
    assert_eq!(
        s.append(&id, futures::stream::empty().boxed())
            .await
            .unwrap(),
        200_000
    );
    s.commit(&id, &d).await.unwrap();
    assert_eq!(upstream.head_blob(&repo, &d).await.unwrap(), Some(200_000));

    assert_eq!(s.head_blob(&repo, &d).await.unwrap(), Some(200_000));
    assert_eq!(s.head_blob(&repo, &Digest::of(b"x")).await.unwrap(), None);
    let got: Vec<Bytes> = s
        .get_blob(&repo, &d, None)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(got.concat(), data);
    let got: Vec<Bytes> = s
        .get_blob(&repo, &d, Some(10..20))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(got.concat(), data[10..20]);

    // Digest mismatch is reported as such.
    let id = s.begin_upload(&repo).await.unwrap();
    s.append(
        &id,
        futures::stream::iter(vec![Ok(Bytes::from_static(b"abc"))]).boxed(),
    )
    .await
    .unwrap();
    assert!(matches!(
        s.commit(&id, &d).await,
        Err(OciError::DigestInvalid)
    ));

    let body = format!(r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","layers":[]}}"#);
    let m = Manifest {
        media_type: OCI_MANIFEST.into(),
        body: Bytes::from(body),
    };
    let tag = Reference::parse("v1").unwrap();
    let md = s.put_manifest(&repo, &tag, m.clone()).await.unwrap();
    assert_eq!(s.get_manifest(&repo, &tag).await.unwrap(), Some(m.clone()));
    assert_eq!(
        s.head_manifest(&repo, &tag).await.unwrap(),
        Some(md.clone())
    );
    assert_eq!(
        s.get_manifest(&repo, &Reference::parse("nope").unwrap())
            .await
            .unwrap(),
        None
    );
    let index = s.list(&repo).await.unwrap();
    assert_eq!(index.tags, vec![("v1".to_string(), md.clone())]);
    assert_eq!(index.manifests, vec![md.clone()]);
    assert!(index.blobs.is_empty());

    let other = Repo::new("ns", "other");
    assert!(s.mount_blob(&repo, &other, &d).await.unwrap());
    assert!(
        !s.mount_blob(&repo, &other, &Digest::of(b"nope"))
            .await
            .unwrap()
    );
    assert_eq!(upstream.head_blob(&other, &d).await.unwrap(), Some(200_000));

    assert!(matches!(
        s.delete_blob(&repo, &d).await,
        Err(OciError::Unsupported)
    ));
    assert!(!s.deletes_blobs());
    s.delete_repo(&repo).await.unwrap();
    assert_eq!(upstream.list(&repo).await.unwrap().manifests, vec![]);
    assert_eq!(s.list(&repo).await.unwrap(), RepoIndex::default());
}

#[tokio::test]
async fn proxies_to_basic_auth_upstream() {
    let upstream: DynStorage = Arc::new(MemoryStorage::new());
    let router = upstream_router(upstream.clone(), BASIC);
    let (listener, port) = bind().await;
    spawn(listener, router);
    let url = format!("127.0.0.1:{port}");

    let s = RegistryStorage::new(&url, Some("user".into()), Some("pass".into()), true).unwrap();
    round_trip(&s, &upstream).await;

    let anonymous = RegistryStorage::new(&url, None, None, true).unwrap();
    assert!(
        anonymous
            .head_blob(&Repo::new("ns", "app"), &Digest::of(b""))
            .await
            .is_err()
    );

    let json = format!(r#"{{"auths":{{"{url}":{{"username":"user","password":"pass"}}}}}}"#);
    let s = RegistryStorage::from_dockerconfigjson(&url, json.as_bytes(), true).unwrap();
    assert_eq!(
        s.head_blob(&Repo::new("ns", "app"), &Digest::of(b""))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn fetches_and_caches_bearer_tokens() {
    let upstream: DynStorage = Arc::new(MemoryStorage::new());
    let (listener, port) = bind().await;
    let realm = format!("http://127.0.0.1:{port}/token");
    let fetches = Arc::new(AtomicUsize::new(0));

    let counter = fetches.clone();
    let token = get(move |headers: HeaderMap| {
        let counter = counter.clone();
        async move {
            if headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                != Some(BASIC)
            {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            counter.fetch_add(1, Ordering::SeqCst);
            axum::Json(serde_json::json!({ "token": "tok", "expires_in": 300 })).into_response()
        }
    });
    let challenge = format!(r#"Bearer realm="{realm}",service="fake""#);
    let router = axum::Router::new()
        .route("/token", token)
        .merge(upstream_router(upstream.clone(), "Bearer tok"))
        .layer(axum::middleware::map_response(move |mut resp: Response| {
            let challenge = challenge.clone();
            async move {
                if resp.status() == StatusCode::UNAUTHORIZED {
                    resp.headers_mut()
                        .insert(header::WWW_AUTHENTICATE, challenge.parse().unwrap());
                }
                resp
            }
        }));
    spawn(listener, router);

    let s = RegistryStorage::new(
        &format!("127.0.0.1:{port}"),
        Some("user".into()),
        Some("pass".into()),
        true,
    )
    .unwrap();
    round_trip(&s, &upstream).await;
    // One token per distinct scope set (pull; push; push+pull for mount; delete), not per request.
    let n = fetches.load(Ordering::SeqCst);
    assert!((1..=5).contains(&n), "{n} token fetches");
}

#[test]
fn url_mapping() {
    let s = RegistryStorage::new("ghcr.io/acme/builds/", None, None, false).unwrap();
    assert_eq!(s.base.as_str(), "https://ghcr.io/");
    assert_eq!(s.path(&Repo::new("ns", "app")), "acme/builds/ns/app");
    let s = RegistryStorage::new("registry:5000", None, None, true).unwrap();
    assert_eq!(s.base.as_str(), "http://registry:5000/");
    assert_eq!(s.path(&Repo::new("ns", "app")), "ns/app");
    let s = RegistryStorage::new("docker.io/acme", None, None, false).unwrap();
    assert_eq!(s.base.as_str(), "https://registry-1.docker.io/");
    assert_eq!(range_len("0-99"), Some(100));
}

#[tokio::test]
async fn cut_slices_a_full_stream() {
    let chunks = vec![
        Ok(Bytes::from_static(b"abcd")),
        Ok(Bytes::from_static(b"efgh")),
    ];
    let got: Vec<Bytes> = cut(futures::stream::iter(chunks).boxed(), 3..6)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(got.concat(), b"def");
}
