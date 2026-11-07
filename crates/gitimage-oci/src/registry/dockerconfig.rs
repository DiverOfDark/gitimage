//! Picks credentials for a registry host out of a `.dockerconfigjson` Secret value.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use crate::OciError;

#[derive(Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, Entry>,
}

#[derive(Deserialize)]
struct Entry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

/// `https://index.docker.io/v1/` -> `index.docker.io`; Docker Hub aliases fold together.
fn normalize(key: &str) -> String {
    let key = key
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = key
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match host.as_str() {
        "docker.io" | "registry-1.docker.io" => "index.docker.io".into(),
        _ => host,
    }
}

/// Username and password for `host` (`registry:5000`), or `None` if the config has none.
pub(super) fn credentials_for(
    json: &[u8],
    host: &str,
) -> Result<Option<(String, String)>, OciError> {
    let config: DockerConfig = serde_json::from_slice(json)
        .map_err(|e| OciError::Backend(format!("invalid dockerconfigjson: {e}")))?;
    let want = normalize(host);
    let Some(entry) = config
        .auths
        .into_iter()
        .find(|(k, _)| normalize(k) == want)
        .map(|(_, e)| e)
    else {
        return Ok(None);
    };
    if let (Some(u), Some(p)) = (entry.username, entry.password) {
        return Ok(Some((u, p)));
    }
    let Some(auth) = entry.auth else {
        return Ok(None);
    };
    let decoded = STANDARD
        .decode(auth.trim())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| OciError::Backend(format!("invalid auth for {host} in dockerconfigjson")))?;
    let (u, p) = decoded
        .split_once(':')
        .ok_or_else(|| OciError::Backend(format!("invalid auth for {host} in dockerconfigjson")))?;
    Ok(Some((u.to_string(), p.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_host() {
        let json = br#"{"auths":{
            "ghcr.io":{"auth":"dTpw"},
            "https://index.docker.io/v1/":{"username":"hub","password":"pw"},
            "registry:5000":{"auth":"bm8tY29sb24="}
        }}"#;
        assert_eq!(
            credentials_for(json, "ghcr.io").unwrap(),
            Some(("u".into(), "p".into()))
        );
        assert_eq!(
            credentials_for(json, "docker.io").unwrap(),
            Some(("hub".into(), "pw".into()))
        );
        assert_eq!(credentials_for(json, "quay.io").unwrap(), None);
        assert!(credentials_for(json, "registry:5000").is_err());
        assert!(credentials_for(b"nope", "ghcr.io").is_err());
    }
}
