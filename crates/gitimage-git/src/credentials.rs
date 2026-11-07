//! Git credentials from a Flux-format Secret. `Debug` never prints secret material.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, PartialEq, Eq, Default)]
pub enum Credentials {
    #[default]
    None,
    /// `username` + `password` keys (a token works as the password).
    Https { username: String, password: String },
    /// `identity` (private key, OpenSSH PEM) + `known_hosts` keys.
    Ssh {
        identity: String,
        known_hosts: String,
    },
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("Credentials::None"),
            Self::Https { username, .. } => {
                write!(
                    f,
                    "Credentials::Https {{ username: {username:?}, password: *** }}"
                )
            }
            Self::Ssh { .. } => f.write_str("Credentials::Ssh { identity: ***, known_hosts: *** }"),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredentialsError {
    #[error("secret has key {0:?} but not {1:?}")]
    Incomplete(&'static str, &'static str),
    #[error("secret has neither identity nor username/password")]
    Unrecognized,
    #[error("secret key {0:?} is not UTF-8")]
    NotUtf8(&'static str),
    #[error("ssh secret needs known_hosts: GitImage never trusts an unknown host key")]
    NoKnownHosts,
}

impl Credentials {
    /// Builds credentials from a Secret's decoded `data`.
    pub fn from_secret_data(data: &BTreeMap<String, Vec<u8>>) -> Result<Self, CredentialsError> {
        let get = |k: &'static str| -> Result<Option<String>, CredentialsError> {
            data.get(k)
                .map(|v| String::from_utf8(v.clone()).map_err(|_| CredentialsError::NotUtf8(k)))
                .transpose()
        };
        if let Some(identity) = get("identity")? {
            let known_hosts = get("known_hosts")?.ok_or(CredentialsError::NoKnownHosts)?;
            return Ok(Self::Ssh {
                identity,
                known_hosts,
            });
        }
        match (get("username")?, get("password")?) {
            (Some(username), Some(password)) => Ok(Self::Https { username, password }),
            (Some(_), None) => Err(CredentialsError::Incomplete("username", "password")),
            (None, Some(password)) => Ok(Self::Https {
                username: "git".into(),
                password,
            }),
            (None, None) => Err(CredentialsError::Unrecognized),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn https() {
        let c = Credentials::from_secret_data(&data(&[("username", "u"), ("password", "p")]));
        assert_eq!(
            c,
            Ok(Credentials::Https {
                username: "u".into(),
                password: "p".into()
            })
        );
    }

    #[test]
    fn ssh_needs_known_hosts() {
        let c = Credentials::from_secret_data(&data(&[("identity", "key")]));
        assert_eq!(c, Err(CredentialsError::NoKnownHosts));
    }

    #[test]
    fn debug_redacts() {
        let c = Credentials::Https {
            username: "u".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
        let s = Credentials::Ssh {
            identity: "PRIVATE".into(),
            known_hosts: "x".into(),
        };
        assert!(!format!("{s:?}").contains("PRIVATE"));
    }
}
