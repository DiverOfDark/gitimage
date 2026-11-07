//! End-to-end against a throwaway Gitea container (`hack/gitea-fixture.sh`): ssh (ed25519,
//! ecdsa, rsa identities; plain and hashed known_hosts) and https, `list_refs`,
//! `clone_commit`, the skip path, the host API and `run_clone_cli`.
//!
//! `cargo test -p gitimage-git -- --ignored gitea` (needs podman or docker, rootless is fine,
//! plus `git`, `openssl`, `ssh-keygen`, `ssh-keyscan` and `curl` on the host; skips without a
//! container runtime).
//!
//! gix never sends credentials over plain http, so Gitea serves https with a throwaway CA. The
//! https half runs in a child test process with `SSL_CERT_FILE` pointing at that CA (the
//! platform verifier reads it at first use; the parent can't set env vars: no `unsafe`).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::host_api::HostApi;
use crate::{CloneArgs, CloneRequest, Credentials, GitClient, GixClient, run_clone_cli};

const CHILD_ENV: &str = "GITIMAGE_GITEA_FIXTURE_ENV";

struct Fixture {
    /// Set when this process started the container (and removes it on drop).
    dir: Option<tempfile::TempDir>,
    env: HashMap<String, String>,
}

fn read_env(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

impl Fixture {
    fn script() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hack/gitea-fixture.sh")
    }

    fn up() -> Option<Self> {
        let have = |b: &str| {
            Command::new("sh")
                .args(["-c", &format!("command -v {b}")])
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !have("podman") && !have("docker") {
            eprintln!("SKIP: no podman or docker");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = Command::new(Self::script())
            .args(["up", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        let env = dir.path().join("env");
        let mut f = Self {
            env: HashMap::new(),
            dir: Some(dir),
        };
        assert!(
            out.status.success(),
            "fixture: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        f.env = read_env(&env);
        Some(f)
    }

    fn get(&self, k: &str) -> &str {
        &self.env[k]
    }

    fn ssh_url(&self) -> String {
        format!(
            "ssh://git@127.0.0.1:{}/{}.git",
            self.get("SSH_PORT"),
            self.get("REPO")
        )
    }

    fn https_url(&self) -> String {
        format!("{}/{}.git", self.get("HTTP"), self.get("REPO"))
    }

    fn ssh_creds(&self, key: &str, known_hosts: &str) -> Credentials {
        Credentials::Ssh {
            identity: std::fs::read_to_string(key).unwrap(),
            known_hosts: std::fs::read_to_string(self.get(known_hosts)).unwrap(),
        }
    }

    fn https_creds(&self) -> Credentials {
        Credentials::Https {
            username: self.get("USER").into(),
            password: self.get("PASSWORD").into(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = Command::new(Self::script())
                .args(["down", dir.path().to_str().unwrap()])
                .status();
        }
    }
}

fn assert_checkout(dest: &Path) {
    assert!(!dest.join(".git").exists(), "no git metadata");
    assert_eq!(
        std::fs::read_to_string(dest.join("README.md")).unwrap(),
        "hello\n"
    );
    // first commit's content, not the tip's
    assert_eq!(
        std::fs::read_to_string(dest.join("docs/index.md")).unwrap(),
        "docs\n"
    );
    let run = dest.join("services/api/bin/run.sh");
    assert_ne!(
        std::fs::metadata(&run).unwrap().permissions().mode() & 0o111,
        0
    );
    let docs = std::fs::metadata(dest.join("README.md"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(docs & 0o111, 0);
    assert_eq!(
        std::fs::read_link(dest.join("services/api/run")).unwrap(),
        Path::new("bin/run.sh")
    );
}

#[tokio::test]
#[ignore]
async fn gitea_end_to_end() {
    let Some(fx) = Fixture::up() else { return };
    let client = GixClient;
    let sha = fx.get("SHA").to_owned();
    let tip = fx.get("TIP").to_owned();
    let context_tree = fx.get("CONTEXT_TREE").to_owned();
    let keys: Vec<String> = fx.get("KEYS").split(' ').map(str::to_owned).collect();

    // ls-refs over ssh with every identity type, plain and hashed known_hosts
    for key in &keys {
        for kh in ["KNOWN_HOSTS", "KNOWN_HOSTS_HASHED"] {
            let refs = client
                .list_refs(&fx.ssh_url(), &fx.ssh_creds(key, kh), "main")
                .await
                .unwrap_or_else(|e| panic!("ssh ls-refs with {key} / {kh}: {e}"));
            let r = crate::resolve(&refs, "main").unwrap();
            assert_eq!(r.sha, tip, "{key} {kh}");
        }
    }
    // (scp-like URLs can't carry a port; their parsing is unit-tested.)

    // host key checks: unknown host and mismatched key are rejected
    let ed = &keys[0];
    let ssh_with_known = |known_hosts: String| Credentials::Ssh {
        identity: std::fs::read_to_string(ed).unwrap(),
        known_hosts,
    };
    let host_key = std::fs::read_to_string(fx.get("KNOWN_HOSTS")).unwrap();
    let host_key = host_key.lines().find(|l| !l.starts_with('#')).unwrap();
    let (_, key) = host_key.split_once(' ').unwrap();
    // the right key, but for another host / port
    for other in [format!("github.com {key}"), format!("127.0.0.1 {key}")] {
        let err = client
            .list_refs(&fx.ssh_url(), &ssh_with_known(other), "main")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not in known_hosts"), "{err}");
    }
    // the right host, another key of the same type (a user key posing as host key)
    let rsa_pub = std::fs::read_to_string(format!("{}.pub", keys[2])).unwrap();
    let err = client
        .list_refs(
            &fx.ssh_url(),
            &ssh_with_known(format!("[127.0.0.1]:{} {rsa_pub}", fx.get("SSH_PORT"))),
            "main",
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("does not match known_hosts"),
        "{err}"
    );
    // an identity the server does not know
    let stranger = tempfile::tempdir().unwrap();
    let stranger_key = stranger.path().join("id");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&stranger_key)
            .status()
            .unwrap()
            .success()
    );
    let err = client
        .list_refs(
            &fx.ssh_url(),
            &fx.ssh_creds(stranger_key.to_str().unwrap(), "KNOWN_HOSTS"),
            "main",
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("rejected the identity"), "{err}");

    // clone over ssh (rsa identity): a non-tip commit, nested context
    let work = tempfile::tempdir().unwrap();
    check_clones(
        &fx,
        &fx.ssh_url(),
        &fx.ssh_creds(&keys[2], "KNOWN_HOSTS"),
        work.path(),
    )
    .await;

    // the init container entry point, credentials from a mounted-secret directory
    let secret = work.path().join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::copy(&keys[1], secret.join("identity")).unwrap();
    std::fs::copy(fx.get("KNOWN_HOSTS"), secret.join("known_hosts")).unwrap();
    let log = work.path().join("termination-log");
    let args = CloneArgs {
        url: fx.ssh_url(),
        sha: sha.clone(),
        dest: work.path().join("cli"),
        context: "services/api".into(),
        skip_if_tree: None,
        credentials_dir: Some(secret.clone()),
        termination_log: log.clone(),
    };
    assert_eq!(run_clone_cli(args.clone()).await.unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("cloned {context_tree}")
    );
    assert_checkout(&args.dest);
    let code = run_clone_cli(CloneArgs {
        dest: work.path().join("cli2"),
        skip_if_tree: Some(context_tree.clone()),
        ..args
    })
    .await
    .unwrap();
    assert_eq!(code, gitimage_api::keys::CLONE_EXIT_UNCHANGED);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("unchanged {context_tree}")
    );

    // the https half, in a child process trusting the fixture's CA
    let env_file = fx.dir.as_ref().unwrap().path().join("env");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "gitea_it::gitea_https_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_ENV, &env_file)
        .env("SSL_CERT_FILE", fx.get("CA"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "https child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs only when spawned by `gitea_end_to_end`.
#[tokio::test]
#[ignore]
async fn gitea_https_child() {
    let Some(env) = std::env::var_os(CHILD_ENV) else {
        eprintln!("SKIP: only runs as a child of gitea_end_to_end");
        return;
    };
    let fx = Fixture {
        dir: None,
        env: read_env(Path::new(&env)),
    };
    let client = GixClient;
    let refs = client
        .list_refs(&fx.https_url(), &fx.https_creds(), "main")
        .await
        .unwrap();
    assert_eq!(crate::resolve(&refs, "main").unwrap().sha, fx.get("TIP"));
    // wrong password is an error, not an empty result
    let bad = Credentials::Https {
        username: fx.get("USER").into(),
        password: "wrong".into(),
    };
    assert!(
        client
            .list_refs(&fx.https_url(), &bad, "main")
            .await
            .is_err()
    );

    let work = tempfile::tempdir().unwrap();
    check_clones(&fx, &fx.https_url(), &fx.https_creds(), work.path()).await;

    // host API (Gitea), token auth
    let api = HostApi::new(
        HostApi::client_builder().build().unwrap(),
        "https://unused.invalid",
        false,
    );
    let token = Credentials::Https {
        username: fx.get("USER").into(),
        password: fx.get("TOKEN").into(),
    };
    let sha = fx.get("SHA");
    assert_eq!(
        api.context_tree(&fx.https_url(), &token, sha, "services/api")
            .await,
        Ok(Some(fx.get("CONTEXT_TREE").to_owned()))
    );
    assert_eq!(
        api.context_tree(&fx.https_url(), &token, sha, ".").await,
        Ok(Some(fx.get("ROOT_TREE").to_owned()))
    );
    // private repo without a token: unavailable, not a wrong answer
    assert!(
        api.context_tree(&fx.https_url(), &Credentials::None, sha, ".")
            .await
            .is_err()
    );
}

/// Clone with a nested context (checked out), the skip path, the root context and a
/// missing context.
async fn check_clones(fx: &Fixture, url: &str, creds: &Credentials, work: &Path) {
    let client = GixClient;
    let context_tree = fx.get("CONTEXT_TREE").to_owned();
    let req = CloneRequest {
        url: url.to_owned(),
        creds: creds.clone(),
        sha: fx.get("SHA").into(),
        dest: work.join("clone"),
        context: "services/api".into(),
        skip_if_tree: Some("0000000000000000000000000000000000000000".into()),
    };
    let out = client
        .clone_commit(req.clone())
        .await
        .unwrap_or_else(|e| panic!("clone {url}: {e}"));
    assert_eq!(out.context_tree, context_tree, "{url}");
    assert!(!out.unchanged);
    assert_checkout(&req.dest);

    let skip_dest = work.join("skip");
    let out = client
        .clone_commit(CloneRequest {
            dest: skip_dest.clone(),
            skip_if_tree: Some(context_tree.clone()),
            ..req.clone()
        })
        .await
        .unwrap();
    assert!(out.unchanged);
    assert_eq!(std::fs::read_dir(&skip_dest).unwrap().count(), 0);

    let root = client
        .clone_commit(CloneRequest {
            dest: work.join("root"),
            context: ".".into(),
            skip_if_tree: Some(fx.get("ROOT_TREE").into()),
            ..req.clone()
        })
        .await
        .unwrap();
    assert_eq!(root.context_tree, fx.get("ROOT_TREE"));
    assert!(root.unchanged);

    let err = client
        .clone_commit(CloneRequest {
            dest: work.join("missing"),
            context: "services/nope".into(),
            ..req
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("services/nope"), "{err}");
    assert!(!work.join("missing/.git").exists());
}
