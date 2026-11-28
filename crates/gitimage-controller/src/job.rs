//! Pure builders for the per-build objects: the Job, its push-auth Secret, and the
//! per-namespace ConfigMap and NetworkPolicy. No I/O; snapshot-tested.

use std::collections::BTreeMap;

use gitimage_api::config::{CacheMode, Config, IsolationMode};
use gitimage_api::{GitImage, Resources, keys};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    AppArmorProfile, Capabilities, ConfigMap, ConfigMapVolumeSource, Container,
    EmptyDirVolumeSource, EnvVar, EnvVarSource, EphemeralVolumeSource, KeyToPath,
    PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimTemplate,
    PersistentVolumeClaimVolumeSource, PodSecurityContext, PodSpec, PodTemplateSpec,
    ResourceRequirements, SeccompProfile, Secret, SecretKeySelector, SecretVolumeSource,
    SecurityContext, Volume, VolumeDevice, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::api::networking::v1::{
    IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyPeer, NetworkPolicyPort,
    NetworkPolicySpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Resource;

/// Name of the per-namespace ConfigMap with the controller CA and `buildkitd.toml`.
pub const BUILD_CONFIGMAP: &str = "gitimage-build";
/// Name of the per-namespace build egress NetworkPolicy.
pub const EGRESS_POLICY: &str = "gitimage-build-egress";

const WORKSPACE: &str = "/workspace";

/// Where the BuildKit image keeps its state and reads its config.
pub struct BuildkitLayout {
    pub image: String,
    pub state_dir: &'static str,
    pub docker_config_dir: &'static str,
    pub buildkitd_toml: &'static str,
}

impl BuildkitLayout {
    pub fn for_config(cfg: &Config) -> Self {
        if cfg.build_policy.isolation.rootful() {
            Self {
                image: cfg.build.rootful_image.clone(),
                state_dir: "/var/lib/buildkit",
                docker_config_dir: "/root/.docker",
                buildkitd_toml: "/etc/buildkit/buildkitd.toml",
            }
        } else {
            Self {
                image: cfg.build.image.clone(),
                state_dir: "/home/user/.local/share/buildkit",
                docker_config_dir: "/home/user/.docker",
                buildkitd_toml: "/home/user/.config/buildkit/buildkitd.toml",
            }
        }
    }
}
const GIT_CREDS_DIR: &str = "/etc/gitimage/git";
const SECRETS_DIR: &str = "/run/secrets";
/// Written by the clone init container (`--meta-dir`), read by the BuildKit container.
const META_DIR: &str = "/etc/gitimage/meta";

/// Everything a build Job needs beyond the GitImage and the config.
#[derive(Clone, Debug)]
pub struct BuildParams {
    pub sha: String,
    pub attempt: u32,
    /// F7 fallback: the clone container exits 3 when the context tree equals this.
    pub skip_if_tree: Option<String>,
    /// Secret holding `config.json` with the push token.
    pub push_auth_secret: String,
    /// Image of `gitimage` itself, for the clone init container.
    pub gitimage_image: String,
    /// Build secrets with their key resolved: (id, secret name, key).
    pub secrets: Vec<(String, String, String)>,
    /// The GitImage's `gitimage.dev/rebuild` value, empty without one: build arg
    /// `GITIMAGE_REBUILD`.
    pub rebuild: String,
    /// Empty the persistent state volume first: the retry of a build that ran out of space.
    pub fresh_state: bool,
}

/// `<name>-<sha7>[-<attempt>]`, kept within 63 characters (Job names become label values).
pub fn job_name(gitimage: &str, sha: &str, attempt: u32) -> String {
    let short = &sha[..sha.len().min(7)];
    let suffix = if attempt == 0 {
        format!("-{short}")
    } else {
        format!("-{short}-{attempt}")
    };
    let max = 63 - suffix.len();
    let base = if gitimage.len() > max {
        gitimage[..max].trim_end_matches('-')
    } else {
        gitimage
    };
    format!("{base}{suffix}")
}

/// Push-auth Secret name for a Job.
pub fn push_auth_secret_name(job: &str) -> String {
    format!("{job}-push")
}

pub fn owner_ref(gi: &GitImage) -> OwnerReference {
    gi.controller_owner_ref(&())
        .expect("GitImage from the API server has a uid")
}

fn labels(gi: &GitImage, sha: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (keys::ROLE_LABEL.into(), keys::ROLE_BUILD.into()),
        (
            keys::NAME_LABEL.into(),
            gi.metadata.name.clone().unwrap_or_default(),
        ),
        (keys::REVISION_LABEL.into(), sha.into()),
        ("app.kubernetes.io/managed-by".into(), "gitimage".into()),
    ])
}

/// URL without user info, for the `org.opencontainers.image.source` label.
pub fn public_source_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
        format!("{scheme}://{rest}")
    } else if let Some((_, rest)) = url.split_once('@') {
        // scp-like git@host:org/repo.git
        format!("ssh://{}", rest.replacen(':', "/", 1))
    } else {
        url.to_string()
    }
}

/// Escapes `$(` so Kubernetes does not expand it in container args.
fn k8s_literal(s: &str) -> String {
    s.replace("$(", "$$(")
}

fn env_name_for_arg(arg: &str) -> String {
    let cleaned: String = arg
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("GITIMAGE_ARG_{cleaned}")
}

/// The `buildctl` arguments. Built from typed settings only, never from tenant free text.
pub fn buildctl_args(gi: &GitImage, cfg: &Config, p: &BuildParams) -> (Vec<String>, Vec<EnvVar>) {
    let ns = gi.metadata.namespace.clone().unwrap_or_default();
    let name = gi.metadata.name.clone().unwrap_or_default();
    let push = cfg.push_endpoint.authority();
    let ctx_dir = context_dir(&gi.spec.build.context);
    let mut args = vec![
        "build".to_string(),
        "--frontend=dockerfile.v0".into(),
        format!("--local=context={ctx_dir}"),
        format!("--local=dockerfile={ctx_dir}"),
        format!("--opt=filename={}", k8s_literal(&gi.spec.build.dockerfile)),
        format!(
            "--output=type=image,name={push}/{ns}/{name}:{},push=true,oci-mediatypes=true",
            p.sha
        ),
        "--progress=rawjson".into(),
    ];
    if !cfg.build.platforms.is_empty() {
        args.push(format!("--opt=platform={}", cfg.build.platforms.join(",")));
    }
    let cache_ref = format!("{push}/{ns}/{name}:buildcache");
    match cfg.build.cache {
        CacheMode::Off => {}
        mode => {
            let m = if mode == CacheMode::Max { "max" } else { "min" };
            args.push(format!("--import-cache=type=registry,ref={cache_ref}"));
            args.push(format!(
                "--export-cache=type=registry,ref={cache_ref},mode={m},oci-mediatypes=true,image-manifest=true"
            ));
        }
    }
    args.push(format!(
        "--opt=label:org.opencontainers.image.revision={}",
        p.sha
    ));
    args.push(format!(
        "--opt=label:org.opencontainers.image.source={}",
        k8s_literal(&public_source_url(&gi.spec.url))
    ));
    for (k, v) in &cfg.build.labels {
        args.push(format!("--opt=label:{}={}", k8s_literal(k), k8s_literal(v)));
    }
    for (k, v) in &cfg.build.extra_opts {
        args.push(format!("--opt={}={}", k8s_literal(k), k8s_literal(v)));
    }
    let mut env = Vec::new();
    for (k, arg) in &gi.spec.build.args {
        if let Some(v) = &arg.value {
            args.push(format!(
                "--opt=build-arg:{}={}",
                k8s_literal(k),
                k8s_literal(v)
            ));
        } else if let Some(src) = &arg.value_from {
            let var = env_name_for_arg(k);
            args.push(format!("--opt=build-arg:{}=$({var})", k8s_literal(k)));
            env.push(EnvVar {
                name: var,
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: src.secret_key_ref.name.clone(),
                        key: src.secret_key_ref.key.clone().unwrap_or_default(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
    }
    // After the tenant's args, so they cannot be overridden. GITIMAGE_COMMIT_TIME is only
    // known after the clone: `commit_time_wrapper` appends it at run time.
    args.push(format!("--opt=build-arg:{}={}", keys::ARG_REVISION, p.sha));
    args.push(format!(
        "--opt=build-arg:{}={}",
        keys::ARG_REBUILD,
        k8s_literal(&p.rebuild)
    ));
    for (id, _, _) in &p.secrets {
        args.push(format!("--secret=id={id},src={SECRETS_DIR}/{id}"));
    }
    (args, env)
}

/// Wraps BuildKit's `command` so the build also gets `GITIMAGE_COMMIT_TIME`, read from the
/// file the clone init container wrote. No `$(` in the script: Kubernetes would try to expand it.
fn commit_time_wrapper(command: Vec<String>) -> Vec<String> {
    let mut wrapped = vec![
        "sh".into(),
        "-c".into(),
        format!(
            "{{ read -r t || [ -n \"$t\" ]; }} < {META_DIR}/{} && exec \"$0\" \"$@\" \"--opt=build-arg:{}=$t\"",
            keys::META_COMMIT_TIME,
            keys::ARG_COMMIT_TIME
        ),
    ];
    wrapped.extend(command);
    wrapped
}

fn context_dir(context: &str) -> String {
    let c = context.trim_matches('/');
    if c.is_empty() || c == "." {
        WORKSPACE.to_string()
    } else {
        format!("{WORKSPACE}/{c}")
    }
}

fn quantities(m: &BTreeMap<String, String>) -> Option<BTreeMap<String, Quantity>> {
    (!m.is_empty()).then(|| {
        m.iter()
            .map(|(k, v)| (k.clone(), Quantity(v.clone())))
            .collect()
    })
}

fn resources(r: &Resources) -> Option<ResourceRequirements> {
    if r.requests.is_empty() && r.limits.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        requests: quantities(&r.requests),
        limits: quantities(&r.limits),
        ..Default::default()
    })
}

const STATE_VOLUME: &str = "buildkit-state";
const STATE_DEVICE: &str = "/dev/buildkit-state";
/// On the persistent state PVC: the settings it was created for (see [`state_claim_key`]).
pub const STATE_KEY_ANNOTATION: &str = "gitimage.dev/state-volume";

/// Name of a GitImage's persistent BuildKit state PVC (`build.stateVolume.persistent`).
pub fn state_claim_name(gitimage: &str) -> String {
    let base = &gitimage[..gitimage.len().min(253 - "-buildkit".len())];
    format!("{base}-buildkit")
}

/// What a persistent state PVC was made for. The PVC is recreated when this changes: a rootful
/// state dir is not writable by rootless BuildKit, and volume mode and class are immutable.
pub fn state_claim_key(cfg: &Config) -> String {
    let sv = &cfg.build.state_volume;
    let (size, class) = sv.claim().unwrap_or_default();
    let layout = if cfg.build_policy.isolation.rootful() {
        "rootful"
    } else {
        "rootless"
    };
    let mode = sv
        .block
        .as_ref()
        .map_or("Filesystem", |b| b.fs_type.as_str());
    format!("{layout},{mode},{size},{}", class.unwrap_or("default"))
}

fn state_claim_spec(cfg: &Config) -> PersistentVolumeClaimSpec {
    let sv = &cfg.build.state_volume;
    let (size, class) = sv.claim().unwrap_or_default();
    PersistentVolumeClaimSpec {
        access_modes: Some(vec!["ReadWriteOnce".into()]),
        volume_mode: Some(
            if sv.block.is_some() {
                "Block"
            } else {
                "Filesystem"
            }
            .into(),
        ),
        storage_class_name: class.map(str::to_string),
        resources: Some(VolumeResourceRequirements {
            requests: Some(BTreeMap::from([(
                "storage".to_string(),
                Quantity(size.to_string()),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `build.stateVolume.labels` plus `ours`, which win.
fn state_claim_labels<const N: usize>(
    cfg: &Config,
    ours: [(String, String); N],
) -> BTreeMap<String, String> {
    let mut labels = cfg.build.state_volume.labels.clone();
    labels.extend(ours);
    labels
}

/// `build.stateVolume.annotations` plus `ours`, which win.
fn state_claim_annotations<const N: usize>(
    cfg: &Config,
    ours: [(String, String); N],
) -> BTreeMap<String, String> {
    let mut annotations = cfg.build.state_volume.annotations.clone();
    annotations.extend(ours);
    annotations
}

/// The GitImage's persistent BuildKit state PVC, if `build.stateVolume.persistent` is set.
/// Owned by the GitImage, so it goes away with it.
pub fn state_claim(gi: &GitImage, cfg: &Config) -> Option<PersistentVolumeClaim> {
    cfg.build.state_volume.persistent.as_ref()?;
    let name = gi.metadata.name.clone().unwrap_or_default();
    Some(PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(state_claim_name(&name)),
            namespace: gi.metadata.namespace.clone(),
            labels: Some(state_claim_labels(
                cfg,
                [
                    (keys::NAME_LABEL.to_string(), name),
                    ("app.kubernetes.io/managed-by".into(), "gitimage".into()),
                ],
            )),
            annotations: Some(state_claim_annotations(
                cfg,
                [(STATE_KEY_ANNOTATION.to_string(), state_claim_key(cfg))],
            )),
            owner_references: Some(vec![owner_ref(gi)]),
            ..Default::default()
        },
        spec: Some(state_claim_spec(cfg)),
        ..Default::default()
    })
}

/// How much of a persistent state volume BuildKit may use, in bytes.
struct StateLimits {
    /// `size`.
    size: u64,
    /// Never pruned below this: 10% of `size`.
    reserved: u64,
    /// Pruned down to this: 80% of `size`.
    max_used: u64,
    /// Pruned until this much is free: `minFreeSpace`.
    min_free: u64,
}

fn state_limits(cfg: &Config) -> Option<StateLimits> {
    let p = cfg.build.state_volume.persistent.as_ref()?;
    let bytes = p.size_bytes()?;
    Some(StateLimits {
        size: bytes,
        reserved: bytes / 10,
        max_used: bytes / 10 * 8,
        min_free: p.min_free_bytes()?,
    })
}

/// `buildkitd.toml` garbage collection for a persistent state volume, to the same
/// [`StateLimits`] as the prune before each build (`state.sh`). Unlike BuildKit's default
/// rules, cache mounts are not capped at 512MB: keeping them is the point of a persistent volume.
fn gc_policy(cfg: &Config) -> Option<String> {
    let StateLimits {
        reserved,
        max_used,
        min_free,
        ..
    } = state_limits(cfg)?;
    Some(format!(
        r#"
# build.stateVolume.persistent
[[worker.oci.gcpolicy]]
  filters = ["type==source.local", "type==source.git.checkout"]
  keepDuration = "48h"
  maxUsedSpace = "512MB"
[[worker.oci.gcpolicy]]
  keepDuration = "1440h"
  reservedSpace = "{reserved}"
  maxUsedSpace = "{max_used}"
  minFreeSpace = "{min_free}"
[[worker.oci.gcpolicy]]
  all = true
  reservedSpace = "{reserved}"
  maxUsedSpace = "{max_used}"
  minFreeSpace = "{min_free}"
"#
    ))
}

/// Prepares a persistent state volume before each build and records how much the build grows
/// it: see `state.sh`, shipped in the build ConfigMap.
pub const STATE_SCRIPT: &str = include_str!("state.sh");
const STATE_SCRIPT_PATH: &str = "/etc/gitimage/state.sh";

/// Wraps BuildKit's `command` in `state.sh` for a persistent state volume; `fresh` empties the
/// volume first (the retry of a build that ran out of space).
fn with_state_script(
    cfg: &Config,
    state_dir: &str,
    claim: &str,
    fresh: bool,
    command: Vec<String>,
) -> Vec<String> {
    let Some(StateLimits {
        size,
        reserved,
        max_used,
        min_free,
    }) = state_limits(cfg)
    else {
        return command;
    };
    let mut wrapped = vec![
        "sh".to_string(),
        STATE_SCRIPT_PATH.into(),
        state_dir.into(),
        claim.into(),
        if fresh { "fresh" } else { "keep" }.into(),
        size.to_string(),
        reserved.to_string(),
        max_used.to_string(),
        min_free.to_string(),
        "--".into(),
    ];
    wrapped.extend(command);
    wrapped
}

/// Formats the state device unless it already holds a filesystem that `e2fsck` can repair (a
/// build killed mid-write leaves an unclean journal). No blocks are reserved for root: a cache
/// has no use for them, and BuildKit's free-space limits then match `df`.
fn format_once_script(fs_type: &str) -> String {
    format!(
        "if dumpe2fs -h {STATE_DEVICE} >/dev/null 2>&1; then \
         e2fsck -p {STATE_DEVICE}; rc=$?; \
         [ $rc -lt 4 ] && {{ tune2fs -m 0 {STATE_DEVICE} >/dev/null; exit 0; }}; \
         echo \"e2fsck exited $rc: reformatting the BuildKit state volume\" >&2; fi; \
         exec mkfs.{fs_type} -q -F -m 0 -E nodiscard {STATE_DEVICE}"
    )
}

/// BuildKit's state volume (`build.stateVolume`), added to `pod` and mounted into `buildkit`
/// (whose command must already be set). With `claim`, the GitImage's persistent PVC of that
/// name (only if `persistent` is set); otherwise an emptyDir (node disk, or tmpfs with
/// `medium: Memory`) or a raw block ephemeral PVC. A block volume is formatted by an init
/// container (a persistent one only once), then BuildKit's command is wrapped to mount it at
/// the state dir before exec'ing the original command.
pub fn attach_state_volume(
    cfg: &Config,
    pod: &mut PodSpec,
    buildkit: &mut Container,
    claim: Option<&str>,
    fresh: bool,
) {
    let sv = &cfg.build.state_volume;
    let claim = claim.filter(|_| sv.persistent.is_some());
    let state_dir = BuildkitLayout::for_config(cfg).state_dir;
    let volumes = pod.volumes.get_or_insert_with(Vec::new);
    let mut volume = Volume {
        name: STATE_VOLUME.into(),
        ..Default::default()
    };
    match (claim, &sv.block) {
        (Some(name), _) => {
            volume.persistent_volume_claim = Some(PersistentVolumeClaimVolumeSource {
                claim_name: name.into(),
                read_only: None,
            });
            // Kubelet otherwise re-chowns every file of the cache on each mount.
            pod.security_context
                .get_or_insert_with(Default::default)
                .fs_group_change_policy = Some("OnRootMismatch".into());
        }
        (None, Some(_)) => {
            volume.ephemeral = Some(EphemeralVolumeSource {
                volume_claim_template: Some(PersistentVolumeClaimTemplate {
                    metadata: Some(ObjectMeta {
                        labels: Some(state_claim_labels(
                            cfg,
                            [("app.kubernetes.io/managed-by".into(), "gitimage".into())],
                        )),
                        annotations: (!sv.annotations.is_empty()).then(|| sv.annotations.clone()),
                        ..Default::default()
                    }),
                    spec: state_claim_spec(cfg),
                }),
            });
        }
        (None, None) => {
            volume.empty_dir = Some(EmptyDirVolumeSource {
                medium: (!sv.medium.is_empty()).then(|| sv.medium.clone()),
                size_limit: sv.size_limit.clone().map(Quantity),
            });
        }
    }
    volumes.push(volume);
    // Inside the mount below: the script needs the mounted state.
    if let Some(name) = claim {
        let original = buildkit.command.take().unwrap_or_default();
        buildkit.command = Some(with_state_script(cfg, state_dir, name, fresh, original));
        buildkit
            .volume_mounts
            .get_or_insert_with(Vec::new)
            .push(VolumeMount {
                name: "build-config".into(),
                mount_path: STATE_SCRIPT_PATH.into(),
                sub_path: Some("state.sh".into()),
                read_only: Some(true),
                ..Default::default()
            });
    }
    let Some(b) = &sv.block else {
        buildkit
            .volume_mounts
            .get_or_insert_with(Vec::new)
            .push(mount(STATE_VOLUME, state_dir, false));
        return;
    };
    let device = || {
        Some(vec![VolumeDevice {
            name: STATE_VOLUME.into(),
            device_path: STATE_DEVICE.into(),
        }])
    };
    // nodiscard skips a slow full-device TRIM on network storage.
    let format = if claim.is_some() {
        vec!["sh".into(), "-c".into(), format_once_script(&b.fs_type)]
    } else {
        vec![
            format!("mkfs.{}", b.fs_type),
            "-q".into(),
            "-F".into(),
            "-m".into(),
            "0".into(),
            "-E".into(),
            "nodiscard".into(),
            STATE_DEVICE.into(),
        ]
    };
    pod.init_containers
        .get_or_insert_with(Vec::new)
        .push(Container {
            name: "format-state".into(),
            image: Some(b.format_image.clone()),
            command: Some(format),
            volume_devices: device(),
            security_context: Some(SecurityContext {
                privileged: Some(true),
                ..Default::default()
            }),
            termination_message_policy: Some("FallbackToLogsOnError".into()),
            ..Default::default()
        });
    buildkit.volume_devices = device();
    let original = buildkit.command.take().unwrap_or_default();
    let mut command = vec![
        "sh".into(),
        "-c".into(),
        format!(
            "mkdir -p {state_dir} && mount -t {} -o noatime {STATE_DEVICE} {state_dir} && exec \"$0\" \"$@\"",
            b.fs_type
        ),
    ];
    command.extend(original);
    buildkit.command = Some(command);
}

/// Isolation fields for the build pod and the BuildKit container (design section 8).
pub fn pod_overlay(cfg: &Config, pod: &mut PodSpec, buildkit: &mut Container) {
    let iso = &cfg.build_policy.isolation;
    let unconfined = SecurityContext {
        run_as_user: Some(1000),
        run_as_group: Some(1000),
        seccomp_profile: Some(SeccompProfile {
            type_: "Unconfined".into(),
            ..Default::default()
        }),
        app_armor_profile: Some(AppArmorProfile {
            type_: "Unconfined".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let privileged = SecurityContext {
        privileged: Some(true),
        ..Default::default()
    };
    // Flags for the buildkitd that buildctl-daemonless.sh starts.
    let mut flags: Vec<&str> = Vec::new();
    match iso.mode {
        IsolationMode::RuntimeClass if iso.privileged_in_sandbox => {
            pod.runtime_class_name = iso.runtime_class_name.clone();
            buildkit.security_context = Some(privileged);
        }
        // Sysbox: root in a pod user namespace; the runtime grants it capabilities there and
        // virtualizes procfs/sysfs, so rootful BuildKit runs without privileged or unconfined
        // profiles (Pod Security baseline).
        IsolationMode::RuntimeClass if iso.rootful_in_sandbox => {
            pod.runtime_class_name = iso.runtime_class_name.clone();
            pod.host_users = Some(false);
            buildkit.security_context = Some(SecurityContext {
                privileged: Some(false),
                ..Default::default()
            });
        }
        IsolationMode::RuntimeClass => {
            pod.runtime_class_name = iso.runtime_class_name.clone();
            buildkit.security_context = Some(unconfined);
            flags.push("--oci-worker-no-process-sandbox");
        }
        IsolationMode::Userns => {
            pod.host_users = Some(false);
            buildkit.security_context = Some(unconfined);
            flags.push("--oci-worker-no-process-sandbox");
        }
        IsolationMode::Privileged => {
            buildkit.security_context = Some(privileged);
        }
    }
    flags.extend(iso.snapshotter.flag());
    if !flags.is_empty() {
        buildkit.env.get_or_insert_with(Vec::new).push(EnvVar {
            name: "BUILDKITD_FLAGS".into(),
            value: Some(flags.join(" ")),
            ..Default::default()
        });
    }
}

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.into(),
        mount_path: path.into(),
        read_only: Some(read_only),
        ..Default::default()
    }
}

fn restricted() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        run_as_non_root: Some(true),
        run_as_user: Some(65532),
        run_as_group: Some(65532),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".into()]),
            ..Default::default()
        }),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The build Job for `p.sha`.
pub fn build_job(gi: &GitImage, cfg: &Config, p: &BuildParams) -> Job {
    let name = gi.metadata.name.clone().unwrap_or_default();
    let ns = gi.metadata.namespace.clone().unwrap_or_default();
    let job = job_name(&name, &p.sha, p.attempt);
    let labels = labels(gi, &p.sha);

    let mut volumes = vec![
        Volume {
            name: "workspace".into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        Volume {
            name: "tmp".into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        Volume {
            name: "meta".into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        Volume {
            name: "push-auth".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(p.push_auth_secret.clone()),
                items: Some(vec![KeyToPath {
                    key: "config.json".into(),
                    path: "config.json".into(),
                    mode: None,
                }]),
                ..Default::default()
            }),
            ..Default::default()
        },
        Volume {
            name: "build-config".into(),
            config_map: Some(ConfigMapVolumeSource {
                name: BUILD_CONFIGMAP.into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];

    let mut clone_args = vec![
        "clone".to_string(),
        format!("--url={}", k8s_literal(&gi.spec.url)),
        format!("--sha={}", p.sha),
        format!("--dest={WORKSPACE}"),
        format!("--context={}", k8s_literal(&gi.spec.build.context)),
        format!("--meta-dir={META_DIR}"),
        "--log-format=json".into(),
    ];
    if let Some(tree) = &p.skip_if_tree {
        clone_args.push(format!("--skip-if-tree={tree}"));
    }
    let mut clone_mounts = vec![
        mount("workspace", WORKSPACE, false),
        mount("tmp", "/tmp", false),
        mount("meta", META_DIR, false),
    ];
    if let Some(sr) = &gi.spec.secret_ref {
        volumes.push(Volume {
            name: "git-credentials".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(sr.name.clone()),
                default_mode: Some(0o440),
                ..Default::default()
            }),
            ..Default::default()
        });
        clone_mounts.push(mount("git-credentials", GIT_CREDS_DIR, true));
        clone_args.push(format!("--credentials-dir={GIT_CREDS_DIR}"));
    }
    let clone = Container {
        name: "clone".into(),
        image: Some(p.gitimage_image.clone()),
        args: Some(clone_args),
        volume_mounts: Some(clone_mounts),
        security_context: Some(restricted()),
        termination_message_policy: Some("File".into()),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".into(), Quantity("50m".into())),
                ("memory".into(), Quantity("64Mi".into())),
            ])),
            limits: Some(BTreeMap::from([(
                "memory".into(),
                Quantity("512Mi".into()),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    };

    let layout = BuildkitLayout::for_config(cfg);
    let (args, mut env) = buildctl_args(gi, cfg, p);
    env.push(EnvVar {
        name: "DOCKER_CONFIG".into(),
        value: Some(layout.docker_config_dir.to_string()),
        ..Default::default()
    });
    let mut bk_mounts = vec![
        mount("workspace", WORKSPACE, true),
        mount("meta", META_DIR, true),
        mount("push-auth", layout.docker_config_dir, true),
        VolumeMount {
            name: "build-config".into(),
            mount_path: layout.buildkitd_toml.to_string(),
            sub_path: Some("buildkitd.toml".into()),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "build-config".into(),
            mount_path: "/etc/gitimage/ca.crt".into(),
            sub_path: Some("ca.crt".into()),
            read_only: Some(true),
            ..Default::default()
        },
    ];
    // Volume names must be RFC 1123 labels, which secret ids (`keystore_password`) need
    // not be; the id is only the file name and the BuildKit `--secret` id.
    for (i, (id, secret, key)) in p.secrets.iter().enumerate() {
        let vol = format!("build-secret-{i}");
        volumes.push(Volume {
            name: vol.clone(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.clone()),
                items: Some(vec![KeyToPath {
                    key: key.clone(),
                    path: id.clone(),
                    mode: Some(0o440),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        });
        bk_mounts.push(VolumeMount {
            name: vol,
            mount_path: format!("{SECRETS_DIR}/{id}"),
            sub_path: Some(id.clone()),
            read_only: Some(true),
            ..Default::default()
        });
    }
    let build_resources = gi
        .spec
        .build
        .resources
        .as_ref()
        .and_then(resources)
        .or_else(|| resources(&cfg.build.default_resources));
    let mut buildkit = Container {
        name: "buildkit".into(),
        image: Some(layout.image.clone()),
        command: Some(commit_time_wrapper(vec!["buildctl-daemonless.sh".into()])),
        args: Some(args),
        env: Some(env),
        volume_mounts: Some(bk_mounts),
        resources: build_resources,
        termination_message_policy: Some("FallbackToLogsOnError".into()),
        ..Default::default()
    };
    let mut pod = PodSpec {
        restart_policy: Some("Never".into()),
        automount_service_account_token: Some(false),
        enable_service_links: Some(false),
        init_containers: Some(vec![clone]),
        volumes: Some(volumes),
        security_context: Some(PodSecurityContext {
            fs_group: Some(1000),
            ..Default::default()
        }),
        ..Default::default()
    };
    pod_overlay(cfg, &mut pod, &mut buildkit);
    attach_state_volume(
        cfg,
        &mut pod,
        &mut buildkit,
        Some(&state_claim_name(&name)),
        p.fresh_state,
    );
    pod.containers = vec![buildkit];

    let mut annotations = BTreeMap::new();
    if let Some(t) = &p.skip_if_tree {
        annotations.insert("gitimage.dev/skip-if-tree".to_string(), t.clone());
    }
    Job {
        metadata: ObjectMeta {
            name: Some(job),
            namespace: Some(ns),
            labels: Some(labels.clone()),
            annotations: (!annotations.is_empty()).then_some(annotations),
            owner_references: Some(vec![owner_ref(gi)]),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            active_deadline_seconds: Some(cfg.build.active_deadline_seconds()),
            ttl_seconds_after_finished: Some(3600),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(pod),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Docker `config.json` for BuildKit with basic auth `gitimage:<token>` for the push host.
pub fn push_auth_secret(job: &Job, cfg: &Config, token: &str) -> Secret {
    use base64::Engine as _;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("gitimage:{token}"));
    let config =
        serde_json::json!({ "auths": { cfg.push_endpoint.authority(): { "auth": auth } } });
    let job_name = job.metadata.name.clone().unwrap_or_default();
    Secret {
        metadata: ObjectMeta {
            name: Some(push_auth_secret_name(&job_name)),
            namespace: job.metadata.namespace.clone(),
            labels: job.metadata.labels.clone(),
            owner_references: job.metadata.uid.as_ref().map(|uid| {
                vec![OwnerReference {
                    api_version: "batch/v1".into(),
                    kind: "Job".into(),
                    name: job_name.clone(),
                    uid: uid.clone(),
                    controller: Some(true),
                    block_owner_deletion: Some(true),
                }]
            }),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from([("config.json".into(), config.to_string())])),
        type_: Some("Opaque".into()),
        ..Default::default()
    }
}

/// Per-namespace ConfigMap: the controller CA and `buildkitd.toml` trusting it for the push host.
pub fn build_configmap(ns: &str, cfg: &Config, ca_pem: &str) -> ConfigMap {
    let mut toml = format!(
        "[registry.\"{}\"]\n  ca = [\"/etc/gitimage/ca.crt\"]\n",
        cfg.push_endpoint.authority()
    );
    let gc = gc_policy(cfg);
    if let Some(gc) = &gc {
        toml.push_str(gc);
    }
    if !cfg.build.buildkitd_config.trim().is_empty() {
        toml.push_str("\n# build.buildkitdConfig\n");
        toml.push_str(cfg.build.buildkitd_config.trim_end());
        toml.push('\n');
    }
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(BUILD_CONFIGMAP.into()),
            namespace: Some(ns.into()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".into(),
                "gitimage".into(),
            )])),
            ..Default::default()
        },
        data: Some({
            let mut data = BTreeMap::from([
                ("ca.crt".to_string(), ca_pem.to_string()),
                ("buildkitd.toml".to_string(), toml),
            ]);
            if gc.is_some() {
                data.insert("state.sh".into(), STATE_SCRIPT.into());
            }
            data
        }),
        ..Default::default()
    }
}

/// Egress for build pods: DNS, the push endpoint, and the internet minus cluster CIDRs.
pub fn egress_policy(ns: &str, cfg: &Config, controller_ns: &str) -> NetworkPolicy {
    let dns = NetworkPolicyEgressRule {
        ports: Some(vec![
            NetworkPolicyPort {
                port: Some(IntOrString::Int(53)),
                protocol: Some("UDP".into()),
                ..Default::default()
            },
            NetworkPolicyPort {
                port: Some(IntOrString::Int(53)),
                protocol: Some("TCP".into()),
                ..Default::default()
            },
        ]),
        to: None,
    };
    let push = NetworkPolicyEgressRule {
        ports: Some(vec![NetworkPolicyPort {
            port: Some(IntOrString::Int(5000)),
            protocol: Some("TCP".into()),
            ..Default::default()
        }]),
        to: Some(vec![NetworkPolicyPeer {
            namespace_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "kubernetes.io/metadata.name".into(),
                    controller_ns.into(),
                )])),
                ..Default::default()
            }),
            pod_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "app.kubernetes.io/component".into(),
                    "controller".into(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }]),
    };
    let mut egress = vec![dns, push];
    let policy = &cfg.build_policy.egress;
    if policy.allow_internet {
        let (v6, v4): (Vec<String>, Vec<String>) = cfg
            .network
            .cluster_cidrs
            .iter()
            .cloned()
            .partition(|c| c.contains(':'));
        for (cidr, except) in [("0.0.0.0/0", v4), ("::/0", v6)] {
            let except = (policy.deny_cluster_cidrs && !except.is_empty()).then_some(except);
            egress.push(NetworkPolicyEgressRule {
                to: Some(vec![NetworkPolicyPeer {
                    ip_block: Some(IPBlock {
                        cidr: cidr.into(),
                        except,
                    }),
                    ..Default::default()
                }]),
                ports: None,
            });
        }
    }
    egress.extend(policy.extra_rules.iter().cloned());
    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(EGRESS_POLICY.into()),
            namespace: Some(ns.into()),
            labels: Some(BTreeMap::from([(
                "app.kubernetes.io/managed-by".into(),
                "gitimage".into(),
            )])),
            ..Default::default()
        },
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    keys::ROLE_LABEL.into(),
                    keys::ROLE_BUILD.into(),
                )])),
                ..Default::default()
            }),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(egress),
            ingress: None,
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gitimage_api::GitImageSpec;

    pub(crate) fn config() -> Config {
        Config::from_yaml(
            r#"
buildPolicy:
  isolation: { mode: runtimeClass, runtimeClassName: kata }
storage:
  type: registry
  registry: { url: registry.example.com/builds, pushSecret: push }
pushEndpoint: { host: gitimage-push.gitimage-system.svc }
network: { clusterCIDRs: [10.244.0.0/16, "fd00::/48"] }
"#,
        )
        .unwrap()
    }

    pub(crate) fn gitimage() -> GitImage {
        let spec: GitImageSpec = serde_saphyr::from_str(
            r#"
url: https://user@github.com/you/myapp.git
secretRef: { name: myapp-git }
build:
  context: services/api
  args:
    NODE_ENV: { value: "production $(HOME)" }
    TOKEN: { valueFrom: { secretKeyRef: { name: tok, key: t } } }
  secrets: [{ id: npmrc, secretRef: { name: npm-token } }]
"#,
        )
        .unwrap();
        let mut gi = GitImage::new("myapp", spec);
        gi.metadata.namespace = Some("team-a".into());
        gi.metadata.uid = Some("uid-1".into());
        gi
    }

    pub(crate) fn params() -> BuildParams {
        BuildParams {
            sha: "3f2c9e1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            attempt: 0,
            skip_if_tree: Some("tree123".into()),
            push_auth_secret: "myapp-3f2c9e1-push".into(),
            gitimage_image: "ghcr.io/diverofdark/gitimage:test".into(),
            secrets: vec![("npmrc".into(), "npm-token".into(), "token".into())],
            rebuild: String::new(),
            fresh_state: false,
        }
    }

    #[test]
    fn job_snapshot() {
        insta::assert_yaml_snapshot!(build_job(&gitimage(), &config(), &params()));
    }

    #[test]
    fn secret_ids_need_not_be_volume_names() {
        // `keystore_password` is a fine BuildKit secret id but not an RFC 1123 label;
        // the API server rejected the whole Job when the id was used in the volume name.
        let mut p = params();
        p.secrets = vec![
            ("keystore_password".into(), "ks".into(), "password".into()),
            ("Npm.RC".into(), "npm-token".into(), "token".into()),
        ];
        let job = build_job(&gitimage(), &config(), &p);
        let pod = job.spec.unwrap().template.spec.unwrap();
        let label = |n: &str| {
            n.len() <= 63
                && n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !n.starts_with('-')
                && !n.ends_with('-')
        };
        for v in pod.volumes.as_ref().unwrap() {
            assert!(
                label(&v.name),
                "volume name {:?} is not an RFC 1123 label",
                v.name
            );
        }
        let bk = pod
            .containers
            .iter()
            .find(|c| c.name == "buildkit")
            .unwrap();
        let mounts = bk.volume_mounts.as_ref().unwrap();
        let ks = mounts
            .iter()
            .find(|m| m.mount_path == "/run/secrets/keystore_password")
            .unwrap();
        assert_eq!(ks.sub_path.as_deref(), Some("keystore_password"));
        assert!(pod.volumes.unwrap().iter().any(|v| v.name == ks.name));
        let (args, _) = buildctl_args(&gitimage(), &config(), &p);
        assert!(args.contains(
            &"--secret=id=keystore_password,src=/run/secrets/keystore_password".to_string()
        ));
    }

    #[test]
    fn every_build_gets_the_rebuild_value_as_a_build_arg() {
        // Without the annotation the arg is empty, so a Dockerfile that declares it still hits
        // the cache; each new value misses it from that ARG on.
        let (args, _) = buildctl_args(&gitimage(), &config(), &params());
        assert!(args.contains(&"--opt=build-arg:GITIMAGE_REBUILD=".to_string()));
        let mut p = params();
        p.rebuild = "1700000000 $(HOME)".into();
        let (args, _) = buildctl_args(&gitimage(), &config(), &p);
        let rebuild = args
            .iter()
            .position(|a| a == "--opt=build-arg:GITIMAGE_REBUILD=1700000000 $$(HOME)")
            .expect("GITIMAGE_REBUILD, escaped for Kubernetes");
        // After the tenant's args, like GITIMAGE_REVISION, so a build arg cannot override it.
        let tenant = args
            .iter()
            .position(|a| a.starts_with("--opt=build-arg:NODE_ENV="))
            .unwrap();
        assert!(tenant < rebuild);
    }

    #[test]
    fn job_names_fit_in_a_label() {
        assert_eq!(job_name("myapp", "3f2c9e1abc", 0), "myapp-3f2c9e1");
        assert_eq!(job_name("myapp", "3f2c9e1abc", 2), "myapp-3f2c9e1-2");
        let long = "a".repeat(100);
        assert!(job_name(&long, "3f2c9e1abc", 1).len() <= 63);
    }

    #[test]
    fn source_url_drops_credentials() {
        assert_eq!(
            public_source_url("https://u:p@github.com/a/b.git"),
            "https://github.com/a/b.git"
        );
        assert_eq!(
            public_source_url("git@github.com:a/b.git"),
            "ssh://github.com/a/b.git"
        );
    }

    #[test]
    fn tenant_text_cannot_inject_k8s_expansion() {
        let (args, env) = buildctl_args(&gitimage(), &config(), &params());
        assert!(args.contains(&"--opt=build-arg:NODE_ENV=production $$(HOME)".to_string()));
        assert!(args.contains(&"--opt=build-arg:TOKEN=$(GITIMAGE_ARG_TOKEN)".to_string()));
        assert_eq!(env[0].name, "GITIMAGE_ARG_TOKEN");
        assert!(
            args.iter()
                .any(|a| a == "--local=context=/workspace/services/api")
        );
    }

    fn buildkitd_flags(job: Job) -> Option<String> {
        job.spec.unwrap().template.spec.unwrap().containers[0]
            .env
            .as_ref()
            .and_then(|e| e.iter().find(|e| e.name == "BUILDKITD_FLAGS"))
            .and_then(|e| e.value.clone())
    }

    #[test]
    fn snapshotter_is_passed_to_buildkitd() {
        // gVisor: rootless, and native because its overlay cannot hard-link lower files.
        let mut cfg = config();
        cfg.build_policy.isolation.runtime_class_name = Some("gvisor".into());
        assert_eq!(
            buildkitd_flags(build_job(&gitimage(), &cfg, &params())).as_deref(),
            Some("--oci-worker-no-process-sandbox")
        );
        cfg.build_policy.isolation.snapshotter = gitimage_api::config::Snapshotter::Native;
        assert_eq!(
            buildkitd_flags(build_job(&gitimage(), &cfg, &params())).as_deref(),
            Some("--oci-worker-no-process-sandbox --oci-worker-snapshotter=native")
        );
        // Rootful modes get the flag on its own.
        cfg.build_policy.isolation.privileged_in_sandbox = true;
        assert_eq!(
            buildkitd_flags(build_job(&gitimage(), &cfg, &params())).as_deref(),
            Some("--oci-worker-snapshotter=native")
        );
    }

    #[test]
    fn userns_and_privileged_overlays() {
        let mut cfg = config();
        cfg.build_policy.isolation.mode = IsolationMode::Userns;
        let job = build_job(&gitimage(), &cfg, &params());
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.host_users, Some(false));
        assert!(pod.runtime_class_name.is_none());
        cfg.build_policy.isolation.mode = IsolationMode::Privileged;
        let job = build_job(&gitimage(), &cfg, &params());
        let c = &job.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(c.security_context.as_ref().unwrap().privileged, Some(true));
    }

    #[test]
    fn kata_runs_rootful_buildkit_privileged_in_the_sandbox() {
        let mut cfg = config();
        cfg.build_policy.isolation.privileged_in_sandbox = true;
        let job = build_job(&gitimage(), &cfg, &params());
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.runtime_class_name.as_deref(), Some("kata"));
        let c = &pod.containers[0];
        assert_eq!(c.image.as_deref(), Some("moby/buildkit:v0.34.2"));
        assert_eq!(c.security_context.as_ref().unwrap().privileged, Some(true));
        let mounts: Vec<&str> = c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mount_path.as_str())
            .collect();
        assert!(mounts.contains(&"/var/lib/buildkit"));
        assert!(mounts.contains(&"/etc/buildkit/buildkitd.toml"));
        assert!(mounts.contains(&"/root/.docker"));
    }

    #[test]
    fn sysbox_runs_rootful_buildkit_unprivileged_in_a_user_namespace() {
        let mut cfg = config();
        cfg.build_policy.isolation.runtime_class_name = Some("sysbox-runc".into());
        cfg.build_policy.isolation.rootful_in_sandbox = true;
        let job = build_job(&gitimage(), &cfg, &params());
        insta::assert_yaml_snapshot!("sysbox_job", job);
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.runtime_class_name.as_deref(), Some("sysbox-runc"));
        assert_eq!(pod.host_users, Some(false));
        let c = &pod.containers[0];
        assert_eq!(c.image.as_deref(), Some("moby/buildkit:v0.34.2"));
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.privileged, Some(false));
        assert!(sc.seccomp_profile.is_none() && sc.app_armor_profile.is_none());
        assert!(sc.capabilities.is_none() && sc.run_as_user.is_none());
        assert!(
            !c.env
                .as_ref()
                .unwrap()
                .iter()
                .any(|e| e.name == "BUILDKITD_FLAGS")
        );
        let mounts: Vec<&str> = c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mount_path.as_str())
            .collect();
        assert!(mounts.contains(&"/var/lib/buildkit"));
        assert!(mounts.contains(&"/etc/buildkit/buildkitd.toml"));
    }

    #[test]
    fn memory_state_volume() {
        let mut cfg = config();
        cfg.build.state_volume.medium = "Memory".into();
        cfg.build.state_volume.size_limit = Some("6Gi".into());
        let job = build_job(&gitimage(), &cfg, &params());
        let vols = job.spec.unwrap().template.spec.unwrap().volumes.unwrap();
        let state = vols
            .iter()
            .find(|v| v.name == "buildkit-state")
            .unwrap()
            .empty_dir
            .clone()
            .unwrap();
        assert_eq!(state.medium.as_deref(), Some("Memory"));
        assert_eq!(state.size_limit, Some(Quantity("6Gi".into())));
    }

    #[test]
    fn block_state_volume_is_formatted_and_mounted_by_buildkit() {
        let mut cfg = config();
        cfg.build_policy.isolation.mode = IsolationMode::Privileged;
        cfg.build.state_volume.block = Some(gitimage_api::config::BlockStateVolume {
            storage_class_name: Some("ceph-block".into()),
            size: "20Gi".into(),
            fs_type: "ext3".into(),
            format_image: "debian:bookworm-slim".into(),
        });
        let job = build_job(&gitimage(), &cfg, &params());
        let pod = job.spec.unwrap().template.spec.unwrap();
        let state = pod
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "buildkit-state")
            .unwrap();
        assert!(state.empty_dir.is_none());
        let claim = state
            .ephemeral
            .clone()
            .unwrap()
            .volume_claim_template
            .unwrap()
            .spec;
        assert_eq!(claim.volume_mode.as_deref(), Some("Block"));
        assert_eq!(claim.storage_class_name.as_deref(), Some("ceph-block"));
        assert_eq!(
            claim.resources.unwrap().requests.unwrap()["storage"],
            Quantity("20Gi".into())
        );
        let inits = pod.init_containers.unwrap();
        let fmt = inits.last().unwrap();
        assert_eq!(fmt.command.as_ref().unwrap()[0], "mkfs.ext3");
        assert_eq!(
            fmt.volume_devices.as_ref().unwrap()[0].device_path,
            "/dev/buildkit-state"
        );
        let bk = &pod.containers[0];
        assert!(
            bk.volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .all(|m| m.name != "buildkit-state")
        );
        assert_eq!(
            bk.volume_devices.as_ref().unwrap()[0].device_path,
            "/dev/buildkit-state"
        );
        let cmd = bk.command.as_ref().unwrap();
        assert_eq!(&cmd[..2], &["sh".to_string(), "-c".to_string()]);
        assert!(cmd[2].contains("mount -t ext3 -o noatime /dev/buildkit-state /var/lib/buildkit"));
        // The mount runs first, then the commit-time wrapper, then BuildKit.
        assert_eq!(&cmd[3..5], &["sh".to_string(), "-c".to_string()]);
        assert!(cmd[5].contains("GITIMAGE_COMMIT_TIME"));
        assert_eq!(cmd[6], "buildctl-daemonless.sh");
        // The isolation check exercises the same volume.
        let check = crate::isolation::check_job(&cfg, "gitimage-system")
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        assert!(
            check
                .init_containers
                .unwrap()
                .iter()
                .any(|c| c.name == "format-state")
        );
        assert_ne!(
            crate::isolation::check_key(&cfg),
            crate::isolation::check_key(&config())
        );
    }

    fn persistent(cfg: &mut Config, block: bool) {
        cfg.build.state_volume.persistent = Some(gitimage_api::config::PersistentStateVolume {
            size: "20Gi".into(),
            storage_class_name: Some("local-path".into()),
            min_free_space: None,
        });
        if block {
            cfg.build_policy.isolation.mode = IsolationMode::Privileged;
            cfg.build.state_volume.block = Some(gitimage_api::config::BlockStateVolume {
                storage_class_name: None,
                size: String::new(),
                fs_type: "ext4".into(),
                format_image: "debian:bookworm-slim".into(),
            });
        }
    }

    fn state_volume(pod: &PodSpec) -> &Volume {
        pod.volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "buildkit-state")
            .unwrap()
    }

    #[test]
    fn persistent_state_volume_is_the_gitimages_pvc() {
        let mut cfg = config();
        persistent(&mut cfg, false);
        let job = build_job(&gitimage(), &cfg, &params());
        // 20Gi: keep 10% at least, 80% at most, and 25% free.
        let toml = build_configmap("team", &cfg, "PEM").data.unwrap()["buildkitd.toml"].clone();
        assert!(toml.contains("reservedSpace = \"2147483648\""), "{toml}");
        assert!(toml.contains("maxUsedSpace = \"17179869184\""), "{toml}");
        assert!(toml.contains("minFreeSpace = \"5368709100\""), "{toml}");
        assert!(!toml.contains("exec.cachemount"), "{toml}");
        let plain =
            build_configmap("team", &config(), "PEM").data.unwrap()["buildkitd.toml"].clone();
        assert!(!plain.contains("gcpolicy"), "{plain}");
        let pod = job.spec.unwrap().template.spec.unwrap();
        let claim = state_volume(&pod).persistent_volume_claim.clone().unwrap();
        assert_eq!(claim.claim_name, "myapp-buildkit");
        assert_eq!(
            pod.security_context
                .unwrap()
                .fs_group_change_policy
                .as_deref(),
            Some("OnRootMismatch")
        );
        assert!(
            pod.containers[0]
                .volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .any(|m| m.name == "buildkit-state"
                    && m.mount_path == "/home/user/.local/share/buildkit")
        );
        // state.sh runs first, to the same limits as the GC, then the commit-time wrapper.
        let bk = &pod.containers[0];
        let cmd = bk.command.as_ref().unwrap();
        assert_eq!(
            &cmd[..10],
            &[
                "sh",
                "/etc/gitimage/state.sh",
                "/home/user/.local/share/buildkit",
                "myapp-buildkit",
                "keep",
                "21474836480",
                "2147483648",
                "17179869184",
                "5368709100",
                "--"
            ]
        );
        assert!(cmd[12].contains("GITIMAGE_COMMIT_TIME"));
        assert_eq!(cmd[13], "buildctl-daemonless.sh");
        assert!(
            bk.volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .any(|m| m.name == "build-config"
                    && m.mount_path == "/etc/gitimage/state.sh"
                    && m.sub_path.as_deref() == Some("state.sh"))
        );
        let data = build_configmap("team", &cfg, "PEM").data.unwrap();
        assert_eq!(data["state.sh"], STATE_SCRIPT);
        assert!(
            !build_configmap("team", &config(), "PEM")
                .data
                .unwrap()
                .contains_key("state.sh")
        );
        // The retry of a build that ran out of space empties the volume first.
        let fresh = build_job(
            &gitimage(),
            &cfg,
            &BuildParams {
                fresh_state: true,
                ..params()
            },
        );
        let fresh = fresh.spec.unwrap().template.spec.unwrap();
        assert_eq!(fresh.containers[0].command.as_ref().unwrap()[4], "fresh");

        let pvc = state_claim(&gitimage(), &cfg).unwrap();
        assert_eq!(pvc.metadata.name.as_deref(), Some("myapp-buildkit"));
        assert_eq!(pvc.metadata.owner_references.unwrap()[0].kind, "GitImage");
        assert_eq!(
            pvc.metadata.annotations.unwrap()[STATE_KEY_ANNOTATION],
            "rootless,Filesystem,20Gi,local-path"
        );
        let spec = pvc.spec.unwrap();
        assert_eq!(spec.volume_mode.as_deref(), Some("Filesystem"));
        assert_eq!(spec.storage_class_name.as_deref(), Some("local-path"));
        assert!(state_claim(&gitimage(), &config()).is_none());

        // The isolation check has no GitImage: it gets a throwaway emptyDir.
        let check = crate::isolation::check_job(&cfg, "gitimage-system")
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        assert!(state_volume(&check).empty_dir.is_some());
    }

    #[test]
    fn persistent_block_state_volume_is_formatted_once() {
        let mut cfg = config();
        persistent(&mut cfg, true);
        let job = build_job(&gitimage(), &cfg, &params());
        let pod = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(
            state_volume(&pod)
                .persistent_volume_claim
                .as_ref()
                .unwrap()
                .claim_name,
            "myapp-buildkit"
        );
        let fmt = pod.init_containers.as_ref().unwrap().last().unwrap();
        let script = &fmt.command.as_ref().unwrap()[2];
        assert!(
            script.starts_with("if dumpe2fs -h /dev/buildkit-state"),
            "{script}"
        );
        assert!(script.contains("e2fsck -p /dev/buildkit-state"), "{script}");
        assert!(
            script.contains("tune2fs -m 0 /dev/buildkit-state"),
            "{script}"
        );
        assert!(
            script.ends_with("exec mkfs.ext4 -q -F -m 0 -E nodiscard /dev/buildkit-state"),
            "{script}"
        );
        // Mount, then ready the mounted state, then build.
        let cmd = pod.containers[0].command.as_ref().unwrap();
        assert!(cmd[2].contains("mount -t ext4 -o noatime /dev/buildkit-state /var/lib/buildkit"));
        assert_eq!(
            &cmd[3..6],
            &["sh", "/etc/gitimage/state.sh", "/var/lib/buildkit"]
        );
        assert!(cmd[15].contains("GITIMAGE_COMMIT_TIME"));
        assert_eq!(cmd[16], "buildctl-daemonless.sh");

        let pvc = state_claim(&gitimage(), &cfg).unwrap();
        assert_eq!(pvc.spec.unwrap().volume_mode.as_deref(), Some("Block"));
        assert_eq!(state_claim_key(&cfg), "rootful,ext4,20Gi,local-path");

        // The isolation check formats a fresh ephemeral volume of the same size.
        let check = crate::isolation::check_job(&cfg, "gitimage-system")
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        let eph = state_volume(&check).ephemeral.clone().unwrap();
        let spec = eph.volume_claim_template.unwrap().spec;
        assert_eq!(
            spec.resources.unwrap().requests.unwrap()["storage"],
            Quantity("20Gi".into())
        );
        let fmt = check.init_containers.unwrap();
        assert_eq!(
            fmt.last().unwrap().command.as_ref().unwrap()[0],
            "mkfs.ext4"
        );
    }

    #[test]
    fn state_pvcs_carry_configured_labels_and_annotations() {
        let mut cfg = config();
        persistent(&mut cfg, true);
        let sv = &mut cfg.build.state_volume;
        sv.labels = BTreeMap::from([
            ("velero.io/exclude-from-backup".into(), "true".into()),
            // Cannot take over a label the controller relies on.
            ("app.kubernetes.io/managed-by".into(), "someone-else".into()),
        ]);
        sv.annotations = BTreeMap::from([("k8up.io/backup".into(), "false".into())]);
        let meta = state_claim(&gitimage(), &cfg).unwrap().metadata;
        let labels = meta.labels.unwrap();
        assert_eq!(labels["velero.io/exclude-from-backup"], "true");
        assert_eq!(labels["app.kubernetes.io/managed-by"], "gitimage");
        let annotations = meta.annotations.unwrap();
        assert_eq!(annotations["k8up.io/backup"], "false");
        assert!(annotations.contains_key(STATE_KEY_ANNOTATION));

        // The isolation check's ephemeral block PVC gets them too.
        let check = crate::isolation::check_job(&cfg, "gitimage-system")
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();
        let tmpl = state_volume(&check)
            .ephemeral
            .clone()
            .unwrap()
            .volume_claim_template
            .unwrap()
            .metadata
            .unwrap();
        assert_eq!(
            tmpl.labels.unwrap()["velero.io/exclude-from-backup"],
            "true"
        );
        assert_eq!(tmpl.annotations.unwrap()["k8up.io/backup"], "false");
    }

    #[test]
    fn state_claim_names_stay_valid() {
        assert_eq!(state_claim_name("myapp"), "myapp-buildkit");
        assert_eq!(state_claim_name(&"a".repeat(253)).len(), 253);
    }

    #[test]
    fn egress_policy_snapshot() {
        insta::assert_yaml_snapshot!(egress_policy("team-a", &config(), "gitimage-system"));
    }

    #[test]
    fn extra_egress_rules_are_appended() {
        let mut cfg = config();
        let rule: NetworkPolicyEgressRule = serde_json::from_value(serde_json::json!({
            "to": [{
                "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": "traefik" } },
                "podSelector": { "matchLabels": { "app.kubernetes.io/name": "traefik" } }
            }],
            "ports": [{ "port": 8443, "protocol": "TCP" }]
        }))
        .unwrap();
        cfg.build_policy.egress.extra_rules = vec![rule.clone()];
        let base = egress_policy("team-a", &config(), "gitimage-system")
            .spec
            .unwrap()
            .egress
            .unwrap();
        let egress = egress_policy("team-a", &cfg, "gitimage-system")
            .spec
            .unwrap()
            .egress
            .unwrap();
        assert_eq!(egress.len(), base.len() + 1);
        assert_eq!(egress.last(), Some(&rule));
        // Also without internet access.
        cfg.build_policy.egress.allow_internet = false;
        let egress = egress_policy("team-a", &cfg, "gitimage-system")
            .spec
            .unwrap()
            .egress
            .unwrap();
        assert_eq!(egress.last(), Some(&rule));
    }

    #[test]
    fn push_auth_has_basic_auth_for_push_host() {
        let mut job = build_job(&gitimage(), &config(), &params());
        job.metadata.uid = Some("job-uid".into());
        let s = push_auth_secret(&job, &config(), "tok");
        let json = &s.string_data.unwrap()["config.json"];
        assert!(json.contains("gitimage-push.gitimage-system.svc:5000"));
        assert!(json.contains("Z2l0aW1hZ2U6dG9r")); // base64("gitimage:tok")
        assert_eq!(s.metadata.owner_references.unwrap()[0].uid, "job-uid");
    }

    #[test]
    fn buildkitd_config_is_appended() {
        let mut cfg = config();
        cfg.build.buildkitd_config =
            "[registry.\"docker.io\"]\n  mirrors = [\"mirror:5000\"]".into();
        let toml = build_configmap("team-a", &cfg, "PEM").data.unwrap()["buildkitd.toml"].clone();
        assert!(toml.starts_with("[registry.\"gitimage-push"));
        assert!(toml.contains("mirrors = [\"mirror:5000\"]"), "{toml}");
    }

    #[test]
    fn buildkitd_toml_trusts_ca_for_push_host() {
        let cm = build_configmap("team-a", &config(), "PEM");
        let d = cm.data.unwrap();
        assert!(
            d["buildkitd.toml"].contains("[registry.\"gitimage-push.gitimage-system.svc:5000\"]")
        );
        assert_eq!(d["ca.crt"], "PEM");
    }
}
