//! Storage GC (PRD F6): hourly mark and sweep per GitImage. Keeps the 5 newest builds, every
//! digest a pod runs, and the build cache; deletes the rest. Blobs get a 1 h grace period so a
//! push that has not written its manifest yet is never swept.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gitimage_api::GitImage;
use gitimage_oci::{Digest, Reference, Repo, Storage};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::ResourceExt;
use kube::api::{Api, ListParams};

use crate::context::Ctx;

pub const KEEP_BUILDS: usize = 5;
const BLOB_GRACE_SECS: i64 = 3600;
pub const CACHE_TAG: &str = "buildcache";

/// Digests referenced by a manifest or index body (children manifests, config, layers).
pub fn referenced(body: &[u8]) -> Vec<Digest> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |d: &serde_json::Value| {
        if let Some(s) = d.get("digest").and_then(|s| s.as_str())
            && let Ok(d) = Digest::parse(s)
        {
            out.push(d);
        }
    };
    if let Some(c) = v.get("config") {
        push(c);
    }
    for key in ["layers", "manifests", "blobs"] {
        if let Some(arr) = v.get(key).and_then(|a| a.as_array()) {
            arr.iter().for_each(&mut push);
        }
    }
    if let Some(s) = v.get("subject") {
        push(s);
    }
    out
}

/// Revisions among `candidates` that status has no build record for yet.
pub fn unrecorded(
    status: &gitimage_api::GitImageStatus,
    candidates: impl Iterator<Item = String>,
) -> Vec<String> {
    candidates.filter(|r| status.record(r).is_none()).collect()
}

/// Digests of `<pull_host>/<ns>/<name>@sha256:...` images that pods currently run.
pub fn digests_in_use(pods: &[Pod], prefix: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for p in pods {
        let spec_images = p.spec.iter().flat_map(|s| {
            s.containers
                .iter()
                .chain(s.init_containers.iter().flatten())
                .filter_map(|c| c.image.clone())
        });
        let status_images = p.status.iter().flat_map(|s| {
            s.container_statuses
                .iter()
                .flatten()
                .chain(s.init_container_statuses.iter().flatten())
                .map(|c| c.image_id.clone())
        });
        for img in spec_images.chain(status_images) {
            if let Some((repo, digest)) = img.split_once('@')
                && repo.ends_with(prefix)
            {
                out.insert(digest.to_string());
            }
        }
    }
    out
}

/// Marks everything reachable from `roots`.
async fn mark(
    storage: &dyn Storage,
    repo: &Repo,
    roots: &[Digest],
) -> (HashSet<Digest>, HashSet<Digest>) {
    let mut manifests = HashSet::new();
    let mut blobs = HashSet::new();
    let mut todo: Vec<Digest> = roots.to_vec();
    while let Some(d) = todo.pop() {
        if !manifests.insert(d.clone()) {
            continue;
        }
        match storage
            .get_manifest(repo, &Reference::Digest(d.clone()))
            .await
        {
            Ok(Some(m)) => {
                for child in referenced(&m.body) {
                    // A child is a manifest if the store knows it as one, else a blob.
                    match storage
                        .head_manifest(repo, &Reference::Digest(child.clone()))
                        .await
                    {
                        Ok(Some(_)) => todo.push(child),
                        _ => {
                            blobs.insert(child);
                        }
                    }
                }
            }
            _ => {
                manifests.remove(&d);
                blobs.insert(d);
            }
        }
    }
    (manifests, blobs)
}

/// One GC pass over one GitImage. Returns how many objects were deleted.
pub async fn collect(ctx: &Ctx, gi: &GitImage, pods: &[Pod]) -> anyhow::Result<u64> {
    let ns = gi.namespace().unwrap_or_default();
    let name = gi.name_any();
    let repo = Repo::new(&ns, &name);
    let storage = ctx.storage.as_ref();
    let index = storage.list(&repo).await?;
    let status = gi.status.clone().unwrap_or_default();

    let mut roots: Vec<Digest> = status
        .builds
        .iter()
        .filter_map(|b| b.digest.as_deref())
        .filter_map(|d| Digest::parse(d).ok())
        .take(KEEP_BUILDS)
        .collect();
    let prefix = format!("/{ns}/{name}");
    for d in digests_in_use(pods, &prefix) {
        if let Ok(d) = Digest::parse(&d) {
            roots.push(d);
        }
    }
    // Revisions in flight: a build Job exists (or it is the newest commit) but status has no
    // record yet, so its tag may be pushed and not yet recorded. Once recorded, the
    // keep-newest-5 rule applies.
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
    let lp = ListParams::default().labels(&format!("{}={}", gitimage_api::keys::NAME_LABEL, name));
    let job_revisions: Vec<String> = jobs
        .list(&lp)
        .await?
        .items
        .iter()
        .filter_map(|j| {
            j.metadata
                .labels
                .as_ref()?
                .get(gitimage_api::keys::REVISION_LABEL)
                .cloned()
        })
        .collect();
    let in_flight = unrecorded(
        &status,
        job_revisions
            .into_iter()
            .chain(status.latest_revision.clone()),
    );
    let kept_tags: HashSet<String> = status
        .builds
        .iter()
        .filter(|b| b.digest.is_some())
        .take(KEEP_BUILDS)
        .map(|b| b.revision.clone())
        .chain([CACHE_TAG.to_string()])
        .chain(in_flight)
        .collect();
    for (tag, d) in &index.tags {
        if kept_tags.contains(tag) {
            roots.push(d.clone());
        }
    }
    let (live_manifests, live_blobs) = mark(storage, &repo, &roots).await;

    let mut deleted = 0;
    for (tag, d) in &index.tags {
        if !kept_tags.contains(tag) && !live_manifests.contains(d) {
            storage.delete_tag(&repo, tag).await?;
            deleted += 1;
        }
    }
    for d in &index.manifests {
        if !live_manifests.contains(d) {
            storage.delete_manifest(&repo, d).await?;
            deleted += 1;
        }
    }
    if storage.deletes_blobs() {
        let now = chrono::Utc::now().timestamp();
        for (d, modified) in &index.blobs {
            if !live_blobs.contains(d) && now - modified > BLOB_GRACE_SECS {
                storage.delete_blob(&repo, d).await?;
                deleted += 1;
            }
        }
    }
    if deleted > 0 {
        tracing::info!(namespace = %ns, name = %name, deleted, "storage GC");
    }
    Ok(deleted)
}

/// Runs a GC pass over every GitImage every hour, while this replica leads.
pub async fn run(ctx: Arc<Ctx>) {
    // GITIMAGE_GC_INTERVAL (e.g. `20s`) exists for the e2e suite; production uses 1 h.
    let every = std::env::var("GITIMAGE_GC_INTERVAL")
        .ok()
        .and_then(|v| humantime::parse_duration(&v).ok())
        .unwrap_or(Duration::from_secs(3600));
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let pods: Api<Pod> = Api::all(ctx.client.clone());
        let pods = match pods.list(&ListParams::default()).await {
            Ok(l) => l.items,
            Err(e) => {
                tracing::warn!(error = %e, "GC: pod list failed; skipping this pass");
                continue;
            }
        };
        for gi in ctx.gitimages.state() {
            match collect(&ctx, &gi, &pods).await {
                Ok(n) => {
                    ctx.metrics.gc_deleted.inc_by(n);
                }
                Err(e) => {
                    tracing::warn!(name = gi.name_any(), error = %e, "GC failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn references_in_manifest_and_index() {
        let m = format!(r#"{{"config":{{"digest":"{A}"}},"layers":[{{"digest":"{B}"}}]}}"#);
        assert_eq!(referenced(m.as_bytes()).len(), 2);
        let i = format!(r#"{{"manifests":[{{"digest":"{A}"}}]}}"#);
        assert_eq!(referenced(i.as_bytes()), vec![Digest::parse(A).unwrap()]);
    }

    #[test]
    fn only_unrecorded_revisions_are_in_flight() {
        let status = gitimage_api::GitImageStatus {
            builds: vec![gitimage_api::BuildRecord {
                revision: "old".into(),
                outcome: gitimage_api::BuildOutcome::Built,
                digest: Some(A.into()),
                context_tree: None,
                source_revision: None,
                reason: None,
                message: None,
                finished_at: "t".into(),
            }],
            ..Default::default()
        };
        let got = unrecorded(&status, ["old".to_string(), "new".to_string()].into_iter());
        assert_eq!(got, vec!["new".to_string()]);
    }

    #[test]
    fn in_use_from_spec_and_status() {
        let pod: Pod = serde_saphyr::from_str(&format!(
            r#"
spec: {{ containers: [ {{ name: web, image: "localhost:25050/team-a/myapp@{A}" }} ] }}
status: {{ containerStatuses: [ {{ name: web, image: x, imageID: "localhost:25050/team-a/myapp@{B}", ready: true, restartCount: 0 }} ] }}
"#
        ))
        .unwrap();
        let used = digests_in_use(&[pod], "/team-a/myapp");
        assert!(used.contains(A) && used.contains(B));
        assert!(digests_in_use(&[], "/team-a/myapp").is_empty());
    }
}
