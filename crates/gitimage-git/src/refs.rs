//! Resolving `spec.ref` against the refs a remote advertises. Pure, no I/O.

/// One advertised ref, already peeled: `commit` is what an annotated tag points to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRef {
    /// Full name, e.g. `refs/heads/main`, `refs/tags/v1.0`, `HEAD`.
    pub name: String,
    /// 40-hex object id of the commit.
    pub commit: String,
}

/// What a `spec.ref` resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub sha: String,
    /// The full ref name it came from; `None` when `spec.ref` was a sha.
    pub full_ref: Option<String>,
    /// Only a branch can move. Tags and shas build once.
    pub movable: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("ref {0:?} not found on the remote (tried refs/tags/{0} and refs/heads/{0})")]
    NotFound(String),
    #[error("ref {0:?} is empty")]
    Empty(String),
}

/// `true` for a full 40-character hex sha.
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Resolves `wanted` the way git does: a full sha is used as is, else a tag, else a branch.
/// `refs/heads/...` and `refs/tags/...` select one namespace explicitly.
pub fn resolve(refs: &[RemoteRef], wanted: &str) -> Result<Resolved, ResolveError> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return Err(ResolveError::Empty(wanted.into()));
    }
    if is_full_sha(wanted) {
        return Ok(Resolved {
            sha: wanted.to_ascii_lowercase(),
            full_ref: None,
            movable: false,
        });
    }
    let candidates: Vec<String> = if wanted.starts_with("refs/") || wanted == "HEAD" {
        vec![wanted.to_string()]
    } else {
        vec![
            format!("refs/tags/{wanted}"),
            format!("refs/heads/{wanted}"),
        ]
    };
    for name in candidates {
        if let Some(r) = refs.iter().find(|r| r.name == name) {
            return Ok(Resolved {
                sha: r.commit.clone(),
                movable: !name.starts_with("refs/tags/"),
                full_ref: Some(name),
            });
        }
    }
    Err(ResolveError::NotFound(wanted.into()))
}

/// Ref prefixes worth asking the server for, so a big repo's `ls-refs` stays small.
pub fn ls_refs_prefixes(wanted: &str) -> Vec<String> {
    if is_full_sha(wanted) {
        return Vec::new();
    }
    if wanted.starts_with("refs/") || wanted == "HEAD" {
        return vec![wanted.to_string()];
    }
    vec![
        format!("refs/tags/{wanted}"),
        format!("refs/heads/{wanted}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = "1111111111111111111111111111111111111111";
    const TAG: &str = "2222222222222222222222222222222222222222";
    const SAME_NAME_BRANCH: &str = "3333333333333333333333333333333333333333";

    fn refs() -> Vec<RemoteRef> {
        vec![
            RemoteRef {
                name: "HEAD".into(),
                commit: MAIN.into(),
            },
            RemoteRef {
                name: "refs/heads/main".into(),
                commit: MAIN.into(),
            },
            RemoteRef {
                name: "refs/heads/v1".into(),
                commit: SAME_NAME_BRANCH.into(),
            },
            RemoteRef {
                name: "refs/tags/v1".into(),
                commit: TAG.into(),
            },
        ]
    }

    #[test]
    fn branch() {
        let r = resolve(&refs(), "main").unwrap();
        assert_eq!(r.sha, MAIN);
        assert!(r.movable);
        assert_eq!(r.full_ref.as_deref(), Some("refs/heads/main"));
    }

    #[test]
    fn tag_wins_over_branch_like_git() {
        let r = resolve(&refs(), "v1").unwrap();
        assert_eq!(r.sha, TAG);
        assert!(!r.movable);
    }

    #[test]
    fn explicit_namespace() {
        assert_eq!(
            resolve(&refs(), "refs/heads/v1").unwrap().sha,
            SAME_NAME_BRANCH
        );
    }

    #[test]
    fn full_sha_is_used_as_is() {
        let sha = "ABCDEFabcdef0123456789abcdef0123456789ab";
        let r = resolve(&[], sha).unwrap();
        assert_eq!(r.sha, sha.to_ascii_lowercase());
        assert!(!r.movable);
        assert!(r.full_ref.is_none());
    }

    #[test]
    fn short_sha_is_not_a_sha() {
        assert_eq!(
            resolve(&refs(), "abc1234"),
            Err(ResolveError::NotFound("abc1234".into()))
        );
    }

    #[test]
    fn prefixes() {
        assert_eq!(
            ls_refs_prefixes("main"),
            vec!["refs/tags/main", "refs/heads/main"]
        );
        assert!(ls_refs_prefixes(MAIN).is_empty());
    }
}
