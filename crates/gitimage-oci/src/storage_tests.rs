//! Conformance-style tests that every writable `Storage` backend must pass.

use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};

use crate::{
    ByteStream, Digest, DynStorage, Manifest, MemoryStorage, OCI_MANIFEST, OciError, Reference,
    Repo, S3Storage,
};

fn stream_of(chunks: Vec<Bytes>) -> ByteStream {
    futures::stream::iter(chunks.into_iter().map(Ok)).boxed()
}

async fn read_all(s: ByteStream) -> Vec<u8> {
    let chunks: Vec<Bytes> = s.try_collect().await.unwrap();
    chunks.concat()
}

/// Pseudo-random bytes so part boundaries are not hidden by repeating content.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 7) % 251) as u8).collect()
}

async fn upload(s: &DynStorage, repo: &Repo, data: &[u8], chunk: usize) -> Digest {
    let id = s.begin_upload(repo).await.unwrap();
    let mut sent = 0u64;
    for part in data.chunks(chunk) {
        // Each part arrives as several stream items, like an HTTP body would.
        let items = part
            .chunks(64 * 1024)
            .map(Bytes::copy_from_slice)
            .collect::<Vec<_>>();
        sent = s.append(&id, stream_of(items)).await.unwrap();
    }
    assert_eq!(sent, data.len() as u64);
    assert_eq!(s.upload_size(&id).await.unwrap(), data.len() as u64);
    let d = Digest::of(data);
    s.commit(&id, &d).await.unwrap();
    d
}

async fn chunked_uploads(s: DynStorage) {
    let repo = Repo::new("ns", "app");
    for (len, chunk) in [
        (0, 1),
        (10, 3),
        (3 * 1024 * 1024, 1024 * 1024),
        (17 * 1024 * 1024 + 5, 5 * 1024 * 1024 + 1),
    ] {
        let data = pattern(len);
        let d = upload(&s, &repo, &data, chunk.max(1)).await;
        assert_eq!(s.head_blob(&repo, &d).await.unwrap(), Some(len as u64));
        assert_eq!(
            read_all(s.get_blob(&repo, &d, None).await.unwrap()).await,
            data
        );
        if len > 10 {
            let got = read_all(s.get_blob(&repo, &d, Some(5..10)).await.unwrap()).await;
            assert_eq!(got, data[5..10]);
        }
    }
    assert_eq!(
        s.head_blob(&Repo::new("ns", "other"), &Digest::of(b""))
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        s.get_blob(&repo, &Digest::of(b"missing"), None).await,
        Err(OciError::BlobUnknown)
    ));
}

async fn digest_mismatch(s: DynStorage) {
    let repo = Repo::new("ns", "app");
    for len in [100, 9 * 1024 * 1024] {
        let id = s.begin_upload(&repo).await.unwrap();
        s.append(&id, stream_of(vec![Bytes::from(pattern(len))]))
            .await
            .unwrap();
        let wrong = Digest::of(b"something else");
        assert!(matches!(
            s.commit(&id, &wrong).await,
            Err(OciError::DigestInvalid)
        ));
        assert_eq!(s.head_blob(&repo, &wrong).await.unwrap(), None);
        assert!(
            s.head_blob(&repo, &Digest::of(&pattern(len)))
                .await
                .unwrap()
                .is_none()
        );
    }
    let id = s.begin_upload(&repo).await.unwrap();
    s.abort(&id).await.unwrap();
    assert!(matches!(
        s.append(&id, stream_of(vec![])).await,
        Err(OciError::BlobUploadUnknown)
    ));
}

fn manifest(n: u8) -> Manifest {
    Manifest {
        media_type: OCI_MANIFEST.into(),
        body: Bytes::from(format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","n":{n}}}"#
        )),
    }
}

async fn manifests_and_tags(s: DynStorage) {
    let repo = Repo::new("ns", "app");
    let m1 = manifest(1);
    let m2 = manifest(2);
    let tag = Reference::parse("v1").unwrap();
    let d1 = s.put_manifest(&repo, &tag, m1.clone()).await.unwrap();
    assert_eq!(d1, m1.digest());
    let d2 = s
        .put_manifest(&repo, &Reference::Digest(m2.digest()), m2.clone())
        .await
        .unwrap();
    assert!(matches!(
        s.put_manifest(&repo, &Reference::Digest(d1.clone()), m2.clone())
            .await,
        Err(OciError::DigestInvalid)
    ));

    assert_eq!(s.get_manifest(&repo, &tag).await.unwrap(), Some(m1.clone()));
    assert_eq!(
        s.get_manifest(&repo, &Reference::Digest(d2.clone()))
            .await
            .unwrap(),
        Some(m2.clone())
    );
    assert_eq!(
        s.head_manifest(&repo, &tag).await.unwrap(),
        Some(d1.clone())
    );
    assert_eq!(
        s.head_manifest(&repo, &Reference::parse("nope").unwrap())
            .await
            .unwrap(),
        None
    );

    // Retag v1 to m2, add latest -> m1.
    s.put_manifest(&repo, &tag, m2.clone()).await.unwrap();
    s.put_manifest(&repo, &Reference::parse("latest").unwrap(), m1.clone())
        .await
        .unwrap();
    let index = s.list(&repo).await.unwrap();
    assert_eq!(
        index.tags,
        vec![("latest".into(), d1.clone()), ("v1".into(), d2.clone())]
    );
    let mut want = vec![d1.clone(), d2.clone()];
    want.sort();
    assert_eq!(index.manifests, want);

    s.delete_tag(&repo, "latest").await.unwrap();
    assert!(matches!(
        s.delete_tag(&repo, "latest").await,
        Err(OciError::ManifestUnknown)
    ));
    s.delete_manifest(&repo, &d2).await.unwrap();
    assert_eq!(s.get_manifest(&repo, &tag).await.unwrap(), None);
    let index = s.list(&repo).await.unwrap();
    assert!(index.tags.is_empty());
    assert_eq!(index.manifests, vec![d1]);
}

async fn list_mount_delete(s: DynStorage) {
    let a = Repo::new("ns", "a");
    let b = Repo::new("ns", "b");
    let d = upload(&s, &a, b"layer", 2).await;
    s.put_manifest(&a, &Reference::parse("t").unwrap(), manifest(3))
        .await
        .unwrap();
    let index = s.list(&a).await.unwrap();
    assert_eq!(index.blobs.len(), 1);
    assert_eq!(index.blobs[0].0, d);
    assert!(index.blobs[0].1 > 1_600_000_000);

    assert!(s.mount_blob(&a, &b, &d).await.unwrap());
    assert!(!s.mount_blob(&a, &b, &Digest::of(b"nope")).await.unwrap());
    assert_eq!(s.head_blob(&b, &d).await.unwrap(), Some(5));

    s.delete_blob(&a, &d).await.unwrap();
    assert!(matches!(
        s.delete_blob(&a, &d).await,
        Err(OciError::BlobUnknown)
    ));
    assert_eq!(s.head_blob(&a, &d).await.unwrap(), None);

    s.delete_repo(&a).await.unwrap();
    assert_eq!(s.list(&a).await.unwrap(), Default::default());
    // The other repo is untouched.
    assert_eq!(s.head_blob(&b, &d).await.unwrap(), Some(5));
    assert!(s.deletes_blobs());
}

async fn conformance(make: impl Fn() -> DynStorage) {
    chunked_uploads(make()).await;
    digest_mismatch(make()).await;
    manifests_and_tags(make()).await;
    list_mount_delete(make()).await;
}

#[tokio::test]
async fn memory_storage_conformance() {
    conformance(|| Arc::new(MemoryStorage::new())).await;
}

#[tokio::test]
async fn s3_storage_conformance() {
    conformance(|| {
        Arc::new(S3Storage::from_store(Arc::new(
            object_store::memory::InMemory::new(),
        )))
    })
    .await;
}
