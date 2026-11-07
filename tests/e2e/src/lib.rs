//! Helpers for the end-to-end suite (PRD N6). The environment comes from `hack/e2e/up.sh`
//! and `hack/e2e/install.sh`; tests are `#[ignore]` and run with
//! `cargo test -p e2e -- --ignored --test-threads 4`.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine as _;
use gitimage_api::{GitImage, GitImageSpec};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config, ResourceExt};
use serde_json::json;

pub const GITEA: &str = "http://127.0.0.1:3300";
/// How pods and the controller reach Gitea (docker network `kind`).
pub const GITEA_IN_CLUSTER: &str = "http://e2e-gitea:3000";
pub const GITEA_USER: &str = "e2e";
pub const GITEA_PASSWORD: &str = "e2e-password";
pub const REGISTRY: &str = "http://127.0.0.1:5001";

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Kubernetes client for the kind cluster, from `hack/.kubeconfig` only.
pub async fn client() -> anyhow::Result<Client> {
    let path = std::env::var("E2E_KUBECONFIG")
        .map(Into::into)
        .unwrap_or_else(|_| repo_root().join("hack/.kubeconfig"));
    let kc = Kubeconfig::read_from(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg = Config::from_custom_kubeconfig(kc, &KubeConfigOptions::default()).await?;
    Ok(Client::try_from(cfg)?)
}

/// Polls `f` every second until it returns `Some`, or fails after `timeout`.
pub async fn wait_for<T, F, Fut>(what: &str, timeout: Duration, mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Option<T>>>,
{
    let start = Instant::now();
    let mut last_err = None;
    loop {
        match f().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
        if start.elapsed() > timeout {
            bail!("timed out after {timeout:?} waiting for {what}; last error: {last_err:?}");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub fn unique(prefix: &str) -> String {
    use rand::RngExt as _;
    let n: u32 = rand::rng().random();
    format!("{prefix}-{:06x}", n & 0xffffff)
}

/// A throwaway namespace, deleted on drop (best effort).
pub struct TestNs {
    pub name: String,
    pub client: Client,
}

impl TestNs {
    pub async fn new(client: &Client, prefix: &str) -> anyhow::Result<Self> {
        let name = unique(prefix);
        let api: Api<Namespace> = Api::all(client.clone());
        let ns: Namespace = serde_json::from_value(json!({ "metadata": { "name": name } }))?;
        api.create(&PostParams::default(), &ns).await?;
        Ok(Self {
            name,
            client: client.clone(),
        })
    }

    pub fn api<K>(&self) -> Api<K>
    where
        K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>,
    {
        Api::namespaced(self.client.clone(), &self.name)
    }
}

impl Drop for TestNs {
    fn drop(&mut self) {
        if std::env::var("E2E_KEEP").is_ok() {
            return;
        }
        let client = self.client.clone();
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                rt.block_on(async {
                    let api: Api<Namespace> = Api::all(client);
                    let _ = api.delete(&name, &DeleteParams::default()).await;
                });
            }
        })
        .join();
    }
}

/// Gitea REST client (the suite needs no git binary).
pub struct Gitea {
    http: reqwest::Client,
}

impl Default for Gitea {
    fn default() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Gitea {
    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.basic_auth(GITEA_USER, Some(GITEA_PASSWORD))
    }

    pub async fn create_repo(&self, name: &str, private: bool) -> anyhow::Result<()> {
        let r = self
            .auth(self.http.post(format!("{GITEA}/api/v1/user/repos")))
            .json(&json!({ "name": name, "private": private, "auto_init": false, "default_branch": "main" }))
            .send()
            .await?;
        if !r.status().is_success() {
            bail!("create repo {name}: {} {}", r.status(), r.text().await?);
        }
        Ok(())
    }

    /// Commits `files` (path -> content) to `branch`; returns the commit sha.
    pub async fn commit(
        &self,
        repo: &str,
        branch: &str,
        files: &[(&str, &str)],
        message: &str,
    ) -> anyhow::Result<String> {
        let existing = self.tree(repo, branch).await.unwrap_or_default();
        let ops: Vec<serde_json::Value> = files
            .iter()
            .map(|(path, content)| {
                let sha = existing
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, s)| s.clone());
                let mut op = json!({
                    "operation": if sha.is_some() { "update" } else { "create" },
                    "path": path,
                    "content": base64::engine::general_purpose::STANDARD.encode(content),
                });
                if let Some(sha) = sha {
                    op["sha"] = json!(sha);
                }
                op
            })
            .collect();
        let mut body = json!({ "files": ops, "message": message });
        if existing.is_empty() {
            body["new_branch"] = json!(branch);
        } else {
            body["branch"] = json!(branch);
        }
        let r = self
            .auth(
                self.http
                    .post(format!("{GITEA}/api/v1/repos/{GITEA_USER}/{repo}/contents")),
            )
            .json(&body)
            .send()
            .await?;
        let status = r.status();
        let v: serde_json::Value = r.json().await?;
        if !status.is_success() {
            bail!("commit to {repo}: {status} {v}");
        }
        v["commit"]["sha"]
            .as_str()
            .map(str::to_string)
            .context("no commit sha in response")
    }

    /// (path, blob sha) of every file on `branch`; empty for a new repo.
    async fn tree(&self, repo: &str, branch: &str) -> anyhow::Result<Vec<(String, String)>> {
        let r = self
            .auth(self.http.get(format!(
                "{GITEA}/api/v1/repos/{GITEA_USER}/{repo}/git/trees/{branch}?recursive=true"
            )))
            .send()
            .await?;
        if !r.status().is_success() {
            return Ok(Vec::new());
        }
        let v: serde_json::Value = r.json().await?;
        Ok(v["tree"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|e| e["type"] == "blob")
                    .map(|e| {
                        (
                            e["path"].as_str().unwrap_or("").to_string(),
                            e["sha"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn tag(&self, repo: &str, tag: &str, target: &str) -> anyhow::Result<()> {
        let r = self
            .auth(
                self.http
                    .post(format!("{GITEA}/api/v1/repos/{GITEA_USER}/{repo}/tags")),
            )
            .json(&json!({ "tag_name": tag, "target": target, "message": tag }))
            .send()
            .await?;
        if !r.status().is_success() {
            bail!("tag {repo}: {} {}", r.status(), r.text().await?);
        }
        Ok(())
    }

    pub fn https_url(repo: &str) -> String {
        format!("{GITEA_IN_CLUSTER}/{GITEA_USER}/{repo}.git")
    }
}

/// Creates a GitImage from a YAML spec.
pub async fn create_gitimage(ns: &TestNs, name: &str, spec_yaml: &str) -> anyhow::Result<GitImage> {
    let spec: GitImageSpec = serde_yaml::from_str(spec_yaml)?;
    let mut gi = GitImage::new(name, spec);
    gi.metadata.namespace = Some(ns.name.clone());
    Ok(ns
        .api::<GitImage>()
        .create(&PostParams::default(), &gi)
        .await?)
}

/// Waits until the GitImage's current revision is `sha` and returns its image ref.
pub async fn wait_built(
    ns: &TestNs,
    name: &str,
    sha: &str,
    timeout: Duration,
) -> anyhow::Result<GitImage> {
    let api = ns.api::<GitImage>();
    wait_for(&format!("GitImage {name} built at {sha}"), timeout, || {
        let api = api.clone();
        async move {
            let gi = api.get(name).await?;
            let st = gi.status.clone().unwrap_or_default();
            if let Some(failed) = st.condition("Failed").filter(|c| c.status == "True") {
                bail!("GitImage failed: {} {}", failed.reason, failed.message);
            }
            Ok((st.revision.as_deref() == Some(sha) && st.image.is_some()).then_some(gi))
        }
    })
    .await
}

/// A Deployment whose container `web` runs GitImage `gitimage`.
pub async fn create_deployment(
    ns: &TestNs,
    name: &str,
    gitimage: &str,
) -> anyhow::Result<Deployment> {
    let d: Deployment = serde_json::from_value(json!({
        "metadata": { "name": name },
        "spec": {
            "replicas": 1,
            "selector": { "matchLabels": { "app": name } },
            "template": {
                "metadata": {
                    "labels": { "app": name },
                    "annotations": { "gitimage.dev/image.web": gitimage }
                },
                "spec": {
                    "terminationGracePeriodSeconds": 1,
                    "containers": [{ "name": "web", "image": "placeholder" }]
                }
            }
        }
    }))?;
    Ok(ns
        .api::<Deployment>()
        .create(&PostParams::default(), &d)
        .await?)
}

/// Running pods of a Deployment with their `web` image.
pub async fn running_images(ns: &TestNs, app: &str) -> anyhow::Result<Vec<String>> {
    let pods = ns
        .api::<Pod>()
        .list(&ListParams::default().labels(&format!("app={app}")))
        .await?;
    Ok(pods
        .items
        .iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .filter_map(|p| {
            p.spec
                .as_ref()?
                .containers
                .iter()
                .find(|c| c.name == "web")?
                .image
                .clone()
        })
        .collect())
}

pub async fn build_jobs(ns: &TestNs, gitimage: &str) -> anyhow::Result<Vec<Job>> {
    Ok(ns
        .api::<Job>()
        .list(&ListParams::default().labels(&format!("gitimage.dev/name={gitimage}")))
        .await?
        .items)
}

pub async fn create_secret(ns: &TestNs, name: &str, data: &[(&str, &str)]) -> anyhow::Result<()> {
    let s = Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(name.into()),
            ..Default::default()
        },
        string_data: Some(
            data.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        ),
        ..Default::default()
    };
    ns.api::<Secret>()
        .create(&PostParams::default(), &s)
        .await?;
    Ok(())
}

/// Runs a one-shot busybox pod and returns its log (for in-cluster HTTP calls).
pub async fn run_pod(ns: &TestNs, name: &str, script: &str) -> anyhow::Result<String> {
    let p: Pod = serde_json::from_value(json!({
        "metadata": { "name": name },
        "spec": {
            "restartPolicy": "Never",
            "containers": [{ "name": "c", "image": "docker.io/library/busybox:1.37", "command": ["sh", "-c", script] }]
        }
    }))?;
    let api = ns.api::<Pod>();
    api.create(&PostParams::default(), &p).await?;
    wait_for(
        &format!("pod {name} finished"),
        Duration::from_secs(120),
        || {
            let api = api.clone();
            async move {
                let p = api.get(name).await?;
                let phase = p.status.and_then(|s| s.phase).unwrap_or_default();
                Ok((phase == "Succeeded" || phase == "Failed").then_some(()))
            }
        },
    )
    .await?;
    Ok(api.logs(name, &Default::default()).await?)
}

/// Fetches every layer of `<repo>@<digest>` from the e2e registry and returns the
/// concatenated uncompressed bytes (for "secret not in any layer" checks).
pub async fn registry_layers(repo: &str, reference: &str) -> anyhow::Result<Vec<u8>> {
    let http = reqwest::Client::new();
    let accept = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";
    let mut manifest: serde_json::Value = http
        .get(format!("{REGISTRY}/v2/{repo}/manifests/{reference}"))
        .header("Accept", accept)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(first) = manifest["manifests"].as_array().and_then(|m| m.first()) {
        let d = first["digest"]
            .as_str()
            .context("index without digest")?
            .to_string();
        manifest = http
            .get(format!("{REGISTRY}/v2/{repo}/manifests/{d}"))
            .header("Accept", accept)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
    }
    let mut out = Vec::new();
    for layer in manifest["layers"].as_array().cloned().unwrap_or_default() {
        let d = layer["digest"].as_str().context("layer digest")?;
        let bytes = http
            .get(format!("{REGISTRY}/v2/{repo}/blobs/{d}"))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let mut dec = flate2::read::GzDecoder::new(&bytes[..]);
        let mut buf = Vec::new();
        use std::io::Read as _;
        if dec.read_to_end(&mut buf).is_ok() {
            out.extend_from_slice(&buf);
        } else {
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}

pub fn gitimage_name(gi: &GitImage) -> String {
    gi.name_any()
}
