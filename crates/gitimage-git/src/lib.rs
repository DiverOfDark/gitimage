//! Git access for GitImage: ref resolution (pure), credentials from Flux-format Secrets,
//! and the `GitClient` trait with its gix implementation. No `git` or `ssh` subprocess.

mod credentials;
mod gix_client;
mod refs;

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
}
