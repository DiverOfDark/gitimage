//! Milestone "Done when" checks, run against `hack/e2e` (see the crate docs).

use std::time::Duration;

use e2e::*;
use gitimage_api::{BuildOutcome, GitImage};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Patch, PatchParams, PostParams};

const BUILD: Duration = Duration::from_secs(420);

fn dockerfile(msg: &str) -> String {
    format!(
        "FROM docker.io/library/busybox:1.37\nRUN echo '{msg}' > /msg\nCMD [\"sh\", \"-c\", \"cat /msg; sleep 3600\"]\n"
    )
}

/// M1 + M2: a commit is built, an annotated Deployment waits gated, then runs the digest;
/// a new commit rolls it.
#[tokio::test]
#[ignore]
async fn commit_builds_gates_and_rolls() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-flow").await?;
    let gitea = Gitea::default();
    let repo = unique("flow");
    gitea.create_repo(&repo, false).await?;
    let sha1 = gitea
        .commit(&repo, "main", &[("Dockerfile", &dockerfile("one"))], "one")
        .await?;

    // The Deployment comes first: its pod must wait gated, not fail.
    create_deployment(&ns, "web", "app").await?;
    let gated = wait_for("gated pod", Duration::from_secs(60), || async {
        let pods = ns.api::<Pod>().list(&Default::default()).await?;
        Ok(pods.items.into_iter().find(|p| {
            p.spec
                .as_ref()
                .and_then(|s| s.scheduling_gates.as_ref())
                .is_some_and(|g| g.iter().any(|g| g.name == "gitimage.dev/build"))
        }))
    })
    .await?;
    assert_eq!(
        gated.status.and_then(|s| s.phase).as_deref(),
        Some("Pending")
    );

    create_gitimage(
        &ns,
        "app",
        &format!("url: {}\ninterval: 20s", Gitea::https_url(&repo)),
    )
    .await?;
    let gi = wait_built(&ns, "app", &sha1, BUILD).await?;
    let image1 = gi.status.as_ref().and_then(|s| s.image.clone()).unwrap();
    assert!(
        image1.contains(&format!("/{}/app@sha256:", ns.name)),
        "{image1}"
    );

    let img = image1.clone();
    wait_for("pod running built digest", Duration::from_secs(180), || {
        let img = img.clone();
        let ns = &ns;
        async move {
            Ok(running_images(ns, "web")
                .await?
                .contains(&img)
                .then_some(()))
        }
    })
    .await?;

    // A new commit rolls the Deployment.
    let sha2 = gitea
        .commit(&repo, "main", &[("Dockerfile", &dockerfile("two"))], "two")
        .await?;
    let gi = wait_built(&ns, "app", &sha2, BUILD).await?;
    let image2 = gi.status.as_ref().and_then(|s| s.image.clone()).unwrap();
    assert_ne!(image1, image2);
    let d = wait_for(
        "template pinned to sha2",
        Duration::from_secs(60),
        || async {
            let d = ns.api::<Deployment>().get("web").await?;
            let pin = d
                .spec
                .as_ref()
                .and_then(|s| s.template.metadata.as_ref())
                .and_then(|m| m.annotations.as_ref())
                .and_then(|a| a.get("gitimage.dev/revision.web"))
                .cloned();
            Ok((pin.as_deref() == Some(sha2.as_str())).then_some(d))
        },
    )
    .await;
    let _ = d?;
    let img = image2.clone();
    wait_for(
        "pod running second digest",
        Duration::from_secs(180),
        || {
            let img = img.clone();
            let ns = &ns;
            async move {
                let imgs = running_images(ns, "web").await?;
                Ok((imgs == vec![img]).then_some(()))
            }
        },
    )
    .await?;
    Ok(())
}

/// A tag builds once; later reconciles start no further Job.
#[tokio::test]
#[ignore]
async fn tag_builds_once() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-tag").await?;
    let gitea = Gitea::default();
    let repo = unique("tag");
    gitea.create_repo(&repo, false).await?;
    let sha = gitea
        .commit(&repo, "main", &[("Dockerfile", &dockerfile("tag"))], "tag")
        .await?;
    gitea.tag(&repo, "v1.0.0", &sha).await?;
    create_gitimage(
        &ns,
        "app",
        &format!(
            "url: {}\nref: v1.0.0\ninterval: 5s",
            Gitea::https_url(&repo)
        ),
    )
    .await?;
    wait_built(&ns, "app", &sha, BUILD).await?;
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert_eq!(build_jobs(&ns, "app").await?.len(), 1);
    Ok(())
}

/// F7: a commit that does not touch `context` reuses the image (host API path on Gitea).
#[tokio::test]
#[ignore]
async fn change_outside_context_reuses_image() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-mono").await?;
    let gitea = Gitea::default();
    let repo = unique("mono");
    gitea.create_repo(&repo, false).await?;
    let sha1 = gitea
        .commit(
            &repo,
            "main",
            &[
                ("api/Dockerfile", &dockerfile("api")),
                ("web/README.md", "web"),
            ],
            "init",
        )
        .await?;
    create_gitimage(
        &ns,
        "api",
        &format!(
            "url: {}\ninterval: 10s\nbuild: {{ context: api }}",
            Gitea::https_url(&repo)
        ),
    )
    .await?;
    let gi = wait_built(&ns, "api", &sha1, BUILD).await?;
    let digest1 = gi.status.unwrap().builds[0].digest.clone();

    let sha2 = gitea
        .commit(
            &repo,
            "main",
            &[("web/README.md", "web changed")],
            "web only",
        )
        .await?;
    let gi = wait_for("reuse record for sha2", BUILD, || async {
        let gi = ns.api::<GitImage>().get("api").await?;
        let st = gi.status.clone().unwrap_or_default();
        Ok(st.record(&sha2).is_some().then_some(gi))
    })
    .await?;
    let st = gi.status.unwrap();
    let rec = st.record(&sha2).unwrap();
    assert_eq!(rec.outcome, BuildOutcome::Reused, "{rec:?}");
    assert_eq!(rec.digest, digest1);
    assert_eq!(rec.source_revision.as_deref(), Some(sha1.as_str()));
    assert_eq!(st.revision.as_deref(), Some(sha2.as_str()));
    Ok(())
}

/// F2: a push hook triggers a check well before the poll interval.
#[tokio::test]
#[ignore]
async fn hook_triggers_build() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-hook").await?;
    let gitea = Gitea::default();
    let repo = unique("hook");
    gitea.create_repo(&repo, false).await?;
    let sha1 = gitea
        .commit(&repo, "main", &[("Dockerfile", &dockerfile("h1"))], "h1")
        .await?;
    create_gitimage(
        &ns,
        "app",
        &format!("url: {}\ninterval: 1h", Gitea::https_url(&repo)),
    )
    .await?;
    let gi = wait_built(&ns, "app", &sha1, BUILD).await?;
    let url = gi
        .status
        .and_then(|s| s.webhook_url)
        .expect("webhookURL with hooks.baseURL set");
    let sha2 = gitea
        .commit(&repo, "main", &[("Dockerfile", &dockerfile("h2"))], "h2")
        .await?;
    let out = run_pod(
        &ns,
        "hook",
        &format!("wget -q -O- --post-data='{{}}' '{url}' && echo sent"),
    )
    .await?;
    assert!(out.contains("sent"), "{out}");
    wait_built(&ns, "app", &sha2, BUILD).await?;
    Ok(())
}

/// Build secrets reach the RUN step but no layer.
#[tokio::test]
#[ignore]
async fn build_secret_is_not_in_any_layer() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-secret").await?;
    let gitea = Gitea::default();
    let repo = unique("secret");
    gitea.create_repo(&repo, false).await?;
    let secret_value = "s3cr3t-value-that-must-not-leak";
    create_secret(&ns, "npm-token", &[("token", secret_value)]).await?;
    let df = "FROM docker.io/library/busybox:1.37\nRUN --mount=type=secret,id=npmrc test \"$(cat /run/secrets/npmrc)\" = 's3cr3t-value-that-must-not-leak' && echo ok > /used\nCMD [\"sleep\", \"3600\"]\n";
    let sha = gitea
        .commit(&repo, "main", &[("Dockerfile", df)], "secret")
        .await?;
    create_gitimage(
        &ns,
        "app",
        &format!(
            "url: {}\nbuild: {{ secrets: [{{ id: npmrc, secretRef: {{ name: npm-token }} }}] }}",
            Gitea::https_url(&repo)
        ),
    )
    .await?;
    let gi = wait_built(&ns, "app", &sha, BUILD).await?;
    if gi
        .status
        .as_ref()
        .and_then(|s| s.image.as_deref())
        .is_some_and(|i| i.starts_with("kind-registry:5000/"))
    {
        let layers = registry_layers(&format!("builds/{}/app", ns.name), &sha).await?;
        assert!(!layers.is_empty());
        assert!(
            !layers
                .windows(secret_value.len())
                .any(|w| w == secret_value.as_bytes()),
            "secret found in a layer"
        );
    }
    Ok(())
}

/// The guard denies a pod that references another namespace's image.
#[tokio::test]
#[ignore]
async fn cross_tenant_reference_is_denied() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-guard").await?;
    let mut denied = Vec::new();
    for image in [
        "kind-registry:5000/builds/some-other-ns/app@sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "localhost:25050/some-other-ns/app@sha256:0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        let p: Pod = serde_json::from_value(serde_json::json!({
            "metadata": { "generateName": "steal-" },
            "spec": { "containers": [{ "name": "x", "image": image }] }
        }))?;
        let err = ns.api::<Pod>().create(&PostParams::default(), &p).await;
        denied.push(
            matches!(err, Err(kube::Error::Api(ref e)) if e.message.contains("some-other-ns")),
        );
    }
    // One of the two hosts is the active pull host; that one must be denied.
    assert!(denied.iter().any(|d| *d), "{denied:?}");
    Ok(())
}

/// A failing Dockerfile ends as Failed and is retried only by the rebuild annotation.
#[tokio::test]
#[ignore]
async fn failed_build_is_reported_and_rebuild_retries() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-fail").await?;
    let gitea = Gitea::default();
    let repo = unique("fail");
    gitea.create_repo(&repo, false).await?;
    let sha = gitea
        .commit(
            &repo,
            "main",
            &[(
                "Dockerfile",
                "FROM docker.io/library/busybox:1.37\nRUN exit 7\n",
            )],
            "bad",
        )
        .await?;
    create_gitimage(
        &ns,
        "app",
        &format!("url: {}\ninterval: 5s", Gitea::https_url(&repo)),
    )
    .await?;
    let api = ns.api::<GitImage>();
    wait_for("Failed=True BuildFailed", BUILD, || async {
        let gi = api.get("app").await?;
        let st = gi.status.unwrap_or_default();
        Ok(st
            .condition("Failed")
            .filter(|c| c.status == "True" && c.reason == "BuildFailed")
            .map(|_| ()))
    })
    .await?;
    assert_eq!(build_jobs(&ns, "app").await?.len(), 1);
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert_eq!(
        build_jobs(&ns, "app").await?.len(),
        1,
        "no automatic retry of a failing Dockerfile"
    );
    let patch =
        serde_json::json!({ "metadata": { "annotations": { "gitimage.dev/rebuild": "1" } } });
    api.patch("app", &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    wait_for("second build job", Duration::from_secs(60), || async {
        Ok((build_jobs(&ns, "app").await?.len() >= 2).then_some(()))
    })
    .await?;
    let _ = sha;
    Ok(())
}

/// A private repo over ssh: the controller lists refs with russh, the clone container fetches
/// with the same transport, and the host key is checked against known_hosts.
#[tokio::test]
#[ignore]
async fn private_repo_over_ssh() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-ssh").await?;
    let gitea = Gitea::default();
    let repo = unique("ssh");
    gitea.create_repo(&repo, true).await?;
    let sha = gitea
        .commit(&repo, "main", &[("Dockerfile", &dockerfile("ssh"))], "ssh")
        .await?;
    let (identity, known_hosts) = ssh_deploy_key(&repo).await?;
    create_secret(
        &ns,
        "git",
        &[("identity", &identity), ("known_hosts", &known_hosts)],
    )
    .await?;
    create_gitimage(
        &ns,
        "app",
        &format!(
            "url: ssh://git@e2e-gitea:2222/{GITEA_USER}/{repo}.git\nsecretRef: {{ name: git }}"
        ),
    )
    .await?;
    wait_built(&ns, "app", &sha, BUILD).await?;

    // A wrong host key must be refused, never trusted on first use.
    create_secret(&ns, "git-bad", &[("identity", &identity), ("known_hosts", "[e2e-gitea]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl")]).await?;
    create_gitimage(
        &ns,
        "bad",
        &format!(
            "url: ssh://git@e2e-gitea:2222/{GITEA_USER}/{repo}.git\nsecretRef: {{ name: git-bad }}"
        ),
    )
    .await?;
    let api = ns.api::<GitImage>();
    let msg = wait_for(
        "ResolveFailed for a wrong host key",
        Duration::from_secs(60),
        || async {
            let gi = api.get("bad").await?;
            Ok(gi
                .status
                .unwrap_or_default()
                .condition("Failed")
                .filter(|c| c.reason == "ResolveFailed")
                .map(|c| c.message.clone()))
        },
    )
    .await?;
    assert!(msg.to_lowercase().contains("host key"), "{msg}");
    Ok(())
}

/// F6: GC keeps the 5 newest builds and deletes older manifests and tags.
#[tokio::test]
#[ignore]
async fn gc_keeps_five_newest_builds() -> anyhow::Result<()> {
    let client = client().await?;
    let ns = TestNs::new(&client, "e2e-gc").await?;
    let gitea = Gitea::default();
    let repo = unique("gc");
    gitea.create_repo(&repo, false).await?;
    let mut shas = Vec::new();
    for i in 0..7 {
        let sha = gitea
            .commit(
                &repo,
                "main",
                &[("Dockerfile", &dockerfile(&format!("gc{i}")))],
                "gc",
            )
            .await?;
        if i == 0 {
            create_gitimage(
                &ns,
                "app",
                &format!("url: {}\ninterval: 3s", Gitea::https_url(&repo)),
            )
            .await?;
        }
        wait_built(&ns, "app", &sha, BUILD).await?;
        shas.push(sha);
    }
    let gi = ns.api::<GitImage>().get("app").await?;
    let image = gi.status.and_then(|s| s.image).unwrap();
    let ns_name = ns.name.clone();
    if image.starts_with("localhost:") {
        // s3 mode: ask gitimage-node on a kind node.
        let tags = wait_for(
            "GC to drop the 2 oldest tags",
            Duration::from_secs(120),
            || {
                let ns_name = ns_name.clone();
                let oldest = shas[..2].to_vec();
                async move {
                    let out = std::process::Command::new("docker")
                        .args([
                            "exec",
                            "gitimage-e2e-worker",
                            "curl",
                            "-s",
                            &format!("http://localhost:25050/v2/{ns_name}/app/tags/list?n=100"),
                        ])
                        .output()?;
                    let body = String::from_utf8_lossy(&out.stdout).to_string();
                    Ok((!oldest.iter().any(|s| body.contains(s.as_str()))
                        && body.contains("buildcache"))
                    .then_some(body))
                }
            },
        )
        .await?;
        for sha in &shas[2..] {
            assert!(
                tags.contains(sha.as_str()),
                "kept tag {sha} missing: {tags}"
            );
        }
    }
    Ok(())
}

/// The installed isolation's Pod Security level for build pods, from `gitimage-config`.
async fn build_pod_security_level(client: &kube::Client) -> anyhow::Result<&'static str> {
    let cms: kube::Api<k8s_openapi::api::core::v1::ConfigMap> =
        kube::Api::namespaced(client.clone(), "gitimage-system");
    let cm = cms.get("gitimage-config").await?;
    let yaml = cm
        .data
        .unwrap_or_default()
        .remove("config.yaml")
        .unwrap_or_default();
    let cfg = gitimage_api::Config::from_yaml(&yaml)?;
    Ok(cfg.build_policy.isolation.pod_security_level())
}

async fn label_pod_security(client: &kube::Client, ns: &str, level: &str) -> anyhow::Result<()> {
    let nss: kube::Api<k8s_openapi::api::core::v1::Namespace> = kube::Api::all(client.clone());
    let patch = serde_json::json!({ "metadata": { "labels": { "pod-security.kubernetes.io/enforce": level } } });
    nss.patch(ns, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Pod Security: a build that cannot create its pod fails fast with the reason, instead of
/// waiting for the build deadline. The namespace gets the level just below what the configured
/// isolation needs (`baseline` for `privileged`, `restricted` for `baseline`).
#[tokio::test]
#[ignore]
async fn pod_security_rejection_fails_fast() -> anyhow::Result<()> {
    let client = client().await?;
    let needed = build_pod_security_level(&client).await?;
    let stricter = if needed == "baseline" {
        "restricted"
    } else {
        "baseline"
    };
    let ns = TestNs::new(&client, "e2e-psa").await?;
    label_pod_security(&client, &ns.name, stricter).await?;
    let gitea = Gitea::default();
    let repo = unique("psa");
    gitea.create_repo(&repo, false).await?;
    gitea
        .commit(&repo, "main", &[("Dockerfile", &dockerfile("psa"))], "psa")
        .await?;
    create_gitimage(&ns, "app", &format!("url: {}", Gitea::https_url(&repo))).await?;
    let api = ns.api::<GitImage>();
    let msg = wait_for("PodCreationFailed", Duration::from_secs(90), || async {
        let gi = api.get("app").await?;
        Ok(gi
            .status
            .unwrap_or_default()
            .condition("Failed")
            .filter(|c| c.reason == "PodCreationFailed")
            .map(|c| c.message.clone()))
    })
    .await?;
    assert!(
        msg.contains(&format!("pod-security.kubernetes.io/enforce={needed}")),
        "{msg}"
    );
    Ok(())
}

/// Isolations documented as Pod Security `baseline` (Sysbox, `rootfulInSandbox`) build in a
/// namespace that enforces `baseline`. A no-op for isolations that need `privileged`.
#[tokio::test]
#[ignore]
async fn baseline_isolation_builds_under_baseline() -> anyhow::Result<()> {
    let client = client().await?;
    if build_pod_security_level(&client).await? != "baseline" {
        return Ok(());
    }
    let ns = TestNs::new(&client, "e2e-baseline").await?;
    label_pod_security(&client, &ns.name, "baseline").await?;
    let gitea = Gitea::default();
    let repo = unique("baseline");
    gitea.create_repo(&repo, false).await?;
    let sha = gitea
        .commit(
            &repo,
            "main",
            &[("Dockerfile", &dockerfile("baseline"))],
            "baseline",
        )
        .await?;
    create_gitimage(&ns, "app", &format!("url: {}", Gitea::https_url(&repo))).await?;
    wait_built(&ns, "app", &sha, BUILD).await?;
    Ok(())
}
