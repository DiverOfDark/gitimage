//! Upstream registry auth: learns the `WWW-Authenticate` challenge on the first 401, then
//! sends Basic credentials or Bearer tokens (fetched with Basic auth, cached per scope set
//! until shortly before they expire).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tokio::sync::{Mutex, RwLock};

use crate::OciError;

/// How the upstream wants to be authenticated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Challenge {
    Basic,
    Bearer {
        realm: String,
        service: Option<String>,
    },
}

/// Parses `Basic realm=".."` or `Bearer realm="..",service="..",scope=".."`.
pub(super) fn parse_challenge(header: &str) -> Option<Challenge> {
    let (scheme, params) = header.trim().split_once(' ').unwrap_or((header.trim(), ""));
    if scheme.eq_ignore_ascii_case("basic") {
        return Some(Challenge::Basic);
    }
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let params = parse_params(params);
    Some(Challenge::Bearer {
        realm: params.get("realm")?.clone(),
        service: params.get("service").cloned(),
    })
}

/// `k="v", k2=v2` with commas allowed inside quoted values.
fn parse_params(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s.trim();
    while let Some((key, after)) = rest.split_once('=') {
        let key = key
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        let after = after.trim_start();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        out.insert(key, value.to_string());
        rest = tail.trim_start().trim_start_matches(',');
    }
    out
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
    expires_in: Option<u64>,
}

pub(super) struct Auth {
    credentials: Option<(String, String)>,
    challenge: RwLock<Option<Challenge>>,
    /// space-joined scopes -> (token, valid until)
    tokens: Mutex<HashMap<String, (String, Instant)>>,
}

impl Auth {
    pub(super) fn new(credentials: Option<(String, String)>) -> Self {
        Self {
            credentials,
            challenge: RwLock::default(),
            tokens: Mutex::default(),
        }
    }

    fn basic(&self) -> Option<String> {
        self.credentials
            .as_ref()
            .map(|(u, p)| format!("Basic {}", STANDARD.encode(format!("{u}:{p}"))))
    }

    /// `Authorization` value for a request needing `scopes`, if the upstream asked for one.
    pub(super) async fn header(
        &self,
        http: &reqwest::Client,
        scopes: &[String],
    ) -> Result<Option<String>, OciError> {
        let challenge = self.challenge.read().await.clone();
        match challenge {
            None => Ok(None),
            Some(Challenge::Basic) => Ok(self.basic()),
            Some(Challenge::Bearer { realm, service }) => {
                let key = scopes.join(" ");
                if let Some((token, until)) = self.tokens.lock().await.get(&key)
                    && Instant::now() < *until
                {
                    return Ok(Some(format!("Bearer {token}")));
                }
                let (token, ttl) = self
                    .fetch_token(http, &realm, service.as_deref(), scopes)
                    .await?;
                self.tokens
                    .lock()
                    .await
                    .insert(key, (token.clone(), Instant::now() + ttl));
                Ok(Some(format!("Bearer {token}")))
            }
        }
    }

    /// Records the challenge from a 401; returns `false` when retrying cannot help.
    pub(super) async fn challenged(
        &self,
        www_authenticate: Option<&str>,
        scopes: &[String],
    ) -> bool {
        let Some(challenge) = www_authenticate.and_then(parse_challenge) else {
            return false;
        };
        let mut current = self.challenge.write().await;
        let changed = current.as_ref() != Some(&challenge);
        *current = Some(challenge);
        // A rejected cached token is useless; drop it so the retry fetches a fresh one.
        let dropped = self.tokens.lock().await.remove(&scopes.join(" ")).is_some();
        changed || dropped
    }

    async fn fetch_token(
        &self,
        http: &reqwest::Client,
        realm: &str,
        service: Option<&str>,
        scopes: &[String],
    ) -> Result<(String, Duration), OciError> {
        let mut url = reqwest::Url::parse(realm)
            .map_err(|e| OciError::Backend(format!("token realm {realm}: {e}")))?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(service) = service {
                query.append_pair("service", service);
            }
            for scope in scopes {
                query.append_pair("scope", scope);
            }
        }
        let mut req = http.get(url);
        if let Some(basic) = self.basic() {
            req = req.header(reqwest::header::AUTHORIZATION, basic);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| OciError::Backend(format!("token endpoint {realm}: {e}")))?;
        if !resp.status().is_success() {
            return Err(OciError::Backend(format!(
                "token endpoint {realm} returned {}",
                resp.status()
            )));
        }
        let body: TokenResponse = resp
            .json()
            .await
            .map_err(|e| OciError::Backend(format!("token endpoint {realm}: {e}")))?;
        let token = body
            .token
            .or(body.access_token)
            .ok_or_else(|| OciError::Backend(format!("token endpoint {realm} sent no token")))?;
        // Renew a little early; the spec default lifetime is 60 seconds.
        let ttl = body.expires_in.unwrap_or(60).max(10);
        Ok((token, Duration::from_secs(ttl - ttl / 10)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges() {
        assert_eq!(
            parse_challenge(r#"Basic realm="x""#),
            Some(Challenge::Basic)
        );
        assert_eq!(
            parse_challenge(
                r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:a/b:pull,push""#
            ),
            Some(Challenge::Bearer {
                realm: "https://auth.docker.io/token".into(),
                service: Some("registry.docker.io".into()),
            })
        );
        assert_eq!(
            parse_challenge("Bearer realm=https://ghcr.io/token, scope=x"),
            Some(Challenge::Bearer {
                realm: "https://ghcr.io/token".into(),
                service: None,
            })
        );
        assert_eq!(parse_challenge("Bearer service=x"), None);
        assert_eq!(parse_challenge("Negotiate"), None);
    }
}
