//! Router tests: push and pull flows, auth, read-only mode, ranges and upload forwarding.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use tower::ServiceExt;

use super::*;
use crate::{Digest, MemoryStorage, OCI_MANIFEST, UploadId};

/// Allows requests carrying `Basic user:pass`.
struct TestAuth;

const GOOD: &str = "Basic dXNlcjpwYXNz";

#[async_trait::async_trait]
impl AuthCheck for TestAuth {
    async fn check(&self, authorization: Option<&str>, _: Option<&Scope>) -> Result<(), OciError> {
        if authorization == Some(GOOD) {
            Ok(())
        } else {
            Err(OciError::Unauthorized)
        }
    }
}

fn state(storage: DynStorage, pod_ip: &str, port: u16) -> PushState {
    PushState {
        storage,
        auth: Arc::new(TestAuth),
        upload_key: Arc::new(b"upload-key".to_vec()),
        pod_ip: pod_ip.into(),
        forward_host: "gitimage-push.test".into(),
        forward_port: port,
        forward_ca_pem: None,
    }
}

fn push() -> Router {
    push_router(state(Arc::new(MemoryStorage::new()), "10.0.0.1", 5000))
}

async fn call(
    router: &Router,
    method: Method,
    uri: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> Response {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, GOOD);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    router
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

fn hdr<'a>(resp: &'a Response, name: &str) -> &'a str {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

async fn body(resp: Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

async fn error_code(resp: Response) -> String {
    let v: serde_json::Value = serde_json::from_slice(&body(resp).await).unwrap();
    v["errors"][0]["code"].as_str().unwrap().to_string()
}

/// POST, two PATCHes, PUT: returns the blob digest.
async fn chunked_push(router: &Router, repo: &str, data: &[u8]) -> Digest {
    let resp = call(
        router,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads/"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let mut location = hdr(&resp, "location").to_string();
    let mid = data.len() / 2;
    for (start, chunk) in [(0, &data[..mid]), (mid, &data[mid..])] {
        let range = format!("{start}-{}", start + chunk.len() - 1);
        let resp = call(
            router,
            Method::PATCH,
            &location,
            chunk.to_vec(),
            &[("content-range", &range)],
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::ACCEPTED,
            "{:?}",
            body(resp).await
        );
        assert_eq!(
            hdr(&resp, "range"),
            format!("0-{}", start + chunk.len() - 1)
        );
        location = hdr(&resp, "location").to_string();
    }
    let d = Digest::of(data);
    let resp = call(
        router,
        Method::PUT,
        &format!("{location}?digest={d}"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(hdr(&resp, "docker-content-digest"), d.as_str());
    d
}

fn manifest_json(layer: &Digest, size: usize) -> Vec<u8> {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": layer.as_str(), "size": size },
        "layers": [{ "mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": layer.as_str(), "size": size }],
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn push_then_pull() {
    let router = push();
    let data: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
    let d = chunked_push(&router, "ns/app", &data).await;

    let resp = call(
        &router,
        Method::HEAD,
        &format!("/v2/ns/app/blobs/{d}"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hdr(&resp, "content-length"), "1000");
    assert_eq!(hdr(&resp, "docker-content-digest"), d.as_str());
    assert!(body(resp).await.is_empty());

    let resp = call(
        &router,
        Method::GET,
        &format!("/v2/ns/app/blobs/{d}"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body(resp).await, data);

    let resp = call(
        &router,
        Method::GET,
        &format!("/v2/ns/app/blobs/{d}"),
        vec![],
        &[("range", "bytes=10-19")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(hdr(&resp, "content-range"), "bytes 10-19/1000");
    assert_eq!(hdr(&resp, "content-length"), "10");
    assert_eq!(body(resp).await, data[10..20]);

    let resp = call(
        &router,
        Method::GET,
        &format!("/v2/ns/app/blobs/{d}"),
        vec![],
        &[("range", "bytes=1000-")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // Manifest by tag, then pull by tag and digest.
    let m = manifest_json(&d, data.len());
    let md = Digest::of(&m);
    let resp = call(
        &router,
        Method::PUT,
        "/v2/ns/app/manifests/v1",
        m.clone(),
        &[("content-type", OCI_MANIFEST)],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(hdr(&resp, "docker-content-digest"), md.as_str());
    assert_eq!(hdr(&resp, "location"), format!("/v2/ns/app/manifests/{md}"));
    for r in ["v1", md.as_str()] {
        let resp = call(
            &router,
            Method::GET,
            &format!("/v2/ns/app/manifests/{r}"),
            vec![],
            &[],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(hdr(&resp, "content-type"), OCI_MANIFEST);
        assert_eq!(hdr(&resp, "docker-content-digest"), md.as_str());
        assert_eq!(body(resp).await, m);
    }
    let resp = call(
        &router,
        Method::HEAD,
        "/v2/ns/app/manifests/v1",
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hdr(&resp, "content-length"), m.len().to_string());

    let resp = call(&router, Method::GET, "/v2/ns/app/tags/list", vec![], &[]).await;
    let v: serde_json::Value = serde_json::from_slice(&body(resp).await).unwrap();
    assert_eq!(v, serde_json::json!({"name": "ns/app", "tags": ["v1"]}));

    let resp = call(
        &router,
        Method::GET,
        "/v2/ns/app/manifests/nope",
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(resp).await, "MANIFEST_UNKNOWN");
}

#[tokio::test]
async fn monolithic_mount_and_errors() {
    let router = push();
    let d = Digest::of(b"hello");
    let resp = call(
        &router,
        Method::POST,
        &format!("/v2/ns/a/blobs/uploads/?digest={d}"),
        b"hello".to_vec(),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = call(
        &router,
        Method::POST,
        &format!("/v2/ns/b/blobs/uploads/?mount={d}&from=ns/a"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(hdr(&resp, "location"), format!("/v2/ns/b/blobs/{d}"));
    let missing = Digest::of(b"nope");
    let resp = call(
        &router,
        Method::POST,
        &format!("/v2/ns/b/blobs/uploads/?mount={missing}&from=ns/a"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = call(
        &router,
        Method::POST,
        &format!("/v2/ns/a/blobs/uploads/?digest={missing}"),
        b"hello".to_vec(),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_code(resp).await, "DIGEST_INVALID");

    let resp = call(&router, Method::GET, "/v2/ns/a/b/manifests/x", vec![], &[]).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(resp).await, "NAME_UNKNOWN");

    let resp = call(
        &router,
        Method::GET,
        &format!("/v2/ns/a/blobs/{missing}"),
        vec![],
        &[],
    )
    .await;
    assert_eq!(error_code(resp).await, "BLOB_UNKNOWN");

    let resp = call(
        &router,
        Method::PUT,
        "/v2/ns/a/manifests/v1",
        b"{not json".to_vec(),
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_code(resp).await, "MANIFEST_INVALID");

    let resp = call(
        &router,
        Method::PUT,
        "/v2/ns/a/manifests/v1",
        b"{}".to_vec(),
        &[],
    )
    .await;
    assert_eq!(error_code(resp).await, "MANIFEST_INVALID");

    let resp = call(
        &router,
        Method::PUT,
        "/v2/ns/a/manifests/v1",
        vec![b' '; 5 * 1024 * 1024],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let resp = call(
        &router,
        Method::PATCH,
        "/v2/ns/a/blobs/uploads/10.0.0.1.eA.0011",
        vec![],
        &[],
    )
    .await;
    assert_eq!(error_code(resp).await, "BLOB_UPLOAD_UNKNOWN");

    // Out-of-order chunk.
    let resp = call(
        &router,
        Method::POST,
        "/v2/ns/a/blobs/uploads/",
        vec![],
        &[],
    )
    .await;
    let location = hdr(&resp, "location").to_string();
    let resp = call(
        &router,
        Method::PATCH,
        &location,
        b"abc".to_vec(),
        &[("content-range", "5-7")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    let resp = call(&router, Method::GET, &location, vec![], &[]).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(hdr(&resp, "range"), "0-0");
    let resp = call(&router, Method::DELETE, &location, vec![], &[]).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = call(&router, Method::GET, &location, vec![], &[]).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auth_rejection_sends_basic_challenge() {
    let router = push();
    for uri in ["/v2/", "/v2/ns/app/tags/list"] {
        let req = Request::get(uri).body(Body::empty()).unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(hdr(&resp, "www-authenticate"), r#"Basic realm="gitimage""#);
        assert_eq!(error_code(resp).await, "UNAUTHORIZED");
    }
    let resp = call(&router, Method::GET, "/v2/", vec![], &[]).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body(resp).await, b"{}");
}

#[tokio::test]
async fn read_only_router_serves_pulls_and_rejects_writes() {
    let storage: DynStorage = Arc::new(MemoryStorage::new());
    let writer = push_router(state(storage.clone(), "10.0.0.1", 5000));
    let d = chunked_push(&writer, "ns/app", b"layer data").await;
    let reader = read_only_router(storage);

    // No Authorization header at all.
    for uri in ["/v2/".to_string(), format!("/v2/ns/app/blobs/{d}")] {
        let resp = reader
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    for (method, uri) in [
        (Method::POST, "/v2/ns/app/blobs/uploads/".to_string()),
        (Method::PUT, "/v2/ns/app/manifests/v1".to_string()),
        (Method::DELETE, format!("/v2/ns/app/blobs/{d}")),
        (Method::PATCH, "/v2/ns/app/blobs/uploads/x".to_string()),
        (Method::GET, "/v2/ns/app/blobs/uploads/x".to_string()),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = reader.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(error_code(resp).await, "UNSUPPORTED");
    }
}

/// Two replicas on 127.0.0.1 and 127.0.0.2 (same port) share storage and upload key.
#[tokio::test]
async fn uploads_are_forwarded_to_their_owner() {
    let a_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = a_listener.local_addr().unwrap().port();
    let b_listener = tokio::net::TcpListener::bind(("127.0.0.2", port))
        .await
        .unwrap();
    let storage: DynStorage = Arc::new(MemoryStorage::new());
    let a = push_router(state(storage.clone(), "127.0.0.1", port));
    let b = push_router(state(storage.clone(), "127.0.0.2", port));
    tokio::spawn(async move { axum::serve(a_listener, a).await.unwrap() });
    tokio::spawn(async move { axum::serve(b_listener, b).await.unwrap() });

    let client = reqwest::Client::new();
    let on = |host: &str, path: &str| format!("http://{host}:{port}{path}");
    let resp = client
        .post(on("127.0.0.1", "/v2/ns/app/blobs/uploads/"))
        .header("authorization", GOOD)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    assert!(location.contains("/127.0.0.1."), "{location}");

    // Chunk and commit through replica B: both are forwarded to A, which owns the upload.
    let data = vec![42u8; 300_000];
    let resp = client
        .patch(on("127.0.0.2", &location))
        .header("authorization", GOOD)
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.headers()["range"], "0-299999");
    let d = Digest::of(&data);
    let resp = client
        .put(on("127.0.0.2", &format!("{location}?digest={d}")))
        .header("authorization", GOOD)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(
        storage
            .head_blob(&Repo::new("ns", "app"), &d)
            .await
            .unwrap(),
        Some(300_000)
    );

    // Forwarded requests keep their Authorization header: without it the owner says 401.
    let resp = client
        .post(on("127.0.0.1", "/v2/ns/app/blobs/uploads/"))
        .header("authorization", GOOD)
        .send()
        .await
        .unwrap();
    let location = resp.headers()["location"].to_str().unwrap().to_string();
    let resp = client.get(on("127.0.0.2", &location)).send().await.unwrap();
    assert_eq!(resp.status(), 401);

    // A tampered id and an unreachable owner are both BLOB_UPLOAD_UNKNOWN.
    let tampered = location.replace("127.0.0.1", "127.0.0.2");
    let resp = client
        .get(on("127.0.0.2", &tampered))
        .header("authorization", GOOD)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let ghost = UploadIds::new(b"upload-key".to_vec(), "127.0.0.3".into())
        .encode(&Repo::new("ns", "app"), &UploadId("x".into()));
    let resp = client
        .get(on(
            "127.0.0.2",
            &format!("/v2/ns/app/blobs/uploads/{ghost}"),
        ))
        .header("authorization", GOOD)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["errors"][0]["code"], "BLOB_UPLOAD_UNKNOWN");
}

#[tokio::test]
async fn large_chunks_stream_into_s3_storage() {
    let storage: DynStorage = Arc::new(crate::S3Storage::from_store(Arc::new(
        object_store::memory::InMemory::new(),
    )));
    let router = push_router(state(storage.clone(), "10.0.0.1", 5000));
    let data: Vec<u8> = (0..(20 * 1024 * 1024 + 7))
        .map(|i: usize| (i % 249) as u8)
        .collect();
    let d = chunked_push(&router, "ns/big", &data).await;
    let resp = call(
        &router,
        Method::GET,
        &format!("/v2/ns/big/blobs/{d}"),
        vec![],
        &[("range", "bytes=9000000-")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body(resp).await, data[9_000_000..]);
}
