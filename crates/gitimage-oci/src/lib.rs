//! OCI distribution for GitImage: the subset of the distribution spec that BuildKit and
//! containerd use, served read-write by the controller's push endpoint and read-only by
//! `gitimage node`, over a `Storage` backend (S3 via object_store, or an upstream registry).
//!
//! INTERFACE CONTRACT: the controller and node depend on every public item in this file.
//! Implementations may change; signatures change only together with their callers.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::BoxStream;

pub mod tokens;

/// `<namespace>/<gitimage>`: the repository path under `/v2/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Repo {
    pub namespace: String,
    pub name: String,
}

impl Repo {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
        }
    }
    /// `team-a/myapp`
    pub fn path(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

/// `sha256:<64 hex>`; only sha256 is accepted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(String);

impl Digest {
    pub fn parse(s: &str) -> Result<Self, OciError> {
        let hex = s.strip_prefix("sha256:").ok_or(OciError::DigestInvalid)?;
        if hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s.to_string()))
        } else {
            Err(OciError::DigestInvalid)
        }
    }
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::Digest as _;
        Self(format!(
            "sha256:{}",
            hex::encode(sha2::Sha256::digest(bytes))
        ))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn hex(&self) -> &str {
        &self.0[7..]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A tag or a digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    Digest(Digest),
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self, OciError> {
        if s.starts_with("sha256:") {
            return Digest::parse(s).map(Self::Digest);
        }
        let ok = !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
            && s.as_bytes()[0] != b'.'
            && s.as_bytes()[0] != b'-';
        if ok {
            Ok(Self::Tag(s.to_string()))
        } else {
            Err(OciError::TagInvalid)
        }
    }
}

/// A stored manifest: raw bytes plus its media type. The digest is of `body`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub media_type: String,
    pub body: Bytes,
}

impl Manifest {
    pub fn digest(&self) -> Digest {
        Digest::of(&self.body)
    }
}

/// Everything stored under one repo, for GC.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoIndex {
    /// tag -> manifest digest
    pub tags: Vec<(String, Digest)>,
    pub manifests: Vec<Digest>,
    /// blob digest and its last-modified time (unix seconds)
    pub blobs: Vec<(Digest, i64)>,
}

pub type ByteStream = BoxStream<'static, Result<Bytes, std::io::Error>>;

/// Opaque upload handle, owned by the replica that started it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UploadId(pub String);

/// OCI distribution error codes, mapped to HTTP status and JSON body by the routes.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    #[error("BLOB_UNKNOWN")]
    BlobUnknown,
    #[error("BLOB_UPLOAD_UNKNOWN")]
    BlobUploadUnknown,
    #[error("DIGEST_INVALID")]
    DigestInvalid,
    #[error("MANIFEST_UNKNOWN")]
    ManifestUnknown,
    #[error("MANIFEST_INVALID: {0}")]
    ManifestInvalid(String),
    #[error("NAME_UNKNOWN")]
    NameUnknown,
    #[error("TAG_INVALID")]
    TagInvalid,
    #[error("UNAUTHORIZED")]
    Unauthorized,
    #[error("DENIED")]
    Denied,
    #[error("UNSUPPORTED")]
    Unsupported,
    #[error("backend: {0}")]
    Backend(String),
}

/// Storage backend. Implementations: `S3Storage` (object_store, s3 mode),
/// `RegistryStorage` (upstream registry proxy, registry mode), `MemoryStorage` (tests).
#[async_trait::async_trait]
pub trait Storage: Send + Sync + 'static {
    /// Size of the blob, if present.
    async fn head_blob(&self, repo: &Repo, d: &Digest) -> Result<Option<u64>, OciError>;
    /// Streams the blob, optionally a byte range `[start, end)`.
    async fn get_blob(
        &self,
        repo: &Repo,
        d: &Digest,
        range: Option<std::ops::Range<u64>>,
    ) -> Result<ByteStream, OciError>;
    async fn begin_upload(&self, repo: &Repo) -> Result<UploadId, OciError>;
    /// Appends a chunk; returns the total bytes received so far.
    async fn append(&self, id: &UploadId, chunk: ByteStream) -> Result<u64, OciError>;
    /// Finishes the upload; fails with `DigestInvalid` if the content's sha256 differs.
    async fn commit(&self, id: &UploadId, d: &Digest) -> Result<(), OciError>;
    async fn abort(&self, id: &UploadId) -> Result<(), OciError>;
    /// Bytes received so far by an open upload, for upload status and chunk offsets.
    /// The default appends an empty chunk; the bundled backends override it with a lookup.
    async fn upload_size(&self, id: &UploadId) -> Result<u64, OciError> {
        self.append(id, Box::pin(futures::stream::empty())).await
    }
    /// Cross-repo mount within this storage; `Ok(false)` if not supported or not found.
    async fn mount_blob(&self, from: &Repo, to: &Repo, d: &Digest) -> Result<bool, OciError>;
    async fn delete_blob(&self, repo: &Repo, d: &Digest) -> Result<(), OciError>;
    async fn get_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Manifest>, OciError>;
    /// Digest a reference points at, without reading the body.
    async fn head_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Digest>, OciError>;
    async fn put_manifest(
        &self,
        repo: &Repo,
        r: &Reference,
        m: Manifest,
    ) -> Result<Digest, OciError>;
    async fn delete_manifest(&self, repo: &Repo, d: &Digest) -> Result<(), OciError>;
    async fn delete_tag(&self, repo: &Repo, tag: &str) -> Result<(), OciError>;
    async fn list(&self, repo: &Repo) -> Result<RepoIndex, OciError>;
    /// Deletes everything under the repo (GitImage deleted).
    async fn delete_repo(&self, repo: &Repo) -> Result<(), OciError>;
    /// `true` when this backend can delete blobs (S3); registries GC blobs themselves.
    fn deletes_blobs(&self) -> bool;
}

pub type DynStorage = Arc<dyn Storage>;

/// OCI image manifest.
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// OCI image index.
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
/// Docker image manifest, schema 2.
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
/// Docker manifest list.
pub const DOCKER_MANIFEST_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
/// Manifest media types accepted on push and requested from upstream registries.
pub const MANIFEST_MEDIA_TYPES: [&str; 4] = [
    OCI_MANIFEST,
    OCI_INDEX,
    DOCKER_MANIFEST,
    DOCKER_MANIFEST_LIST,
];

mod memory;
mod registry;
mod routes;
mod s3;
#[cfg(test)]
mod storage_tests;

pub use memory::MemoryStorage;
pub use registry::RegistryStorage;
pub use routes::{Access, AuthCheck, PushState, Scope, push_router, read_only_router};
pub use s3::S3Storage;
