//! Workload roller: when a GitImage gets a new revision, pins it in the pod template of every
//! Deployment, StatefulSet and DaemonSet that uses the GitImage, which rolls them.

use gitimage_api::{GitImage, keys};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::core::v1::PodTemplateSpec;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::context::Ctx;

/// Containers of `template` bound to GitImage `name` whose pin differs from `revision`.
pub fn stale_containers(template: &PodTemplateSpec, name: &str, revision: &str) -> Vec<String> {
    let Some(ann) = template
        .metadata
        .as_ref()
        .and_then(|m| m.annotations.as_ref())
    else {
        return Vec::new();
    };
    ann.iter()
        .filter_map(|(k, v)| {
            let c = keys::image_annotation_container(k)?;
            (v.trim() == name
                && ann.get(&keys::revision_annotation(c)).map(String::as_str) != Some(revision))
            .then(|| c.to_string())
        })
        .collect()
}

trait HasTemplate {
    fn template(&self) -> Option<&PodTemplateSpec>;
}

impl HasTemplate for Deployment {
    fn template(&self) -> Option<&PodTemplateSpec> {
        self.spec.as_ref().map(|s| &s.template)
    }
}
impl HasTemplate for StatefulSet {
    fn template(&self) -> Option<&PodTemplateSpec> {
        self.spec.as_ref().map(|s| &s.template)
    }
}
impl HasTemplate for DaemonSet {
    fn template(&self) -> Option<&PodTemplateSpec> {
        self.spec.as_ref().map(|s| &s.template)
    }
}

async fn roll_kind<K>(ctx: &Ctx, ns: &str, name: &str, revision: &str)
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>
        + HasTemplate
        + Clone
        + DeserializeOwned
        + serde::Serialize
        + std::fmt::Debug,
{
    let api: Api<K> = Api::namespaced(ctx.client.clone(), ns);
    let list = match api.list(&ListParams::default()).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(kind = %K::kind(&()), error = %e, "roller: list failed");
            return;
        }
    };
    for w in list.items {
        if w.annotations()
            .get(keys::ROLLOUT_ANNOTATION)
            .is_some_and(|v| v == "manual")
        {
            continue;
        }
        let Some(template) = w.template() else {
            continue;
        };
        let stale = stale_containers(template, name, revision);
        if stale.is_empty() {
            continue;
        }
        let annotations: serde_json::Map<String, serde_json::Value> = stale
            .iter()
            .map(|c| (keys::revision_annotation(c), json!(revision)))
            .collect();
        let patch = json!({
            "apiVersion": K::api_version(&()),
            "kind": K::kind(&()),
            "metadata": { "name": w.name_any(), "namespace": ns },
            "spec": { "template": { "metadata": { "annotations": annotations } } }
        });
        let pp = PatchParams::apply(keys::FIELD_MANAGER).force();
        match api.patch(&w.name_any(), &pp, &Patch::Apply(&patch)).await {
            Ok(_) => {
                tracing::info!(kind = %K::kind(&()), namespace = ns, workload = w.name_any(), revision, "rolled")
            }
            Err(e) => {
                tracing::warn!(kind = %K::kind(&()), workload = w.name_any(), error = %e, "roll failed")
            }
        }
    }
}

/// Rolls every workload in the GitImage's namespace that uses it.
pub async fn roll(ctx: &Ctx, gi: &GitImage, revision: &str) {
    let ns = gi.namespace().unwrap_or_default();
    let name = gi.name_any();
    roll_kind::<Deployment>(ctx, &ns, &name, revision).await;
    roll_kind::<StatefulSet>(ctx, &ns, &name, revision).await;
    roll_kind::<DaemonSet>(ctx, &ns, &name, revision).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_unpinned_and_outdated_containers() {
        let t: PodTemplateSpec = serde_saphyr::from_str(
            r#"
metadata:
  annotations:
    gitimage.dev/image.web: myapp
    gitimage.dev/image.worker: myapp
    gitimage.dev/revision.worker: new
    gitimage.dev/image.other: otherapp
"#,
        )
        .unwrap();
        assert_eq!(stale_containers(&t, "myapp", "new"), vec!["web"]);
        assert!(stale_containers(&t, "nothing", "new").is_empty());
    }
}
