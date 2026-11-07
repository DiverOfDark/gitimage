//! `known_hosts` content (from the Secret, not a file): plain and hashed (`|1|salt|hash`)
//! host patterns, `[host]:port`, wildcards, negation and the `@revoked` marker.
//! Unknown hosts are rejected: there is no trust on first use.

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{Algorithm, EcdsaCurve, HashAlg, PublicKey};
use sha1::Sha1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HostKeyError {
    #[error("host key of {host} is not in known_hosts (GitImage never trusts on first use)")]
    Unknown { host: String },
    #[error(
        "host key of {host} does not match known_hosts: server presented {presented}, known_hosts has {known}"
    )]
    Mismatch {
        host: String,
        presented: String,
        known: String,
    },
    #[error("host key {presented} of {host} is marked @revoked in known_hosts")]
    Revoked { host: String, presented: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    None,
    Revoked,
    CertAuthority,
}

#[derive(Debug, Clone)]
struct Entry {
    marker: Marker,
    patterns: Vec<String>,
    key: PublicKey,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct KnownHosts {
    entries: Vec<Entry>,
}

/// The name OpenSSH looks up: `host` on port 22, `[host]:port` otherwise.
pub(crate) fn host_port_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

impl KnownHosts {
    /// Parses known_hosts content. Malformed lines are skipped (with a warning naming the line
    /// number); they can never make a host trusted.
    pub(crate) fn parse(content: &str) -> Self {
        let mut entries = Vec::new();
        for (n, raw) in content.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match parse_line(line) {
                Some(e) => entries.push(e),
                None => tracing::warn!(line = n + 1, "skipping malformed known_hosts line"),
            }
        }
        Self { entries }
    }

    fn matching(&self, name: &str) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(move |e| host_matches(name, &e.patterns))
    }

    /// Keys trusted for `host:port` (no CA or revoked entries).
    pub(crate) fn keys_for(&self, host: &str, port: u16) -> Vec<&PublicKey> {
        let name = host_port_name(host, port);
        self.matching(&name)
            .filter(|e| e.marker == Marker::None)
            .map(|e| &e.key)
            .collect()
    }

    /// Host key algorithms to offer, in russh's preference order, restricted to the key types
    /// known_hosts has for this host. Negotiating only those makes the server present a key we
    /// can actually verify, even when it also has keys of other types.
    pub(crate) fn algorithms_for(&self, host: &str, port: u16) -> Vec<Algorithm> {
        let keys = self.keys_for(host, port);
        let has = |want: &dyn Fn(&Algorithm) -> bool| keys.iter().any(|k| want(&k.algorithm()));
        let mut out = Vec::new();
        if has(&|a| *a == Algorithm::Ed25519) {
            out.push(Algorithm::Ed25519);
        }
        for curve in [
            EcdsaCurve::NistP256,
            EcdsaCurve::NistP384,
            EcdsaCurve::NistP521,
        ] {
            if has(&|a| *a == Algorithm::Ecdsa { curve }) {
                out.push(Algorithm::Ecdsa { curve });
            }
        }
        if has(&|a| matches!(a, Algorithm::Rsa { .. })) {
            out.push(Algorithm::Rsa {
                hash: Some(HashAlg::Sha512),
            });
            out.push(Algorithm::Rsa {
                hash: Some(HashAlg::Sha256),
            });
        }
        out
    }

    /// Accepts `presented` for `host:port` only if a matching, non-revoked entry has that key.
    pub(crate) fn verify(
        &self,
        host: &str,
        port: u16,
        presented: &PublicKey,
    ) -> Result<(), HostKeyError> {
        let name = host_port_name(host, port);
        let fingerprint = presented.fingerprint(HashAlg::Sha256).to_string();
        let mut known = Vec::new();
        for e in self.matching(&name) {
            match e.marker {
                Marker::Revoked if e.key.key_data() == presented.key_data() => {
                    return Err(HostKeyError::Revoked {
                        host: name.clone(),
                        presented: fingerprint,
                    });
                }
                Marker::None => known.push(e),
                _ => {}
            }
        }
        if known
            .iter()
            .any(|e| e.key.key_data() == presented.key_data())
        {
            return Ok(());
        }
        if known.is_empty() {
            return Err(HostKeyError::Unknown { host: name });
        }
        Err(HostKeyError::Mismatch {
            host: name,
            presented: format!("{} {fingerprint}", presented.algorithm()),
            known: known
                .iter()
                .map(|e| {
                    format!(
                        "{} {}",
                        e.key.algorithm(),
                        e.key.fingerprint(HashAlg::Sha256)
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        })
    }
}

fn parse_line(line: &str) -> Option<Entry> {
    let mut fields = line.split_whitespace();
    let mut first = fields.next()?;
    let marker = match first {
        "@revoked" => Marker::Revoked,
        "@cert-authority" => Marker::CertAuthority,
        m if m.starts_with('@') => return None,
        _ => Marker::None,
    };
    if marker != Marker::None {
        first = fields.next()?;
    }
    let key_type = fields.next()?;
    let key_b64 = fields.next()?;
    let key = PublicKey::from_openssh(&format!("{key_type} {key_b64}")).ok()?;
    Some(Entry {
        marker,
        patterns: first.split(',').map(str::to_owned).collect(),
        key,
    })
}

/// OpenSSH `match_hostname`: any positive match and no negated match.
fn host_matches(name: &str, patterns: &[String]) -> bool {
    let mut matched = false;
    for p in patterns {
        if let Some(hashed) = p.strip_prefix("|1|") {
            if hashed_matches(name, hashed) {
                matched = true;
            }
        } else if let Some(neg) = p.strip_prefix('!') {
            if wildcard(&neg.to_ascii_lowercase(), name) {
                return false;
            }
        } else if wildcard(&p.to_ascii_lowercase(), name) {
            matched = true;
        }
    }
    matched
}

fn hashed_matches(name: &str, salt_and_hash: &str) -> bool {
    let Some((salt, hash)) = salt_and_hash.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    let Ok(mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.chain_update(name.as_bytes())
        .verify_slice(&hash)
        .is_ok()
}

/// `*` and `?` glob over bytes.
fn wildcard(pattern: &str, s: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), s.as_bytes());
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real public host keys of github.com (published by GitHub).
    const GH_ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
    const GH_ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBEmKSENjQEezOmxkZMy7opKgwFB9nkt5YRrYMjNuG5N87uRgg6CLrbo5wAdT/y6v0mKV0U2w0WZ2YB/++Tpockg=";
    // An unrelated ed25519 key.
    const OTHER_ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOhIYPxgAjuXwr0j00r24MM3+/JfIzCZv2VX1RlPNnBP";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    #[test]
    fn plain_entry_with_multiple_hosts() {
        let kh = KnownHosts::parse(&format!(
            "# comment\n\ngitlab.com,github.com {GH_ED25519} some comment\n"
        ));
        assert_eq!(kh.verify("github.com", 22, &key(GH_ED25519)), Ok(()));
        assert_eq!(kh.verify("GitHub.com", 22, &key(GH_ED25519)), Ok(()));
        assert_eq!(
            kh.verify("example.com", 22, &key(GH_ED25519)),
            Err(HostKeyError::Unknown {
                host: "example.com".into()
            })
        );
    }

    #[test]
    fn mismatch_is_rejected_with_fingerprints() {
        let kh = KnownHosts::parse(&format!("github.com {GH_ED25519}\n"));
        let err = kh
            .verify("github.com", 22, &key(OTHER_ED25519))
            .unwrap_err();
        assert!(matches!(err, HostKeyError::Mismatch { .. }));
        let msg = err.to_string();
        assert!(msg.contains("SHA256:"), "{msg}");
        assert!(msg.contains("ssh-ed25519"), "{msg}");
    }

    #[test]
    fn non_default_port_uses_bracket_form() {
        let kh = KnownHosts::parse(&format!("[git.example.com]:2222 {GH_ED25519}\n"));
        assert_eq!(kh.verify("git.example.com", 2222, &key(GH_ED25519)), Ok(()));
        assert!(kh.verify("git.example.com", 22, &key(GH_ED25519)).is_err());
        assert!(
            kh.verify("git.example.com", 2223, &key(GH_ED25519))
                .is_err()
        );
    }

    #[test]
    fn hashed_entries() {
        // `ssh-keygen -H` output for "github.com" and "[localhost]:2222"
        let salt = BASE64.encode(b"0123456789abcdefghij");
        let hash = |name: &str| {
            let mac = Hmac::<Sha1>::new_from_slice(b"0123456789abcdefghij")
                .unwrap()
                .chain_update(name.as_bytes());
            BASE64.encode(&mac.finalize().into_bytes())
        };
        let kh = KnownHosts::parse(&format!(
            "|1|{salt}|{} {GH_ED25519}\n|1|{salt}|{} {GH_ECDSA}\n",
            hash("github.com"),
            hash("[localhost]:2222"),
        ));
        assert_eq!(kh.verify("github.com", 22, &key(GH_ED25519)), Ok(()));
        assert_eq!(kh.verify("localhost", 2222, &key(GH_ECDSA)), Ok(()));
        assert!(kh.verify("localhost", 22, &key(GH_ECDSA)).is_err());
        assert!(kh.verify("gitlab.com", 22, &key(GH_ED25519)).is_err());
    }

    #[test]
    fn hashed_pattern_parsing() {
        // Hash of "localhost" with an arbitrary salt; malformed hashes never match.
        let salt = "8J4XZA6pmbAm0j9Nk7nLGWaGeAo=";
        let salt_bytes = BASE64.decode(salt.as_bytes()).unwrap();
        let mac = Hmac::<Sha1>::new_from_slice(&salt_bytes)
            .unwrap()
            .chain_update(b"localhost");
        let hash = BASE64.encode(&mac.finalize().into_bytes());
        assert!(hashed_matches("localhost", &format!("{salt}|{hash}")));
        assert!(!hashed_matches("localhost2", &format!("{salt}|{hash}")));
        assert!(!hashed_matches("localhost", "garbage"));
    }

    #[test]
    fn multiple_key_types_and_algorithms() {
        let kh = KnownHosts::parse(&format!("github.com {GH_ED25519}\ngithub.com {GH_ECDSA}\n"));
        assert_eq!(kh.keys_for("github.com", 22).len(), 2);
        assert_eq!(kh.verify("github.com", 22, &key(GH_ECDSA)), Ok(()));
        assert_eq!(
            kh.algorithms_for("github.com", 22),
            vec![
                Algorithm::Ed25519,
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP256
                }
            ]
        );
        assert!(kh.algorithms_for("gitlab.com", 22).is_empty());
    }

    #[test]
    fn wildcards_negation_and_markers() {
        let kh = KnownHosts::parse(&format!(
            "*.example.com,!evil.example.com {GH_ED25519}\n@revoked * {OTHER_ED25519}\n@cert-authority *.example.com {GH_ECDSA}\nbogus line\n"
        ));
        assert_eq!(kh.verify("git.example.com", 22, &key(GH_ED25519)), Ok(()));
        assert!(kh.verify("evil.example.com", 22, &key(GH_ED25519)).is_err());
        assert!(matches!(
            kh.verify("git.example.com", 22, &key(OTHER_ED25519)),
            Err(HostKeyError::Revoked { .. })
        ));
        // CA keys are not host keys
        assert!(kh.verify("git.example.com", 22, &key(GH_ECDSA)).is_err());
        assert!(wildcard("git?.host", "git1.host"));
        assert!(!wildcard("git?.host", "git12.host"));
    }
}
