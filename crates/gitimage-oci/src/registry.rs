//! `Storage` that proxies to an upstream OCI registry (registry mode).
//!
//! `Repo { namespace, name }` maps to `<url-path-prefix>/<namespace>/<name>` upstream. Blob
//! bodies stream through in both directions; uploads follow the upstream's `Location`
//! headers (POST, PATCH per chunk, PUT), with the current location kept in this process.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use tokio::sync::Mutex;

use crate::{
    ByteStream, Digest, MANIFEST_MEDIA_TYPES, Manifest, OciError, Reference, Repo, RepoIndex,
    Storage, UploadId,
};

mod auth;
mod dockerconfig;

use auth::Auth;

/// Upstream registry proxy.
pub struct RegistryStorage {
    http: reqwest::Client,
    /// `https://ghcr.io`
    base: Url,
    /// `acme/builds`, or empty
    prefix: String,
    auth: Auth,
    uploads: StdMutex<HashMap<String, Arc<Mutex<Upload>>>>,
}

struct Upload {
    /// Upstream repository path, for auth scopes.
    path: String,
    /// Where the next PATCH/PUT goes, as returned by the upstream.
    location: Url,
    size: u64,
}

/// An upstream request; kept so it can be replayed after an auth challenge.
struct Req {
    method: Method,
    url: Url,
    scopes: Vec<String>,
    headers: HeaderMap,
}

fn backend(e: impl std::fmt::Display) -> OciError {
    OciError::Backend(format!("upstream registry: {e}"))
}

fn pull_scope(path: &str) -> String {
    format!("repository:{path}:pull")
}

fn push_scope(path: &str) -> String {
    format!("repository:{path}:pull,push")
}

fn delete_scope(path: &str) -> String {
    format!("repository:{path}:pull,push,delete")
}

fn manifest_accept() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::ACCEPT,
        HeaderValue::from_str(&MANIFEST_MEDIA_TYPES.join(", ")).expect("static media types"),
    );
    h
}

/// Maps an unexpected upstream reply to an error, keeping OCI error codes where they exist.
async fn upstream_error(resp: reqwest::Response) -> OciError {
    #[derive(serde::Deserialize)]
    struct Errors {
        errors: Vec<ErrorItem>,
    }
    #[derive(serde::Deserialize)]
    struct ErrorItem {
        code: String,
        #[serde(default)]
        message: String,
    }
    let status = resp.status();
    let body = resp.bytes().await.unwrap_or_default();
    let item = serde_json::from_slice::<Errors>(&body)
        .ok()
        .and_then(|e| e.errors.into_iter().next());
    match item.as_ref().map(|i| i.code.as_str()) {
        Some("BLOB_UNKNOWN") => OciError::BlobUnknown,
        Some("BLOB_UPLOAD_UNKNOWN") => OciError::BlobUploadUnknown,
        Some("DIGEST_INVALID") => OciError::DigestInvalid,
        Some("MANIFEST_UNKNOWN") => OciError::ManifestUnknown,
        Some("MANIFEST_INVALID" | "MANIFEST_BLOB_UNKNOWN") => {
            OciError::ManifestInvalid(item.map(|i| i.message).unwrap_or_default())
        }
        Some("NAME_UNKNOWN") => OciError::NameUnknown,
        Some("TAG_INVALID") => OciError::TagInvalid,
        Some("UNSUPPORTED") => OciError::Unsupported,
        _ if status == StatusCode::METHOD_NOT_ALLOWED => OciError::Unsupported,
        _ => backend(format!(
            "{status}: {}",
            String::from_utf8_lossy(&body[..body.len().min(512)])
        )),
    }
}

fn header_str(resp: &reqwest::Response, name: header::HeaderName) -> Option<&str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// Parses `0-<last>` (or `bytes=0-<last>`) into a byte count.
fn range_len(v: &str) -> Option<u64> {
    let v = v.strip_prefix("bytes=").unwrap_or(v);
    v.split_once('-')?
        .1
        .trim()
        .parse::<u64>()
        .ok()
        .map(|n| n + 1)
}

/// Returns `range` of a stream that holds the whole blob (upstream ignored `Range`).
fn cut(stream: ByteStream, range: Range<u64>) -> ByteStream {
    let mut pos = 0u64;
    stream
        .try_filter_map(move |chunk: Bytes| {
            let (start, end) = (pos, pos + chunk.len() as u64);
            pos = end;
            let from = range.start.clamp(start, end) - start;
            let to = range.end.clamp(start, end) - start;
            futures::future::ok((from < to).then(|| chunk.slice(from as usize..to as usize)))
        })
        .boxed()
}

impl RegistryStorage {
    /// `url` is `<host>[:port][/<prefix>]`, e.g. `ghcr.io/acme/builds` or `registry:5000/builds`
    /// (an explicit `https://`/`http://` is also accepted). `insecure_http` talks plain HTTP.
    pub fn new(
        url: &str,
        username: Option<String>,
        password: Option<String>,
        insecure_http: bool,
    ) -> Result<Self, OciError> {
        let (base, prefix) = parse_url(url, insecure_http)?;
        let credentials = username.zip(password);
        let http = reqwest::Client::builder()
            .user_agent(concat!("gitimage/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(backend)?;
        Ok(Self {
            http,
            base,
            prefix,
            auth: Auth::new(credentials),
            uploads: StdMutex::default(),
        })
    }

    /// Like `new`, with the credentials for `url`'s host taken from a `.dockerconfigjson`
    /// value (anonymous when it has no entry for the host).
    pub fn from_dockerconfigjson(
        url: &str,
        json: &[u8],
        insecure_http: bool,
    ) -> Result<Self, OciError> {
        let (base, _) = parse_url(url, insecure_http)?;
        let host = match base.port() {
            Some(p) => format!("{}:{p}", base.host_str().unwrap_or_default()),
            None => base.host_str().unwrap_or_default().to_string(),
        };
        let host = if host == "registry-1.docker.io" {
            "docker.io".to_string()
        } else {
            host
        };
        let creds = dockerconfig::credentials_for(json, &host)?;
        let (u, p) = creds.unzip();
        Self::new(url, u, p, insecure_http)
    }

    /// Upstream repository path for `repo`.
    fn path(&self, repo: &Repo) -> String {
        if self.prefix.is_empty() {
            repo.path()
        } else {
            format!("{}/{}", self.prefix, repo.path())
        }
    }

    fn url(&self, path: &str, tail: &str) -> Result<Url, OciError> {
        self.base
            .join(&format!("/v2/{path}/{tail}"))
            .map_err(backend)
    }

    fn req(&self, method: Method, url: Url, scopes: Vec<String>) -> Req {
        Req {
            method,
            url,
            scopes,
            headers: HeaderMap::new(),
        }
    }

    async fn send_once(
        &self,
        req: &Req,
        body: Option<reqwest::Body>,
    ) -> Result<reqwest::Response, OciError> {
        let mut rb = self
            .http
            .request(req.method.clone(), req.url.clone())
            .headers(req.headers.clone());
        if let Some(auth) = self.auth.header(&self.http, &req.scopes).await? {
            rb = rb.header(header::AUTHORIZATION, auth);
        }
        if let Some(body) = body {
            rb = rb.body(body);
        }
        rb.send().await.map_err(backend)
    }

    /// Sends a request with a replayable body, answering one auth challenge.
    async fn send(&self, req: Req, body: Option<Bytes>) -> Result<reqwest::Response, OciError> {
        let resp = self.send_once(&req, body.clone().map(Into::into)).await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let www = header_str(&resp, header::WWW_AUTHENTICATE).map(str::to_string);
        if !self.auth.challenged(www.as_deref(), &req.scopes).await {
            return Ok(resp);
        }
        self.send_once(&req, body.map(Into::into)).await
    }

    fn upload(&self, id: &UploadId) -> Result<Arc<Mutex<Upload>>, OciError> {
        self.uploads
            .lock()
            .expect("uploads lock")
            .get(&id.0)
            .cloned()
            .ok_or(OciError::BlobUploadUnknown)
    }

    fn take_upload(&self, id: &UploadId) -> Result<Arc<Mutex<Upload>>, OciError> {
        self.uploads
            .lock()
            .expect("uploads lock")
            .remove(&id.0)
            .ok_or(OciError::BlobUploadUnknown)
    }

    fn location(&self, resp: &reqwest::Response) -> Result<Url, OciError> {
        let loc = header_str(resp, header::LOCATION)
            .ok_or_else(|| backend("upload response without Location"))?;
        resp.url().join(loc).map_err(backend)
    }

    async fn delete_manifest_ref(&self, repo: &Repo, reference: &str) -> Result<(), OciError> {
        let path = self.path(repo);
        let url = self.url(&path, &format!("manifests/{reference}"))?;
        let resp = self
            .send(
                self.req(Method::DELETE, url, vec![delete_scope(&path)]),
                None,
            )
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(()),
            StatusCode::NOT_FOUND => Err(OciError::ManifestUnknown),
            _ => Err(upstream_error(resp).await),
        }
    }

    async fn tags(&self, repo: &Repo) -> Result<Vec<String>, OciError> {
        #[derive(serde::Deserialize)]
        struct TagList {
            #[serde(default)]
            tags: Option<Vec<String>>,
        }
        let path = self.path(repo);
        let mut url = self.url(&path, "tags/list?n=1000")?;
        let mut tags = Vec::new();
        loop {
            let resp = self
                .send(
                    self.req(Method::GET, url.clone(), vec![pull_scope(&path)]),
                    None,
                )
                .await?;
            if resp.status() == StatusCode::NOT_FOUND {
                return Ok(tags);
            }
            if !resp.status().is_success() {
                return Err(upstream_error(resp).await);
            }
            // `Link: </v2/...?last=x&n=1000>; rel="next"`
            let next = header_str(&resp, header::LINK)
                .and_then(|l| l.split_once('<'))
                .and_then(|(_, l)| l.split_once('>'))
                .and_then(|(l, _)| resp.url().join(l).ok());
            let page: TagList = resp.json().await.map_err(backend)?;
            tags.extend(page.tags.unwrap_or_default());
            match next {
                Some(n) => url = n,
                None => return Ok(tags),
            }
        }
    }
}

/// Splits `[scheme://]host[:port][/prefix]` into a base URL and a path prefix.
fn parse_url(url: &str, insecure_http: bool) -> Result<(Url, String), OciError> {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, rest)) => (s, rest),
        None if insecure_http => ("http", url),
        None => ("https", url),
    };
    let (host, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if host.is_empty() {
        return Err(backend(format!("invalid registry url {url}")));
    }
    let host = match host {
        "docker.io" | "index.docker.io" => "registry-1.docker.io",
        h => h,
    };
    let base = Url::parse(&format!("{scheme}://{host}"))
        .map_err(|e| backend(format!("invalid registry url {url}: {e}")))?;
    Ok((base, prefix.trim_matches('/').to_string()))
}

#[async_trait::async_trait]
impl Storage for RegistryStorage {
    async fn head_blob(&self, repo: &Repo, d: &Digest) -> Result<Option<u64>, OciError> {
        let path = self.path(repo);
        let url = self.url(&path, &format!("blobs/{d}"))?;
        let resp = self
            .send(self.req(Method::HEAD, url, vec![pull_scope(&path)]), None)
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(Some(
                header_str(&resp, header::CONTENT_LENGTH)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| backend("blob HEAD without Content-Length"))?,
            )),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(upstream_error(resp).await),
        }
    }

    async fn get_blob(
        &self,
        repo: &Repo,
        d: &Digest,
        range: Option<Range<u64>>,
    ) -> Result<ByteStream, OciError> {
        let path = self.path(repo);
        let url = self.url(&path, &format!("blobs/{d}"))?;
        let mut req = self.req(Method::GET, url, vec![pull_scope(&path)]);
        if let Some(r) = &range {
            let v = format!("bytes={}-{}", r.start, r.end.saturating_sub(1));
            req.headers
                .insert(header::RANGE, HeaderValue::from_str(&v).map_err(backend)?);
        }
        let resp = self.send(req, None).await?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Err(OciError::BlobUnknown);
        }
        if !status.is_success() {
            return Err(upstream_error(resp).await);
        }
        let stream = resp.bytes_stream().map_err(std::io::Error::other).boxed();
        Ok(match range {
            Some(r) if status == StatusCode::OK => cut(stream, r),
            _ => stream,
        })
    }

    async fn begin_upload(&self, repo: &Repo) -> Result<UploadId, OciError> {
        let path = self.path(repo);
        let url = self.url(&path, "blobs/uploads/")?;
        let resp = self
            .send(self.req(Method::POST, url, vec![push_scope(&path)]), None)
            .await?;
        if resp.status() != StatusCode::ACCEPTED {
            return Err(upstream_error(resp).await);
        }
        let location = self.location(&resp)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.uploads.lock().expect("uploads lock").insert(
            id.clone(),
            Arc::new(Mutex::new(Upload {
                path,
                location,
                size: 0,
            })),
        );
        Ok(UploadId(id))
    }

    async fn append(&self, id: &UploadId, mut chunk: ByteStream) -> Result<u64, OciError> {
        let upload = self.upload(id)?;
        let mut upload = upload.lock().await;
        // Skip the round trip for empty chunks (e.g. a PUT without a final chunk).
        let first = match chunk.next().await {
            None => return Ok(upload.size),
            Some(first) => first.map_err(backend)?,
        };
        let sent = Arc::new(AtomicU64::new(0));
        let counter = sent.clone();
        let body = futures::stream::once(futures::future::ok(first))
            .chain(chunk)
            .inspect_ok(move |b| {
                counter.fetch_add(b.len() as u64, Ordering::Relaxed);
            });
        let mut req = self.req(
            Method::PATCH,
            upload.location.clone(),
            vec![push_scope(&upload.path)],
        );
        req.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        let resp = self
            .send_once(&req, Some(reqwest::Body::wrap_stream(body)))
            .await?;
        if resp.status() != StatusCode::ACCEPTED {
            return Err(upstream_error(resp).await);
        }
        upload.location = self.location(&resp)?;
        upload.size = header_str(&resp, header::RANGE)
            .and_then(range_len)
            .unwrap_or(upload.size + sent.load(Ordering::Relaxed));
        Ok(upload.size)
    }

    async fn upload_size(&self, id: &UploadId) -> Result<u64, OciError> {
        Ok(self.upload(id)?.lock().await.size)
    }

    async fn commit(&self, id: &UploadId, d: &Digest) -> Result<(), OciError> {
        let upload = self.take_upload(id)?;
        let upload = upload.lock().await;
        let mut url = upload.location.clone();
        url.query_pairs_mut().append_pair("digest", d.as_str());
        let resp = self
            .send(
                self.req(Method::PUT, url, vec![push_scope(&upload.path)]),
                None,
            )
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(upstream_error(resp).await)
        }
    }

    async fn abort(&self, id: &UploadId) -> Result<(), OciError> {
        let upload = self.take_upload(id)?;
        let upload = upload.lock().await;
        let req = self.req(
            Method::DELETE,
            upload.location.clone(),
            vec![push_scope(&upload.path)],
        );
        // Registries expire abandoned uploads themselves; a failed cancel is not an error.
        if let Err(e) = self.send(req, None).await {
            tracing::debug!(error = %e, "cancelling upstream upload failed");
        }
        Ok(())
    }

    async fn mount_blob(&self, from: &Repo, to: &Repo, d: &Digest) -> Result<bool, OciError> {
        let (from_path, to_path) = (self.path(from), self.path(to));
        let mut url = self.url(&to_path, "blobs/uploads/")?;
        url.query_pairs_mut()
            .append_pair("mount", d.as_str())
            .append_pair("from", &from_path);
        let scopes = vec![push_scope(&to_path), pull_scope(&from_path)];
        let resp = self
            .send(self.req(Method::POST, url, scopes.clone()), None)
            .await?;
        match resp.status() {
            StatusCode::CREATED => Ok(true),
            StatusCode::ACCEPTED => {
                // The upstream started an upload instead; cancel it.
                if let Ok(loc) = self.location(&resp) {
                    let _ = self.send(self.req(Method::DELETE, loc, scopes), None).await;
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn delete_blob(&self, _repo: &Repo, _d: &Digest) -> Result<(), OciError> {
        Err(OciError::Unsupported)
    }

    async fn get_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Manifest>, OciError> {
        let path = self.path(repo);
        let url = self.url(&path, &format!("manifests/{}", reference_str(r)))?;
        let mut req = self.req(Method::GET, url, vec![pull_scope(&path)]);
        req.headers = manifest_accept();
        let resp = self.send(req, None).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(upstream_error(resp).await);
        }
        let media_type = header_str(&resp, header::CONTENT_TYPE)
            .map(|v| v.split(';').next().unwrap_or_default().trim().to_string())
            .ok_or_else(|| backend("manifest without Content-Type"))?;
        let body = resp.bytes().await.map_err(backend)?;
        Ok(Some(Manifest { media_type, body }))
    }

    async fn head_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Digest>, OciError> {
        let path = self.path(repo);
        let url = self.url(&path, &format!("manifests/{}", reference_str(r)))?;
        let mut req = self.req(Method::HEAD, url, vec![pull_scope(&path)]);
        req.headers = manifest_accept();
        let resp = self.send(req, None).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(upstream_error(resp).await);
        }
        match header_str(
            &resp,
            header::HeaderName::from_static("docker-content-digest"),
        ) {
            Some(d) => Digest::parse(d).map(Some),
            None => Ok(self.get_manifest(repo, r).await?.map(|m| m.digest())),
        }
    }

    async fn put_manifest(
        &self,
        repo: &Repo,
        r: &Reference,
        m: Manifest,
    ) -> Result<Digest, OciError> {
        let digest = m.digest();
        if let Reference::Digest(d) = r
            && d != &digest
        {
            return Err(OciError::DigestInvalid);
        }
        let path = self.path(repo);
        let url = self.url(&path, &format!("manifests/{}", reference_str(r)))?;
        let mut req = self.req(Method::PUT, url, vec![push_scope(&path)]);
        req.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&m.media_type).map_err(backend)?,
        );
        let resp = self.send(req, Some(m.body)).await?;
        if resp.status().is_success() {
            Ok(digest)
        } else {
            Err(upstream_error(resp).await)
        }
    }

    async fn delete_manifest(&self, repo: &Repo, d: &Digest) -> Result<(), OciError> {
        self.delete_manifest_ref(repo, d.as_str()).await
    }

    async fn delete_tag(&self, repo: &Repo, tag: &str) -> Result<(), OciError> {
        self.delete_manifest_ref(repo, tag).await
    }

    async fn list(&self, repo: &Repo) -> Result<RepoIndex, OciError> {
        let mut index = RepoIndex::default();
        for tag in self.tags(repo).await? {
            if let Some(d) = self
                .head_manifest(repo, &Reference::Tag(tag.clone()))
                .await?
            {
                index.tags.push((tag, d));
            }
        }
        index.tags.sort();
        index.manifests = index.tags.iter().map(|(_, d)| d.clone()).collect();
        index.manifests.sort();
        index.manifests.dedup();
        Ok(index)
    }

    async fn delete_repo(&self, repo: &Repo) -> Result<(), OciError> {
        let mut first_error = None;
        for d in self.list(repo).await?.manifests {
            match self.delete_manifest(repo, &d).await {
                Ok(()) | Err(OciError::ManifestUnknown) => {}
                Err(e) => {
                    tracing::warn!(%repo, digest = %d, error = %e, "deleting upstream manifest failed");
                    first_error.get_or_insert(e);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn deletes_blobs(&self) -> bool {
        false
    }
}

fn reference_str(r: &Reference) -> &str {
    match r {
        Reference::Tag(t) => t,
        Reference::Digest(d) => d.as_str(),
    }
}

#[cfg(test)]
mod tests;
