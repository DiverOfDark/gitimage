//! `GET /api/gitimages/{ns}/{name}/logs?revision=<sha>`: the log of one build as Server-Sent
//! Events, read from the build pod with the caller's own permissions (`pods` get/list and
//! `pods/log` get in that namespace). Follows a running build; replays a finished one for as
//! long as its pod exists.
//!
//! Events: `status` (waiting for the pod or a container), `text` (a plain line), `build`
//! (BuildKit steps, their output, and the layer work inside them), `end` (always last).

use std::convert::Infallible;
use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{AsyncBufReadExt, Stream, TryStreamExt};
use gitimage_api::keys;
use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use kube::ResourceExt;
use kube::api::{Api, ListParams, LogParams};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use super::auth::Identity;
use super::buildlog::{self, Parsed};

const POLL: Duration = Duration::from_secs(2);
/// How long to wait for the build pod to appear before giving up.
const POD_WAIT: Duration = Duration::from_secs(90);

#[derive(Deserialize)]
pub struct LogQuery {
    revision: String,
    /// `false` for a build that already ended: its pod exists now or never will again.
    #[serde(default = "yes")]
    wait: bool,
}

fn yes() -> bool {
    true
}

/// Label values only: nothing here may change the meaning of the selector.
fn label_safe(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn event(name: &str, data: serde_json::Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

enum ContainerState {
    Started,
    /// Not running yet; why, if the kubelet says.
    Waiting(Option<String>),
    /// The pod is over and this container never ran.
    Never,
}

fn container_state(pod: &Pod, container: &str) -> ContainerState {
    let status = pod.status.as_ref();
    let find = |list: Option<&Vec<ContainerStatus>>| {
        list.and_then(|l| l.iter().find(|c| c.name == container))
            .cloned()
    };
    let cs = find(status.and_then(|s| s.init_container_statuses.as_ref()))
        .or_else(|| find(status.and_then(|s| s.container_statuses.as_ref())));
    let state = cs.and_then(|c| c.state);
    if state
        .as_ref()
        .is_some_and(|s| s.running.is_some() || s.terminated.is_some())
    {
        return ContainerState::Started;
    }
    let phase = status.and_then(|s| s.phase.as_deref()).unwrap_or_default();
    if phase == "Failed" || phase == "Succeeded" {
        return ContainerState::Never;
    }
    let reason = state
        .and_then(|s| s.waiting)
        .and_then(|w| w.reason)
        .or_else(|| {
            // Not scheduled yet: the PodScheduled condition says why.
            status
                .and_then(|s| s.conditions.as_ref())
                .and_then(|c| c.iter().find(|c| c.type_ == "PodScheduled"))
                .filter(|c| c.status != "True")
                .and_then(|c| c.message.clone().or_else(|| c.reason.clone()))
        });
    ContainerState::Waiting(reason)
}

/// Why a finished pod failed, from its terminated containers.
fn failure_of(pod: &Pod) -> Option<String> {
    let status = pod.status.as_ref()?;
    status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten())
        .find_map(|c| {
            let t = c.state.as_ref()?.terminated.as_ref()?;
            (t.exit_code != 0).then(|| {
                format!(
                    "container {} exited with code {}{}",
                    c.name,
                    t.exit_code,
                    t.reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                )
            })
        })
}

async fn run(id: Identity, ns: String, name: String, q: LogQuery, tx: mpsc::Sender<Event>) {
    let LogQuery { revision, wait } = q;
    macro_rules! send {
        ($name:expr, $data:expr) => {
            if tx.send(event($name, $data)).await.is_err() {
                return; // the browser went away
            }
        };
    }
    if !label_safe(&name) || !label_safe(&revision) {
        send!(
            "end",
            json!({ "phase": "Error", "message": "invalid name or revision" })
        );
        return;
    }
    let pods: Api<Pod> = Api::namespaced(id.client.clone(), &ns);
    let selector = format!(
        "{}={},{}={name},{}={revision}",
        keys::ROLE_LABEL,
        keys::ROLE_BUILD,
        keys::NAME_LABEL,
        keys::REVISION_LABEL
    );
    let lp = ListParams::default().labels(&selector);

    // The newest pod of this commit (a rebuild makes a second Job for the same sha).
    let started = tokio::time::Instant::now();
    let pod_name = loop {
        match pods.list(&lp).await {
            Ok(list) => {
                if let Some(p) = list
                    .items
                    .into_iter()
                    .max_by_key(|p| p.metadata.creation_timestamp.clone())
                {
                    break p.name_any();
                }
            }
            Err(e) => {
                send!("end", json!({ "phase": "Error", "message": e.to_string() }));
                return;
            }
        }
        if !wait || started.elapsed() >= POD_WAIT {
            send!(
                "end",
                json!({
                    "phase": "Gone",
                    "message": "No build pod for this commit. Build pods are removed one hour after the build ends."
                })
            );
            return;
        }
        send!("status", json!({ "detail": "waiting for the build pod" }));
        tokio::time::sleep(POLL).await;
    };

    for container in ["clone", "buildkit"] {
        // Wait until the container has logs to read.
        let ran = loop {
            if id.expired() {
                send!(
                    "end",
                    json!({ "phase": "Error", "message": "session expired" })
                );
                return;
            }
            let pod = match pods.get(&pod_name).await {
                Ok(p) => p,
                Err(e) => {
                    send!("end", json!({ "phase": "Gone", "message": e.to_string() }));
                    return;
                }
            };
            match container_state(&pod, container) {
                ContainerState::Started => break true,
                ContainerState::Never => break false,
                ContainerState::Waiting(reason) => {
                    let detail = match reason {
                        Some(r) => format!("{container}: {r}"),
                        None => format!("waiting for {container} to start"),
                    };
                    send!("status", json!({ "detail": detail }));
                    tokio::time::sleep(POLL).await;
                }
            }
        };
        if !ran {
            continue;
        }
        let lp = LogParams {
            container: Some(container.into()),
            follow: true,
            ..Default::default()
        };
        let stream = match pods.log_stream(&pod_name, &lp).await {
            Ok(s) => s,
            // An empty log must not pass for "nothing was printed".
            Err(e) => {
                let message = match &e {
                    kube::Error::Api(s) if s.code == 403 => format!(
                        "You may not read pod logs in {ns} (get on pods/log): {}",
                        s.message
                    ),
                    other => format!("reading the log of {container}: {other}"),
                };
                send!("end", json!({ "phase": "Error", "message": message }));
                return;
            }
        };
        send!("status", json!({ "detail": "" }));
        let mut lines = stream.lines();
        while let Ok(Some(line)) = lines.try_next().await {
            if container == "clone" {
                let text = buildlog::prettify_clone_line(&line);
                if !text.is_empty() {
                    send!("text", json!({ "source": container, "text": text }));
                }
                continue;
            }
            match buildlog::parse_line(&line) {
                Parsed::Build { steps, logs, tasks } => {
                    send!(
                        "build",
                        json!({ "steps": steps, "logs": logs, "tasks": tasks })
                    );
                }
                Parsed::Text(text) => send!("text", json!({ "source": container, "text": text })),
                Parsed::Nothing => {}
            }
        }
    }

    // The log ends when the container exits; the pod's phase follows a moment later.
    let mut phase = String::from("Unknown");
    let mut message = None;
    for _ in 0..10 {
        match pods.get(&pod_name).await {
            Ok(p) => {
                phase = p
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.clone())
                    .unwrap_or_else(|| "Unknown".into());
                message = failure_of(&p);
            }
            Err(e) => {
                phase = "Gone".into();
                message = Some(e.to_string());
            }
        }
        if phase != "Running" && phase != "Pending" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    send!("end", json!({ "phase": phase, "message": message }));
}

pub async fn stream(
    Extension(id): Extension<Identity>,
    Path((ns, name)): Path<(String, String)>,
    Query(q): Query<LogQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut rx) = mpsc::channel::<Event>(256);
    tokio::spawn(run(id, ns, name, q, tx));
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx).map(|e| e.map(Ok)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_values_are_plain_label_values() {
        assert!(label_safe("myapp"));
        assert!(label_safe("3f2c9e1a0b"));
        assert!(!label_safe("a,b=c"));
        assert!(!label_safe("a b"));
        assert!(!label_safe(""));
    }

    fn pod(yaml: &str) -> Pod {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn container_states() {
        let running = pod(r#"
status:
  phase: Running
  initContainerStatuses:
    - { name: clone, ready: true, restartCount: 0, image: i, imageID: i, state: { terminated: { exitCode: 0 } } }
  containerStatuses:
    - { name: buildkit, ready: true, restartCount: 0, image: i, imageID: i, state: { running: {} } }
"#);
        assert!(matches!(
            container_state(&running, "clone"),
            ContainerState::Started
        ));
        assert!(matches!(
            container_state(&running, "buildkit"),
            ContainerState::Started
        ));

        let pulling = pod(r#"
status:
  phase: Pending
  containerStatuses:
    - { name: buildkit, ready: false, restartCount: 0, image: i, imageID: "", state: { waiting: { reason: ImagePullBackOff } } }
"#);
        assert!(matches!(
            container_state(&pulling, "buildkit"),
            ContainerState::Waiting(Some(r)) if r == "ImagePullBackOff"
        ));

        let unschedulable = pod(r#"
status:
  phase: Pending
  conditions:
    - { type: PodScheduled, status: "False", reason: Unschedulable, message: "0/3 nodes are available" }
"#);
        assert!(matches!(
            container_state(&unschedulable, "clone"),
            ContainerState::Waiting(Some(r)) if r == "0/3 nodes are available"
        ));

        let failed_early = pod(r#"
status:
  phase: Failed
  initContainerStatuses:
    - { name: clone, ready: false, restartCount: 0, image: i, imageID: i, state: { terminated: { exitCode: 1, reason: Error } } }
"#);
        assert!(matches!(
            container_state(&failed_early, "buildkit"),
            ContainerState::Never
        ));
        assert_eq!(
            failure_of(&failed_early).as_deref(),
            Some("container clone exited with code 1 (Error)")
        );
    }
}
