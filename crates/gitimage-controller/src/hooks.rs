//! Push-hook receiver (PRD F2), served on every replica: `POST /hooks/<token>` marks every
//! GitImage of the same repo in that namespace with `gitimage.dev/hook-at`; the leader's
//! watch turns that into a reconcile. The payload is never trusted (only its size is limited).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use gitimage_api::{GitImage, keys};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::reflector::Store;
use kube::{Client, ResourceExt};
use sha2::Digest as _;

use crate::metrics::Metrics;
use crate::util::{normalize_git_url, now_rfc3339};

#[derive(Clone)]
pub struct HookState {
    pub client: Client,
    pub gitimages: Store<GitImage>,
    pub metrics: Arc<Metrics>,
}

/// GitImages a token selects: the owner plus every GitImage in its namespace with the same repo.
pub fn targets(all: &[Arc<GitImage>], token: &str) -> Vec<Arc<GitImage>> {
    let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
    let Some(owner) = all.iter().find(|g| {
        g.status.as_ref().and_then(|s| s.hook_token_hash.as_deref()) == Some(hash.as_str())
    }) else {
        return Vec::new();
    };
    let ns = owner.namespace();
    let url = normalize_git_url(&owner.spec.url);
    all.iter()
        .filter(|g| g.namespace() == ns && normalize_git_url(&g.spec.url) == url)
        .cloned()
        .collect()
}

async fn hook(State(st): State<HookState>, Path(token): Path<String>) -> StatusCode {
    let all = st.gitimages.state();
    let targets = targets(&all, &token);
    if targets.is_empty() {
        return StatusCode::NOT_FOUND;
    }
    st.metrics.hooks.inc();
    let now = now_rfc3339();
    for gi in targets {
        let api: Api<GitImage> =
            Api::namespaced(st.client.clone(), &gi.namespace().unwrap_or_default());
        let patch =
            serde_json::json!({ "metadata": { "annotations": { keys::HOOK_AT_ANNOTATION: now } } });
        if let Err(e) = api
            .patch(
                &gi.name_any(),
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await
        {
            tracing::warn!(name = gi.name_any(), error = %e, "hook: annotate failed");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    }
    StatusCode::ACCEPTED
}

pub fn router(state: HookState) -> Router {
    Router::new()
        .route("/hooks/{token}", post(hook))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(1 << 20))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitimage_api::{GitImageSpec, GitImageStatus};

    fn gi(ns: &str, name: &str, url: &str, token: Option<&str>) -> Arc<GitImage> {
        let mut g = GitImage::new(
            name,
            serde_saphyr::from_str::<GitImageSpec>(&format!("url: {url}")).unwrap(),
        );
        g.metadata.namespace = Some(ns.into());
        g.status = Some(GitImageStatus {
            hook_token_hash: token.map(|t| hex::encode(sha2::Sha256::digest(t.as_bytes()))),
            ..Default::default()
        });
        Arc::new(g)
    }

    #[test]
    fn fans_out_to_same_repo_in_same_namespace_only() {
        let all = vec![
            gi("a", "api", "git@github.com:o/mono.git", Some("tok")),
            gi("a", "web", "https://github.com/o/mono", None),
            gi("a", "other", "https://github.com/o/other", None),
            gi("b", "api", "https://github.com/o/mono", None),
        ];
        let mut names: Vec<String> = targets(&all, "tok").iter().map(|g| g.name_any()).collect();
        names.sort();
        assert_eq!(names, vec!["api", "web"]);
        assert!(targets(&all, "wrong").is_empty());
    }
}
