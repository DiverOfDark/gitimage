//! Context tree id through the git host's HTTP API (GitHub, GitLab, Gitea/Forgejo), so the
//! controller can skip a build without starting a Job. Best effort: anything unexpected is
//! `None` (logged at debug) and the caller falls back to the in-Job check.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use reqwest::{RequestBuilder, Url};
use serde::Deserialize;

use crate::gix_client::context_components;
use crate::{Credentials, is_full_sha};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds the pages read from one GitLab tree listing.
const MAX_PAGES: u32 = 20;

#[derive(Deserialize)]
struct Sha {
    sha: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    GitHub,
    GitLab,
    Gitea,
}

/// A parsed https repo URL: `https://host[:port]/<path>[.git]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoUrl {
    /// `scheme://host[:port]`
    origin: String,
    host: String,
    /// Path segments without a trailing `.git`.
    segments: Vec<String>,
}

impl RepoUrl {
    pub(crate) fn parse(url: &str, allow_http: bool) -> Option<Self> {
        let u = Url::parse(url).ok()?;
        if !(u.scheme() == "https" || (allow_http && u.scheme() == "http")) {
            return None;
        }
        let host = u.host_str()?.to_ascii_lowercase();
        let origin = match u.port() {
            Some(p) => format!("{}://{host}:{p}", u.scheme()),
            None => format!("{}://{host}", u.scheme()),
        };
        let mut segments: Vec<String> = u
            .path_segments()?
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if let Some(last) = segments.last_mut()
            && let Some(stripped) = last.strip_suffix(".git")
        {
            *last = stripped.to_owned();
        }
        if segments.len() < 2 || segments.iter().any(|s| s.is_empty()) {
            return None;
        }
        Some(Self {
            origin,
            host,
            segments,
        })
    }

    /// `owner`, `repo` and the path prefix before them (Gitea under a sub-path).
    fn owner_repo(&self) -> (String, &str, &str) {
        let n = self.segments.len();
        let prefix: String = self.segments[..n - 2]
            .iter()
            .map(|s| format!("/{s}"))
            .collect();
        (prefix, &self.segments[n - 2], &self.segments[n - 1])
    }
}

pub(crate) struct HostApi {
    client: reqwest::Client,
    github_api: String,
    allow_http: bool,
    gitea_hosts: Mutex<HashMap<String, bool>>,
}

impl HostApi {
    pub(crate) fn new(client: reqwest::Client, github_api: &str, allow_http: bool) -> Self {
        Self {
            client,
            github_api: github_api.trim_end_matches('/').to_owned(),
            allow_http,
            gitea_hosts: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn client_builder() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(TIMEOUT)
            .user_agent(concat!("gitimage/", env!("CARGO_PKG_VERSION")))
    }

    fn shared() -> &'static Self {
        static API: OnceLock<HostApi> = OnceLock::new();
        API.get_or_init(|| {
            let client = Self::client_builder()
                .https_only(true)
                .build()
                .unwrap_or_default();
            Self::new(client, "https://api.github.com", false)
        })
    }

    async fn kind(&self, repo: &RepoUrl) -> Option<Kind> {
        if repo.host == "github.com" {
            return Some(Kind::GitHub);
        }
        if repo.host.contains("gitlab") {
            return Some(Kind::GitLab);
        }
        let (prefix, _, _) = repo.owner_repo();
        let base = format!("{}{prefix}", repo.origin);
        let cached = self
            .gitea_hosts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&base)
            .copied();
        let is_gitea = match cached {
            Some(v) => v,
            None => {
                #[derive(Deserialize)]
                struct Version {
                    #[allow(dead_code)]
                    version: String,
                }
                let v = match self
                    .client
                    .get(format!("{base}/api/v1/version"))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => r.json::<Version>().await.is_ok(),
                    _ => false,
                };
                self.gitea_hosts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(base, v);
                v
            }
        };
        is_gitea.then_some(Kind::Gitea)
    }

    pub(crate) async fn context_tree(
        &self,
        url: &str,
        creds: &Credentials,
        sha: &str,
        context: &str,
    ) -> Result<Option<String>, String> {
        let token = match creds {
            Credentials::None => None,
            Credentials::Https { password, .. } if password.is_empty() => None,
            Credentials::Https { password, .. } => Some(password.as_str()),
            Credentials::Ssh { .. } => return Ok(None),
        };
        if !is_full_sha(sha) {
            return Ok(None);
        }
        let Some(repo) = RepoUrl::parse(url, self.allow_http) else {
            return Ok(None);
        };
        let components = context_components(context).map_err(|e| e.to_string())?;
        match self.kind(&repo).await {
            Some(Kind::GitHub) => {
                let (_, owner, name) = repo.owner_repo();
                let base = format!("{}/repos/{owner}/{name}", self.github_api);
                let auth = |r: RequestBuilder| match token {
                    Some(t) => r.bearer_auth(t),
                    None => r,
                };
                if components.is_empty() {
                    // GET /git/commits/{sha} -> {tree: {sha}}
                    #[derive(Deserialize)]
                    struct Commit {
                        tree: Sha,
                    }
                    let url = format!("{base}/git/commits/{sha}");
                    let c = self.get_json::<Commit>(auth(self.client.get(url))).await?;
                    return Ok(Some(c.tree.sha));
                }
                self.walk_trees(&format!("{base}/git/trees"), sha, &components, &auth)
                    .await
            }
            Some(Kind::Gitea) => {
                let (prefix, owner, name) = repo.owner_repo();
                let base = format!("{}{prefix}/api/v1/repos/{owner}/{name}", repo.origin);
                let auth = |r: RequestBuilder| match token {
                    Some(t) => r.header("Authorization", format!("token {t}")),
                    None => r,
                };
                if components.is_empty() {
                    // Gitea echoes the commit sha as tree sha on both the commits and the trees
                    // endpoint: hash the root listing instead.
                    return self
                        .gitea_root(&format!("{base}/git/trees/{sha}"), &auth)
                        .await;
                }
                self.walk_trees(&format!("{base}/git/trees"), sha, &components, &auth)
                    .await
            }
            Some(Kind::GitLab) => self.gitlab(&repo, token, sha, &components).await,
            None => Ok(None),
        }
    }

    /// GitHub and Gitea: `GET {base}/{tree-ish}` gives `{tree: [{path, type, sha}]}`; the commit
    /// sha works as tree-ish for the first step (both echo it back as `sha`, so the root tree id
    /// comes from the commit endpoint instead). `components` is non-empty.
    async fn walk_trees(
        &self,
        base: &str,
        sha: &str,
        components: &[&str],
        auth: &(dyn Fn(RequestBuilder) -> RequestBuilder + Sync),
    ) -> Result<Option<String>, String> {
        #[derive(Deserialize)]
        struct Entry {
            path: String,
            #[serde(rename = "type")]
            kind: String,
            sha: String,
        }
        #[derive(Deserialize)]
        struct Tree {
            #[serde(default)]
            tree: Vec<Entry>,
            #[serde(default)]
            truncated: bool,
        }
        let mut current = sha.to_owned();
        for want in components {
            let url = format!("{base}/{current}");
            let t = self.get_json::<Tree>(auth(self.client.get(url))).await?;
            let Some(e) = t.tree.into_iter().find(|e| e.path == *want) else {
                if t.truncated {
                    return Err(format!("tree listing for {want:?} truncated"));
                }
                return Err(format!("context component {want:?} not found"));
            };
            if e.kind != "tree" {
                return Err(format!("context component {want:?} is a {}", e.kind));
            }
            current = e.sha;
        }
        if components.is_empty() {
            return Err("walk_trees needs a non-root context".into());
        }
        Ok(Some(current))
    }

    /// Gitea root tree: all pages of `trees/{commit}`, hashed.
    async fn gitea_root(
        &self,
        url: &str,
        auth: &(dyn Fn(RequestBuilder) -> RequestBuilder + Sync),
    ) -> Result<Option<String>, String> {
        #[derive(Deserialize)]
        struct Entry {
            path: String,
            mode: String,
            sha: String,
        }
        #[derive(Deserialize)]
        struct Page {
            #[serde(default)]
            tree: Vec<Entry>,
            #[serde(default)]
            total_count: Option<usize>,
        }
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let req = auth(self.client.get(format!("{url}?per_page=1000&page={page}")));
            let p = self.get_json::<Page>(req).await?;
            let n = p.tree.len();
            all.extend(p.tree.into_iter().map(|e| (e.mode, e.path, e.sha)));
            match p.total_count {
                Some(total) if all.len() < total && n > 0 => continue,
                Some(total) if all.len() != total => return Err("incomplete tree listing".into()),
                _ => return tree_id(&all).map(Some),
            }
        }
        Err("tree listing has too many pages".into())
    }

    /// GitLab lists the parent directory; the root tree id is hashed from the root listing.
    async fn gitlab(
        &self,
        repo: &RepoUrl,
        token: Option<&str>,
        sha: &str,
        components: &[&str],
    ) -> Result<Option<String>, String> {
        #[derive(Deserialize)]
        struct Entry {
            id: String,
            name: String,
            #[serde(rename = "type")]
            kind: String,
            mode: String,
        }
        let mut url =
            Url::parse(&format!("{}/api/v4/projects", repo.origin)).map_err(|e| e.to_string())?;
        url.path_segments_mut()
            .map_err(|_| "bad base url".to_string())?
            .push(&repo.segments.join("/"))
            .extend(["repository", "tree"]);
        let (parent, last) = match components.split_last() {
            Some((last, parent)) => (parent.join("/"), Some(*last)),
            None => (String::new(), None),
        };
        let mut all: Vec<Entry> = Vec::new();
        let mut complete = false;
        for page in 1..=MAX_PAGES {
            let mut u = url.clone();
            {
                let mut q = u.query_pairs_mut();
                if !parent.is_empty() {
                    q.append_pair("path", &parent);
                }
                q.append_pair("ref", sha)
                    .append_pair("per_page", "100")
                    .append_pair("page", &page.to_string());
            }
            let mut req = self.client.get(u);
            if let Some(t) = token {
                req = req.header("PRIVATE-TOKEN", t);
            }
            let entries = self.get_json::<Vec<Entry>>(req).await?;
            let n = entries.len();
            all.extend(entries);
            if let Some(last) = last
                && let Some(e) = all.iter().find(|e| e.name == last)
            {
                if e.kind != "tree" {
                    return Err(format!("context component {last:?} is a {}", e.kind));
                }
                return Ok(Some(e.id.clone()));
            }
            if n < 100 {
                complete = true;
                break;
            }
        }
        match last {
            Some(last) => Err(format!("context component {last:?} not found")),
            None if complete => {
                let entries: Vec<_> = all.into_iter().map(|e| (e.mode, e.name, e.id)).collect();
                tree_id(&entries).map(Some)
            }
            None => Err("tree listing has too many pages".into()),
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        req: RequestBuilder,
    ) -> Result<T, String> {
        // Errors carry the URL (no credentials: they travel in headers) and status only.
        let resp = req.send().await.map_err(|e| e.without_url().to_string())?;
        let status = resp.status();
        let url = resp.url().clone();
        if !status.is_success() {
            return Err(format!("GET {url}: {status}"));
        }
        resp.json::<T>()
            .await
            .map_err(|e| format!("GET {url}: {}", e.without_url()))
    }
}

/// Git tree object id (sha1) from a complete listing of `(mode, name, entry sha)`.
/// Forges print tree modes as `040000`; git stores `40000`. Entries are sorted the git way
/// (directories compare as `name/`).
pub(crate) fn tree_id(entries: &[(String, String, String)]) -> Result<String, String> {
    use sha1::Digest;
    let mut sorted: Vec<(&str, &str, Vec<u8>)> = Vec::with_capacity(entries.len());
    for (mode, name, sha) in entries {
        let mode = mode.trim_start_matches('0');
        if !matches!(mode, "40000" | "100644" | "100755" | "120000" | "160000") {
            return Err(format!("unexpected tree entry mode {mode:?}"));
        }
        let raw = data_encoding::HEXLOWER_PERMISSIVE
            .decode(sha.as_bytes())
            .ok()
            .filter(|b| b.len() == 20)
            .ok_or_else(|| format!("bad entry sha {sha:?}"))?;
        sorted.push((mode, name.as_str(), raw));
    }
    let key = |(mode, name, _): &(&str, &str, Vec<u8>)| {
        let mut k = name.as_bytes().to_vec();
        if *mode == "40000" {
            k.push(b'/');
        }
        k
    };
    sorted.sort_by_key(key);
    let mut body = Vec::new();
    for (mode, name, raw) in &sorted {
        body.extend_from_slice(mode.as_bytes());
        body.push(b' ');
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(raw);
    }
    let mut h = sha1::Sha1::new();
    h.update(format!("tree {}\0", body.len()).as_bytes());
    h.update(&body);
    Ok(data_encoding::HEXLOWER.encode(&h.finalize()))
}

pub(crate) async fn context_tree(
    url: &str,
    creds: &Credentials,
    sha: &str,
    context: &str,
) -> Option<String> {
    HostApi::shared()
        .context_tree(url, creds, sha, context)
        .await
        .unwrap_or_else(|why| {
            tracing::debug!(url, sha, context, "host API tree lookup unavailable: {why}");
            None
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    // `git mktree` of README (BLOB) + services/ (SVC) is ROOT.
    const ROOT: &str = "57248eb5fb142b034509b02805742c26b9ea67b7";
    const BLOB: &str = "ce013625030ba8dba906f756967f9e9ca394464a";
    const SVC: &str = "8ad19a58b0f99baf6ff6078abc1764694ff959aa";
    const API: &str = "1111111111111111111111111111111111111111";
    const WEB: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn parses_repo_urls() {
        let r = RepoUrl::parse("https://github.com/you/app.git", false).unwrap();
        assert_eq!(r.origin, "https://github.com");
        assert_eq!(r.segments, vec!["you", "app"]);
        let r = RepoUrl::parse("https://u@git.example.com:8443/sub/you/app", false).unwrap();
        assert_eq!(r.origin, "https://git.example.com:8443");
        assert_eq!(r.owner_repo(), ("/sub".to_string(), "you", "app"));
        assert!(RepoUrl::parse("http://github.com/you/app", false).is_none());
        assert!(RepoUrl::parse("http://github.com/you/app", true).is_some());
        assert!(RepoUrl::parse("https://github.com/you", false).is_none());
        assert!(RepoUrl::parse("git@github.com:you/app.git", false).is_none());
        assert!(RepoUrl::parse("ssh://git@github.com/you/app.git", false).is_none());
    }

    #[test]
    fn tree_ids_hash_like_git() {
        let e = |m: &str, n: &str, s: &str| (m.to_string(), n.to_string(), s.to_string());
        // `git mktree --missing` of the same entries, given in a different order
        assert_eq!(
            tree_id(&[
                e("100644", "a.b", BLOB),
                e("040000", "a", SVC),
                e("100755", "a-c", BLOB),
                e("120000", "z", BLOB),
                e("160000", "sub", BLOB)
            ]),
            Ok("6c274696220019af2d5f766adb6c740ca54fd330".into())
        );
        assert_eq!(
            tree_id(&[e("040000", "services", SVC), e("100644", "README", BLOB)]),
            Ok(ROOT.into())
        );
        assert!(tree_id(&[e("100600", "x", BLOB)]).is_err());
        assert!(tree_id(&[e("100644", "x", "abc")]).is_err());
    }

    #[derive(Default)]
    struct Seen {
        auth: Mutex<Vec<String>>,
        version_calls: AtomicUsize,
    }

    fn record(seen: &Seen, headers: &HeaderMap, name: &str) {
        if let Some(v) = headers.get(name) {
            seen.auth
                .lock()
                .unwrap()
                .push(format!("{name}: {}", v.to_str().unwrap()));
        }
    }

    /// One fake git host answering GitHub-, Gitea- and GitLab-style requests.
    /// Repo layout: root tree ROOT, `services/` = SVC, `services/api/` = API, `README` a blob.
    fn trees(id: &str) -> Option<Value> {
        let entry = |path: &str, kind: &str, sha: &str| {
            let mode = if kind == "tree" { "040000" } else { "100644" };
            json!({"path": path, "type": kind, "sha": sha, "mode": mode})
        };
        match id {
            SHA | ROOT => Some(
                json!({"sha": id, "truncated": false, "page": 1, "total_count": 2, "tree": [
                entry("README", "blob", BLOB), entry("services", "tree", SVC)]}),
            ),
            SVC => Some(
                json!({"sha": SVC, "tree": [entry("api", "tree", API), entry("web", "tree", WEB)]}),
            ),
            API => Some(json!({"sha": API, "tree": []})),
            _ => None,
        }
    }

    async fn fake_host() -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let app = Router::new()
            .route(
                "/gh/repos/{o}/{r}/git/trees/{id}",
                get(
                    |State(s): State<Arc<Seen>>, h: HeaderMap, Path((o, r, id)): Path<(String, String, String)>| async move {
                        record(&s, &h, "authorization");
                        match (o.as_str(), r.as_str(), trees(&id)) {
                            ("you", "app", Some(t)) => Ok(Json(t)),
                            _ => Err(StatusCode::NOT_FOUND),
                        }
                    },
                ),
            )
            .route(
                "/gh/repos/{o}/{r}/git/commits/{id}",
                get(|Path((o, r, id)): Path<(String, String, String)>| async move {
                    if (o.as_str(), r.as_str(), id.as_str()) == ("you", "app", SHA) {
                        Ok(Json(json!({"sha": SHA, "tree": {"sha": ROOT}})))
                    } else {
                        Err(StatusCode::NOT_FOUND)
                    }
                }),
            )
            .route(
                "/api/v1/version",
                get(|State(s): State<Arc<Seen>>| async move {
                    s.version_calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"version": "1.24.0"}))
                }),
            )
            .route(
                "/api/v1/repos/{o}/{r}/git/trees/{id}",
                get(
                    |State(s): State<Arc<Seen>>, h: HeaderMap, Path((_o, _r, id)): Path<(String, String, String)>| async move {
                        record(&s, &h, "authorization");
                        trees(&id).map(Json).ok_or(StatusCode::NOT_FOUND)
                    },
                ),
            )
            .route(
                "/api/v4/projects/{project}/repository/tree",
                get(
                    |State(s): State<Arc<Seen>>, h: HeaderMap, Path(project): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                        record(&s, &h, "private-token");
                        if project != "group/sub/app" || q.get("ref").map(String::as_str) != Some(SHA) {
                            return Err(StatusCode::NOT_FOUND);
                        }
                        let e = |name: &str, kind: &str, id: &str| {
                            let mode = if kind == "tree" { "040000" } else { "100644" };
                            json!({"id": id, "name": name, "type": kind, "path": name, "mode": mode})
                        };
                        match q.get("path").map(String::as_str) {
                            None => Ok(Json(json!([e("README", "blob", BLOB), e("services", "tree", SVC)]))),
                            Some("services") => Ok(Json(json!([e("api", "tree", API)]))),
                            _ => Ok(Json(json!([]))),
                        }
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("127.0.0.1:{}", addr.port()), seen)
    }

    fn api(addr: &str) -> HostApi {
        let sock: std::net::SocketAddr = addr.parse().unwrap();
        let client = HostApi::client_builder()
            .resolve("gitlab.example", sock)
            .resolve("gitea.example", sock)
            .build()
            .unwrap();
        HostApi::new(client, &format!("http://{addr}/gh"), true)
    }

    fn token(t: &str) -> Credentials {
        Credentials::Https {
            username: "u".into(),
            password: t.into(),
        }
    }

    #[tokio::test]
    async fn github_walks_trees() {
        let (addr, seen) = fake_host().await;
        let api = api(&addr);
        let url = "https://github.com/you/app.git";
        let none = Credentials::None;
        assert_eq!(
            api.context_tree(url, &none, SHA, ".").await,
            Ok(Some(ROOT.into()))
        );
        assert_eq!(
            api.context_tree(url, &token("ghp_x"), SHA, "services/api")
                .await,
            Ok(Some(API.into()))
        );
        assert_eq!(
            api.context_tree(url, &none, SHA, "./services/").await,
            Ok(Some(SVC.into()))
        );
        assert!(api.context_tree(url, &none, SHA, "missing").await.is_err());
        assert!(api.context_tree(url, &none, SHA, "README").await.is_err());
        assert!(
            api.context_tree("https://github.com/you/other", &none, SHA, ".")
                .await
                .is_err()
        );
        assert_eq!(api.context_tree(url, &none, "main", ".").await, Ok(None));
        assert_eq!(
            *seen.auth.lock().unwrap(),
            vec!["authorization: Bearer ghp_x", "authorization: Bearer ghp_x"]
        );
    }

    #[tokio::test]
    async fn gitea_is_detected_once_and_walked() {
        let (addr, seen) = fake_host().await;
        let api = api(&addr);
        let port = addr.split(':').nth(1).unwrap();
        let url = format!("http://gitea.example:{port}/you/app.git");
        assert_eq!(
            api.context_tree(&url, &token("t0k"), SHA, ".").await,
            Ok(Some(ROOT.into()))
        );
        assert_eq!(
            api.context_tree(&url, &token("t0k"), SHA, "services/api")
                .await,
            Ok(Some(API.into()))
        );
        assert_eq!(seen.version_calls.load(Ordering::SeqCst), 1);
        assert!(
            seen.auth
                .lock()
                .unwrap()
                .iter()
                .all(|a| a == "authorization: token t0k")
        );
    }

    #[tokio::test]
    async fn gitlab_lists_parent() {
        let (addr, seen) = fake_host().await;
        let api = api(&addr);
        let port = addr.split(':').nth(1).unwrap();
        let url = format!("http://gitlab.example:{port}/group/sub/app.git");
        assert_eq!(
            api.context_tree(&url, &token("glpat"), SHA, ".").await,
            Ok(Some(ROOT.into()))
        );
        assert_eq!(
            api.context_tree(&url, &token("glpat"), SHA, "services")
                .await,
            Ok(Some(SVC.into()))
        );
        assert_eq!(
            api.context_tree(&url, &token("glpat"), SHA, "services/api")
                .await,
            Ok(Some(API.into()))
        );
        assert!(
            api.context_tree(&url, &token("glpat"), SHA, "README")
                .await
                .is_err()
        );
        assert!(
            api.context_tree(&url, &token("glpat"), SHA, "nope")
                .await
                .is_err()
        );
        assert!(
            seen.auth
                .lock()
                .unwrap()
                .iter()
                .all(|a| a == "private-token: glpat")
        );
    }

    #[tokio::test]
    async fn unknown_hosts_ssh_and_errors_are_none() {
        let (addr, _) = fake_host().await;
        let api = api(&addr);
        let ssh = Credentials::Ssh {
            identity: "k".into(),
            known_hosts: "h".into(),
        };
        assert_eq!(
            api.context_tree("https://github.com/you/app", &ssh, SHA, ".")
                .await,
            Ok(None)
        );
        assert_eq!(
            api.context_tree("git@github.com:you/app.git", &Credentials::None, SHA, ".")
                .await,
            Ok(None)
        );
        // not a forge: version endpoint fails -> unknown host
        assert_eq!(
            api.context_tree("http://127.0.0.1:1/you/app", &Credentials::None, SHA, ".")
                .await,
            Ok(None)
        );
    }

    /// Needs network; uses 3 of GitHub's 60 unauthenticated API calls per hour.
    /// `cargo test -p gitimage-git -- --ignored github_api`
    #[tokio::test]
    #[ignore]
    async fn github_api_public_repo() {
        let url = "https://github.com/moby/buildkit.git";
        let sha = "8c579433824224dc64a158cf8734495b6e2b186a";
        // `git rev-parse <sha>^{tree}` and `<sha>:frontend/dockerfile`
        let root = context_tree(url, &Credentials::None, sha, ".").await;
        assert_eq!(
            root.as_deref(),
            Some("4437a69fcfa1c393ae2bd7b2ab6b4625ef46a2ed")
        );
        let nested = context_tree(url, &Credentials::None, sha, "frontend/dockerfile").await;
        assert_eq!(
            nested.as_deref(),
            Some("4c5fa36237c369c3e0cfddc1fe9aa3116e018a2c")
        );
    }
}
