//! In-memory `Storage` for tests and e2e: everything lives in maps behind async locks.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use tokio::sync::{Mutex, RwLock};

use crate::{
    ByteStream, Digest, Manifest, OciError, Reference, Repo, RepoIndex, Storage, UploadId,
};

/// In-memory storage; blobs are fully buffered, so use it only for tests.
#[derive(Default)]
pub struct MemoryStorage {
    repos: RwLock<HashMap<Repo, RepoData>>,
    uploads: Mutex<HashMap<String, Upload>>,
}

#[derive(Default)]
struct RepoData {
    /// digest -> (content, last-modified unix seconds)
    blobs: HashMap<Digest, (Bytes, i64)>,
    manifests: HashMap<Digest, Manifest>,
    tags: HashMap<String, Digest>,
}

struct Upload {
    repo: Repo,
    data: BytesMut,
}

impl MemoryStorage {
    /// Empty storage.
    pub fn new() -> Self {
        Self::default()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Slices `[range.start, min(range.end, len))`; an empty or out-of-bounds range is an error.
pub(crate) fn slice_range(data: &Bytes, range: Option<Range<u64>>) -> Result<Bytes, OciError> {
    let Some(r) = range else {
        return Ok(data.clone());
    };
    let len = data.len() as u64;
    if r.start >= r.end || r.start >= len {
        return Err(OciError::Backend(format!(
            "range {r:?} outside blob of {len} bytes"
        )));
    }
    Ok(data.slice(r.start as usize..r.end.min(len) as usize))
}

#[async_trait::async_trait]
impl Storage for MemoryStorage {
    async fn head_blob(&self, repo: &Repo, d: &Digest) -> Result<Option<u64>, OciError> {
        let repos = self.repos.read().await;
        Ok(repos
            .get(repo)
            .and_then(|r| r.blobs.get(d))
            .map(|(b, _)| b.len() as u64))
    }

    async fn get_blob(
        &self,
        repo: &Repo,
        d: &Digest,
        range: Option<Range<u64>>,
    ) -> Result<ByteStream, OciError> {
        let repos = self.repos.read().await;
        let (data, _) = repos
            .get(repo)
            .and_then(|r| r.blobs.get(d))
            .ok_or(OciError::BlobUnknown)?;
        let chunk = slice_range(data, range)?;
        Ok(futures::stream::once(async move { Ok(chunk) }).boxed())
    }

    async fn begin_upload(&self, repo: &Repo) -> Result<UploadId, OciError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.uploads.lock().await.insert(
            id.clone(),
            Upload {
                repo: repo.clone(),
                data: BytesMut::new(),
            },
        );
        Ok(UploadId(id))
    }

    async fn append(&self, id: &UploadId, mut chunk: ByteStream) -> Result<u64, OciError> {
        let mut uploads = self.uploads.lock().await;
        let upload = uploads.get_mut(&id.0).ok_or(OciError::BlobUploadUnknown)?;
        while let Some(part) = chunk.next().await {
            let part = part.map_err(|e| OciError::Backend(e.to_string()))?;
            upload.data.extend_from_slice(&part);
        }
        Ok(upload.data.len() as u64)
    }

    async fn upload_size(&self, id: &UploadId) -> Result<u64, OciError> {
        let uploads = self.uploads.lock().await;
        let upload = uploads.get(&id.0).ok_or(OciError::BlobUploadUnknown)?;
        Ok(upload.data.len() as u64)
    }

    async fn commit(&self, id: &UploadId, d: &Digest) -> Result<(), OciError> {
        let upload = {
            let mut uploads = self.uploads.lock().await;
            let upload = uploads.get(&id.0).ok_or(OciError::BlobUploadUnknown)?;
            if &Digest::of(&upload.data) != d {
                return Err(OciError::DigestInvalid);
            }
            uploads.remove(&id.0).ok_or(OciError::BlobUploadUnknown)?
        };
        let mut repos = self.repos.write().await;
        repos
            .entry(upload.repo)
            .or_default()
            .blobs
            .insert(d.clone(), (upload.data.freeze(), now()));
        Ok(())
    }

    async fn abort(&self, id: &UploadId) -> Result<(), OciError> {
        self.uploads
            .lock()
            .await
            .remove(&id.0)
            .map(|_| ())
            .ok_or(OciError::BlobUploadUnknown)
    }

    async fn mount_blob(&self, from: &Repo, to: &Repo, d: &Digest) -> Result<bool, OciError> {
        let mut repos = self.repos.write().await;
        let Some((data, _)) = repos.get(from).and_then(|r| r.blobs.get(d)).cloned() else {
            return Ok(false);
        };
        repos
            .entry(to.clone())
            .or_default()
            .blobs
            .insert(d.clone(), (data, now()));
        Ok(true)
    }

    async fn delete_blob(&self, repo: &Repo, d: &Digest) -> Result<(), OciError> {
        let mut repos = self.repos.write().await;
        repos
            .get_mut(repo)
            .and_then(|r| r.blobs.remove(d))
            .map(|_| ())
            .ok_or(OciError::BlobUnknown)
    }

    async fn get_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Manifest>, OciError> {
        let repos = self.repos.read().await;
        let Some(data) = repos.get(repo) else {
            return Ok(None);
        };
        let digest = match r {
            Reference::Digest(d) => Some(d),
            Reference::Tag(t) => data.tags.get(t),
        };
        Ok(digest.and_then(|d| data.manifests.get(d)).cloned())
    }

    async fn head_manifest(&self, repo: &Repo, r: &Reference) -> Result<Option<Digest>, OciError> {
        Ok(self.get_manifest(repo, r).await?.map(|m| m.digest()))
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
        let mut repos = self.repos.write().await;
        let data = repos.entry(repo.clone()).or_default();
        data.manifests.insert(digest.clone(), m);
        if let Reference::Tag(t) = r {
            data.tags.insert(t.clone(), digest.clone());
        }
        Ok(digest)
    }

    async fn delete_manifest(&self, repo: &Repo, d: &Digest) -> Result<(), OciError> {
        let mut repos = self.repos.write().await;
        let data = repos.get_mut(repo).ok_or(OciError::ManifestUnknown)?;
        data.manifests.remove(d).ok_or(OciError::ManifestUnknown)?;
        data.tags.retain(|_, t| t != d);
        Ok(())
    }

    async fn delete_tag(&self, repo: &Repo, tag: &str) -> Result<(), OciError> {
        let mut repos = self.repos.write().await;
        repos
            .get_mut(repo)
            .and_then(|r| r.tags.remove(tag))
            .map(|_| ())
            .ok_or(OciError::ManifestUnknown)
    }

    async fn list(&self, repo: &Repo) -> Result<RepoIndex, OciError> {
        let repos = self.repos.read().await;
        let Some(data) = repos.get(repo) else {
            return Ok(RepoIndex::default());
        };
        let mut index = RepoIndex {
            tags: data
                .tags
                .iter()
                .map(|(t, d)| (t.clone(), d.clone()))
                .collect(),
            manifests: data.manifests.keys().cloned().collect(),
            blobs: data
                .blobs
                .iter()
                .map(|(d, (_, t))| (d.clone(), *t))
                .collect(),
        };
        index.tags.sort();
        index.manifests.sort();
        index.blobs.sort();
        Ok(index)
    }

    async fn delete_repo(&self, repo: &Repo) -> Result<(), OciError> {
        self.repos.write().await.remove(repo);
        self.uploads.lock().await.retain(|_, u| &u.repo != repo);
        Ok(())
    }

    fn deletes_blobs(&self) -> bool {
        true
    }
}
