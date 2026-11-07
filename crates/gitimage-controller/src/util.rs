//! Small helpers shared by the loops.

use std::collections::BTreeMap;

use gitimage_api::Condition;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::core::ObjectMeta;

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Sets a condition, keeping `lastTransitionTime` when the status did not change.
pub fn set_condition(
    conditions: &mut Vec<Condition>,
    type_: &str,
    status: bool,
    reason: &str,
    message: &str,
    generation: Option<i64>,
) {
    let status_str = if status { "True" } else { "False" };
    if let Some(c) = conditions.iter_mut().find(|c| c.type_ == type_) {
        if c.status != status_str {
            c.last_transition_time = now_rfc3339();
        }
        c.status = status_str.into();
        c.reason = reason.into();
        c.message = message.into();
        c.observed_generation = generation;
    } else {
        conditions.push(Condition {
            type_: type_.into(),
            status: status_str.into(),
            reason: reason.into(),
            message: message.into(),
            last_transition_time: now_rfc3339(),
            observed_generation: generation,
        });
    }
}

/// Decoded data of a Secret.
pub fn secret_data(s: &Secret) -> BTreeMap<String, Vec<u8>> {
    let mut out: BTreeMap<String, Vec<u8>> = s
        .data
        .as_ref()
        .map(|d| d.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect())
        .unwrap_or_default();
    if let Some(sd) = &s.string_data {
        for (k, v) in sd {
            out.insert(k.clone(), v.as_bytes().to_vec());
        }
    }
    out
}

/// Gets a Secret, or creates it from `make()` if absent. Race-safe across replicas:
/// on AlreadyExists the winner's Secret is read back.
pub async fn get_or_create_secret(
    api: &Api<Secret>,
    name: &str,
    make: impl FnOnce() -> anyhow::Result<BTreeMap<String, Vec<u8>>>,
    labels: BTreeMap<String, String>,
) -> anyhow::Result<Secret> {
    if let Some(s) = api.get_opt(name).await? {
        return Ok(s);
    }
    let data = make()?;
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(name.into()),
            labels: Some(labels),
            ..Default::default()
        },
        data: Some(data.into_iter().map(|(k, v)| (k, ByteString(v))).collect()),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &secret).await {
        Ok(s) => Ok(s),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(api.get(name).await?),
        Err(e) => Err(e.into()),
    }
}

/// Server-side apply with the GitImage field manager, forced.
pub async fn apply<K>(api: &Api<K>, name: &str, obj: &K) -> kube::Result<K>
where
    K: kube::Resource + Clone + serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let pp = PatchParams::apply(gitimage_api::keys::FIELD_MANAGER).force();
    api.patch(name, &pp, &Patch::Apply(obj)).await
}

/// `host/path` of a git URL, lowercased host, without scheme, user, port or `.git`.
/// Two URLs of the same repo (ssh and https) normalize equal.
pub fn normalize_git_url(url: &str) -> String {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
        rest.split_once('/').unwrap_or((rest, ""))
    } else if let Some((left, path)) = url.split_once(':') {
        (left.rsplit_once('@').map(|(_, h)| h).unwrap_or(left), path)
    } else {
        (url, "")
    };
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let path = path
        .trim_matches('/')
        .trim_end_matches(".git")
        .trim_matches('/');
    format!("{host}/{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_ssh_and_https_alike() {
        let a = normalize_git_url("git@GitHub.com:You/App.git");
        let b = normalize_git_url("https://user:tok@github.com/You/App");
        let c = normalize_git_url("ssh://git@github.com:22/You/App.git/");
        assert_eq!(a, "github.com/You/App");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn condition_transition_time_moves_only_on_change() {
        let mut c = Vec::new();
        set_condition(&mut c, "Ready", false, "Pending", "", None);
        let t0 = c[0].last_transition_time.clone();
        set_condition(&mut c, "Ready", false, "Pending", "again", None);
        assert_eq!(c[0].last_transition_time, t0);
        assert_eq!(c[0].message, "again");
    }
}
