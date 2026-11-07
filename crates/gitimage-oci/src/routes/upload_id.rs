//! Signed upload ids `<pod-ip>.<storage-id>.<mac>`: the replica that owns an upload is
//! readable from the id, and the HMAC (over ip, storage id and repo) stops clients from
//! forging ids, pointing them at other replicas, or reusing them on another repo.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{KeyInit, Mac};

use crate::{Repo, UploadId};

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// MAC length in bytes (hex-encoded in the id).
const MAC_LEN: usize = 16;

/// Who serves an upload.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Owner {
    /// This replica, with the storage's own id.
    Local(UploadId),
    /// Another replica, by pod IP.
    Remote(String),
}

pub(super) struct UploadIds {
    key: Vec<u8>,
    pod_ip: String,
}

impl UploadIds {
    pub(super) fn new(key: Vec<u8>, pod_ip: String) -> Self {
        Self { key, pod_ip }
    }

    fn mac(&self, payload: &str, repo: &Repo) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        mac.update(b"\n");
        mac.update(repo.path().as_bytes());
        mac
    }

    /// Public id for a storage upload id owned by this replica.
    pub(super) fn encode(&self, repo: &Repo, id: &UploadId) -> String {
        let payload = format!("{}.{}", self.pod_ip, URL_SAFE_NO_PAD.encode(&id.0));
        let tag = self.mac(&payload, repo).finalize().into_bytes();
        format!("{payload}.{}", hex::encode(&tag[..MAC_LEN]))
    }

    /// Verifies the id and says which replica owns it; `None` for malformed or forged ids.
    pub(super) fn decode(&self, repo: &Repo, id: &str) -> Option<Owner> {
        let (payload, tag) = id.rsplit_once('.')?;
        let tag = hex::decode(tag).ok().filter(|t| t.len() == MAC_LEN)?;
        self.mac(payload, repo).verify_truncated_left(&tag).ok()?;
        let (ip, storage_id) = payload.rsplit_once('.')?;
        if ip != self.pod_ip {
            return Some(Owner::Remote(ip.to_string()));
        }
        let storage_id = URL_SAFE_NO_PAD.decode(storage_id).ok()?;
        Some(Owner::Local(UploadId(String::from_utf8(storage_id).ok()?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_owner_and_tamper() {
        let repo = Repo::new("ns", "app");
        let a = UploadIds::new(b"key".to_vec(), "10.0.0.1".into());
        let b = UploadIds::new(b"key".to_vec(), "fd00::2".into());
        let id = a.encode(&repo, &UploadId("u.1-x".into()));
        assert_eq!(
            a.decode(&repo, &id),
            Some(Owner::Local(UploadId("u.1-x".into())))
        );
        assert_eq!(b.decode(&repo, &id), Some(Owner::Remote("10.0.0.1".into())));
        let v6 = b.encode(&repo, &UploadId("y".into()));
        assert_eq!(a.decode(&repo, &v6), Some(Owner::Remote("fd00::2".into())));

        assert_eq!(a.decode(&Repo::new("ns", "other"), &id), None);
        assert_eq!(
            a.decode(&repo, &id.replacen("10.0.0.1", "10.0.0.9", 1)),
            None
        );
        assert_eq!(a.decode(&repo, "garbage"), None);
        let other_key = UploadIds::new(b"other".to_vec(), "10.0.0.1".into());
        assert_eq!(other_key.decode(&repo, &id), None);
    }
}
