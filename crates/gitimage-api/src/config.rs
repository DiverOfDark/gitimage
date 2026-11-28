//! Controller configuration: the `config.yaml` key of the `gitimage-config` ConfigMap,
//! rendered by Helm from `values.yaml`. Unknown keys fail parsing, so a typo crashes the
//! controller at startup instead of being ignored.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::gitimage::Resources;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config.yaml: {0}")]
    Parse(#[from] serde_saphyr::Error),
    #[error("config.yaml: {0}")]
    Invalid(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub build_policy: BuildPolicy,
    pub storage: Storage,
    pub push_endpoint: PushEndpoint,
    #[serde(default)]
    pub build: BuildDefaults,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub node: Node,
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub controller: ControllerSettings,
    #[serde(default)]
    pub ui: Ui,
}

/// The status UI served by the controller on its own port.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ui {
    #[serde(default)]
    pub enabled: bool,
    /// Public URL of the UI, e.g. `https://gitimage.example.com`. Required for OIDC (the
    /// redirect URI is `<baseURL>/auth/callback`).
    #[serde(default, rename = "baseURL", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: UiAuth,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiAuth {
    #[serde(default)]
    pub mode: UiAuthMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<UiOidc>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UiAuthMode {
    /// No login: every visitor sees what the controller's ServiceAccount sees. Only for
    /// `kubectl port-forward` or a proxy that authenticates in front.
    #[default]
    None,
    /// Login against an OIDC issuer the API server also trusts; every Kubernetes request the
    /// UI makes carries the user's ID token, so cluster RBAC decides what they see.
    Oidc,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiOidc {
    /// Issuer URL, exactly as in the API server's authentication configuration.
    pub issuer: String,
    /// Client id; must be one of the API server's accepted audiences. May instead come from
    /// the `GITIMAGE_UI_OIDC_CLIENT_ID` environment variable.
    #[serde(default, rename = "clientID", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
    /// ID token claim shown as the user name.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
}

fn default_oidc_scopes() -> Vec<String> {
    vec!["openid".into(), "email".into(), "profile".into()]
}

fn default_username_claim() -> String {
    "email".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hooks {
    /// Public base URL of the hooks Ingress, e.g. `https://gitimage.example.com`.
    /// `status.webhookURL` is `<baseURL>/hooks/<token>`; without it the URL is not shown.
    #[serde(default, rename = "baseURL", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerSettings {
    /// Image of `gitimage` itself, used for the `clone` init container. Set by the chart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The roller also writes the built image into the pod template, not only the revision
    /// annotation, so tools that read templates (Trivy Operator) see the real image.
    #[serde(default)]
    pub pin_template_image: bool,
}

impl Config {
    pub fn from_yaml(s: &str) -> Result<Self, ConfigError> {
        let cfg: Self = serde_saphyr::from_str(s)?;
        cfg.validate()?;
        cfg.check_extra_egress_rules(s)?;
        Ok(cfg)
    }

    /// The Kubernetes types ignore unknown fields. A misspelled `to` would leave a rule that
    /// allows every destination, so the rules must read back exactly as written.
    fn check_extra_egress_rules(&self, s: &str) -> Result<(), ConfigError> {
        let rules = &self.build_policy.egress.extra_rules;
        if rules.is_empty() {
            return Ok(());
        }
        if rules
            .iter()
            .any(|r| r.to.as_ref().is_none_or(Vec::is_empty))
        {
            return Err(ConfigError::Invalid(
                "buildPolicy.egress.extraRules: every rule needs `to`; for any destination say \
                 ipBlock: { cidr: 0.0.0.0/0 }"
                    .into(),
            ));
        }
        let raw: serde_json::Value = serde_saphyr::from_str(s)?;
        let written = raw.pointer("/buildPolicy/egress/extraRules");
        let parsed = serde_json::to_value(rules).ok();
        if written != parsed.as_ref() {
            return Err(ConfigError::Invalid(
                "buildPolicy.egress.extraRules has unknown fields: a rule takes `to` (ipBlock, \
                 namespaceSelector, podSelector) and `ports` (port, endPort, protocol)"
                    .into(),
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let iso = &self.build_policy.isolation;
        match iso.mode {
            IsolationMode::RuntimeClass if iso.runtime_class_name.is_none() => {
                return Err(ConfigError::Invalid(
                    "buildPolicy.isolation.runtimeClassName is required for mode runtimeClass"
                        .into(),
                ));
            }
            IsolationMode::Privileged if !iso.allow_privileged => {
                return Err(ConfigError::Invalid(
                    "buildPolicy.isolation.mode privileged needs allowPrivileged: true".into(),
                ));
            }
            mode if iso.privileged_in_sandbox && mode != IsolationMode::RuntimeClass => {
                return Err(ConfigError::Invalid(
                    "buildPolicy.isolation.privilegedInSandbox is only valid with mode runtimeClass"
                        .into(),
                ));
            }
            mode if iso.rootful_in_sandbox && mode != IsolationMode::RuntimeClass => {
                return Err(ConfigError::Invalid(
                    "buildPolicy.isolation.rootfulInSandbox is only valid with mode runtimeClass"
                        .into(),
                ));
            }
            _ => {}
        }
        if iso.privileged_in_sandbox && iso.rootful_in_sandbox {
            return Err(ConfigError::Invalid(
                "buildPolicy.isolation.privilegedInSandbox and rootfulInSandbox are mutually exclusive"
                    .into(),
            ));
        }
        let sv = &self.build.state_volume;
        if !matches!(sv.medium.as_str(), "" | "Memory") {
            return Err(ConfigError::Invalid(
                "build.stateVolume.medium must be \"\" or \"Memory\"".into(),
            ));
        }
        if (!sv.labels.is_empty() || !sv.annotations.is_empty()) && sv.claim().is_none() {
            return Err(ConfigError::Invalid(
                "build.stateVolume.labels and annotations apply to PVCs: set block or persistent"
                    .into(),
            ));
        }
        if let Some(p) = &sv.persistent {
            if !sv.medium.is_empty() || sv.size_limit.is_some() {
                return Err(ConfigError::Invalid(
                    "build.stateVolume.persistent excludes medium and sizeLimit".into(),
                ));
            }
            let Some(size) = p.size_bytes().filter(|b| *b >= 1 << 30) else {
                return Err(ConfigError::Invalid(
                    "build.stateVolume.persistent.size must be a quantity of at least 1Gi, e.g. 20Gi"
                        .into(),
                ));
            };
            // BuildKit never prunes below 10% of `size`: more free space than the rest is unreachable.
            if p.min_free_bytes()
                .is_none_or(|b| b == 0 || b >= size / 10 * 9)
            {
                return Err(ConfigError::Invalid(
                    "build.stateVolume.persistent.minFreeSpace must be a quantity (8Gi) or a percentage of size (30%), above 0 and below 90% of size"
                        .into(),
                ));
            }
            if let Some(b) = &sv.block
                && (!b.size.is_empty() || b.storage_class_name.is_some())
            {
                return Err(ConfigError::Invalid(
                    "with build.stateVolume.persistent, set size and storageClassName there, not in block"
                        .into(),
                ));
            }
        }
        if let Some(b) = &sv.block {
            if !sv.medium.is_empty() || sv.size_limit.is_some() {
                return Err(ConfigError::Invalid(
                    "build.stateVolume.block excludes medium and sizeLimit".into(),
                ));
            }
            if !matches!(b.fs_type.as_str(), "ext3" | "ext4") {
                return Err(ConfigError::Invalid(
                    "build.stateVolume.block.fsType must be ext3 or ext4".into(),
                ));
            }
            if b.size.is_empty() && sv.persistent.is_none() {
                return Err(ConfigError::Invalid(
                    "build.stateVolume.block.size is required".into(),
                ));
            }
            // Formatting and mounting a block device needs a privileged BuildKit container.
            let privileged = iso.mode == IsolationMode::Privileged
                || (iso.mode == IsolationMode::RuntimeClass && iso.privileged_in_sandbox);
            if !privileged {
                return Err(ConfigError::Invalid(
                    "build.stateVolume.block needs isolation mode privileged or runtimeClass with privilegedInSandbox".into(),
                ));
            }
        }
        if self.ui.auth.mode == UiAuthMode::Oidc {
            if self.ui.auth.oidc.is_none() {
                return Err(ConfigError::Invalid(
                    "ui.auth.oidc is required for ui.auth.mode oidc".into(),
                ));
            }
            if self.ui.base_url.as_deref().is_none_or(str::is_empty) {
                return Err(ConfigError::Invalid(
                    "ui.baseURL is required for ui.auth.mode oidc".into(),
                ));
            }
        }
        match self.storage.kind {
            StorageType::Registry if self.storage.registry.is_none() => Err(ConfigError::Invalid(
                "storage.registry is required for storage.type registry".into(),
            )),
            StorageType::S3 if self.storage.s3.is_none() => Err(ConfigError::Invalid(
                "storage.s3 is required for storage.type s3".into(),
            )),
            _ => Ok(()),
        }
    }

    /// Host every image ref written into a pod starts with.
    pub fn pull_host(&self) -> String {
        match self.storage.kind {
            StorageType::S3 => format!("localhost:{}", self.node.port),
            StorageType::Registry => self
                .storage
                .registry
                .as_ref()
                .map(|r| r.url.trim_end_matches('/').to_string())
                .unwrap_or_default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildPolicy {
    pub isolation: Isolation,
    #[serde(default)]
    pub egress: Egress,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Isolation {
    /// Required, no default (PRD N1).
    pub mode: IsolationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_class_name: Option<String>,
    #[serde(default)]
    pub allow_privileged: bool,
    /// runtimeClass mode only: run rootful BuildKit as a privileged container *inside* the
    /// sandbox. Needed for Kata, whose shared filesystem drops the file capabilities rootless
    /// BuildKit's newuidmap relies on. Safe only when the runtime keeps privileged inside the
    /// guest (containerd `privileged_without_host_devices = true`, the kata-deploy default).
    #[serde(default)]
    pub privileged_in_sandbox: bool,
    /// runtimeClass mode only: run rootful BuildKit as an *unprivileged* container in a pod
    /// user namespace (`hostUsers: false`). For runtimes that virtualize procfs/sysfs and give
    /// root in the pod its capabilities inside the user namespace, such as Sysbox
    /// (`sysbox-runc`). Build pods then fit the Pod Security `baseline` level.
    #[serde(default)]
    pub rootful_in_sandbox: bool,
    /// BuildKit's snapshotter. gVisor needs `native`: it cannot copy up a lower-layer file that
    /// a process has mapped when the layers live on a volume (`ENODEV`), so e.g. `apt-get`
    /// upgrading a library it has loaded fails with "unable to make backup link ... No such
    /// device".
    #[serde(default)]
    pub snapshotter: Snapshotter,
}

/// `--oci-worker-snapshotter` of the build's BuildKit daemon.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Snapshotter {
    /// BuildKit's own choice (overlayfs where the kernel allows it).
    #[default]
    Auto,
    Overlayfs,
    /// Copies each layer: slower and bigger, but needs nothing from the filesystem.
    Native,
}

impl Snapshotter {
    /// The `buildkitd` flag, if one is needed.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Overlayfs => Some("--oci-worker-snapshotter=overlayfs"),
            Self::Native => Some("--oci-worker-snapshotter=native"),
        }
    }
}

impl Isolation {
    /// Whether builds run rootful BuildKit (privileged mode, or rootful inside a sandbox).
    pub fn rootful(&self) -> bool {
        self.mode == IsolationMode::Privileged
            || self.privileged_in_sandbox
            || self.rootful_in_sandbox
    }

    /// The lowest Pod Security level build pods pass in their namespace.
    pub fn pod_security_level(&self) -> &'static str {
        if self.rootful_in_sandbox {
            "baseline"
        } else {
            "privileged"
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IsolationMode {
    RuntimeClass,
    Userns,
    Privileged,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Egress {
    #[serde(default = "yes")]
    pub allow_internet: bool,
    #[serde(default = "yes", rename = "denyClusterCIDRs")]
    pub deny_cluster_cidrs: bool,
    /// NetworkPolicy egress rules added to the build policy, e.g. an in-cluster registry
    /// mirror behind an ingress controller: a LoadBalancer IP is translated to pod IPs, which
    /// `denyClusterCIDRs` blocks. Each rule needs `to`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_rules: Vec<k8s_openapi::api::networking::v1::NetworkPolicyEgressRule>,
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            allow_internet: true,
            deny_cluster_cidrs: true,
            extra_rules: Vec::new(),
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Storage {
    #[serde(rename = "type")]
    pub kind: StorageType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryStorage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Storage>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StorageType {
    Registry,
    S3,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistryStorage {
    /// Images land at `<url>/<namespace>/<gitimage>:<sha>`.
    pub url: String,
    /// Secret in gitimage-system with push credentials (dockerconfigjson).
    pub push_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_secret: Option<String>,
    /// Plain HTTP to the upstream registry (tests only).
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct S3Storage {
    pub bucket: String,
    pub endpoint: String,
    #[serde(default = "default_region")]
    pub region: String,
    /// Secret in gitimage-system with `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.
    pub secret: String,
    /// Read-only credentials for gitimage-node; defaults to `secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_secret: Option<String>,
    #[serde(default)]
    pub path_style: bool,
}

fn default_region() -> String {
    "us-east-1".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PushEndpoint {
    /// In-cluster DNS name of the push Service, e.g. `gitimage-push.gitimage-system.svc`.
    pub host: String,
    #[serde(default = "default_push_port")]
    pub port: u16,
}

fn default_push_port() -> u16 {
    5000
}

impl PushEndpoint {
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildDefaults {
    /// Rootless BuildKit image (userns, and runtimeClass without privileged/rootfulInSandbox).
    #[serde(default = "default_buildkit_image")]
    pub image: String,
    /// Rootful BuildKit image (privileged, and runtimeClass with privileged/rootfulInSandbox).
    #[serde(default = "default_buildkit_rootful_image")]
    pub rootful_image: String,
    #[serde(default)]
    pub default_resources: Resources,
    #[serde(default = "default_deadline")]
    pub active_deadline: String,
    #[serde(default = "default_platforms")]
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub cache: CacheMode,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_opts: BTreeMap<String, String>,
    /// Extra `buildkitd.toml` appended to the generated one: registry mirrors, private or
    /// plain-HTTP registries (`[registry."docker.io"] mirrors = ["mirror.example.com"]`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub buildkitd_config: String,
    /// Volume for BuildKit's state (snapshots, cache).
    #[serde(default)]
    pub state_volume: StateVolume,
}

/// Where BuildKit keeps its state during a build.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateVolume {
    /// `""` (node disk) or `Memory` (tmpfs). Use `Memory` on Kata nodes whose virtiofs shares
    /// do not support extended attributes (e.g. Talos): BuildKit then fails with
    /// "failed to create hash ...: operation not supported". Memory counts against the build
    /// pod's memory limit, so size `build.defaultResources` accordingly.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub medium: String,
    /// emptyDir sizeLimit, e.g. `6Gi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<String>,
    /// A raw block (`volumeMode: Block`) ephemeral PVC instead of an emptyDir, formatted per
    /// build and mounted by BuildKit itself. Kata hands block volumes to the VM as a virtio-blk
    /// disk, so the state lives on a real ext3/ext4 filesystem rather than a virtiofs share.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockStateVolume>,
    /// One PVC per GitImage (`<name>-buildkit`), kept between its builds, so BuildKit's local
    /// cache and `RUN --mount=type=cache` survive. With `block`, the PVC is `volumeMode: Block`
    /// and formatted only when it holds no filesystem yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent: Option<PersistentStateVolume>,
    /// Extra labels on the state PVCs (`block` or `persistent`), e.g.
    /// `velero.io/exclude-from-backup: "true"`: they hold only a rebuildable cache.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Extra annotations on the state PVCs, e.g. `k8up.io/backup: "false"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersistentStateVolume {
    /// Requested size, e.g. `20Gi`. BuildKit keeps its state within 80% of it.
    pub size: String,
    /// StorageClass of the PVCs; the default class if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Free space each build starts with at least: before building, BuildKit prunes its least
    /// recently used cache until this much of the volume is free, or 1.25 times the most any
    /// of the last 5 builds grew it by, if that is more. A quantity (`8Gi`) or a percentage of
    /// `size` (`30%`), below 90%; default 25%.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_space: Option<String>,
}

impl PersistentStateVolume {
    /// Default [`Self::min_free_space`], in percent of `size`.
    pub const DEFAULT_MIN_FREE_PERCENT: u64 = 25;

    /// `size` in bytes, if it parses.
    pub fn size_bytes(&self) -> Option<u64> {
        quantity_bytes(&self.size)
    }

    /// [`Self::min_free_space`] in bytes, if `size` and it parse.
    pub fn min_free_bytes(&self) -> Option<u64> {
        let size = self.size_bytes()?;
        match self.min_free_space.as_deref().map(str::trim) {
            None => Some(size / 100 * Self::DEFAULT_MIN_FREE_PERCENT),
            Some(q) => match q.strip_suffix('%') {
                Some(pct) => {
                    let pct: f64 = pct.trim().parse().ok()?;
                    (pct.is_finite() && pct >= 0.0).then(|| (size as f64 * pct / 100.0) as u64)
                }
                None => quantity_bytes(q),
            },
        }
    }
}

impl StateVolume {
    /// Size and StorageClass of the PVC backing BuildKit's state, if it is one.
    pub fn claim(&self) -> Option<(&str, Option<&str>)> {
        match (&self.persistent, &self.block) {
            (Some(p), _) => Some((&p.size, p.storage_class_name.as_deref())),
            (None, Some(b)) => Some((&b.size, b.storage_class_name.as_deref())),
            (None, None) => None,
        }
    }
}

/// Bytes in a Kubernetes quantity such as `20Gi`, `500M` or `1.5Ti`; `None` if unparsable.
pub fn quantity_bytes(q: &str) -> Option<u64> {
    let q = q.trim();
    let split = q
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(q.len());
    let (num, suffix) = q.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match suffix {
        "" => 1.0,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        _ => return None,
    };
    let bytes = num * mult;
    (bytes.is_finite() && bytes >= 0.0).then_some(bytes as u64)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BlockStateVolume {
    /// StorageClass that supports `volumeMode: Block` (e.g. Ceph RBD); default class if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Requested size, e.g. `20Gi`. Required unless `persistent` is set, which sizes the PVC.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub size: String,
    /// `ext4` (default) or `ext3`.
    #[serde(default = "default_fs_type")]
    pub fs_type: String,
    /// Image providing `mkfs.<fsType>` (e2fsprogs) for the format init container.
    #[serde(default = "default_format_image")]
    pub format_image: String,
}

fn default_fs_type() -> String {
    "ext4".into()
}

fn default_format_image() -> String {
    "debian:bookworm-slim".into()
}

impl Default for BuildDefaults {
    fn default() -> Self {
        Self {
            image: default_buildkit_image(),
            rootful_image: default_buildkit_rootful_image(),
            default_resources: Resources::default(),
            active_deadline: default_deadline(),
            platforms: default_platforms(),
            labels: BTreeMap::new(),
            cache: CacheMode::default(),
            extra_opts: BTreeMap::new(),
            buildkitd_config: String::new(),
            state_volume: StateVolume::default(),
        }
    }
}

impl BuildDefaults {
    pub fn active_deadline_seconds(&self) -> i64 {
        humantime::parse_duration(&self.active_deadline)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(3600)
    }
}

fn default_buildkit_image() -> String {
    "moby/buildkit:v0.34.2-rootless".into()
}

fn default_buildkit_rootful_image() -> String {
    "moby/buildkit:v0.34.2".into()
}

fn default_deadline() -> String {
    "1h".into()
}

fn default_platforms() -> Vec<String> {
    vec!["linux/amd64".into()]
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CacheMode {
    #[default]
    Max,
    Min,
    Off,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Network {
    #[serde(default, rename = "clusterCIDRs")]
    pub cluster_cidrs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Node {
    #[serde(default = "default_node_port")]
    pub port: u16,
    #[serde(default = "default_node_health_port")]
    pub health_port: u16,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            port: default_node_port(),
            health_port: default_node_health_port(),
        }
    }
}

fn default_node_port() -> u16 {
    25050
}

fn default_node_health_port() -> u16 {
    25051
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOMELAB: &str = r#"
buildPolicy:
  isolation: { mode: runtimeClass, runtimeClassName: kata }
storage:
  type: s3
  s3: { bucket: gitimage, endpoint: http://rook-ceph-rgw.rook-ceph.svc, secret: s3-creds, pathStyle: true }
pushEndpoint: { host: gitimage-push.gitimage-system.svc }
network: { clusterCIDRs: [10.244.0.0/16, 10.96.0.0/12] }
"#;

    #[test]
    fn homelab_config_parses_with_defaults() {
        let cfg = Config::from_yaml(HOMELAB).unwrap();
        assert_eq!(cfg.pull_host(), "localhost:25050");
        assert_eq!(
            cfg.push_endpoint.authority(),
            "gitimage-push.gitimage-system.svc:5000"
        );
        assert_eq!(cfg.build.active_deadline_seconds(), 3600);
        assert_eq!(cfg.build.platforms, vec!["linux/amd64"]);
        assert!(cfg.build_policy.egress.deny_cluster_cidrs);
    }

    #[test]
    fn snapshotter_defaults_to_auto_and_rejects_unknown_values() {
        assert_eq!(
            Config::from_yaml(HOMELAB)
                .unwrap()
                .build_policy
                .isolation
                .snapshotter,
            Snapshotter::Auto
        );
        let native = HOMELAB.replace(
            "runtimeClassName: kata }",
            "runtimeClassName: gvisor, snapshotter: native }",
        );
        assert_eq!(
            Config::from_yaml(&native)
                .unwrap()
                .build_policy
                .isolation
                .snapshotter,
            Snapshotter::Native
        );
        let bad = HOMELAB.replace(
            "runtimeClassName: kata }",
            "runtimeClassName: gvisor, snapshotter: zfs }",
        );
        assert!(Config::from_yaml(&bad).is_err());
    }

    #[test]
    fn typo_fails() {
        let bad = HOMELAB.replace("pushEndpoint", "pushEndpiont");
        assert!(Config::from_yaml(&bad).is_err());
    }

    #[test]
    fn isolation_mode_is_required() {
        let bad = HOMELAB.replace("mode: runtimeClass, ", "");
        assert!(Config::from_yaml(&bad).is_err());
    }

    #[test]
    fn runtime_class_needs_a_name() {
        let bad = HOMELAB.replace(", runtimeClassName: kata", "");
        let err = Config::from_yaml(&bad).unwrap_err().to_string();
        assert!(err.contains("runtimeClassName"), "{err}");
    }

    #[test]
    fn privileged_in_sandbox_needs_runtime_class() {
        let ok = HOMELAB.replace(
            "runtimeClassName: kata",
            "runtimeClassName: kata, privilegedInSandbox: true",
        );
        assert!(
            Config::from_yaml(&ok)
                .unwrap()
                .build_policy
                .isolation
                .rootful()
        );
        let bad = HOMELAB.replace(
            "mode: runtimeClass, runtimeClassName: kata",
            "mode: userns, privilegedInSandbox: true",
        );
        assert!(Config::from_yaml(&bad).is_err());
    }

    #[test]
    fn rootful_in_sandbox_needs_runtime_class_and_excludes_privileged_in_sandbox() {
        let ok = HOMELAB.replace(
            "runtimeClassName: kata",
            "runtimeClassName: sysbox-runc, rootfulInSandbox: true",
        );
        let iso = Config::from_yaml(&ok).unwrap().build_policy.isolation;
        assert!(iso.rootful());
        assert_eq!(iso.pod_security_level(), "baseline");
        let bad = HOMELAB.replace(
            "mode: runtimeClass, runtimeClassName: kata",
            "mode: userns, rootfulInSandbox: true",
        );
        let err = Config::from_yaml(&bad).unwrap_err().to_string();
        assert!(err.contains("rootfulInSandbox"), "{err}");
        let bad = HOMELAB.replace(
            "mode: runtimeClass, runtimeClassName: kata",
            "mode: privileged, allowPrivileged: true, rootfulInSandbox: true",
        );
        assert!(Config::from_yaml(&bad).is_err());
        let both = HOMELAB.replace(
            "runtimeClassName: kata",
            "runtimeClassName: kata, privilegedInSandbox: true, rootfulInSandbox: true",
        );
        let err = Config::from_yaml(&both).unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn privileged_needs_opt_in() {
        let bad = HOMELAB.replace(
            "mode: runtimeClass, runtimeClassName: kata",
            "mode: privileged",
        );
        assert!(Config::from_yaml(&bad).is_err());
    }

    #[test]
    fn block_state_volume_needs_privileged_buildkit() {
        let kata = HOMELAB.replace(
            "runtimeClassName: kata }",
            "runtimeClassName: kata, privilegedInSandbox: true }",
        );
        let ok = format!(
            "{kata}build: {{ stateVolume: {{ block: {{ storageClassName: ceph-block, size: 20Gi }} }} }}\n"
        );
        let b = Config::from_yaml(&ok)
            .unwrap()
            .build
            .state_volume
            .block
            .unwrap();
        assert_eq!(b.fs_type, "ext4");
        assert_eq!(b.format_image, "debian:bookworm-slim");
        let ext2 = ok.replace("size: 20Gi", "size: 20Gi, fsType: ext2");
        assert!(Config::from_yaml(&ext2).is_err());
        let mixed = ok.replace("{ block:", "{ medium: Memory, block:");
        assert!(Config::from_yaml(&mixed).is_err());
        let unprivileged = ok.replace(", privilegedInSandbox: true", "");
        let err = Config::from_yaml(&unprivileged).unwrap_err().to_string();
        assert!(err.contains("privilegedInSandbox"), "{err}");
    }

    #[test]
    fn persistent_state_volume() {
        let ok = format!(
            "{HOMELAB}build: {{ stateVolume: {{ persistent: {{ size: 20Gi, storageClassName: local-path }} }} }}\n"
        );
        let sv = Config::from_yaml(&ok).unwrap().build.state_volume;
        assert_eq!(sv.claim(), Some(("20Gi", Some("local-path"))));
        for bad in ["512Mi", "lots", "\"\""] {
            let cfg = ok.replace("20Gi", bad);
            assert!(Config::from_yaml(&cfg).is_err(), "{bad}");
        }
        let memory = ok.replace("{ persistent:", "{ medium: Memory, persistent:");
        assert!(Config::from_yaml(&memory).is_err());

        let p = Config::from_yaml(&ok)
            .unwrap()
            .build
            .state_volume
            .persistent
            .unwrap();
        assert_eq!(p.min_free_bytes(), Some(20 * (1 << 30) / 100 * 25));
        let free = |v: &str| ok.replace("size: 20Gi", &format!("size: 20Gi, minFreeSpace: {v}"));
        for (v, bytes) in [
            ("\"30%\"", 6_442_450_944),
            ("8Gi", 8 << 30),
            ("\"12.5%\"", 2_684_354_560),
        ] {
            let p = Config::from_yaml(&free(v))
                .unwrap()
                .build
                .state_volume
                .persistent
                .unwrap();
            assert_eq!(p.min_free_bytes(), Some(bytes), "{v}");
        }
        for bad in ["\"0%\"", "\"90%\"", "18Gi", "\"-5%\"", "lots", "\"x%\""] {
            let err = Config::from_yaml(&free(bad)).unwrap_err().to_string();
            assert!(err.contains("minFreeSpace"), "{bad}: {err}");
        }

        let kata = HOMELAB.replace(
            "runtimeClassName: kata }",
            "runtimeClassName: kata, privilegedInSandbox: true }",
        );
        let block = format!(
            "{kata}build: {{ stateVolume: {{ persistent: {{ size: 20Gi }}, block: {{ fsType: ext4 }} }} }}\n"
        );
        let sv = Config::from_yaml(&block).unwrap().build.state_volume;
        assert_eq!(sv.claim(), Some(("20Gi", None)));
        let sized_twice = block.replace("fsType: ext4", "size: 10Gi");
        let err = Config::from_yaml(&sized_twice).unwrap_err().to_string();
        assert!(err.contains("not in block"), "{err}");
    }

    #[test]
    fn state_volume_labels_need_a_pvc() {
        let labels = "labels: { velero.io/exclude-from-backup: \"true\" }";
        let ok = format!(
            "{HOMELAB}build: {{ stateVolume: {{ persistent: {{ size: 20Gi }}, {labels} }} }}\n"
        );
        let sv = Config::from_yaml(&ok).unwrap().build.state_volume;
        assert_eq!(sv.labels["velero.io/exclude-from-backup"], "true");
        let empty_dir = format!("{HOMELAB}build: {{ stateVolume: {{ {labels} }} }}\n");
        let err = Config::from_yaml(&empty_dir).unwrap_err().to_string();
        assert!(err.contains("apply to PVCs"), "{err}");
    }

    #[test]
    fn extra_egress_rules_are_strict() {
        let traefik = "{ namespaceSelector: { matchLabels: { kubernetes.io/metadata.name: traefik } }, podSelector: { matchLabels: { app.kubernetes.io/name: traefik } } }";
        let with = |rule: &str| {
            HOMELAB.replace(
                "runtimeClassName: kata }",
                &format!("runtimeClassName: kata }}\n  egress: {{ extraRules: [{rule}] }}"),
            )
        };
        let ok = with(&format!(
            "{{ to: [{traefik}], ports: [{{ port: 8443, protocol: TCP }}] }}"
        ));
        let rules = Config::from_yaml(&ok)
            .unwrap()
            .build_policy
            .egress
            .extra_rules;
        assert_eq!(rules.len(), 1);
        assert!(
            rules[0].to.as_ref().unwrap()[0]
                .namespace_selector
                .is_some()
        );

        // `too` would otherwise be dropped, leaving port 8443 open to everything.
        let typo = ok.replace("{ to:", "{ too:");
        assert!(Config::from_yaml(&typo).is_err());
        let typo = ok.replace("podSelector", "podSelecter");
        let err = Config::from_yaml(&typo).unwrap_err().to_string();
        assert!(err.contains("unknown fields"), "{err}");
        let no_to = with("{ ports: [{ port: 8443 }] }");
        let err = Config::from_yaml(&no_to).unwrap_err().to_string();
        assert!(err.contains("needs `to`"), "{err}");
    }

    #[test]
    fn quantities() {
        assert_eq!(quantity_bytes("20Gi"), Some(20 << 30));
        assert_eq!(quantity_bytes("1.5Ti"), Some(3 << 39));
        assert_eq!(quantity_bytes("500M"), Some(500_000_000));
        assert_eq!(quantity_bytes("1024"), Some(1024));
        assert_eq!(quantity_bytes("20GB"), None);
        assert_eq!(quantity_bytes(""), None);
    }
}
