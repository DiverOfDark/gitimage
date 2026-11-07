//! The `GitImage` custom resource (`gitimage.dev/v1alpha1`, namespaced).

use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A git repository whose commits GitImage builds into container images.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[kube(
    group = "gitimage.dev",
    version = "v1alpha1",
    kind = "GitImage",
    plural = "gitimages",
    shortname = "gi",
    namespaced,
    status = "GitImageStatus",
    printcolumn = r#"{"name":"Revision","type":"string","jsonPath":".status.revision"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type=='Ready')].status"}"#,
    printcolumn = r#"{"name":"Status","type":"string","jsonPath":".status.conditions[?(@.type=='Building')].message"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitImageSpec {
    /// Repository URL, ssh (`git@host:org/repo.git`, `ssh://...`) or https.
    pub url: String,
    /// Branch, tag or full commit sha; `refs/heads/...` or `refs/tags/...` to disambiguate.
    #[serde(default = "default_ref", rename = "ref")]
    pub git_ref: String,
    /// Secret in the same namespace, Flux format: `identity` + `known_hosts` for ssh,
    /// `username` + `password` for https.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    #[serde(default)]
    pub build: BuildSpec,
    /// Poll interval, e.g. `5m`. A push webhook also triggers a check.
    #[serde(default = "default_interval")]
    #[schemars(regex(pattern = r"^([0-9]+(ms|s|m|h))+$"))]
    pub interval: String,
}

fn default_ref() -> String {
    "main".into()
}

fn default_interval() -> String {
    "5m".into()
}

impl GitImageSpec {
    /// Parsed `interval`; falls back to 5 minutes on a value the schema let through.
    pub fn interval(&self) -> std::time::Duration {
        humantime::parse_duration(&self.interval).unwrap_or(std::time::Duration::from_secs(300))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretRef {
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretKeyRef {
    pub name: String,
    /// Key inside the Secret; may be left out when the Secret has exactly one key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildSpec {
    /// Working directory relative to the repo root; also the path F7 watches for changes.
    #[serde(default = "default_context")]
    pub context: String,
    /// Dockerfile path relative to `context`.
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,
    /// Build args, each a literal value or a Secret key. They end up in image history:
    /// use `secrets` for real secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, BuildArg>,
    /// BuildKit `--secret` mounts; never written into a layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<BuildSecret>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// What a new commit does while a build runs.
    #[serde(default)]
    pub on_new_commit: OnNewCommit,
}

impl Default for BuildSpec {
    fn default() -> Self {
        Self {
            context: default_context(),
            dockerfile: default_dockerfile(),
            args: BTreeMap::new(),
            secrets: Vec::new(),
            resources: None,
            on_new_commit: OnNewCommit::default(),
        }
    }
}

fn default_context() -> String {
    ".".into()
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildArg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<BuildArgSource>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildArgSource {
    pub secret_key_ref: SecretKeyRef,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildSecret {
    /// The id a Dockerfile uses in `RUN --mount=type=secret,id=<id>`.
    pub id: String,
    pub secret_ref: SecretKeyRef,
}

/// Resource requests and limits of the build pod, as Kubernetes quantity strings.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requests: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub limits: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OnNewCommit {
    /// Let the running build finish, then build only the newest sha.
    #[default]
    Wait,
    /// Delete the running build and start the newest sha at once.
    Cancel,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitImageStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Pre-authorized push-hook URL to paste into the git host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    /// sha256 of the hook token; every replica's hooks receiver indexes on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_token_hash: Option<String>,
    /// Newest sha seen on the ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_revision: Option<String>,
    /// sha whose image is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// `<host>/<ns>/<name>@sha256:...`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Newest first, at most 5 (F6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builds: Vec<BuildRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// Last `gitimage.dev/rebuild` value acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled_rebuild: Option<String>,
    /// Last `gitimage.dev/hook-at` value acted on (debounce).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled_hook_at: Option<String>,
}

impl GitImageStatus {
    /// Digest built for `revision`, if any build record has one.
    pub fn digest_for(&self, revision: &str) -> Option<&str> {
        self.builds
            .iter()
            .find(|b| b.revision == revision)
            .and_then(|b| b.digest.as_deref())
    }

    /// The record for `revision`, if any.
    pub fn record(&self, revision: &str) -> Option<&BuildRecord> {
        self.builds.iter().find(|b| b.revision == revision)
    }

    /// The newest record that produced or reused an image.
    pub fn last_good(&self) -> Option<&BuildRecord> {
        self.builds.iter().find(|b| b.digest.is_some())
    }

    pub fn condition(&self, type_: &str) -> Option<&Condition> {
        self.conditions.iter().find(|c| c.type_ == type_)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildRecord {
    /// The commit.
    pub revision: String,
    pub outcome: BuildOutcome,
    /// `sha256:...`; absent for a superseded build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Git tree id of `<context>` at `revision` (F7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tree: Option<String>,
    /// F7: the older commit whose image this one reuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    /// Failure reason for `Failed` records, e.g. `BuildFailed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// RFC 3339.
    pub finished_at: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum BuildOutcome {
    Built,
    /// F7: the context tree did not change; `digest` is the reused one.
    Reused,
    /// Cancelled by a newer commit (`onNewCommit: cancel`).
    Superseded,
    /// The build failed; `reason` says why. Retried only by `gitimage.dev/rebuild`.
    Failed,
}

/// Same shape as `metav1.Condition`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    /// `True`, `False` or `Unknown`.
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// RFC 3339.
    pub last_transition_time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

pub mod condition_types {
    pub const READY: &str = "Ready";
    pub const BUILDING: &str = "Building";
    pub const FAILED: &str = "Failed";
}

pub mod failure_reasons {
    pub const CLONE_FAILED: &str = "CloneFailed";
    pub const BUILD_FAILED: &str = "BuildFailed";
    pub const PUSH_FAILED: &str = "PushFailed";
    pub const DEADLINE_EXCEEDED: &str = "DeadlineExceeded";
    pub const SECRET_MISSING: &str = "SecretMissing";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_a_minimal_spec() {
        let spec: GitImageSpec =
            serde_yaml::from_str("url: https://github.com/you/myapp.git").unwrap();
        assert_eq!(spec.git_ref, "main");
        assert_eq!(spec.interval(), std::time::Duration::from_secs(300));
        assert_eq!(spec.build.context, ".");
        assert_eq!(spec.build.dockerfile, "Dockerfile");
        assert_eq!(spec.build.on_new_commit, OnNewCommit::Wait);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_yaml::from_str::<GitImageSpec>("url: x\nrawArgs: [--output]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("rawArgs"), "{err}");
    }

    #[test]
    fn prd_example_parses() {
        let spec: GitImageSpec = serde_yaml::from_str(
            r#"
url: git@github.com:you/myapp.git
ref: main
secretRef: { name: myapp-git }
build:
  context: .
  dockerfile: Dockerfile
  args:
    NODE_ENV: { value: production }
    SENTRY_DSN: { valueFrom: { secretKeyRef: { name: sentry, key: dsn } } }
  secrets: [{ id: npmrc, secretRef: { name: npm-token } }]
interval: 5m
"#,
        )
        .unwrap();
        assert_eq!(spec.secret_ref.unwrap().name, "myapp-git");
        assert_eq!(spec.build.secrets[0].id, "npmrc");
        assert_eq!(spec.build.args.len(), 2);
    }

    #[test]
    fn digest_lookup_skips_superseded() {
        let status = GitImageStatus {
            builds: vec![
                BuildRecord {
                    revision: "b".into(),
                    outcome: BuildOutcome::Superseded,
                    digest: None,
                    context_tree: None,
                    source_revision: None,
                    reason: None,
                    finished_at: "2026-11-07T00:00:00Z".into(),
                },
                BuildRecord {
                    revision: "a".into(),
                    outcome: BuildOutcome::Built,
                    digest: Some("sha256:aa".into()),
                    context_tree: None,
                    source_revision: None,
                    reason: None,
                    finished_at: "2026-11-07T00:00:00Z".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(status.digest_for("a"), Some("sha256:aa"));
        assert_eq!(status.digest_for("b"), None);
    }
}
