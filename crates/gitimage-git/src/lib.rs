//! Git access for GitImage: ref resolution (pure), credentials from Flux-format Secrets,
//! and the `GitClient` trait with its gix implementation. No `git` or `ssh` subprocess.

mod cli;
mod credentials;
#[cfg(test)]
mod gitea_it;
mod gix_client;
mod host_api;
mod refs;
mod ssh;
#[cfg(test)]
mod sshd_it;

pub use cli::{CloneArgs, run_clone_cli};
pub use credentials::{Credentials, CredentialsError};
pub use gix_client::GixClient;

pub use refs::{RemoteRef, ResolveError, Resolved, is_full_sha, ls_refs_prefixes, resolve};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("remote: {0}")]
    Remote(String),
    #[error("local: {0}")]
    Local(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// Everything the controller and `gitimage clone` need from git.
pub trait GitClient: Send + Sync {
    /// Lists the refs that could match `wanted` (protocol v2 `ls-refs`, prefix-filtered).
    /// Returns nothing for a full sha: there is nothing to list.
    fn list_refs(
        &self,
        url: &str,
        creds: &Credentials,
        wanted: &str,
    ) -> impl std::future::Future<Output = Result<Vec<RemoteRef>, GitError>> + Send;

    /// Fetches `sha` at depth 1 into a new repo at `dest` and reads the git tree id of
    /// `context` (repo-relative, "." = root). If it equals `skip_if_tree`, returns
    /// `unchanged: true` without checking out. Otherwise checks the commit's tree out into
    /// `dest` (worktree files only; `.git` may be removed after).
    fn clone_commit(
        &self,
        req: CloneRequest,
    ) -> impl std::future::Future<Output = Result<CloneOutcome, GitError>> + Send;

    /// Tree id of `context` at `sha` through the git host's HTTP API (GitHub, GitLab,
    /// Gitea/Forgejo) with https credentials, without cloning. `Ok(None)` when the host is
    /// unknown, the URL is ssh, or the API is unavailable: the caller then falls back to the
    /// in-Job check (`clone_commit` with `skip_if_tree`).
    fn context_tree_via_host(
        &self,
        url: &str,
        creds: &Credentials,
        sha: &str,
        context: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, GitError>> + Send;
}

#[derive(Clone, Debug)]
pub struct CloneRequest {
    pub url: String,
    pub creds: Credentials,
    pub sha: String,
    pub dest: std::path::PathBuf,
    pub context: String,
    pub skip_if_tree: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneOutcome {
    /// Git tree id of `context` at `sha`.
    pub context_tree: String,
    pub unchanged: bool,
}
