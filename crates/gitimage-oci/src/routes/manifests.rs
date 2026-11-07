//! Manifests, tag listing and the referrers API.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::Query;
use axum::http::{HeaderMap, HeaderName, StatusCode, Uri, header};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{Value, json};

use super::error::ApiError;
use super::{DOCKER_CONTENT_DIGEST, build};
use crate::{
    Digest, DynStorage, MANIFEST_MEDIA_TYPES, Manifest, OCI_INDEX, OCI_MANIFEST, OciError,
    Reference, Repo,
};

/// Largest manifest accepted on push.
pub(super) const MAX_MANIFEST: usize = 4 * 1024 * 1024;

const OCI_SUBJECT: HeaderName = HeaderName::from_static("oci-subject");
const OCI_FILTERS_APPLIED: HeaderName = HeaderName::from_static("oci-filters-applied");

fn json_response(content_type: &str, value: &Value) -> Result<Response, ApiError> {
    build(
        Response::builder().header(header::CONTENT_TYPE, content_type),
        Body::from(value.to_string()),
    )
}

pub(super) async fn get(
    storage: &DynStorage,
    repo: &Repo,
    reference: &str,
    head: bool,
) -> Result<Response, ApiError> {
    let r = Reference::parse(reference)?;
    let m = storage
        .get_manifest(repo, &r)
        .await?
        .ok_or(OciError::ManifestUnknown)?;
    let d = m.digest();
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, &m.media_type)
        .header(DOCKER_CONTENT_DIGEST, d.as_str())
        .header(header::ETAG, format!("\"{d}\""))
        .header(header::CONTENT_LENGTH, m.body.len());
    build(
        resp,
        if head {
            Body::empty()
        } else {
            Body::from(m.body)
        },
    )
}

/// The media type of a pushed manifest: its `mediaType` field, else the `Content-Type`.
fn media_type(doc: &Value, headers: &HeaderMap) -> Result<String, OciError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or_default().trim().to_string())
        .filter(|v| !v.is_empty());
    let media_type = match doc.get("mediaType") {
        Some(Value::String(s)) => s.clone(),
        Some(_) => {
            return Err(OciError::ManifestInvalid(
                "mediaType is not a string".into(),
            ));
        }
        None => content_type
            .ok_or_else(|| OciError::ManifestInvalid("no mediaType or Content-Type".into()))?,
    };
    if !MANIFEST_MEDIA_TYPES.contains(&media_type.as_str()) {
        return Err(OciError::ManifestInvalid(format!(
            "unsupported media type {media_type}"
        )));
    }
    Ok(media_type)
}

fn subject(doc: &Value) -> Option<&str> {
    doc.get("subject")?.get("digest")?.as_str()
}

pub(super) async fn put(
    storage: &DynStorage,
    repo: &Repo,
    reference: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let r = Reference::parse(reference)?;
    let body: Bytes = axum::body::to_bytes(body, MAX_MANIFEST)
        .await
        .map_err(|_| ApiError::size_invalid(format!("manifest over {MAX_MANIFEST} bytes")))?;
    let doc: Value = serde_json::from_slice(&body)
        .map_err(|e| OciError::ManifestInvalid(format!("not JSON: {e}")))?;
    if !doc.is_object() {
        return Err(OciError::ManifestInvalid("not a JSON object".into()).into());
    }
    let media_type = media_type(&doc, headers)?;
    let d = storage
        .put_manifest(repo, &r, Manifest { media_type, body })
        .await?;
    let mut resp = Response::builder()
        .status(StatusCode::CREATED)
        .header(header::LOCATION, format!("/v2/{repo}/manifests/{d}"))
        .header(DOCKER_CONTENT_DIGEST, d.as_str());
    if let Some(s) = subject(&doc) {
        resp = resp.header(OCI_SUBJECT, s);
    }
    build(resp, Body::empty())
}

pub(super) async fn delete(
    storage: &DynStorage,
    repo: &Repo,
    reference: &str,
) -> Result<Response, ApiError> {
    match Reference::parse(reference)? {
        Reference::Digest(d) => storage.delete_manifest(repo, &d).await?,
        Reference::Tag(t) => storage.delete_tag(repo, &t).await?,
    }
    build(
        Response::builder().status(StatusCode::ACCEPTED),
        Body::empty(),
    )
}

/// `GET /v2/<repo>/tags/list?n=&last=`, lexically sorted, with a `Link` to the next page.
pub(super) async fn tags(
    storage: &DynStorage,
    repo: &Repo,
    uri: &Uri,
) -> Result<Response, ApiError> {
    let q = Query::<HashMap<String, String>>::try_from_uri(uri)
        .map(|q| q.0)
        .unwrap_or_default();
    let mut tags: Vec<String> = storage
        .list(repo)
        .await?
        .tags
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    tags.sort();
    if let Some(last) = q.get("last") {
        tags.retain(|t| t > last);
    }
    let mut next = None;
    if let Some(n) = q.get("n").and_then(|n| n.parse::<usize>().ok())
        && tags.len() > n
    {
        tags.truncate(n);
        next = tags
            .last()
            .map(|last| format!(r#"</v2/{repo}/tags/list?n={n}&last={last}>; rel="next""#));
    }
    let mut resp = json_response(
        "application/json",
        &json!({ "name": repo.path(), "tags": tags }),
    )?;
    if let Some(link) = next
        && let Ok(v) = link.parse()
    {
        resp.headers_mut().insert(header::LINK, v);
    }
    Ok(resp)
}

/// Descriptor of `m` for the referrers index, if its `subject` is `target`.
fn referrer(m: &Manifest, target: &Digest) -> Option<Value> {
    let doc: Value = serde_json::from_slice(&m.body).ok()?;
    if subject(&doc)? != target.as_str() {
        return None;
    }
    let artifact_type = doc.get("artifactType").cloned().or_else(|| {
        (m.media_type == OCI_MANIFEST)
            .then(|| doc.get("config")?.get("mediaType").cloned())
            .flatten()
    });
    let mut desc = json!({
        "mediaType": m.media_type,
        "digest": m.digest().as_str(),
        "size": m.body.len(),
    });
    if let Some(t) = artifact_type {
        desc["artifactType"] = t;
    }
    if let Some(a) = doc.get("annotations") {
        desc["annotations"] = a.clone();
    }
    Some(desc)
}

/// `GET /v2/<repo>/referrers/<digest>?artifactType=`: scans the repo's manifests.
pub(super) async fn referrers(
    storage: &DynStorage,
    repo: &Repo,
    digest: &str,
    uri: &Uri,
) -> Result<Response, ApiError> {
    let target = Digest::parse(digest)?;
    let q = Query::<HashMap<String, String>>::try_from_uri(uri)
        .map(|q| q.0)
        .unwrap_or_default();
    let filter = q.get("artifactType");
    let mut manifests = Vec::new();
    for d in storage.list(repo).await?.manifests {
        let Some(m) = storage.get_manifest(repo, &Reference::Digest(d)).await? else {
            continue;
        };
        if let Some(desc) = referrer(&m, &target)
            && filter.is_none_or(|f| desc.get("artifactType").and_then(Value::as_str) == Some(f))
        {
            manifests.push(desc);
        }
    }
    let index = json!({ "schemaVersion": 2, "mediaType": OCI_INDEX, "manifests": manifests });
    let mut resp = json_response(OCI_INDEX, &index)?;
    if filter.is_some() {
        resp.headers_mut().insert(
            OCI_FILTERS_APPLIED,
            header::HeaderValue::from_static("artifactType"),
        );
    }
    Ok(resp)
}
