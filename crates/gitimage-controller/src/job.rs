//! Pure builders for the per-build objects: the Job, its push-auth Secret, and the
//! per-namespace ConfigMap and NetworkPolicy. No I/O; snapshot-tested.

use std::collections::BTreeMap;

use gitimage_api::config::{CacheMode, Config, IsolationMode};
use gitimage_api::{GitImage, Resources, keys};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    AppArmorProfile, Capabilities, ConfigMap, ConfigMapVolumeSource, Container,
    EmptyDirVolumeSource, EnvVar, EnvVarSource, KeyToPath, PodSecurityContext, PodSpec,
    PodTemplateSpec, ResourceRequirements, SeccompProfile, Secret, SecretKeySelector,
    SecretVolumeSource, SecurityContext, Volume, VolumeMount,
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
    for (id, _, _) in &p.secrets {
        args.push(format!("--secret=id={id},src={SECRETS_DIR}/{id}"));
    }
    (args, env)
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
    let env = buildkit.env.get_or_insert_with(Vec::new);
    let privileged = SecurityContext {
        privileged: Some(true),
        ..Default::default()
    };
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
            env.push(EnvVar {
                name: "BUILDKITD_FLAGS".into(),
                value: Some("--oci-worker-no-process-sandbox".into()),
                ..Default::default()
            });
        }
        IsolationMode::Userns => {
            pod.host_users = Some(false);
            buildkit.security_context = Some(unconfined);
            env.push(EnvVar {
                name: "BUILDKITD_FLAGS".into(),
                value: Some("--oci-worker-no-process-sandbox".into()),
                ..Default::default()
            });
        }
        IsolationMode::Privileged => {
            buildkit.security_context = Some(privileged);
        }
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
            name: "buildkit-state".into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        },
        Volume {
            name: "tmp".into(),
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
        "--log-format=json".into(),
    ];
    if let Some(tree) = &p.skip_if_tree {
        clone_args.push(format!("--skip-if-tree={tree}"));
    }
    let mut clone_mounts = vec![
        mount("workspace", WORKSPACE, false),
        mount("tmp", "/tmp", false),
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
        mount("buildkit-state", layout.state_dir, false),
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
    for (id, secret, key) in &p.secrets {
        let vol = format!("build-secret-{id}");
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
        command: Some(vec!["buildctl-daemonless.sh".into()]),
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
        data: Some(BTreeMap::from([
            ("ca.crt".into(), ca_pem.into()),
            ("buildkitd.toml".into(), toml),
        ])),
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
        }
    }

    #[test]
    fn job_snapshot() {
        insta::assert_yaml_snapshot!(build_job(&gitimage(), &config(), &params()));
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
    fn egress_policy_snapshot() {
        insta::assert_yaml_snapshot!(egress_policy("team-a", &config(), "gitimage-system"));
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
