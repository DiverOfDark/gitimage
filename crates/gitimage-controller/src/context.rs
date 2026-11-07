//! Shared state of the controller process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gitimage_api::{Config, GitImage};
use gitimage_git::{GixClient, Resolved};
use gitimage_oci::DynStorage;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::runtime::events::Recorder;
use kube::runtime::reflector::{ObjectRef, Store};
use tokio::sync::mpsc;

use crate::metrics::Metrics;

/// Build progress reported by a log-follower task.
#[derive(Clone, Debug, Default)]
pub struct JobProgress {
    pub step: Option<String>,
    pub error: Option<String>,
}

pub struct Ctx {
    pub client: Client,
    pub config: Config,
    /// Namespace the controller runs in (`gitimage-system`).
    pub namespace: String,
    /// Image of `gitimage` itself, for the clone init container.
    pub gitimage_image: String,
    pub git: GixClient,
    pub storage: DynStorage,
    /// HS256 key for push tokens.
    pub push_key: Vec<u8>,
    /// PEM of the controller CA, copied into tenant namespaces for BuildKit.
    pub ca_pem: String,
    pub recorder: Recorder,
    pub gitimages: Store<GitImage>,
    /// Gated pods, filled by the gate releaser; used for progress Events.
    pub gated_pods: Mutex<Option<Store<Pod>>>,
    /// `<ns>/<name>` -> last resolution, to keep ls-refs off the Job-event path.
    pub resolve_cache: Mutex<HashMap<String, (Resolved, Instant)>>,
    /// Build Job uid -> progress.
    pub progress: Mutex<HashMap<String, JobProgress>>,
    /// Build Job uids with a running log follower.
    pub followers: Mutex<std::collections::HashSet<String>>,
    /// Re-triggers a GitImage reconcile (progress updates).
    pub trigger: mpsc::UnboundedSender<ObjectRef<GitImage>>,
    pub metrics: Arc<Metrics>,
}

impl Ctx {
    /// `<pull_host>/<ns>/<name>@<digest>`
    pub fn image_ref(&self, ns: &str, name: &str, digest: &str) -> String {
        format!(
            "{}/{ns}/{name}@{digest}",
            self.config.pull_host().trim_end_matches('/')
        )
    }
}
