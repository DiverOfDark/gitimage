//! `Storage` on S3-compatible object storage via object_store (s3 mode).
//!
//! Key layout inside the bucket:
//!
//! - `repos/<ns>/<name>/blobs/sha256/<hex>`: blob content.
//! - `repos/<ns>/<name>/manifests/sha256/<hex>`: manifest body; its media type is the
//!   object's `Content-Type` attribute (S3 and `InMemory` both keep it, also across copies).
//! - `repos/<ns>/<name>/tags/<tag>`: body is the manifest digest string.
//! - `uploads/<uuid>`: in-flight multipart uploads of blobs larger than one part. They are
//!   copied to the blob key after the digest is verified and then deleted, so a blob key only
//!   ever holds complete, verified content. Leftovers from crashed replicas can be expired with
//!   a bucket lifecycle rule on the `uploads/` prefix.
//!
//! Upload state (multipart handle, running sha256, size) lives in this process's memory;
//! the push routes forward chunks to the replica that owns an upload.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use futures::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{
    Attribute, Attributes, GetOptions, GetRange, MultipartUpload, ObjectStore, ObjectStoreExt,
    PutOptions, PutPayload,
};
use sha2::Digest as _;
use tokio::sync::Mutex;

use crate::{
    ByteStream, Digest, Manifest, OciError, Reference, Repo, RepoIndex, Storage, UploadId,
};

/// Multipart part size; S3 requires at least 5 MiB for every part but the last.
const PART_SIZE: usize = 8 * 1024 * 1024;
/// Uploads untouched for this long are aborted when the next upload begins.
const UPLOAD_TTL: Duration = Duration::from_secs(24 * 3600);

/// S3 storage. Cheap to share; clone the `Arc<dyn ObjectStore>` inside freely.
pub struct S3Storage {
    store: Arc<dyn ObjectStore>,
    uploads: StdMutex<HashMap<String, Arc<Mutex<Upload>>>>,
}

struct Upload {
    repo: Repo,
    temp: Path,
    /// Started lazily once the first full part is buffered; small blobs use one plain PUT.
    multipart: Option<Box<dyn MultipartUpload>>,
    buffer: BytesMut,
    hasher: sha2::Sha256,
    size: u64,
    touched: Instant,
}

impl Upload {
    async fn abort(&mut self) {
        if let Some(mp) = self.multipart.as_mut()
            && let Err(e) = mp.abort().await
        {
            tracing::warn!(error = %e, temp = %self.temp, "aborting multipart upload failed");
        }
    }
}

fn backend(e: object_store::Error) -> OciError {
    OciError::Backend(e.to_string())
}

fn is_not_found(e: &object_store::Error) -> bool {
    matches!(e, object_store::Error::NotFound { .. })
}

/// Endpoint for virtual-hosted-style requests: object_store wants the bucket in the host.
fn virtual_hosted_endpoint(endpoint: &str, bucket: &str) -> String {
    match endpoint.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{bucket}.{rest}"),
        None => format!("https://{bucket}.{endpoint}"),
    }
}

impl S3Storage {
    /// Connects to the bucket in `cfg` with static credentials. HTTP endpoints are allowed
    /// (in-cluster MinIO); an empty `endpoint` means AWS.
    pub fn new(
        cfg: &gitimage_api::config::S3Storage,
        access_key: String,
        secret_key: String,
    ) -> Result<Self, OciError> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&cfg.bucket)
            .with_region(&cfg.region)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key)
            .with_allow_http(true)
            .with_virtual_hosted_style_request(!cfg.path_style);
        let endpoint = cfg.endpoint.trim_end_matches('/');
        if !endpoint.is_empty() {
            builder = builder.with_endpoint(if cfg.path_style {
                endpoint.to_string()
            } else {
                virtual_hosted_endpoint(endpoint, &cfg.bucket)
            });
        }
        let store = builder.build().map_err(backend)?;
        Ok(Self::from_store(Arc::new(store)))
    }

    /// Wraps any object store, e.g. `object_store::memory::InMemory` in tests.
    pub fn from_store(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            store,
            uploads: StdMutex::default(),
        }
    }

    fn repo_prefix(repo: &Repo) -> Path {
        Path::from_iter(["repos", repo.namespace.as_str(), repo.name.as_str()])
    }

    fn blob_path(repo: &Repo, d: &Digest) -> Path {
        Self::repo_prefix(repo)
            .join("blobs")
            .join("sha256")
            .join(d.hex())
    }

    fn manifest_path(repo: &Repo, d: &Digest) -> Path {
        Self::repo_prefix(repo)
            .join("manifests")
            .join("sha256")
            .join(d.hex())
    }

    fn tag_path(repo: &Repo, tag: &str) -> Path {
        Self::repo_prefix(repo).join("tags").join(tag)
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

    /// Aborts uploads nobody touched for `UPLOAD_TTL` (clients that vanished mid-push).
    async fn sweep_stale_uploads(&self) {
        let stale: Vec<Arc<Mutex<Upload>>> = {
            let mut uploads = self.uploads.lock().expect("uploads lock");
            let ids: Vec<String> = uploads
                .iter()
                .filter(|(_, u)| u.try_lock().is_ok_and(|u| u.touched.elapsed() > UPLOAD_TTL))
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| uploads.remove(id)).collect()
        };
        for upload in stale {
            upload.lock().await.abort().await;
        }
    }

    async fn read_tag(&self, repo: &Repo, tag: &str) -> Result<Option<Digest>, OciError> {
        match self.store.get(&Self::tag_path(repo, tag)).await {
            Ok(r) => {
                let body = r.bytes().await.map_err(backend)?;
                let s = std::str::from_utf8(&body)
                    .map_err(|_| OciError::Backend(format!("tag {tag} is not utf-8")))?;
                Digest::parse(s.trim()).map(Some)
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(backend(e)),
        }
    }

    async fn resolve(&self, repo: &Repo, r: &Reference) -> Result<Option<Digest>, OciError> {
        match r {
            Reference::Digest(d) => Ok(Some(d.clone())),
            Reference::Tag(t) => self.read_tag(repo, t).await,
        }
    }

    async fn exists(&self, path: &Path) -> Result<Option<u64>, OciError> {
        match self.store.head(path).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(backend(e)),
        }
    }

    async fn delete_paths(&self, paths: Vec<Path>) -> Result<(), OciError> {
        let stream = futures::stream::iter(paths.into_iter().map(Ok)).boxed();
        self.store
            .delete_stream(stream)
            .try_collect::<Vec<_>>()
            .await
            .map_err(backend)?;
        Ok(())
    }

    /// Writes the verified upload to its blob key.
    async fn finish_upload(&self, upload: &mut Upload, d: &Digest) -> Result<(), OciError> {
        let target = Self::blob_path(&upload.repo, d);
        let rest = std::mem::take(&mut upload.buffer).freeze();
        let Some(mp) = upload.multipart.as_mut() else {
            self.store
                .put(&target, PutPayload::from_bytes(rest))
                .await
                .map_err(backend)?;
            return Ok(());
        };
        if !rest.is_empty() {
            mp.put_part(PutPayload::from_bytes(rest))
                .await
                .map_err(backend)?;
        }
        mp.complete().await.map_err(backend)?;
        upload.multipart = None;
        let copied = self
            .store
            .copy(&upload.temp, &target)
            .await
            .map_err(backend);
        if let Err(e) = self.store.delete(&upload.temp).await {
            tracing::warn!(error = %e, temp = %upload.temp, "deleting upload temp object failed");
        }
        copied
    }
}

#[async_trait::async_trait]
impl Storage for S3Storage {
    async fn head_blob(&self, repo: &Repo, d: &Digest) -> Result<Option<u64>, OciError> {
        self.exists(&Self::blob_path(repo, d)).await
    }

    async fn get_blob(
        &self,
        repo: &Repo,
        d: &Digest,
        range: Option<Range<u64>>,
    ) -> Result<ByteStream, OciError> {
        let opts = GetOptions::new().with_range(range.map(GetRange::Bounded));
        match self.store.get_opts(&Self::blob_path(repo, d), opts).await {
            Ok(r) => Ok(r.into_stream().map_err(std::io::Error::other).boxed()),
            Err(e) if is_not_found(&e) => Err(OciError::BlobUnknown),
            Err(e) => Err(backend(e)),
        }
    }

    async fn begin_upload(&self, repo: &Repo) -> Result<UploadId, OciError> {
        self.sweep_stale_uploads().await;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let upload = Upload {
            repo: repo.clone(),
            temp: Path::from_iter(["uploads", id.as_str()]),
            multipart: None,
            buffer: BytesMut::new(),
            hasher: sha2::Sha256::new(),
            size: 0,
            touched: Instant::now(),
        };
        self.uploads
            .lock()
            .expect("uploads lock")
            .insert(id.clone(), Arc::new(Mutex::new(upload)));
        Ok(UploadId(id))
    }

    async fn append(&self, id: &UploadId, mut chunk: ByteStream) -> Result<u64, OciError> {
        let upload = self.upload(id)?;
        let mut upload = upload.lock().await;
        upload.touched = Instant::now();
        while let Some(data) = chunk.next().await {
            let data = data.map_err(|e| OciError::Backend(format!("reading upload body: {e}")))?;
            upload.hasher.update(&data);
            upload.size += data.len() as u64;
            upload.buffer.extend_from_slice(&data);
            while upload.buffer.len() >= PART_SIZE {
                let part = upload.buffer.split_to(PART_SIZE).freeze();
                if upload.multipart.is_none() {
                    let mp = self
                        .store
                        .put_multipart(&upload.temp)
                        .await
                        .map_err(backend)?;
                    upload.multipart = Some(mp);
                }
                let mp = upload.multipart.as_mut().expect("multipart started");
                mp.put_part(PutPayload::from_bytes(part))
                    .await
                    .map_err(backend)?;
            }
        }
        Ok(upload.size)
    }

    async fn upload_size(&self, id: &UploadId) -> Result<u64, OciError> {
        Ok(self.upload(id)?.lock().await.size)
    }

    async fn commit(&self, id: &UploadId, d: &Digest) -> Result<(), OciError> {
        let upload = self.take_upload(id)?;
        let mut upload = upload.lock().await;
        let actual = format!("sha256:{}", hex::encode(upload.hasher.clone().finalize()));
        if actual != d.as_str() {
            upload.abort().await;
            return Err(OciError::DigestInvalid);
        }
        let result = self.finish_upload(&mut upload, d).await;
        if result.is_err() {
            upload.abort().await;
        }
        result
    }

    async fn abort(&self, id: &UploadId) -> Result<(), OciError> {
        let upload = self.take_upload(id)?;
        upload.lock().await.abort().await;
        Ok(())
    }

    async fn mount_blob(&self, from: &Repo, to: &Repo, d: &Digest) -> Result<bool, OciError> {
        let src = Self::blob_path(from, d);
        match self.store.copy(&src, &Self::blob_path(to, d)).await {
            Ok(()) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(backend(e)),
        }
    }

    async fn delete_blob(&self, repo: &Repo, d: &Digest) -> Result<(), OciError> {
        let path = Self::blob_path(repo, d);
        // S3 deletes of missing keys succeed, so check first to report BLOB_UNKNOWN.
        if self.exists(&path).await?.is_none() {
            return Err(OciError::BlobUnknown);
        }
        self.store.delete(&path).await.map_err(backend)
    }

    async fn get_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Manifest>, OciError> {
        let Some(d) = self.resolve(repo, r).await? else {
            return Ok(None);
        };
        let result = match self.store.get(&Self::manifest_path(repo, &d)).await {
            Ok(r) => r,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        let media_type = result
            .attributes
            .get(&Attribute::ContentType)
            .map(|v| v.to_string())
            .ok_or_else(|| OciError::Backend(format!("manifest {d} has no content type")))?;
        let body = result.bytes().await.map_err(backend)?;
        Ok(Some(Manifest { media_type, body }))
    }

    async fn head_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Digest>, OciError> {
        let Some(d) = self.resolve(repo, r).await? else {
            return Ok(None);
        };
        Ok(self
            .exists(&Self::manifest_path(repo, &d))
            .await?
            .map(|_| d))
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
        let mut attributes = Attributes::new();
        attributes.insert(Attribute::ContentType, m.media_type.into());
        let opts = PutOptions::from(attributes);
        self.store
            .put_opts(
                &Self::manifest_path(repo, &digest),
                PutPayload::from_bytes(m.body),
                opts,
            )
            .await
            .map_err(backend)?;
        if let Reference::Tag(t) = r {
            self.store
                .put(&Self::tag_path(repo, t), digest.to_string().into())
                .await
                .map_err(backend)?;
        }
        Ok(digest)
    }

    async fn delete_manifest(&self, repo: &Repo, d: &Digest) -> Result<(), OciError> {
        let path = Self::manifest_path(repo, d);
        if self.exists(&path).await?.is_none() {
            return Err(OciError::ManifestUnknown);
        }
        let mut paths = vec![path];
        let index = self.list(repo).await?;
        paths.extend(
            index
                .tags
                .iter()
                .filter(|(_, t)| t == d)
                .map(|(tag, _)| Self::tag_path(repo, tag)),
        );
        self.delete_paths(paths).await
    }

    async fn delete_tag(&self, repo: &Repo, tag: &str) -> Result<(), OciError> {
        let path = Self::tag_path(repo, tag);
        if self.exists(&path).await?.is_none() {
            return Err(OciError::ManifestUnknown);
        }
        self.store.delete(&path).await.map_err(backend)
    }

    async fn list(&self, repo: &Repo) -> Result<RepoIndex, OciError> {
        let prefix = Self::repo_prefix(repo);
        let objects: Vec<_> = self
            .store
            .list(Some(&prefix))
            .try_collect()
            .await
            .map_err(backend)?;
        let mut index = RepoIndex::default();
        let mut tags = Vec::new();
        for meta in objects {
            let Some(parts) = meta.location.prefix_match(&prefix) else {
                continue;
            };
            let parts: Vec<String> = parts.map(|p| p.as_ref().to_string()).collect();
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            match parts.as_slice() {
                ["blobs", "sha256", hex] => {
                    if let Ok(d) = Digest::parse(&format!("sha256:{hex}")) {
                        index.blobs.push((d, meta.last_modified.timestamp()));
                    }
                }
                ["manifests", "sha256", hex] => {
                    if let Ok(d) = Digest::parse(&format!("sha256:{hex}")) {
                        index.manifests.push(d);
                    }
                }
                ["tags", tag] => tags.push(tag.to_string()),
                _ => {}
            }
        }
        for tag in tags {
            if let Some(d) = self.read_tag(repo, &tag).await? {
                index.tags.push((tag, d));
            }
        }
        index.tags.sort();
        index.manifests.sort();
        index.blobs.sort();
        Ok(index)
    }

    async fn delete_repo(&self, repo: &Repo) -> Result<(), OciError> {
        let prefix = Self::repo_prefix(repo);
        let paths: Vec<Path> = self
            .store
            .list(Some(&prefix))
            .map_ok(|m| m.location)
            .try_collect()
            .await
            .map_err(backend)?;
        self.delete_paths(paths).await
    }

    fn deletes_blobs(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn virtual_hosted_endpoint_inserts_bucket() {
        assert_eq!(
            virtual_hosted_endpoint("https://s3.example.com", "b"),
            "https://b.s3.example.com"
        );
        assert_eq!(
            virtual_hosted_endpoint("minio:9000", "b"),
            "https://b.minio:9000"
        );
    }

    #[tokio::test]
    async fn large_upload_leaves_no_temp_objects() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let s = S3Storage::from_store(store.clone());
        let repo = Repo::new("ns", "app");
        let data = vec![7u8; PART_SIZE * 2 + 3];
        for digest in [Digest::of(b"wrong"), Digest::of(&data)] {
            let id = s.begin_upload(&repo).await.unwrap();
            let body = futures::stream::once(futures::future::ok(Bytes::from(data.clone())));
            s.append(&id, body.boxed()).await.unwrap();
            let _ = s.commit(&id, &digest).await;
        }
        let keys: Vec<String> = store
            .list(None)
            .map_ok(|m| m.location.to_string())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            keys,
            vec![format!(
                "repos/ns/app/blobs/sha256/{}",
                Digest::of(&data).hex()
            )]
        );
    }

    #[test]
    fn layout() {
        let repo = Repo::new("team-a", "app");
        let d = Digest::of(b"x");
        assert_eq!(
            S3Storage::blob_path(&repo, &d).as_ref(),
            format!("repos/team-a/app/blobs/sha256/{}", d.hex())
        );
        assert_eq!(
            S3Storage::tag_path(&repo, "v1").as_ref(),
            "repos/team-a/app/tags/v1"
        );
    }
}
