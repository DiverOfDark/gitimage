//! Pure admission logic: pod in, JSON patch or denial out. No I/O.

use gitimage_api::{GitImage, keys};
use json_patch::{AddOperation, Patch, PatchOperation, ReplaceOperation};
use jsonptr::PointerBuf;
use k8s_openapi::api::core::v1::{Container, Pod};
use serde_json::{Value, json};

/// What the webhook needs from the cluster config.
#[derive(Clone, Debug)]
pub struct MutateConfig {
    /// Host every GitImage image ref starts with: `localhost:25050` (s3) or the registry URL.
    pub pull_host: String,
    /// `host:port` of the push endpoint; the guard also protects refs to it.
    pub push_authority: String,
    /// Name of the pull Secret the controller copies into tenant namespaces, if any.
    pub pull_secret: Option<String>,
}

impl MutateConfig {
    /// `<pull_host>/<ns>/<name>@<digest>`
    pub fn image_ref(&self, ns: &str, name: &str, digest: &str) -> String {
        format!(
            "{}/{ns}/{name}@{digest}",
            self.pull_host.trim_end_matches('/')
        )
    }
}

/// The admission decision.
#[derive(Debug, PartialEq)]
pub enum Decision {
    Allow(Vec<PatchOperation>),
    Deny(String),
}

/// One annotated container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub container: String,
    pub gitimage: String,
    pub revision_pin: Option<String>,
}

/// Annotated containers of a pod, sorted by container name.
pub fn bindings(pod: &Pod) -> Vec<Binding> {
    let Some(ann) = pod.metadata.annotations.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<Binding> = ann
        .iter()
        .filter_map(|(k, v)| {
            keys::image_annotation_container(k).map(|c| Binding {
                container: c.to_string(),
                gitimage: v.trim().to_string(),
                revision_pin: ann.get(&keys::revision_annotation(c)).cloned(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.container.cmp(&b.container));
    out
}

/// Where a container lives in the pod spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Container(usize),
    Init(usize),
}

impl Slot {
    pub fn image_pointer(self) -> PointerBuf {
        match self {
            Slot::Container(i) => ptr(&["spec", "containers", &i.to_string(), "image"]),
            Slot::Init(i) => ptr(&["spec", "initContainers", &i.to_string(), "image"]),
        }
    }
}

pub fn find_container(pod: &Pod, name: &str) -> Option<Slot> {
    let spec = pod.spec.as_ref()?;
    if let Some(i) = spec.containers.iter().position(|c| c.name == name) {
        return Some(Slot::Container(i));
    }
    spec.init_containers
        .as_ref()?
        .iter()
        .position(|c| c.name == name)
        .map(Slot::Init)
}

/// The digest a binding resolves to right now, if its build exists.
pub fn resolve_digest<'a>(binding: &Binding, gi: Option<&'a GitImage>) -> Option<&'a str> {
    let status = gi?.status.as_ref()?;
    let revision = binding
        .revision_pin
        .as_deref()
        .or(status.revision.as_deref())?;
    status.digest_for(revision)
}

/// Mutation for a pod being created in `namespace`.
pub fn mutate(
    pod: &Pod,
    namespace: &str,
    lookup: impl Fn(&str) -> Option<GitImage>,
    cfg: &MutateConfig,
) -> Decision {
    if let Some(reason) = cross_tenant_violation(pod, namespace, cfg) {
        return Decision::Deny(reason);
    }
    let bindings = bindings(pod);
    if bindings.is_empty() {
        return Decision::Allow(Vec::new());
    }
    let mut ops = Vec::new();
    let mut gated = false;
    for b in &bindings {
        if b.gitimage.is_empty() {
            return Decision::Deny(format!(
                "annotation {}{} has an empty GitImage name",
                keys::IMAGE_ANNOTATION_PREFIX,
                b.container
            ));
        }
        let Some(slot) = find_container(pod, &b.container) else {
            return Decision::Deny(format!(
                "annotation {}{} names container {:?}, but the pod has no such container",
                keys::IMAGE_ANNOTATION_PREFIX,
                b.container,
                b.container
            ));
        };
        let gi = lookup(&b.gitimage);
        match resolve_digest(b, gi.as_ref()) {
            Some(digest) => ops.push(replace(
                slot.image_pointer(),
                json!(cfg.image_ref(namespace, &b.gitimage, digest)),
            )),
            None => gated = true,
        }
    }
    if gated {
        ops.extend(gate_ops(pod));
    }
    if let Some(secret) = &cfg.pull_secret {
        ops.extend(pull_secret_ops(pod, secret));
    }
    Decision::Allow(ops)
}

fn gate_ops(pod: &Pod) -> Vec<PatchOperation> {
    let mut ops = Vec::new();
    let gates = pod.spec.as_ref().and_then(|s| s.scheduling_gates.as_ref());
    let has_gate = gates.is_some_and(|g| g.iter().any(|g| g.name == keys::SCHEDULING_GATE));
    if !has_gate {
        let gate = json!({ "name": keys::SCHEDULING_GATE });
        match gates {
            Some(_) => ops.push(add(ptr(&["spec", "schedulingGates", "-"]), gate)),
            None => ops.push(add(ptr(&["spec", "schedulingGates"]), json!([gate]))),
        }
    }
    match pod.metadata.labels.as_ref() {
        Some(_) => ops.push(add(
            ptr(&["metadata", "labels", keys::GATED_LABEL]),
            json!("true"),
        )),
        None => ops.push(add(
            ptr(&["metadata", "labels"]),
            json!({ keys::GATED_LABEL: "true" }),
        )),
    }
    ops
}

fn pull_secret_ops(pod: &Pod, secret: &str) -> Vec<PatchOperation> {
    let existing = pod
        .spec
        .as_ref()
        .and_then(|s| s.image_pull_secrets.as_ref());
    let entry = json!({ "name": secret });
    match existing {
        Some(list) if list.iter().any(|s| s.name == secret) => Vec::new(),
        Some(_) => vec![add(ptr(&["spec", "imagePullSecrets", "-"]), entry)],
        None => vec![add(ptr(&["spec", "imagePullSecrets"]), json!([entry]))],
    }
}

fn all_containers(pod: &Pod) -> impl Iterator<Item = &str> {
    let spec = pod.spec.as_ref();
    let main = spec.map(|s| s.containers.as_slice()).unwrap_or_default();
    let init: &[Container] = spec
        .and_then(|s| s.init_containers.as_deref())
        .unwrap_or_default();
    let eph = spec
        .and_then(|s| s.ephemeral_containers.as_deref())
        .unwrap_or_default();
    main.iter()
        .chain(init.iter())
        .filter_map(|c| c.image.as_deref())
        .chain(eph.iter().filter_map(|c| c.image.as_deref()))
}

/// A pod may only reference GitImage hosts under its own namespace.
pub fn cross_tenant_violation(pod: &Pod, namespace: &str, cfg: &MutateConfig) -> Option<String> {
    let hosts = [
        cfg.pull_host.trim_end_matches('/'),
        cfg.push_authority.as_str(),
    ];
    for image in all_containers(pod) {
        for host in hosts.iter().filter(|h| !h.is_empty()) {
            if let Some(rest) = image.strip_prefix(&format!("{host}/")) {
                let ns = rest.split('/').next().unwrap_or_default();
                if ns != namespace {
                    return Some(format!(
                        "image {image:?} belongs to namespace {ns:?}; pods in {namespace:?} may only use GitImage images from their own namespace"
                    ));
                }
            }
        }
    }
    None
}

/// Patch that releases a gated pod: set each image, drop the gate and the label.
/// `test` on resourceVersion makes it fail if the pod changed since it was read.
pub fn release_patch(pod: &Pod, images: &[(Slot, String)]) -> Patch {
    let mut ops = Vec::new();
    if let Some(rv) = &pod.metadata.resource_version {
        ops.push(PatchOperation::Test(json_patch::TestOperation {
            path: ptr(&["metadata", "resourceVersion"]),
            value: json!(rv),
        }));
    }
    for (slot, image) in images {
        ops.push(replace(slot.image_pointer(), json!(image)));
    }
    if let Some(gates) = pod.spec.as_ref().and_then(|s| s.scheduling_gates.as_ref()) {
        let rest: Vec<Value> = gates
            .iter()
            .filter(|g| g.name != keys::SCHEDULING_GATE)
            .map(|g| json!({ "name": g.name }))
            .collect();
        ops.push(replace(
            ptr(&["spec", "schedulingGates"]),
            Value::Array(rest),
        ));
    }
    if pod
        .metadata
        .labels
        .as_ref()
        .is_some_and(|l| l.contains_key(keys::GATED_LABEL))
    {
        ops.push(PatchOperation::Remove(json_patch::RemoveOperation {
            path: ptr(&["metadata", "labels", keys::GATED_LABEL]),
        }));
    }
    Patch(ops)
}

fn ptr(parts: &[&str]) -> PointerBuf {
    PointerBuf::from_tokens(parts.iter().copied())
}

fn add(path: PointerBuf, value: Value) -> PatchOperation {
    PatchOperation::Add(AddOperation { path, value })
}

fn replace(path: PointerBuf, value: Value) -> PatchOperation {
    PatchOperation::Replace(ReplaceOperation { path, value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitimage_api::{BuildOutcome, BuildRecord, GitImageSpec, GitImageStatus};

    const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const OLD: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    fn cfg() -> MutateConfig {
        MutateConfig {
            pull_host: "localhost:25050".into(),
            push_authority: "gitimage-push.gitimage-system.svc:5000".into(),
            pull_secret: None,
        }
    }

    fn pod(yaml: &str) -> Pod {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn web_pod(extra_ann: &str) -> Pod {
        pod(&format!(
            r#"
metadata:
  annotations:
    gitimage.dev/image.web: myapp
{extra_ann}
spec:
  containers:
    - name: sidecar
      image: nginx
    - name: web
      image: placeholder
"#
        ))
    }

    fn built() -> GitImage {
        let mut gi = GitImage::new(
            "myapp",
            serde_yaml::from_str::<GitImageSpec>("url: x").unwrap(),
        );
        gi.status = Some(GitImageStatus {
            revision: Some("bbb".into()),
            builds: vec![record("bbb", Some(DIGEST)), record("aaa", Some(OLD))],
            ..Default::default()
        });
        gi
    }

    fn record(rev: &str, digest: Option<&str>) -> BuildRecord {
        BuildRecord {
            revision: rev.into(),
            outcome: BuildOutcome::Built,
            digest: digest.map(Into::into),
            context_tree: None,
            source_revision: None,
            reason: None,
            finished_at: "2026-11-07T00:00:00Z".into(),
        }
    }

    fn apply(pod: &Pod, ops: Vec<PatchOperation>) -> Pod {
        let mut v = serde_json::to_value(pod).unwrap();
        json_patch::patch(&mut v, &Patch(ops)).unwrap();
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn built_image_is_patched_without_gate() {
        let p = web_pod("");
        let Decision::Allow(ops) = mutate(&p, "team-a", |_| Some(built()), &cfg()) else {
            panic!()
        };
        let out = apply(&p, ops);
        let spec = out.spec.unwrap();
        assert_eq!(
            spec.containers[1].image.as_deref(),
            Some(format!("localhost:25050/team-a/myapp@{DIGEST}").as_str())
        );
        assert_eq!(spec.containers[0].image.as_deref(), Some("nginx"));
        assert!(spec.scheduling_gates.is_none());
    }

    #[test]
    fn revision_pin_selects_older_build() {
        let p = web_pod("    gitimage.dev/revision.web: aaa");
        let Decision::Allow(ops) = mutate(&p, "team-a", |_| Some(built()), &cfg()) else {
            panic!()
        };
        let out = apply(&p, ops);
        assert!(
            out.spec.unwrap().containers[1]
                .image
                .clone()
                .unwrap()
                .ends_with(OLD)
        );
    }

    #[test]
    fn missing_gitimage_gates_the_pod() {
        let p = web_pod("");
        let Decision::Allow(ops) = mutate(&p, "team-a", |_| None, &cfg()) else {
            panic!()
        };
        let out = apply(&p, ops);
        assert_eq!(
            out.spec.unwrap().scheduling_gates.unwrap()[0].name,
            "gitimage.dev/build"
        );
        assert_eq!(out.metadata.labels.unwrap()["gitimage.dev/gated"], "true");
    }

    #[test]
    fn unknown_container_is_denied() {
        let p = pod(
            "metadata: { annotations: { gitimage.dev/image.api: myapp } }\nspec: { containers: [ { name: web, image: x } ] }",
        );
        assert!(
            matches!(mutate(&p, "team-a", |_| None, &cfg()), Decision::Deny(m) if m.contains("api"))
        );
    }

    #[test]
    fn existing_gates_and_labels_are_kept() {
        let p = pod(r#"
metadata: { labels: { app: web }, annotations: { gitimage.dev/image.web: myapp } }
spec:
  schedulingGates: [ { name: other } ]
  containers: [ { name: web, image: x } ]
"#);
        let Decision::Allow(ops) = mutate(&p, "team-a", |_| None, &cfg()) else {
            panic!()
        };
        let out = apply(&p, ops);
        let gates: Vec<String> = out
            .spec
            .unwrap()
            .scheduling_gates
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(gates, vec!["other", "gitimage.dev/build"]);
        let labels = out.metadata.labels.unwrap();
        assert_eq!(labels["app"], "web");
        assert_eq!(labels["gitimage.dev/gated"], "true");
    }

    #[test]
    fn pull_secret_is_added_once() {
        let mut c = cfg();
        c.pull_secret = Some("gitimage-pull".into());
        let p = web_pod("");
        let Decision::Allow(ops) = mutate(&p, "team-a", |_| Some(built()), &c) else {
            panic!()
        };
        let out = apply(&p, ops);
        assert_eq!(
            out.spec.unwrap().image_pull_secrets.unwrap()[0].name,
            "gitimage-pull"
        );
    }

    #[test]
    fn cross_tenant_ref_is_denied_even_without_annotation() {
        let p = pod(
            "spec: { containers: [ { name: x, image: 'localhost:25050/team-b/app@sha256:1' } ] }",
        );
        assert!(matches!(
            mutate(&p, "team-a", |_| None, &cfg()),
            Decision::Deny(_)
        ));
        let own = pod(
            "spec: { containers: [ { name: x, image: 'localhost:25050/team-a/app@sha256:1' } ] }",
        );
        assert_eq!(
            mutate(&own, "team-a", |_| None, &cfg()),
            Decision::Allow(vec![])
        );
        let push = pod(
            "spec: { initContainers: [ { name: i, image: 'gitimage-push.gitimage-system.svc:5000/team-b/x:1' } ], containers: [] }",
        );
        assert!(matches!(
            mutate(&push, "team-a", |_| None, &cfg()),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn unrelated_pod_is_untouched() {
        let p = pod("spec: { containers: [ { name: x, image: nginx } ] }");
        assert_eq!(
            mutate(&p, "team-a", |_| None, &cfg()),
            Decision::Allow(vec![])
        );
    }

    #[test]
    fn release_patch_ungates() {
        let p = web_pod("");
        let Decision::Allow(ops) = mutate(&p, "team-a", |_| None, &cfg()) else {
            panic!()
        };
        let mut gated = apply(&p, ops);
        gated.metadata.resource_version = Some("42".into());
        let patch = release_patch(&gated, &[(Slot::Container(1), "img@sha256:x".into())]);
        let mut v = serde_json::to_value(&gated).unwrap();
        json_patch::patch(&mut v, &patch).unwrap();
        let out: Pod = serde_json::from_value(v).unwrap();
        assert_eq!(
            out.spec.as_ref().unwrap().containers[1].image.as_deref(),
            Some("img@sha256:x")
        );
        assert!(out.spec.unwrap().scheduling_gates.unwrap().is_empty());
        assert!(
            !out.metadata
                .labels
                .unwrap()
                .contains_key("gitimage.dev/gated")
        );
    }
}
