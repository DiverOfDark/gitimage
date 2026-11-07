//! Gate releaser: watches pods the webhook gated and releases each one once every annotated
//! container has a digest. Also posts build progress Events on waiting pods.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use gitimage_api::{GitImage, keys};
use gitimage_webhook::{bindings, find_container, release_patch, resolve_digest};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::events::{Event, EventType};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Resource, ResourceExt};

use crate::context::Ctx;

static NOT_FOUND_REPORTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kubernetes: {0}")]
    Kube(#[from] kube::Error),
}

async fn reconcile(pod: Arc<Pod>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = pod.namespace().unwrap_or_default();
    let mut images = Vec::new();
    let mut missing = Vec::new();
    for b in bindings(&pod) {
        let gi = ctx.gitimages.get(&ObjectRef::new(&b.gitimage).within(&ns));
        let Some(slot) = find_container(&pod, &b.container) else {
            continue;
        };
        match resolve_digest(&b, gi.as_deref()) {
            Some(d) => images.push((slot, ctx.image_ref(&ns, &b.gitimage, d))),
            None => {
                if gi.is_none() {
                    missing.push(b.gitimage.clone());
                }
                return report_missing(&ctx, &pod, &missing).await;
            }
        }
    }
    let api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    let patch = release_patch(&pod, &images);
    match api
        .patch(
            &pod.name_any(),
            &PatchParams::default(),
            &Patch::<()>::Json(patch),
        )
        .await
    {
        Ok(_) => {
            tracing::info!(namespace = %ns, pod = pod.name_any(), "released gated pod");
            Ok(Action::await_change())
        }
        // resourceVersion test failed or pod gone: the watch brings the new version.
        Err(kube::Error::Api(e)) if e.code == 422 || e.code == 409 || e.code == 404 => {
            Ok(Action::requeue(Duration::from_secs(2)))
        }
        Err(e) => Err(e.into()),
    }
}

async fn report_missing(ctx: &Ctx, pod: &Pod, missing: &[String]) -> Result<Action, Error> {
    let Some(uid) = pod.metadata.uid.clone() else {
        return Ok(Action::await_change());
    };
    if missing.is_empty() {
        return Ok(Action::await_change());
    }
    let first = {
        let mut set = NOT_FOUND_REPORTED.lock().unwrap();
        set.get_or_insert_with(HashSet::new).insert(uid)
    };
    if first {
        let ev = Event {
            type_: EventType::Warning,
            reason: "GitImageNotFound".into(),
            note: Some(format!(
                "waiting for GitImage {} in namespace {}",
                missing.join(", "),
                pod.namespace().unwrap_or_default()
            )),
            action: "Gate".into(),
            secondary: None,
        };
        let _ = ctx.recorder.publish(&ev, &pod.object_ref(&())).await;
    }
    Ok(Action::await_change())
}

fn error_policy(_pod: Arc<Pod>, err: &Error, ctx: Arc<Ctx>) -> Action {
    tracing::warn!(error = %err, "gate release failed");
    ctx.metrics.error("gates");
    Action::requeue(Duration::from_secs(10))
}

/// Posts `Building myapp@3f2c9e1: step 4/12 RUN npm ci` on every pod waiting for `name`.
pub async fn progress_events(ctx: &Ctx, ns: &str, name: &str, step: &str) {
    let store = ctx.gated_pods.lock().unwrap().clone();
    let Some(store) = store else { return };
    let rev = ctx
        .gitimages
        .get(&ObjectRef::new(name).within(ns))
        .and_then(|g| g.status.as_ref().and_then(|s| s.latest_revision.clone()))
        .unwrap_or_default();
    for pod in store.state() {
        if pod.namespace().as_deref() != Some(ns)
            || !bindings(&pod).iter().any(|b| b.gitimage == name)
        {
            continue;
        }
        let ev = Event {
            type_: EventType::Normal,
            reason: "Building".into(),
            note: Some(format!(
                "Building {name}@{}: {step}",
                &rev[..rev.len().min(7)]
            )),
            action: "Gate".into(),
            secondary: None,
        };
        let _ = ctx.recorder.publish(&ev, &pod.object_ref(&())).await;
    }
}

/// Runs the gate releaser until the stream ends.
pub async fn run(ctx: Arc<Ctx>) {
    let pods: Api<Pod> = Api::all(ctx.client.clone());
    let gitimages: Api<GitImage> = Api::all(ctx.client.clone());
    let cfg = watcher::Config::default().labels(&format!("{}=true", keys::GATED_LABEL));
    let controller = Controller::new(pods, cfg);
    let store = controller.store();
    *ctx.gated_pods.lock().unwrap() = Some(store.clone());
    controller
        .watches(
            gitimages,
            watcher::Config::default(),
            move |gi: GitImage| {
                let ns = gi.namespace().unwrap_or_default();
                let name = gi.name_any();
                store
                    .state()
                    .into_iter()
                    .filter(|p| p.namespace().as_deref() == Some(ns.as_str()))
                    .filter(|p| bindings(p).iter().any(|b| b.gitimage == name))
                    .map(|p| ObjectRef::from_obj(&*p))
                    .collect::<Vec<_>>()
            },
        )
        .shutdown_on_signal()
        .run(reconcile, error_policy, ctx)
        .for_each(|_| futures::future::ready(()))
        .await;
}
