//! `GitClient` on gix: protocol v2 `ls-refs` and a depth-1 fetch of one commit, over https
//! (gix's reqwest transport) or ssh (our russh transport, design D3).

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::credentials::helper::{Action, NextAction};
use gix::credentials::protocol::Outcome;
use gix::protocol::handshake::Ref;
use gix::protocol::transport::client::blocking_io::Transport;
use gix::remote::Direction;
use gix::remote::fetch::{Shallow, Tags};
use tokio::runtime::Handle;

use crate::ssh::{SshTransport, is_ssh};
use crate::{
    CloneOutcome, CloneRequest, Credentials, GitClient, GitError, RemoteRef, is_full_sha,
    refs::ls_refs_prefixes,
};

#[derive(Clone, Debug, Default)]
pub struct GixClient;

impl GitClient for GixClient {
    async fn list_refs(
        &self,
        url: &str,
        creds: &Credentials,
        wanted: &str,
    ) -> Result<Vec<RemoteRef>, GitError> {
        let url = url.to_string();
        let creds = creds.clone();
        let wanted = wanted.to_string();
        let rt = Handle::current();
        tokio::task::spawn_blocking(move || list_refs_blocking(rt, &url, &creds, &wanted))
            .await
            .map_err(|e| GitError::Remote(format!("ls-refs task: {e}")))?
    }

    async fn clone_commit(&self, req: CloneRequest) -> Result<CloneOutcome, GitError> {
        let rt = Handle::current();
        tokio::task::spawn_blocking(move || clone_blocking(rt, req))
            .await
            .map_err(|e| GitError::Local(format!("clone task: {e}")))?
    }

    async fn context_tree_via_host(
        &self,
        url: &str,
        creds: &Credentials,
        sha: &str,
        context: &str,
    ) -> Result<Option<String>, GitError> {
        Ok(crate::host_api::context_tree(url, creds, sha, context).await)
    }
}

/// Error text with its whole source chain: gix's top-level messages alone are often generic.
pub(crate) fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let msg = s.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        cur = s.source();
    }
    out
}

fn remote_err(e: impl std::error::Error + 'static) -> GitError {
    GitError::Remote(chain(&e))
}

fn local_err(e: impl std::error::Error + 'static) -> GitError {
    GitError::Local(chain(&e))
}

type BoxedTransport = Box<dyn Transport + Send>;

/// Connects `remote` over ssh (russh) or https (gix/reqwest, with a credentials callback
/// answering from `creds`).
fn connect<'r, 'repo>(
    rt: Handle,
    remote: &'r gix::Remote<'repo>,
    url: &str,
    creds: &Credentials,
) -> Result<gix::remote::Connection<'r, 'static, 'repo, BoxedTransport>, GitError> {
    if is_ssh(url) {
        let Credentials::Ssh {
            identity,
            known_hosts,
        } = creds
        else {
            return Err(GitError::Local(format!(
                "{url} is an ssh url: the git secret needs `identity` and `known_hosts`"
            )));
        };
        let transport = SshTransport::connect(rt, url, identity, known_hosts)?;
        return Ok(remote.to_connection_with_transport(Box::new(transport) as BoxedTransport));
    }
    if matches!(creds, Credentials::Ssh { .. }) {
        return Err(GitError::Local(format!(
            "{url} is not an ssh url but the git secret holds ssh credentials"
        )));
    }
    let connection = remote.connect(Direction::Fetch).map_err(remote_err)?;
    let account = match creds {
        Credentials::Https { username, password } => Some((username.clone(), password.clone())),
        _ => None,
    };
    Ok(
        connection.with_credentials(move |action| match (action, &account) {
            (Action::Get(ctx), Some((username, password))) => Ok(Some(Outcome {
                identity: gix::sec::identity::Account {
                    username: username.clone(),
                    password: password.clone(),
                    oauth_refresh_token: None,
                },
                next: NextAction::from(ctx),
            })),
            _ => Ok(None),
        }),
    )
}

fn list_refs_blocking(
    rt: Handle,
    url: &str,
    creds: &Credentials,
    wanted: &str,
) -> Result<Vec<RemoteRef>, GitError> {
    let prefixes = ls_refs_prefixes(wanted);
    if prefixes.is_empty() {
        return Ok(Vec::new());
    }
    let scratch = tempfile::tempdir().map_err(|e| GitError::Local(e.to_string()))?;
    let repo = gix::init_bare(scratch.path()).map_err(local_err)?;
    let specs: Vec<String> = prefixes.iter().map(|p| format!("+{p}:{p}")).collect();
    let remote = repo
        .remote_at(url)
        .map_err(remote_err)?
        .with_fetch_tags(Tags::None)
        .with_refspecs(specs.iter().map(String::as_str), Direction::Fetch)
        .map_err(remote_err)?;
    let connection = connect(rt, &remote, url, creds)?;
    let (map, _handshake) = connection
        .ref_map(gix::progress::Discard, Default::default())
        .map_err(remote_err)?;

    Ok(map
        .remote_refs
        .iter()
        .filter_map(|r| match r {
            Ref::Peeled {
                full_ref_name,
                object,
                ..
            }
            | Ref::Direct {
                full_ref_name,
                object,
            }
            | Ref::Symbolic {
                full_ref_name,
                object,
                ..
            } => Some(RemoteRef {
                name: full_ref_name.to_string(),
                commit: object.to_string(),
            }),
            Ref::Unborn { .. } => None,
        })
        .collect())
}

/// Repo-relative context path components; `.`/`""` is the root. `..` is rejected.
pub(crate) fn context_components(context: &str) -> Result<Vec<&str>, GitError> {
    let mut out = Vec::new();
    for c in context.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                return Err(GitError::Local(format!(
                    "context {context:?} must not contain '..'"
                )));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

fn clone_blocking(rt: Handle, req: CloneRequest) -> Result<CloneOutcome, GitError> {
    if !is_full_sha(&req.sha) {
        return Err(GitError::Local(format!(
            "clone needs a full commit sha, got {:?}",
            req.sha
        )));
    }
    let sha = req.sha.to_ascii_lowercase();
    let components = context_components(&req.context)?;
    std::fs::create_dir_all(&req.dest)
        .map_err(|e| GitError::Local(format!("create {}: {e}", req.dest.display())))?;
    if req.dest.join(".git").exists() {
        return Err(GitError::Local(format!(
            "{} already contains .git",
            req.dest.display()
        )));
    }

    let result = fetch_and_checkout(rt, &req, &sha, &components);
    // The build context gets files only, never git metadata (also on errors).
    remove_git_dir(&req.dest)?;
    result
}

fn remove_git_dir(dest: &Path) -> Result<(), GitError> {
    match std::fs::remove_dir_all(dest.join(".git")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(GitError::Local(format!(
            "remove {}: {e}",
            dest.join(".git").display()
        ))),
    }
}

fn fetch_and_checkout(
    rt: Handle,
    req: &CloneRequest,
    sha: &str,
    components: &[&str],
) -> Result<CloneOutcome, GitError> {
    let repo = gix::init(&req.dest).map_err(local_err)?;
    // An object-id refspec: `want <sha>` (protocol v2 servers allow reachable commits). A dummy
    // extra refspec keeps ls-refs from listing every ref of the remote.
    let remote = repo
        .remote_at(req.url.as_str())
        .map_err(remote_err)?
        .with_fetch_tags(Tags::None)
        .with_refspecs([sha], Direction::Fetch)
        .map_err(remote_err)?;
    let connection = connect(rt, &remote, &req.url, &req.creds)?;
    let options = gix::remote::ref_map::Options {
        extra_refspecs: vec![
            gix::refspec::parse(
                "refs/gitimage/none".into(),
                gix::refspec::parse::Operation::Fetch,
            )
            .map_err(local_err)?
            .to_owned(),
        ],
        ..Default::default()
    };
    let one = NonZeroU32::new(1).expect("1 is non-zero");
    let interrupt = AtomicBool::new(false);
    connection
        .prepare_fetch(gix::progress::Discard, options)
        .map_err(remote_err)?
        .with_shallow(Shallow::DepthAtRemote(one))
        .receive(gix::progress::Discard, &interrupt)
        .map_err(|e| GitError::Remote(format!("fetch {sha}: {}", chain(&e))))?;

    let id = gix::ObjectId::from_hex(sha.as_bytes()).map_err(local_err)?;
    let commit = repo
        .find_object(id)
        .map_err(|e| GitError::Remote(format!("commit {sha} not received: {}", chain(&e))))?
        .try_into_commit()
        .map_err(|_| GitError::Remote(format!("{sha} is not a commit")))?;
    let root = commit.tree_id().map_err(local_err)?.detach();
    let context_tree = context_tree_id(&repo, root, components, &req.context)?;

    if req.skip_if_tree.as_deref() == Some(context_tree.as_str()) {
        tracing::info!(sha, context = %req.context, tree = %context_tree, "context tree unchanged, not checking out");
        return Ok(CloneOutcome {
            context_tree,
            unchanged: true,
        });
    }
    checkout(&repo, root)?;
    Ok(CloneOutcome {
        context_tree,
        unchanged: false,
    })
}

fn context_tree_id(
    repo: &gix::Repository,
    root: gix::ObjectId,
    components: &[&str],
    context: &str,
) -> Result<String, GitError> {
    if components.is_empty() {
        return Ok(root.to_string());
    }
    let tree = repo.find_tree(root).map_err(local_err)?;
    let entry = tree
        .lookup_entry_by_path(components.join("/"))
        .map_err(local_err)?
        .ok_or_else(|| {
            GitError::Local(format!("context {context:?} does not exist in the commit"))
        })?;
    if !entry.mode().is_tree() {
        return Err(GitError::Local(format!(
            "context {context:?} is not a directory in the commit"
        )));
    }
    Ok(entry.object_id().to_string())
}

fn checkout(repo: &gix::Repository, root: gix::ObjectId) -> Result<(), GitError> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| GitError::Local("repository has no worktree".into()))?
        .to_owned();
    let mut index = repo.index_from_tree(&root).map_err(local_err)?;
    for entry in index.entries() {
        if entry.mode == gix::index::entry::Mode::COMMIT {
            tracing::warn!(path = %entry.path(&index), "skipping submodule (gitlink): submodules are not supported");
        }
    }
    let mut opts = repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .map_err(local_err)?;
    opts.destination_is_initially_empty = true;
    let objects = repo
        .objects
        .clone()
        .into_arc()
        .map_err(|e| GitError::Local(format!("object database: {e}")))?;
    let interrupt = AtomicBool::new(false);
    let outcome = gix::worktree::state::checkout(
        &mut index,
        workdir,
        objects,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &interrupt,
        opts,
    )
    .map_err(local_err)?;
    if let Some(e) = outcome.errors.first() {
        return Err(GitError::Local(format!(
            "checkout of {} failed: {}",
            e.path,
            chain(&e.error)
        )));
    }
    if let Some(c) = outcome.collisions.first() {
        return Err(GitError::Local(format!(
            "checkout collision at {} (case-insensitive filesystem?)",
            c.path
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_paths() {
        assert!(context_components(".").unwrap().is_empty());
        assert!(context_components("").unwrap().is_empty());
        assert!(context_components("./").unwrap().is_empty());
        assert_eq!(
            context_components("./services/api/").unwrap(),
            vec!["services", "api"]
        );
        assert_eq!(context_components("/a//b").unwrap(), vec!["a", "b"]);
        assert!(context_components("a/../b").is_err());
    }

    /// Needs network. `cargo test -p gitimage-git -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn lists_and_resolves_a_public_repo() {
        let client = GixClient;
        let url = "https://github.com/moby/buildkit.git";
        let tag = client
            .list_refs(url, &Credentials::None, "v0.34.2")
            .await
            .unwrap();
        let resolved = crate::resolve(&tag, "v0.34.2").unwrap();
        assert!(!resolved.movable);
        // annotated tag: the peeled commit, not the tag object a1b68eb
        assert_eq!(resolved.sha, "8c579433824224dc64a158cf8734495b6e2b186a");
        let master = client
            .list_refs(url, &Credentials::None, "master")
            .await
            .unwrap();
        assert!(crate::resolve(&master, "master").unwrap().movable);
    }

    /// Needs network (one depth-1 fetch of moby/buildkit, twice).
    /// `cargo test -p gitimage-git -- --ignored clones_a_public_commit`
    #[tokio::test]
    #[ignore]
    async fn clones_a_public_commit() {
        use std::os::unix::fs::PermissionsExt;
        let sha = "8c579433824224dc64a158cf8734495b6e2b186a";
        let dir = tempfile::tempdir().unwrap();
        let req = CloneRequest {
            url: "https://github.com/moby/buildkit.git".into(),
            creds: Credentials::None,
            sha: sha.into(),
            dest: dir.path().join("src"),
            context: "frontend/dockerfile".into(),
            skip_if_tree: None,
        };
        let out = GixClient.clone_commit(req.clone()).await.unwrap();
        // `git rev-parse <sha>:frontend/dockerfile`
        let tree = "4c5fa36237c369c3e0cfddc1fe9aa3116e018a2c";
        assert_eq!(
            out,
            CloneOutcome {
                context_tree: tree.into(),
                unchanged: false
            }
        );
        let dest = &req.dest;
        assert!(!dest.join(".git").exists());
        assert!(dest.join("go.mod").is_file());
        let script = dest.join("examples/buildctl-daemonless/buildctl-daemonless.sh");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "exec bit kept");
        let link = dest.join("examples/kube-consistent-hash");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let skip = CloneRequest {
            dest: dir.path().join("skip"),
            skip_if_tree: Some(tree.into()),
            ..req
        };
        let out = GixClient.clone_commit(skip.clone()).await.unwrap();
        assert!(out.unchanged);
        assert_eq!(std::fs::read_dir(&skip.dest).unwrap().count(), 0);
    }
}
