//! `GitClient` on gix: protocol v2 `ls-refs`, nothing fetched. https only for now;
//! the russh ssh transport (design D3) plugs in through `to_connection_with_transport`.

use gix::credentials::helper::{Action, NextAction};
use gix::credentials::protocol::Outcome;
use gix::protocol::handshake::Ref;
use gix::remote::Direction;

use crate::{Credentials, GitClient, GitError, RemoteRef, refs::ls_refs_prefixes};

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
        tokio::task::spawn_blocking(move || list_refs_blocking(&url, &creds, &wanted))
            .await
            .map_err(|e| GitError::Remote(format!("ls-refs task: {e}")))?
    }
}

fn is_ssh(url: &str) -> bool {
    url.starts_with("ssh://") || (!url.contains("://") && url.contains('@') && url.contains(':'))
}

fn list_refs_blocking(
    url: &str,
    creds: &Credentials,
    wanted: &str,
) -> Result<Vec<RemoteRef>, GitError> {
    if is_ssh(url) {
        return Err(GitError::Unsupported(
            "ssh transport is not implemented yet (design D3)",
        ));
    }
    let prefixes = ls_refs_prefixes(wanted);
    if prefixes.is_empty() {
        return Ok(Vec::new());
    }
    let scratch = tempfile::tempdir().map_err(|e| GitError::Local(e.to_string()))?;
    let repo = gix::init_bare(scratch.path()).map_err(|e| GitError::Local(e.to_string()))?;
    let specs: Vec<String> = prefixes.iter().map(|p| format!("+{p}:{p}")).collect();
    let remote = repo
        .remote_at(url)
        .map_err(|e| GitError::Remote(e.to_string()))?
        .with_refspecs(specs.iter().map(String::as_str), Direction::Fetch)
        .map_err(|e| GitError::Remote(e.to_string()))?;
    let connection = remote
        .connect(Direction::Fetch)
        .map_err(|e| GitError::Remote(e.to_string()))?;

    let account = match creds {
        Credentials::Https { username, password } => Some((username.clone(), password.clone())),
        _ => None,
    };
    let connection = connection.with_credentials(move |action| match (action, &account) {
        (Action::Get(ctx), Some((username, password))) => Ok(Some(Outcome {
            identity: gix::sec::identity::Account {
                username: username.clone(),
                password: password.clone(),
                oauth_refresh_token: None,
            },
            next: NextAction::from(ctx),
        })),
        _ => Ok(None),
    });

    let (map, _handshake) = connection
        .ref_map(gix::progress::Discard, Default::default())
        .map_err(|e| GitError::Remote(e.to_string()))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ssh_urls() {
        assert!(is_ssh("git@github.com:you/app.git"));
        assert!(is_ssh("ssh://git@host:22/you/app.git"));
        assert!(!is_ssh("https://github.com/you/app.git"));
        assert!(!is_ssh("https://user@github.com/you/app.git"));
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
        assert_eq!(resolved.sha.len(), 40);
        let master = client
            .list_refs(url, &Credentials::None, "master")
            .await
            .unwrap();
        assert!(crate::resolve(&master, "master").unwrap().movable);
    }
}
