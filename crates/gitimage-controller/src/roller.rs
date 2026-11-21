//! Workload roller: when a GitImage gets a new revision, pins it in the pod template of every
//! Deployment, StatefulSet and DaemonSet that uses the GitImage, which rolls them. With
//! `controller.pinTemplateImage` it also writes the built image into the template, so tools
//! that read templates instead of pods (Trivy Operator) see the image that actually runs.

use gitimage_api::{GitImage, keys};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{Container, PodTemplateSpec};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::context::Ctx;

/// Template patch that pins every container bound to GitImage `name` to `revision` and, if
/// given, to `image`. `None` when the template is already pinned.
///
/// The patch is strategic-merge: `containers` and `initContainers` merge by name, so only the
/// bound containers' `image` changes. Server-side apply would drop fields this manager set
/// earlier for another GitImage's containers, because each roll sends one GitImage's fields.
pub fn template_patch(
    template: &PodTemplateSpec,
    name: &str,
    revision: &str,
    image: Option<&str>,
) -> Option<Value> {
    let ann = template.metadata.as_ref()?.annotations.as_ref()?;
    let spec = template.spec.as_ref();
    let find = |list: Option<&Vec<Container>>, c: &str| -> Option<Option<String>> {
        list?.iter().find(|x| x.name == c).map(|x| x.image.clone())
    };
    let mut annotations = serde_json::Map::new();
    let mut containers = Vec::new();
    let mut init_containers = Vec::new();
    for (k, v) in ann {
        let Some(c) = keys::image_annotation_container(k) else {
            continue;
        };
        if v.trim() != name {
            continue;
        }
        let pinned = ann.get(&keys::revision_annotation(c)).map(String::as_str) == Some(revision);
        // A bound container missing from the spec makes the webhook deny its pods; leave the
        // image alone and pin the revision as before.
        let (slot, current) = match (
            find(spec.map(|s| &s.containers), c),
            find(spec.and_then(|s| s.init_containers.as_ref()), c),
        ) {
            (Some(img), _) => (Some(&mut containers), img),
            (None, Some(img)) => (Some(&mut init_containers), img),
            (None, None) => (None, None),
        };
        let image_stale = image.is_some_and(|i| slot.is_some() && current.as_deref() != Some(i));
        if pinned && !image_stale {
            continue;
        }
        annotations.insert(keys::revision_annotation(c), json!(revision));
        if let (Some(slot), Some(image)) = (slot, image) {
            slot.push(json!({ "name": c, "image": image }));
        }
    }
    if annotations.is_empty() {
        return None;
    }
    let mut patch = json!({ "metadata": { "annotations": annotations } });
    if !containers.is_empty() {
        patch["spec"]["containers"] = Value::Array(containers);
    }
    if !init_containers.is_empty() {
        patch["spec"]["initContainers"] = Value::Array(init_containers);
    }
    Some(patch)
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

async fn roll_kind<K>(ctx: &Ctx, ns: &str, name: &str, revision: &str, image: Option<&str>)
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
        let Some(template_patch) = template_patch(template, name, revision, image) else {
            continue;
        };
        let patch = json!({ "spec": { "template": template_patch } });
        let pp = PatchParams {
            field_manager: Some(keys::FIELD_MANAGER.into()),
            ..Default::default()
        };
        match api
            .patch(&w.name_any(), &pp, &Patch::Strategic(&patch))
            .await
        {
            Ok(_) => {
                tracing::info!(kind = %K::kind(&()), namespace = ns, workload = w.name_any(), revision, image, "rolled")
            }
            Err(e) => {
                tracing::warn!(kind = %K::kind(&()), workload = w.name_any(), error = %e, "roll failed")
            }
        }
    }
}

/// Rolls every workload in the GitImage's namespace that uses it. `image` is written into the
/// template too when `controller.pinTemplateImage` is set.
pub async fn roll(ctx: &Ctx, gi: &GitImage, revision: &str, image: Option<&str>) {
    let ns = gi.namespace().unwrap_or_default();
    let name = gi.name_any();
    roll_kind::<Deployment>(ctx, &ns, &name, revision, image).await;
    roll_kind::<StatefulSet>(ctx, &ns, &name, revision, image).await;
    roll_kind::<DaemonSet>(ctx, &ns, &name, revision, image).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: &str = "localhost:25050/ns/myapp@sha256:1";

    fn template(yaml: &str) -> PodTemplateSpec {
        serde_saphyr::from_str(yaml).unwrap()
    }

    fn pinned(patch: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = patch["metadata"]["annotations"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn pins_unpinned_and_outdated_containers() {
        let t = template(
            r#"
metadata:
  annotations:
    gitimage.dev/image.web: myapp
    gitimage.dev/image.worker: myapp
    gitimage.dev/revision.worker: new
    gitimage.dev/image.other: otherapp
spec:
  containers:
    - { name: web, image: placeholder }
    - { name: worker, image: placeholder }
    - { name: other, image: placeholder }
"#,
        );
        let p = template_patch(&t, "myapp", "new", None).unwrap();
        assert_eq!(pinned(&p), vec!["gitimage.dev/revision.web"]);
        assert!(p.get("spec").is_none(), "{p}");
        assert!(template_patch(&t, "nothing", "new", None).is_none());
    }

    #[test]
    fn writes_the_image_of_bound_containers_only() {
        let t = template(
            r#"
metadata:
  annotations:
    gitimage.dev/image.web: myapp
    gitimage.dev/image.migrate: myapp
    gitimage.dev/image.other: otherapp
spec:
  initContainers:
    - { name: migrate, image: placeholder }
  containers:
    - { name: sidecar, image: nginx }
    - { name: web, image: placeholder }
    - { name: other, image: placeholder }
"#,
        );
        let p = template_patch(&t, "myapp", "new", Some(IMG)).unwrap();
        assert_eq!(
            pinned(&p),
            vec!["gitimage.dev/revision.migrate", "gitimage.dev/revision.web"]
        );
        assert_eq!(
            p["spec"]["containers"],
            json!([{ "name": "web", "image": IMG }])
        );
        assert_eq!(
            p["spec"]["initContainers"],
            json!([{ "name": "migrate", "image": IMG }])
        );
    }

    #[test]
    fn pinned_revision_with_wrong_image_is_repinned() {
        let t = template(
            r#"
metadata:
  annotations:
    gitimage.dev/image.web: myapp
    gitimage.dev/revision.web: new
spec:
  containers:
    - { name: web, image: placeholder }
"#,
        );
        assert!(template_patch(&t, "myapp", "new", None).is_none());
        let p = template_patch(&t, "myapp", "new", Some(IMG)).unwrap();
        assert_eq!(p["spec"]["containers"][0]["image"], IMG);

        let done = template(&format!(
            r#"
metadata:
  annotations:
    gitimage.dev/image.web: myapp
    gitimage.dev/revision.web: new
spec:
  containers:
    - {{ name: web, image: "{IMG}" }}
"#
        ));
        assert!(template_patch(&done, "myapp", "new", Some(IMG)).is_none());
    }

    #[test]
    fn missing_container_still_gets_its_revision() {
        let t = template(
            r#"
metadata:
  annotations:
    gitimage.dev/image.api: myapp
spec:
  containers:
    - { name: web, image: placeholder }
"#,
        );
        let p = template_patch(&t, "myapp", "new", Some(IMG)).unwrap();
        assert_eq!(pinned(&p), vec!["gitimage.dev/revision.api"]);
        assert!(p.get("spec").is_none(), "{p}");
    }
}
