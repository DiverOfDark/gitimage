//! Blob uploads: `POST /v2/<repo>/blobs/uploads/` (with monolithic `?digest=` and
//! cross-repo `?mount=&from=`), then `PATCH`/`PUT`/`GET`/`DELETE` on the upload id.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::Response;
use futures::{StreamExt, TryStreamExt};

use super::error::ApiError;
use super::path::parse_repo_path;
use super::{DOCKER_CONTENT_DIGEST, Push, build};
use crate::routes::{Access, Scope};
use crate::{ByteStream, Digest, DynStorage, OciError, Repo, UploadId};

const DOCKER_UPLOAD_UUID: &str = "docker-upload-uuid";

fn query(uri: &Uri) -> HashMap<String, String> {
    Query::<HashMap<String, String>>::try_from_uri(uri)
        .map(|q| q.0)
        .unwrap_or_default()
}

fn body_stream(body: Body) -> ByteStream {
    body.into_data_stream()
        .map_err(std::io::Error::other)
        .boxed()
}

/// `0-<n-1>`, the inclusive range of bytes received (`0-0` when empty, like distribution).
fn received(n: u64) -> String {
    format!("0-{}", n.saturating_sub(1))
}

fn upload_url(repo: &Repo, id: &str) -> String {
    format!("/v2/{repo}/blobs/uploads/{id}")
}

/// 202 telling the client where the upload continues and how much has arrived.
fn upload_accepted(
    status: StatusCode,
    repo: &Repo,
    id: &str,
    n: u64,
) -> Result<Response, ApiError> {
    build(
        Response::builder()
            .status(status)
            .header(header::LOCATION, upload_url(repo, id))
            .header(header::RANGE, received(n))
            .header(DOCKER_UPLOAD_UUID, id),
        Body::empty(),
    )
}

fn blob_created(repo: &Repo, d: &Digest) -> Result<Response, ApiError> {
    build(
        Response::builder()
            .status(StatusCode::CREATED)
            .header(header::LOCATION, format!("/v2/{repo}/blobs/{d}"))
            .header(DOCKER_CONTENT_DIGEST, d.as_str()),
        Body::empty(),
    )
}

fn digest_param(q: &HashMap<String, String>) -> Result<Digest, ApiError> {
    Ok(Digest::parse(
        q.get("digest").ok_or(OciError::DigestInvalid)?,
    )?)
}

/// Appends then commits; aborts the upload if either step fails.
async fn finish(
    storage: &DynStorage,
    id: &UploadId,
    body: Body,
    d: &Digest,
) -> Result<(), OciError> {
    let result = match storage.append(id, body_stream(body)).await {
        Ok(_) => storage.commit(id, d).await,
        Err(e) => Err(e),
    };
    if result.is_err() {
        // The upload may already be gone (commit consumes it); nothing else to clean up.
        let _ = storage.abort(id).await;
    }
    result
}

/// `POST /v2/<repo>/blobs/uploads/`.
pub(super) async fn start(
    storage: &DynStorage,
    push: &Push,
    repo: &Repo,
    authorization: Option<&str>,
    uri: &Uri,
    body: Body,
) -> Result<Response, ApiError> {
    let q = query(uri);
    if let Some(mount) = q.get("mount") {
        let d = Digest::parse(mount)?;
        if let Some(from) = q.get("from").and_then(|f| parse_repo_path(f)) {
            let scope = Scope {
                repo: from.clone(),
                access: Access::Pull,
            };
            // Without pull access to the source, fall back to a normal upload.
            if push.auth.check(authorization, Some(&scope)).await.is_ok()
                && storage.mount_blob(&from, repo, &d).await?
            {
                return blob_created(repo, &d);
            }
        }
    }
    let id = storage.begin_upload(repo).await?;
    if q.contains_key("digest") {
        let d = digest_param(&q)?;
        finish(storage, &id, body, &d).await?;
        return blob_created(repo, &d);
    }
    upload_accepted(StatusCode::ACCEPTED, repo, &push.ids.encode(repo, &id), 0)
}

/// Parses a PATCH `Content-Range: <start>-<end>` (optionally `bytes ` prefixed) start offset.
fn content_range_start(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let v = v.strip_prefix("bytes ").unwrap_or(v);
    v.split_once('-')?.0.trim().parse().ok()
}

/// `PATCH` on an upload owned by this replica.
pub(super) async fn patch(
    storage: &DynStorage,
    repo: &Repo,
    public_id: &str,
    id: &UploadId,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    if let Some(start) = content_range_start(headers) {
        let have = storage.upload_size(id).await?;
        if start != have {
            return Err(ApiError::range_invalid(format!(
                "chunk starts at {start}, upload has {have} bytes"
            ))
            .with_header(header::LOCATION, &upload_url(repo, public_id))
            .with_header(header::RANGE, &received(have)));
        }
    }
    let n = storage.append(id, body_stream(body)).await?;
    upload_accepted(StatusCode::ACCEPTED, repo, public_id, n)
}

/// `PUT ...?digest=` on an upload owned by this replica, with an optional final chunk.
pub(super) async fn put(
    storage: &DynStorage,
    repo: &Repo,
    id: &UploadId,
    uri: &Uri,
    body: Body,
) -> Result<Response, ApiError> {
    let d = digest_param(&query(uri))?;
    finish(storage, id, body, &d).await?;
    blob_created(repo, &d)
}

/// `GET` upload status.
pub(super) async fn status(
    storage: &DynStorage,
    repo: &Repo,
    public_id: &str,
    id: &UploadId,
) -> Result<Response, ApiError> {
    let n = storage.upload_size(id).await?;
    upload_accepted(StatusCode::NO_CONTENT, repo, public_id, n)
}

/// `DELETE` cancels the upload.
pub(super) async fn cancel(storage: &DynStorage, id: &UploadId) -> Result<Response, ApiError> {
    storage.abort(id).await?;
    build(
        Response::builder().status(StatusCode::NO_CONTENT),
        Body::empty(),
    )
}
