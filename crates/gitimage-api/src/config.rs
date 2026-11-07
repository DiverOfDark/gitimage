//! Controller configuration: the `config.yaml` key of the `gitimage-config` ConfigMap,
//! rendered by Helm from `values.yaml`. Unknown keys fail parsing, so a typo crashes the
//! controller at startup instead of being ignored.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::gitimage::Resources;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config.yaml: {0}")]
    Parse(#[from] serde_yaml::Error),
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
}

impl Config {
    pub fn from_yaml(s: &str) -> Result<Self, ConfigError> {
        let cfg: Self = serde_yaml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
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
            _ => {}
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
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            allow_internet: true,
            deny_cluster_cidrs: true,
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
    #[serde(default = "default_buildkit_image")]
    pub image: String,
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
}

impl Default for BuildDefaults {
    fn default() -> Self {
        Self {
            image: default_buildkit_image(),
            default_resources: Resources::default(),
            active_deadline: default_deadline(),
            platforms: default_platforms(),
            labels: BTreeMap::new(),
            cache: CacheMode::default(),
            extra_opts: BTreeMap::new(),
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
    fn privileged_needs_opt_in() {
        let bad = HOMELAB.replace(
            "mode: runtimeClass, runtimeClassName: kata",
            "mode: privileged",
        );
        assert!(Config::from_yaml(&bad).is_err());
    }
}
