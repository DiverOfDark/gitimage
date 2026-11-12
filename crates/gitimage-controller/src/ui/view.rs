//! JSON shapes of the UI API, derived from GitImages, workloads and pods.

use gitimage_api::gitimage::condition_types as cond;
use gitimage_api::{BuildRecord, GitImage, keys};
use gitimage_webhook::bindings;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, PodTemplateSpec};
use kube::ResourceExt;
use serde::Serialize;

use crate::job::public_source_url;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// A build of the newest commit is running.
    Building,
    /// The newest commit failed to build.
    Failed,
    /// The newest commit has an image.
    Ready,
    /// Nothing built yet and nothing running.
    Pending,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub reason: String,
    pub message: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GitImageView {
    pub namespace: String,
    pub name: String,
    /// Without user info.
    pub url: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub context: String,
    pub dockerfile: String,
    pub state: State,
    /// Current BuildKit step while building, e.g. `step 4/12 RUN npm ci`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub building_since: Option<String>,
    /// Commit of the running build. With `onNewCommit: wait` this can be older than
    /// `latestRevision`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub building_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// Commit whose image pods get now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Newest commit seen on the ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Only in the detail view: the URL carries the hook token.
    #[serde(rename = "webhookURL", skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Pods held by the scheduling gate until this GitImage has an image.
    pub gated_pods: usize,
    pub builds: Vec<BuildRecord>,
}

/// `step` is the part after `<sha7>: ` of the Building condition message.
fn step_of(message: &str) -> Option<String> {
    let step = message.split_once(": ").map(|(_, s)| s).unwrap_or(message);
    (!step.is_empty()).then(|| step.to_string())
}

pub fn gitimage_view(
    gi: &GitImage,
    gated_pods: usize,
    with_webhook_url: bool,
    running: Option<String>,
) -> GitImageView {
    let status = gi.status.clone().unwrap_or_default();
    let is_true = |t: &str| status.condition(t).is_some_and(|c| c.status == "True");
    let building = status
        .condition(cond::BUILDING)
        .filter(|c| c.status == "True");
    let failed = status
        .condition(cond::FAILED)
        .filter(|c| c.status == "True");
    let state = if building.is_some() {
        State::Building
    } else if failed.is_some() {
        State::Failed
    } else if is_true(cond::READY) {
        State::Ready
    } else {
        State::Pending
    };
    GitImageView {
        namespace: gi.namespace().unwrap_or_default(),
        name: gi.name_any(),
        url: public_source_url(&gi.spec.url),
        git_ref: gi.spec.git_ref.clone(),
        context: gi.spec.build.context.clone(),
        dockerfile: gi.spec.build.dockerfile.clone(),
        state,
        step: building.and_then(|c| step_of(&c.message)),
        building_since: building.map(|c| c.last_transition_time.clone()),
        building_revision: building
            .and_then(|_| running.or_else(|| status.latest_revision.clone())),
        failure: failed.map(|c| Failure {
            reason: c.reason.clone(),
            message: c.message.clone(),
        }),
        revision: status.revision.clone(),
        latest_revision: status.latest_revision.clone(),
        image: status.image.clone(),
        webhook_url: status.webhook_url.clone().filter(|_| with_webhook_url),
        created_at: gi
            .metadata
            .creation_timestamp
            .as_ref()
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| v.as_str().map(str::to_string)),
        gated_pods,
        builds: status.builds,
    }
}

/// Commit of the newest unfinished build Job of GitImage `name` in `ns`.
pub fn running_revision(jobs: &[std::sync::Arc<Job>], ns: &str, name: &str) -> Option<String> {
    jobs.iter()
        .filter(|j| j.namespace().as_deref() == Some(ns))
        .filter(|j| j.labels().get(keys::NAME_LABEL).map(String::as_str) == Some(name))
        .filter(|j| {
            j.status
                .as_ref()
                .is_none_or(|s| s.succeeded.unwrap_or(0) == 0 && s.failed.unwrap_or(0) == 0)
        })
        .max_by_key(|j| j.metadata.creation_timestamp.clone())
        .and_then(|j| j.labels().get(keys::REVISION_LABEL).cloned())
}

/// Gated pods in `ns` that wait for GitImage `name`.
pub fn count_gated(pods: &[std::sync::Arc<Pod>], ns: &str, name: &str) -> usize {
    pods.iter()
        .filter(|p| p.namespace().as_deref() == Some(ns))
        .filter(|p| bindings(p).iter().any(|b| b.gitimage == name))
        .count()
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BoundContainer {
    pub container: String,
    /// `gitimage.dev/revision.<container>`, written by the roller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkloadView {
    pub kind: &'static str,
    pub name: String,
    pub containers: Vec<BoundContainer>,
    /// `gitimage.dev/rollout: manual`.
    pub manual: bool,
    /// Every bound container is pinned to the GitImage's current revision.
    pub up_to_date: bool,
    pub replicas: i32,
    pub ready: i32,
    pub updated: i32,
}

fn bound_containers(template: &PodTemplateSpec, name: &str) -> Vec<BoundContainer> {
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
            (v.trim() == name).then(|| BoundContainer {
                container: c.to_string(),
                revision: ann.get(&keys::revision_annotation(c)).cloned(),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn workload(
    kind: &'static str,
    name: String,
    annotations: &std::collections::BTreeMap<String, String>,
    template: Option<&PodTemplateSpec>,
    gitimage: &str,
    revision: Option<&str>,
    counts: (i32, i32, i32),
) -> Option<WorkloadView> {
    let containers = bound_containers(template?, gitimage);
    if containers.is_empty() {
        return None;
    }
    Some(WorkloadView {
        kind,
        name,
        manual: annotations
            .get(keys::ROLLOUT_ANNOTATION)
            .is_some_and(|v| v == "manual"),
        up_to_date: revision.is_some()
            && containers.iter().all(|c| c.revision.as_deref() == revision),
        containers,
        replicas: counts.0,
        ready: counts.1,
        updated: counts.2,
    })
}

pub fn deployment_view(d: &Deployment, gitimage: &str, rev: Option<&str>) -> Option<WorkloadView> {
    let s = d.status.as_ref();
    workload(
        "Deployment",
        d.name_any(),
        d.annotations(),
        d.spec.as_ref().map(|s| &s.template),
        gitimage,
        rev,
        (
            d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1),
            s.and_then(|s| s.ready_replicas).unwrap_or(0),
            s.and_then(|s| s.updated_replicas).unwrap_or(0),
        ),
    )
}

pub fn statefulset_view(
    d: &StatefulSet,
    gitimage: &str,
    rev: Option<&str>,
) -> Option<WorkloadView> {
    let s = d.status.as_ref();
    workload(
        "StatefulSet",
        d.name_any(),
        d.annotations(),
        d.spec.as_ref().map(|s| &s.template),
        gitimage,
        rev,
        (
            d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1),
            s.and_then(|s| s.ready_replicas).unwrap_or(0),
            s.and_then(|s| s.updated_replicas).unwrap_or(0),
        ),
    )
}

pub fn daemonset_view(d: &DaemonSet, gitimage: &str, rev: Option<&str>) -> Option<WorkloadView> {
    let s = d.status.as_ref();
    workload(
        "DaemonSet",
        d.name_any(),
        d.annotations(),
        d.spec.as_ref().map(|s| &s.template),
        gitimage,
        rev,
        (
            s.map(|s| s.desired_number_scheduled).unwrap_or(0),
            s.map(|s| s.number_ready).unwrap_or(0),
            s.and_then(|s| s.updated_number_scheduled).unwrap_or(0),
        ),
    )
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GatedPodView {
    pub name: String,
    /// Containers of the pod that wait for this GitImage.
    pub containers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

pub fn gated_pod_view(pod: &Pod, gitimage: &str) -> Option<GatedPodView> {
    let containers: Vec<String> = bindings(pod)
        .into_iter()
        .filter(|b| b.gitimage == gitimage)
        .map(|b| b.container)
        .collect();
    let gated = pod
        .spec
        .as_ref()
        .and_then(|s| s.scheduling_gates.as_ref())
        .is_some_and(|g| g.iter().any(|g| g.name == keys::SCHEDULING_GATE));
    if containers.is_empty() || !gated {
        return None;
    }
    Some(GatedPodView {
        name: pod.name_any(),
        containers,
        owner: pod
            .metadata
            .owner_references
            .as_ref()
            .and_then(|o| o.first())
            .map(|o| format!("{}/{}", o.kind, o.name)),
        created_at: pod
            .metadata
            .creation_timestamp
            .as_ref()
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| v.as_str().map(str::to_string)),
    })
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    pub gitimage: GitImageView,
    pub workloads: Vec<WorkloadView>,
    pub gated_pods: Vec<GatedPodView>,
    /// What could not be loaded, e.g. `deployments: forbidden`.
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitimage_api::{GitImageSpec, GitImageStatus};

    fn gi(status_yaml: &str) -> GitImage {
        let mut g = GitImage::new(
            "myapp",
            serde_saphyr::from_str::<GitImageSpec>(
                "url: https://bot:secret@git.example.com/a/b.git",
            )
            .unwrap(),
        );
        g.metadata.namespace = Some("team-a".into());
        g.status = Some(serde_saphyr::from_str::<GitImageStatus>(status_yaml).unwrap());
        g
    }

    #[test]
    fn building_wins_and_carries_the_step() {
        let v = gitimage_view(
            &gi(r#"
revision: aaaaaaa
latestRevision: bbbbbbb
webhookURL: https://x/hooks/token
conditions:
  - { type: Ready, status: "True", reason: Built, lastTransitionTime: "2026-01-01T00:00:00Z" }
  - { type: Building, status: "True", reason: Started, message: "bbbbbbb: step 4/12 RUN npm ci", lastTransitionTime: "2026-01-02T00:00:00Z" }
"#),
            2,
            false,
            None,
        );
        assert_eq!(v.state, State::Building);
        assert_eq!(v.step.as_deref(), Some("step 4/12 RUN npm ci"));
        assert_eq!(v.building_since.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(v.gated_pods, 2);
        assert_eq!(v.url, "https://git.example.com/a/b.git");
        assert_eq!(v.webhook_url, None);
    }

    #[test]
    fn the_running_build_can_be_older_than_the_newest_commit() {
        let job = |name: &str, rev: &str, created: &str, status: &str| -> std::sync::Arc<Job> {
            std::sync::Arc::new(
                serde_saphyr::from_str(&format!(
                    "metadata:\n  name: {name}\n  namespace: team-a\n  creationTimestamp: \"{created}\"\n  labels: {{ gitimage.dev/name: myapp, gitimage.dev/revision: {rev} }}\nstatus: {status}\n"
                ))
                .unwrap(),
            )
        };
        let jobs = vec![
            job(
                "done",
                "aaaaaaa",
                "2026-01-01T00:00:00Z",
                "{ succeeded: 1 }",
            ),
            job("run", "bbbbbbb", "2026-01-02T00:00:00Z", "{ active: 1 }"),
        ];
        let running = running_revision(&jobs, "team-a", "myapp");
        assert_eq!(running.as_deref(), Some("bbbbbbb"));
        assert_eq!(running_revision(&jobs, "team-b", "myapp"), None);
        let g = gi(r#"
latestRevision: ccccccc
conditions:
  - { type: Building, status: "True", reason: Started, message: "bbbbbbb: starting", lastTransitionTime: "2026-01-02T00:00:00Z" }
"#);
        let v = gitimage_view(&g, 0, false, running);
        assert_eq!(v.building_revision.as_deref(), Some("bbbbbbb"));
        // Without a Job in the cache yet, the newest commit is the best guess.
        let v = gitimage_view(&g, 0, false, None);
        assert_eq!(v.building_revision.as_deref(), Some("ccccccc"));
        // Not building: no running revision.
        let idle = gitimage_view(&gi("latestRevision: ccccccc"), 0, false, Some("x".into()));
        assert_eq!(idle.building_revision, None);
    }

    #[test]
    fn failed_ready_and_pending() {
        let failed = gitimage_view(
            &gi(r#"
conditions:
  - { type: Ready, status: "True", reason: Built, lastTransitionTime: "2026-01-01T00:00:00Z" }
  - { type: Building, status: "False", reason: Idle, lastTransitionTime: "2026-01-01T00:00:00Z" }
  - { type: Failed, status: "True", reason: BuildFailed, message: "build of bbbbbbb failed", lastTransitionTime: "2026-01-01T00:00:00Z" }
"#),
            0,
            true,
            None,
        );
        assert_eq!(failed.state, State::Failed);
        assert_eq!(failed.failure.unwrap().reason, "BuildFailed");
        let ready = gitimage_view(
            &gi(r#"
conditions:
  - { type: Ready, status: "True", reason: Built, lastTransitionTime: "2026-01-01T00:00:00Z" }
  - { type: Failed, status: "False", lastTransitionTime: "2026-01-01T00:00:00Z" }
"#),
            0,
            true,
            None,
        );
        assert_eq!(ready.state, State::Ready);
        assert_eq!(
            gitimage_view(&gi("{}"), 0, true, None).state,
            State::Pending
        );
    }

    #[test]
    fn workload_views_know_pins_and_manual_rollout() {
        let d: Deployment = serde_saphyr::from_str(
            r#"
metadata:
  name: web
  annotations: { gitimage.dev/rollout: manual }
spec:
  replicas: 3
  selector: {}
  template:
    metadata:
      annotations:
        gitimage.dev/image.web: myapp
        gitimage.dev/revision.web: old
        gitimage.dev/image.side: other
status: { readyReplicas: 2, updatedReplicas: 1 }
"#,
        )
        .unwrap();
        let v = deployment_view(&d, "myapp", Some("new")).unwrap();
        assert!(v.manual && !v.up_to_date);
        assert_eq!((v.replicas, v.ready, v.updated), (3, 2, 1));
        assert_eq!(v.containers.len(), 1);
        assert!(
            deployment_view(&d, "myapp", Some("old"))
                .unwrap()
                .up_to_date
        );
        assert!(deployment_view(&d, "nothing", Some("old")).is_none());
    }

    #[test]
    fn gated_pod_view_needs_the_gate() {
        let pod: Pod = serde_saphyr::from_str(
            r#"
metadata:
  name: web-1
  annotations: { gitimage.dev/image.web: myapp }
  ownerReferences: [{ apiVersion: apps/v1, kind: ReplicaSet, name: web-abc, uid: u }]
spec:
  schedulingGates: [{ name: gitimage.dev/build }]
  containers: [{ name: web, image: placeholder }]
"#,
        )
        .unwrap();
        let v = gated_pod_view(&pod, "myapp").unwrap();
        assert_eq!(v.containers, vec!["web"]);
        assert_eq!(v.owner.as_deref(), Some("ReplicaSet/web-abc"));
        assert!(gated_pod_view(&pod, "other").is_none());
        let mut released = pod.clone();
        released.spec.as_mut().unwrap().scheduling_gates = None;
        assert!(gated_pod_view(&released, "myapp").is_none());
    }
}
