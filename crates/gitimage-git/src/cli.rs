//! `gitimage clone`: the build Job's init container. Fetches one commit, checks it out unless
//! the context tree is unchanged, and reports the tree id in the termination log.

use std::path::PathBuf;

use crate::{CloneRequest, Credentials, GitClient, GixClient};

#[derive(Debug, Clone, clap::Args)]
pub struct CloneArgs {
    /// Repository URL (https or ssh).
    #[arg(long)]
    pub url: String,
    /// Full commit sha to fetch.
    #[arg(long)]
    pub sha: String,
    /// Directory to check the commit out into.
    #[arg(long)]
    pub dest: PathBuf,
    /// Build context, repo-relative ("." = root).
    #[arg(long, default_value = ".")]
    pub context: String,
    /// Skip the checkout (exit 3) if the context tree id equals this.
    #[arg(long)]
    pub skip_if_tree: Option<String>,
    /// Directory where the git Secret is mounted (`identity`, `known_hosts`, `username`,
    /// `password` as files).
    #[arg(long)]
    pub credentials_dir: Option<PathBuf>,
    /// Where to write `cloned <tree>` / `unchanged <tree>`.
    #[arg(long, default_value = "/dev/termination-log")]
    pub termination_log: PathBuf,
}

/// Runs the clone; returns the process exit code: 0 when checked out,
/// [`gitimage_api::keys::CLONE_EXIT_UNCHANGED`] when the context tree is unchanged.
pub async fn run_clone_cli(args: CloneArgs) -> anyhow::Result<i32> {
    let creds = match &args.credentials_dir {
        Some(dir) => Credentials::from_dir(dir)?,
        None => Credentials::None,
    };
    let outcome = GixClient
        .clone_commit(CloneRequest {
            url: args.url.clone(),
            creds,
            sha: args.sha.clone(),
            dest: args.dest.clone(),
            context: args.context.clone(),
            skip_if_tree: args.skip_if_tree.clone(),
        })
        .await?;
    let (word, code) = if outcome.unchanged {
        ("unchanged", gitimage_api::keys::CLONE_EXIT_UNCHANGED)
    } else {
        ("cloned", 0)
    };
    let line = format!("{word} {}", outcome.context_tree);
    if let Err(e) = std::fs::write(&args.termination_log, &line) {
        tracing::debug!(path = %args.termination_log.display(), "termination log not written: {e}");
    }
    tracing::info!(sha = %args.sha, context = %args.context, "{line}");
    Ok(code)
}
