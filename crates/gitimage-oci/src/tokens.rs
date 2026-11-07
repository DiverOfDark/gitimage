//! Short-lived push tokens (HS256): one per build Job, scoped to `<ns>/<name>`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub ns: String,
    pub name: String,
    /// UID of the build Job; the token dies with the Job.
    pub job_uid: String,
    /// Unix seconds.
    pub exp: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid token: {0}")]
    Invalid(String),
}

/// Signs `claims` with `key` (32+ random bytes from Secret `gitimage-push-token-key`).
pub fn sign(key: &[u8], claims: &Claims) -> Result<String, TokenError> {
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        claims,
        &jsonwebtoken::EncodingKey::from_secret(key),
    )
    .map_err(|e| TokenError::Invalid(e.to_string()))
}

/// Verifies signature and expiry. Scope and Job liveness are checked by the caller.
pub fn verify(key: &[u8], token: &str) -> Result<Claims, TokenError> {
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.set_required_spec_claims(&["exp"]);
    validation.leeway = 30;
    jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(key),
        &validation,
    )
    .map(|d| d.claims)
    .map_err(|e| TokenError::Invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_wrong_key() {
        let claims = Claims {
            ns: "team-a".into(),
            name: "myapp".into(),
            job_uid: "uid".into(),
            exp: 4_000_000_000,
        };
        let t = sign(b"0123456789abcdef0123456789abcdef", &claims).unwrap();
        assert_eq!(
            verify(b"0123456789abcdef0123456789abcdef", &t).unwrap(),
            claims
        );
        assert!(verify(b"another-key-another-key-another-k", &t).is_err());
    }

    #[test]
    fn expired_is_rejected() {
        let claims = Claims {
            ns: "a".into(),
            name: "b".into(),
            job_uid: "c".into(),
            exp: 1,
        };
        let t = sign(b"k", &claims).unwrap();
        assert!(verify(b"k", &t).is_err());
    }
}
