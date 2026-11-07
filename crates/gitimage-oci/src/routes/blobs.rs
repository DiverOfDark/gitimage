//! `HEAD`/`GET`/`DELETE /v2/<repo>/blobs/<digest>`, with single `Range` support.

use std::ops::Range;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;

use super::error::ApiError;
use super::{DOCKER_CONTENT_DIGEST, build};
use crate::{Digest, DynStorage, OciError, Repo};

/// Parses `bytes=a-b`, `bytes=a-` and `bytes=-n` against a blob of `size` bytes into a
/// half-open range. Anything else is ignored (`Ok(None)`), as HTTP allows.
fn parse_range(value: &str, size: u64) -> Result<Option<Range<u64>>, ApiError> {
    let Some((start, end)) = value
        .strip_prefix("bytes=")
        .filter(|s| !s.contains(','))
        .and_then(|s| s.split_once('-'))
    else {
        return Ok(None);
    };
    let unsatisfiable = || {
        ApiError::range_invalid(format!("range {value} outside blob of {size} bytes"))
            .with_header(header::CONTENT_RANGE, &format!("bytes */{size}"))
    };
    let (start, end) = (start.trim(), end.trim());
    let range = match (start.parse::<u64>(), end.parse::<u64>()) {
        (Ok(s), Ok(e)) if s <= e => s..e.saturating_add(1).min(size),
        (Ok(s), Err(_)) if end.is_empty() => s..size,
        (Err(_), Ok(n)) if start.is_empty() && n > 0 => size.saturating_sub(n)..size,
        _ => return Ok(None),
    };
    if range.start >= size || range.is_empty() {
        return Err(unsatisfiable());
    }
    Ok(Some(range))
}

pub(super) async fn get(
    storage: &DynStorage,
    repo: &Repo,
    digest: &str,
    head: bool,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let d = Digest::parse(digest)?;
    let size = storage
        .head_blob(repo, &d)
        .await?
        .ok_or(OciError::BlobUnknown)?;
    let range = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(v) => parse_range(v, size)?,
        None => None,
    };
    let mut resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(DOCKER_CONTENT_DIGEST, d.as_str())
        .header(header::ETAG, format!("\"{d}\""))
        .header(header::ACCEPT_RANGES, "bytes");
    let len = match &range {
        Some(r) => {
            resp = resp.status(StatusCode::PARTIAL_CONTENT).header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{size}", r.start, r.end - 1),
            );
            r.end - r.start
        }
        None => size,
    };
    resp = resp.header(header::CONTENT_LENGTH, len);
    if head {
        return build(resp, Body::empty());
    }
    let body = if len == 0 {
        Body::empty()
    } else {
        Body::from_stream(storage.get_blob(repo, &d, range).await?)
    };
    build(resp, body)
}

pub(super) async fn delete(
    storage: &DynStorage,
    repo: &Repo,
    digest: &str,
) -> Result<Response, ApiError> {
    let d = Digest::parse(digest)?;
    storage.delete_blob(repo, &d).await?;
    build(
        Response::builder().status(StatusCode::ACCEPTED),
        Body::empty(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let r = |s: &str| parse_range(s, 100).map_err(|_| ());
        assert_eq!(r("bytes=0-9"), Ok(Some(0..10)));
        assert_eq!(r("bytes=90-200"), Ok(Some(90..100)));
        assert_eq!(r("bytes=95-"), Ok(Some(95..100)));
        assert_eq!(r("bytes=-10"), Ok(Some(90..100)));
        assert_eq!(r("bytes=-1000"), Ok(Some(0..100)));
        assert_eq!(r("bytes=100-"), Err(()));
        assert_eq!(r("bytes=150-160"), Err(()));
        assert_eq!(r("bytes=5-1"), Ok(None));
        assert_eq!(r("bytes=0-1,4-5"), Ok(None));
        assert_eq!(r("items=0-1"), Ok(None));
        assert_eq!(parse_range("bytes=0-0", 0).map_err(|_| ()), Err(()));
    }
}
