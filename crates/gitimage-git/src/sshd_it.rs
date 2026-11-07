//! The russh transport against a real OpenSSH `sshd`, run unprivileged on a random port:
//! protocol v2 via the `GIT_PROTOCOL` env request (`AcceptEnv`) and the v1 fallback when
//! the server ignores it (OpenSSH's default), host-key algorithm selection from known_hosts,
//! and the remote's stderr surfacing in errors.
//!
//! `cargo test -p gitimage-git -- --ignored sshd` (needs `sshd`, `ssh-keygen`, `git`; skips
//! when `sshd` is missing).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{CloneRequest, Credentials, GitClient, GixClient};

struct Sshd {
    pid_file: PathBuf,
    port: u16,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.pid_file) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

fn sh(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn keygen(t: &str, path: &Path) {
    sh(Command::new("ssh-keygen")
        .args(["-q", "-t", t, "-N", ""])
        .arg("-f")
        .arg(path));
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_sshd(sshd: &Path, w: &Path, name: &str, accept_env: bool) -> Sshd {
    let port = free_port();
    let pid_file = w.join(format!("{name}.pid"));
    let config = w.join(format!("{name}.conf"));
    let d = w.display();
    std::fs::write(
        &config,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_ed25519\nHostKey {d}/host_ecdsa\n\
             HostKey {d}/host_rsa\nAuthorizedKeysFile {d}/authorized_keys\nStrictModes no\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\n\
             PidFile {}\n{}",
            pid_file.display(),
            if accept_env { "AcceptEnv GIT_PROTOCOL\n" } else { "" }
        ),
    )
    .unwrap();
    sh(Command::new(sshd)
        .arg("-f")
        .arg(&config)
        .arg("-E")
        .arg(w.join(format!("{name}.log"))));
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() && pid_file.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Sshd { pid_file, port }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn sshd_end_to_end() {
    let Some(sshd) = ["/usr/sbin/sshd", "/usr/bin/sshd"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
    else {
        eprintln!("SKIP: no sshd");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let w = tmp.path();
    for t in ["ed25519", "ecdsa", "rsa"] {
        keygen(t, &w.join(format!("host_{t}")));
    }
    keygen("ecdsa", &w.join("user"));
    // A forced command records what the client asked for, then runs it.
    let log = w.join("requests.log");
    let wrap = w.join("wrap.sh");
    std::fs::write(
        &wrap,
        format!(
            "#!/bin/sh\necho \"proto=$GIT_PROTOCOL cmd=$SSH_ORIGINAL_COMMAND\" >> {}\nexec sh -c \"$SSH_ORIGINAL_COMMAND\"\n",
            log.display()
        ),
    )
    .unwrap();
    sh(Command::new("chmod").arg("+x").arg(&wrap));
    let user_pub = std::fs::read_to_string(w.join("user.pub")).unwrap();
    std::fs::write(
        w.join("authorized_keys"),
        format!("command=\"{}\" {user_pub}", wrap.display()),
    )
    .unwrap();

    // repo: two commits, the first has an exec bit
    let src = w.join("src");
    std::fs::create_dir_all(src.join("app")).unwrap();
    std::fs::write(src.join("app/run.sh"), "#!/bin/sh\n").unwrap();
    sh(Command::new("chmod").arg("+x").arg(src.join("app/run.sh")));
    let git = |args: &[&str]| {
        sh(Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-C"])
            .arg(&src)
            .args(args))
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "one"]);
    let sha = git(&["rev-parse", "HEAD"]);
    let tree = git(&["rev-parse", "HEAD:app"]);
    std::fs::write(src.join("b"), "b").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "two"]);
    let tip = git(&["rev-parse", "HEAD"]);
    let repo = w.join("repo.git");
    sh(Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(&src)
        .arg(&repo));
    // let upload-pack serve a non-tip commit to v0/v1 clients too (v2 allows it by default)
    sh(Command::new("git").arg("-C").arg(&repo).args([
        "config",
        "uploadpack.allowAnySHA1InWant",
        "true",
    ]));

    let v2 = start_sshd(&sshd, w, "v2", true);
    let v1 = start_sshd(&sshd, w, "v1", false);
    let user = sh(Command::new("id").arg("-un"));
    let identity = std::fs::read_to_string(w.join("user")).unwrap();
    let host_pub = |t: &str| std::fs::read_to_string(w.join(format!("host_{t}.pub"))).unwrap();
    let client = GixClient;

    for (server, expect_proto) in [(&v2, "version=2"), (&v1, "")] {
        let url = format!("ssh://{user}@127.0.0.1:{}{}", server.port, repo.display());
        // known_hosts with only one key type: that's the one negotiated
        for t in ["ecdsa", "rsa", "ed25519"] {
            let creds = Credentials::Ssh {
                identity: identity.clone(),
                known_hosts: format!("[127.0.0.1]:{} {}", server.port, host_pub(t)),
            };
            let _ = std::fs::remove_file(&log);
            let refs = client
                .list_refs(&url, &creds, "main")
                .await
                .unwrap_or_else(|e| panic!("{url} with {t} host key: {e}"));
            assert_eq!(crate::resolve(&refs, "main").unwrap().sha, tip);
            let seen = std::fs::read_to_string(&log).unwrap();
            assert_eq!(
                seen.trim(),
                format!(
                    "proto={expect_proto} cmd=git-upload-pack '{}'",
                    repo.display()
                )
            );
        }
        let creds = Credentials::Ssh {
            identity: identity.clone(),
            known_hosts: format!("[127.0.0.1]:{} {}", server.port, host_pub("ed25519")),
        };
        let dest = w.join(format!("clone-{}", server.port));
        let out = client
            .clone_commit(CloneRequest {
                url: url.clone(),
                creds: creds.clone(),
                sha: sha.clone(),
                dest: dest.clone(),
                context: "app".into(),
                skip_if_tree: None,
            })
            .await
            .unwrap_or_else(|e| panic!("clone {url}: {e}"));
        assert_eq!(out.context_tree, tree);
        assert!(dest.join("app/run.sh").is_file());
        assert!(!dest.join("b").exists(), "first commit, not the tip");
        assert!(!dest.join(".git").exists());

        // the remote's stderr makes it into the error
        let missing = format!(
            "ssh://{user}@127.0.0.1:{}{}/nope.git",
            server.port,
            w.display()
        );
        let err = client
            .list_refs(&missing, &creds, "main")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("not appear to be a git repository"),
            "{err}"
        );
    }
}
