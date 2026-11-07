//! Parses `/v2/...` request paths into routes. Repos are exactly `<namespace>/<name>`.

use crate::{OciError, Repo};

/// A parsed distribution API path.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// `/v2/`
    Base,
    /// `/v2/<repo>/blobs/<digest>`
    Blob(Repo, String),
    /// `/v2/<repo>/blobs/uploads/`
    UploadStart(Repo),
    /// `/v2/<repo>/blobs/uploads/<id>`
    Upload(Repo, String),
    /// `/v2/<repo>/manifests/<reference>`
    Manifest(Repo, String),
    /// `/v2/<repo>/tags/list`
    Tags(Repo),
    /// `/v2/<repo>/referrers/<digest>`
    Referrers(Repo, String),
}

impl Route {
    pub(super) fn parse(path: &str) -> Result<Self, OciError> {
        let rest = path.strip_prefix("/v2").ok_or(OciError::NameUnknown)?;
        if rest.is_empty() || rest == "/" {
            return Ok(Self::Base);
        }
        let rest = rest.strip_prefix('/').ok_or(OciError::NameUnknown)?;
        let segments: Vec<&str> = rest.split('/').collect();
        let [ns, name, kind, tail @ ..] = segments.as_slice() else {
            return Err(OciError::NameUnknown);
        };
        let repo = parse_repo(ns, name).ok_or(OciError::NameUnknown)?;
        let one = |s: &[&str]| match s {
            [x] if !x.is_empty() => Some(x.to_string()),
            _ => None,
        };
        let route = match (*kind, tail) {
            ("blobs", ["uploads"] | ["uploads", ""]) => Self::UploadStart(repo),
            ("blobs", ["uploads", id]) => Self::Upload(repo, id.to_string()),
            ("blobs", t) => Self::Blob(repo, one(t).ok_or(OciError::NameUnknown)?),
            ("manifests", t) => Self::Manifest(repo, one(t).ok_or(OciError::NameUnknown)?),
            ("tags", ["list"]) => Self::Tags(repo),
            ("referrers", t) => Self::Referrers(repo, one(t).ok_or(OciError::NameUnknown)?),
            _ => return Err(OciError::NameUnknown),
        };
        Ok(route)
    }

    /// Low-cardinality name for logs and spans.
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Blob(..) => "blob",
            Self::UploadStart(..) => "upload_start",
            Self::Upload(..) => "upload",
            Self::Manifest(..) => "manifest",
            Self::Tags(..) => "tags",
            Self::Referrers(..) => "referrers",
        }
    }

    pub(super) fn repo(&self) -> Option<&Repo> {
        match self {
            Self::Base => None,
            Self::Blob(r, _)
            | Self::UploadStart(r)
            | Self::Upload(r, _)
            | Self::Manifest(r, _)
            | Self::Tags(r)
            | Self::Referrers(r, _) => Some(r),
        }
    }
}

/// `<ns>/<name>` as in a `from=` mount parameter.
pub(super) fn parse_repo_path(s: &str) -> Option<Repo> {
    let (ns, name) = s.split_once('/')?;
    parse_repo(ns, name)
}

/// Namespace: DNS-1123 label. Name: DNS-1123 subdomain (Kubernetes object names).
fn parse_repo(ns: &str, name: &str) -> Option<Repo> {
    let ok = |s: &str, max: usize, dots: bool| {
        !s.is_empty()
            && s.len() <= max
            && s.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || (dots && b == b'.')
            })
            && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
            && s.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
    };
    (ok(ns, 63, false) && ok(name, 253, true)).then(|| Repo::new(ns, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repo {
        Repo::new("team-a", "my.app")
    }

    #[test]
    fn parses_routes() {
        assert_eq!(Route::parse("/v2/").unwrap(), Route::Base);
        assert_eq!(Route::parse("/v2").unwrap(), Route::Base);
        assert_eq!(
            Route::parse("/v2/team-a/my.app/blobs/sha256:ab").unwrap(),
            Route::Blob(repo(), "sha256:ab".into())
        );
        assert_eq!(
            Route::parse("/v2/team-a/my.app/blobs/uploads/").unwrap(),
            Route::UploadStart(repo())
        );
        assert_eq!(
            Route::parse("/v2/team-a/my.app/blobs/uploads").unwrap(),
            Route::UploadStart(repo())
        );
        assert_eq!(
            Route::parse("/v2/team-a/my.app/blobs/uploads/x.y.z").unwrap(),
            Route::Upload(repo(), "x.y.z".into())
        );
        assert_eq!(
            Route::parse("/v2/team-a/my.app/manifests/latest").unwrap(),
            Route::Manifest(repo(), "latest".into())
        );
        assert_eq!(
            Route::parse("/v2/team-a/my.app/tags/list").unwrap(),
            Route::Tags(repo())
        );
    }

    #[test]
    fn rejects_other_repo_shapes() {
        for p in [
            "/v2/a/manifests/latest",
            "/v2/a/b/c/manifests/latest",
            "/v2/A/b/manifests/latest",
            "/v2/a/-b/manifests/latest",
            "/v2/a/b/manifests/",
            "/v2/a/b/other/x",
            "/v3/a/b/manifests/x",
            "/v2/a.b/c/manifests/x",
        ] {
            assert!(Route::parse(p).is_err(), "{p}");
        }
        assert_eq!(parse_repo_path("a/b"), Some(Repo::new("a", "b")));
        assert_eq!(parse_repo_path("a/b/c"), None);
    }
}
