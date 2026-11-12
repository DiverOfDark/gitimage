//! Who is asking. Mode `none`: everyone is the controller's ServiceAccount. Mode `oidc`:
//! authorization code flow with PKCE; the ID token is kept in an encrypted cookie (no
//! server-side session, so any replica accepts it) and sent to the Kubernetes API as the
//! bearer token of every request made for that user. The API server, not this process,
//! decides what the user may see.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use jsonwebtoken::jwk::JwkSet;
use kube::Client;
use ring::rand::SecureRandom as _;
use ring::{aead, hmac, rand};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::UiState;

const SESSION_COOKIE: &str = "gitimage_session";
const LOGIN_COOKIE: &str = "gitimage_login";
/// Browsers cap a cookie at about 4096 bytes including its name and attributes.
const COOKIE_CHUNK: usize = 3600;
const MAX_CHUNKS: usize = 8;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AuthError(pub String);

fn err(msg: impl Into<String>) -> AuthError {
    AuthError(msg.into())
}

/// Authenticated encryption for cookies, keyed from the controller's existing secret.
pub struct Sealer {
    key: aead::LessSafeKey,
    rng: rand::SystemRandom,
}

impl Sealer {
    pub fn new(secret: &[u8]) -> Self {
        let derived = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, secret),
            b"gitimage-ui-session-v1",
        );
        let key = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, derived.as_ref())
            .expect("HMAC-SHA256 output is a ChaCha20 key");
        Self {
            key: aead::LessSafeKey::new(key),
            rng: rand::SystemRandom::new(),
        }
    }

    /// `purpose` binds the ciphertext to one cookie, so one cannot be replayed as the other.
    pub fn seal(&self, purpose: &str, plain: &[u8]) -> String {
        let mut nonce = [0u8; aead::NONCE_LEN];
        self.rng.fill(&mut nonce).expect("system randomness");
        let mut buf = plain.to_vec();
        self.key
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(purpose.as_bytes()),
                &mut buf,
            )
            .expect("sealing a cookie");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        B64.encode(out)
    }

    pub fn open(&self, purpose: &str, sealed: &str) -> Option<Vec<u8>> {
        let raw = B64.decode(sealed).ok()?;
        if raw.len() < aead::NONCE_LEN {
            return None;
        }
        let (nonce, ct) = raw.split_at(aead::NONCE_LEN);
        let mut buf = ct.to_vec();
        let plain = self
            .key
            .open_in_place(
                aead::Nonce::try_assume_unique_for_key(nonce).ok()?,
                aead::Aad::from(purpose.as_bytes()),
                &mut buf,
            )
            .ok()?;
        Some(plain.to_vec())
    }

    /// 32 random bytes, base64url.
    pub fn random(&self) -> String {
        let mut b = [0u8; 32];
        self.rng.fill(&mut b).expect("system randomness");
        B64.encode(b)
    }
}

/// What the session cookie holds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The ID token; the bearer token for the Kubernetes API.
    pub token: String,
    pub user: String,
    /// Unix seconds; the token's `exp`.
    pub exp: i64,
}

/// State of a login in flight.
#[derive(Serialize, Deserialize)]
struct LoginState {
    state: String,
    nonce: String,
    verifier: String,
    return_to: String,
}

/// The caller of an API request, placed in the request extensions by [`require`].
#[derive(Clone)]
pub struct Identity {
    /// Kubernetes client acting as this caller.
    pub client: Client,
    pub user: Option<String>,
    /// Cache key for access decisions.
    pub key: String,
    pub exp: Option<i64>,
    /// Mode `none`: no per-namespace filtering.
    pub unrestricted: bool,
}

impl Identity {
    pub fn expired(&self) -> bool {
        self.exp.is_some_and(|e| e <= now())
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn cookies(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

fn set_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .expect("cookie values are base64url")
}

/// The sealed session split over as many cookies as it needs, plus deletions of leftovers.
fn session_cookies(sealed: &str, max_age: i64, secure: bool) -> Vec<HeaderValue> {
    let chunks: Vec<&str> = sealed
        .as_bytes()
        .chunks(COOKIE_CHUNK)
        .map(|c| std::str::from_utf8(c).expect("base64url is ASCII"))
        .collect();
    (0..MAX_CHUNKS)
        .map(|i| match chunks.get(i) {
            Some(c) => set_cookie(&format!("{SESSION_COOKIE}_{i}"), c, max_age, secure),
            None => set_cookie(&format!("{SESSION_COOKIE}_{i}"), "", 0, secure),
        })
        .collect()
}

fn read_session(sealer: &Sealer, cookies: &HashMap<String, String>) -> Option<Session> {
    let mut sealed = String::new();
    for i in 0..MAX_CHUNKS {
        match cookies.get(&format!("{SESSION_COOKIE}_{i}")) {
            Some(c) if !c.is_empty() => sealed.push_str(c),
            _ => break,
        }
    }
    let plain = sealer.open(SESSION_COOKIE, &sealed)?;
    let session: Session = serde_json::from_slice(&plain).ok()?;
    (session.exp > now()).then_some(session)
}

#[derive(Deserialize, Clone)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// The OIDC client.
pub struct Oidc {
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    scopes: Vec<String>,
    username_claim: String,
    redirect_uri: String,
    http: reqwest::Client,
    meta: tokio::sync::Mutex<Option<(Metadata, JwkSet, Instant)>>,
}

impl Oidc {
    pub fn new(
        cfg: &gitimage_api::config::UiOidc,
        base_url: &str,
        client_id: String,
        client_secret: Option<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            issuer: cfg.issuer.clone(),
            client_id,
            client_secret,
            scopes: cfg.scopes.clone(),
            username_claim: cfg.username_claim.clone(),
            redirect_uri: format!("{}/auth/callback", base_url.trim_end_matches('/')),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            meta: tokio::sync::Mutex::new(None),
        })
    }

    /// Discovery document and signing keys, cached for an hour.
    async fn meta(&self, refresh: bool) -> Result<(Metadata, JwkSet), AuthError> {
        let mut guard = self.meta.lock().await;
        if let Some((m, k, at)) = guard.as_ref()
            && !refresh
            && at.elapsed() < Duration::from_secs(3600)
        {
            return Ok((m.clone(), k.clone()));
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let meta: Metadata = self.get_json(&url).await?;
        if meta.issuer != self.issuer {
            return Err(err(format!(
                "issuer mismatch: configured {}, discovery says {}",
                self.issuer, meta.issuer
            )));
        }
        let keys: JwkSet = self.get_json(&meta.jwks_uri).await?;
        *guard = Some((meta.clone(), keys.clone(), Instant::now()));
        Ok((meta, keys))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, AuthError> {
        self.http
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| err(format!("GET {url}: {e}")))?
            .json()
            .await
            .map_err(|e| err(format!("GET {url}: {e}")))
    }

    async fn authorize_url(&self, login: &LoginState) -> Result<String, AuthError> {
        let (meta, _) = self.meta(false).await?;
        let challenge = B64.encode(sha2::Sha256::digest(login.verifier.as_bytes()));
        let mut url = url::Url::parse(&meta.authorization_endpoint)
            .map_err(|e| err(format!("authorization endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", &self.scopes.join(" "))
            .append_pair("state", &login.state)
            .append_pair("nonce", &login.nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    async fn exchange(&self, code: &str, login: &LoginState) -> Result<Session, AuthError> {
        let (meta, keys) = self.meta(false).await?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("client_id", &self.client_id)
            .append_pair("code_verifier", &login.verifier)
            .finish();
        let mut req = self
            .http
            .post(&meta.token_endpoint)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .body(body);
        if let Some(secret) = &self.client_secret {
            req = req.basic_auth(&self.client_id, Some(secret));
        }
        let resp: TokenResponse = req
            .send()
            .await
            .map_err(|e| err(format!("token endpoint: {e}")))?
            .json()
            .await
            .map_err(|e| err(format!("token endpoint: {e}")))?;
        let Some(token) = resp.id_token else {
            return Err(err(format!(
                "token endpoint returned no id_token: {} {}",
                resp.error.unwrap_or_default(),
                resp.error_description.unwrap_or_default()
            )));
        };
        match self.verify(&token, &keys, &login.nonce) {
            Ok(s) => Ok(s),
            // The issuer may have rotated its keys since they were cached.
            Err(_) => {
                let (_, keys) = self.meta(true).await?;
                self.verify(&token, &keys, &login.nonce)
            }
        }
    }

    /// Checks signature, issuer, audience, expiry and nonce of an ID token.
    fn verify(&self, token: &str, keys: &JwkSet, nonce: &str) -> Result<Session, AuthError> {
        use jsonwebtoken::Algorithm as A;
        let header =
            jsonwebtoken::decode_header(token).map_err(|e| err(format!("ID token: {e}")))?;
        if matches!(header.alg, A::HS256 | A::HS384 | A::HS512) {
            return Err(err("ID token is signed with a shared secret"));
        }
        let jwk = match &header.kid {
            Some(kid) => keys.find(kid),
            None if keys.keys.len() == 1 => keys.keys.first(),
            None => None,
        }
        .ok_or_else(|| err("ID token is signed with an unknown key"))?;
        let key = jsonwebtoken::DecodingKey::from_jwk(jwk)
            .map_err(|e| err(format!("issuer key: {e}")))?;
        let mut validation = jsonwebtoken::Validation::new(header.alg);
        validation.set_audience(&[&self.client_id]);
        validation.set_issuer(&[&self.issuer]);
        let claims = jsonwebtoken::decode::<serde_json::Map<String, serde_json::Value>>(
            token,
            &key,
            &validation,
        )
        .map_err(|e| err(format!("ID token: {e}")))?
        .claims;
        if claims.get("nonce").and_then(|v| v.as_str()) != Some(nonce) {
            return Err(err("ID token nonce does not match this login"));
        }
        let exp = claims
            .get("exp")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| err("ID token has no exp"))?;
        let user = [self.username_claim.as_str(), "preferred_username", "sub"]
            .iter()
            .find_map(|c| claims.get(*c).and_then(|v| v.as_str()))
            .unwrap_or("unknown")
            .to_string();
        Ok(Session {
            token: token.to_string(),
            user,
            exp,
        })
    }
}

/// Only same-site paths: never redirect to another origin after login.
fn safe_return_to(path: Option<&str>) -> String {
    match path {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') => p.into(),
        _ => "/".into(),
    }
}

fn problem(status: StatusCode, msg: &str) -> Response {
    tracing::warn!(error = msg, "ui: login failed");
    (
        status,
        format!("Sign-in failed: {msg}\n\nGo back to / to try again."),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct LoginQuery {
    return_to: Option<String>,
}

/// `GET /auth/login`: redirects to the issuer.
pub async fn login(State(ui): State<Arc<UiState>>, Query(q): Query<LoginQuery>) -> Response {
    let Some(oidc) = &ui.oidc else {
        return Redirect::to("/").into_response();
    };
    let login = LoginState {
        state: ui.sealer.random(),
        nonce: ui.sealer.random(),
        verifier: ui.sealer.random(),
        return_to: safe_return_to(q.return_to.as_deref()),
    };
    let url = match oidc.authorize_url(&login).await {
        Ok(u) => u,
        Err(e) => return problem(StatusCode::BAD_GATEWAY, &e.0),
    };
    let sealed = ui.sealer.seal(
        LOGIN_COOKIE,
        &serde_json::to_vec(&login).expect("serializable"),
    );
    let mut resp = Redirect::to(&url).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(LOGIN_COOKIE, &sealed, 600, ui.secure_cookies),
    );
    resp
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /auth/callback`: exchanges the code and sets the session cookie.
pub async fn callback(
    State(ui): State<Arc<UiState>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = &ui.oidc else {
        return Redirect::to("/").into_response();
    };
    if let Some(e) = q.error {
        return problem(
            StatusCode::BAD_REQUEST,
            &format!("{e} {}", q.error_description.unwrap_or_default()),
        );
    }
    let login: Option<LoginState> = cookies(&headers)
        .get(LOGIN_COOKIE)
        .and_then(|c| ui.sealer.open(LOGIN_COOKIE, c))
        .and_then(|p| serde_json::from_slice(&p).ok());
    let (Some(login), Some(code), Some(state)) = (login, q.code, q.state) else {
        return problem(
            StatusCode::BAD_REQUEST,
            "this login expired or was not started here",
        );
    };
    if state != login.state {
        return problem(StatusCode::BAD_REQUEST, "state does not match this login");
    }
    let session = match oidc.exchange(&code, &login).await {
        Ok(s) => s,
        Err(e) => return problem(StatusCode::BAD_GATEWAY, &e.0),
    };
    tracing::info!(user = %session.user, "ui: signed in");
    let sealed = ui.sealer.seal(
        SESSION_COOKIE,
        &serde_json::to_vec(&session).expect("serializable"),
    );
    if sealed.len() > COOKIE_CHUNK * MAX_CHUNKS {
        return problem(
            StatusCode::BAD_GATEWAY,
            "the ID token is too large for cookies",
        );
    }
    let mut resp = Redirect::to(&login.return_to).into_response();
    let h = resp.headers_mut();
    h.append(
        header::SET_COOKIE,
        set_cookie(LOGIN_COOKIE, "", 0, ui.secure_cookies),
    );
    for c in session_cookies(&sealed, (session.exp - now()).max(60), ui.secure_cookies) {
        h.append(header::SET_COOKIE, c);
    }
    resp
}

/// `GET /auth/logout`: drops the session cookie. The issuer's own session stays.
pub async fn logout(State(ui): State<Arc<UiState>>) -> Response {
    let mut resp = Redirect::to("/signed-out").into_response();
    for c in session_cookies("", 0, ui.secure_cookies) {
        resp.headers_mut().append(header::SET_COOKIE, c);
    }
    resp
}

/// Middleware of every `/api` route: resolves the caller or answers 401.
pub async fn require(State(ui): State<Arc<UiState>>, mut req: Request, next: Next) -> Response {
    let identity = if ui.oidc.is_none() {
        Identity {
            client: ui.client.clone(),
            user: None,
            key: "controller".into(),
            exp: None,
            unrestricted: true,
        }
    } else {
        let Some(session) = read_session(&ui.sealer, &cookies(req.headers())) else {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({ "error": "sign in", "login": "/auth/login" })),
            )
                .into_response();
        };
        let key = hex::encode(sha2::Sha256::digest(session.token.as_bytes()));
        let client = match ui.client_for(&key, &session) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "ui: building a user client failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        Identity {
            client,
            user: Some(session.user),
            key,
            exp: Some(session.exp),
            unrestricted: false,
        }
    };
    req.extensions_mut().insert(identity);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens_only_for_the_same_purpose_and_key() {
        let s = Sealer::new(b"secret");
        let sealed = s.seal("a", b"hello");
        assert_eq!(s.open("a", &sealed).as_deref(), Some(&b"hello"[..]));
        assert_eq!(s.open("b", &sealed), None);
        assert_eq!(Sealer::new(b"other").open("a", &sealed), None);
        let mut tampered = sealed.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        assert_eq!(s.open("a", std::str::from_utf8(&tampered).unwrap()), None);
        assert_eq!(s.open("a", "short"), None);
    }

    fn jar(set: &[HeaderValue]) -> HashMap<String, String> {
        let mut headers = HeaderMap::new();
        let pairs: Vec<String> = set
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
            .filter(|kv| !kv.ends_with('='))
            .collect();
        headers.insert(header::COOKIE, pairs.join("; ").parse().unwrap());
        cookies(&headers)
    }

    #[test]
    fn a_large_session_survives_chunking() {
        let s = Sealer::new(b"secret");
        let session = Session {
            token: "t".repeat(9000),
            user: "me@example.com".into(),
            exp: now() + 3600,
        };
        let sealed = s.seal(SESSION_COOKIE, &serde_json::to_vec(&session).unwrap());
        let set = session_cookies(&sealed, 3600, true);
        assert_eq!(set.len(), MAX_CHUNKS);
        assert!(set[0].to_str().unwrap().contains("; Secure"));
        assert!(set.iter().all(|c| c.len() < 4096));
        assert_eq!(read_session(&s, &jar(&set)), Some(session));
    }

    #[test]
    fn an_expired_or_cleared_session_is_no_session() {
        let s = Sealer::new(b"secret");
        let old = Session {
            token: "t".into(),
            user: "u".into(),
            exp: now() - 1,
        };
        let sealed = s.seal(SESSION_COOKIE, &serde_json::to_vec(&old).unwrap());
        assert_eq!(
            read_session(&s, &jar(&session_cookies(&sealed, 60, false))),
            None
        );
        assert_eq!(read_session(&s, &jar(&session_cookies("", 0, false))), None);
    }

    /// An Ed25519 issuer key, its JWK set, and a signer for claims.
    fn issuer() -> (JwkSet, impl Fn(serde_json::Value) -> String) {
        use ring::signature::KeyPair as _;
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&rand::SystemRandom::new()).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let jwks: JwkSet = serde_json::from_value(serde_json::json!({ "keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig", "kid": "k1",
            "x": B64.encode(pair.public_key().as_ref()),
        }]}))
        .unwrap();
        let key = jsonwebtoken::EncodingKey::from_ed_der(pkcs8.as_ref());
        let sign = move |claims: serde_json::Value| {
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
            header.kid = Some("k1".into());
            jsonwebtoken::encode(&header, &claims, &key).unwrap()
        };
        (jwks, sign)
    }

    fn oidc() -> Oidc {
        Oidc::new(
            &gitimage_api::config::UiOidc {
                issuer: "https://issuer.example.com".into(),
                client_id: None,
                scopes: vec!["openid".into()],
                username_claim: "email".into(),
            },
            "https://gitimage.example.com/",
            "client-1".into(),
            None,
        )
        .unwrap()
    }

    fn claims() -> serde_json::Value {
        serde_json::json!({
            "iss": "https://issuer.example.com", "aud": "client-1", "sub": "u1",
            "email": "me@example.com", "nonce": "n1", "exp": now() + 600,
        })
    }

    #[test]
    fn accepts_a_valid_id_token() {
        gitimage_tls::install_crypto_provider();
        let (jwks, sign) = issuer();
        let o = oidc();
        assert_eq!(o.redirect_uri, "https://gitimage.example.com/auth/callback");
        let token = sign(claims());
        let session = o.verify(&token, &jwks, "n1").unwrap();
        assert_eq!(session.user, "me@example.com");
        assert_eq!(session.token, token);
    }

    #[test]
    fn rejects_id_tokens_that_are_not_for_this_login() {
        gitimage_tls::install_crypto_provider();
        let (jwks, sign) = issuer();
        let o = oidc();
        let with = |k: &str, v: serde_json::Value| {
            let mut c = claims();
            c[k] = v;
            sign(c)
        };
        assert!(o.verify(&sign(claims()), &jwks, "other-nonce").is_err());
        assert!(
            o.verify(&with("aud", "someone-else".into()), &jwks, "n1")
                .is_err()
        );
        assert!(
            o.verify(&with("iss", "https://evil.example.com".into()), &jwks, "n1")
                .is_err()
        );
        assert!(
            o.verify(&with("exp", (now() - 600).into()), &jwks, "n1")
                .is_err()
        );
        // Signed by a different key with the same kid.
        let (_, forge) = issuer();
        assert!(o.verify(&forge(claims()), &jwks, "n1").is_err());
        // Shared-secret algorithms are refused outright.
        let hs = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims(),
            &jsonwebtoken::EncodingKey::from_secret(b"client-1"),
        )
        .unwrap();
        assert!(o.verify(&hs, &jwks, "n1").is_err());
    }

    #[test]
    fn return_to_stays_on_this_origin() {
        assert_eq!(safe_return_to(Some("/gi/team-a/myapp")), "/gi/team-a/myapp");
        assert_eq!(safe_return_to(Some("//evil.example.com")), "/");
        assert_eq!(safe_return_to(Some("https://evil.example.com")), "/");
        assert_eq!(safe_return_to(Some("/\\evil.example.com")), "/");
        assert_eq!(safe_return_to(None), "/");
    }
}
