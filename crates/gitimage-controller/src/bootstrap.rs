//! Startup objects in the controller namespace (design D5): the CA, the webhook and push
//! serving certificates, the push-token and upload keys, and the webhook `caBundle`.
//! Every replica runs `ensure` (create-if-absent is race-safe); the leader renews.

use std::collections::BTreeMap;

use base64::Engine as _;
use gitimage_tls::{CertPair, generate_ca, issue_server_cert, needs_renewal};
use k8s_openapi::ByteString;
use k8s_openapi::api::admissionregistration::v1::MutatingWebhookConfiguration;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::{Api, Patch, PatchParams, PostParams};

use crate::util::{get_or_create_secret, secret_data};

pub const CA_SECRET: &str = "gitimage-ca";
pub const WEBHOOK_TLS_SECRET: &str = "gitimage-webhook-tls";
pub const PUSH_TLS_SECRET: &str = "gitimage-push-tls";
pub const KEYS_SECRET: &str = "gitimage-keys";
pub const WEBHOOK_CONFIG: &str = "gitimage";

pub struct Keys {
    pub ca: CertPair,
    pub push_token_key: Vec<u8>,
    pub upload_key: Vec<u8>,
}

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([("app.kubernetes.io/managed-by".into(), "gitimage".into())])
}

fn pair_from(s: &Secret) -> Option<CertPair> {
    let d = secret_data(s);
    Some(CertPair {
        cert_pem: String::from_utf8(d.get("tls.crt")?.clone()).ok()?,
        key_pem: String::from_utf8(d.get("tls.key")?.clone()).ok()?,
    })
}

fn pair_data(p: &CertPair, ca_pem: &str) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        ("tls.crt".into(), p.cert_pem.clone().into_bytes()),
        ("tls.key".into(), p.key_pem.clone().into_bytes()),
        ("ca.crt".into(), ca_pem.as_bytes().to_vec()),
    ])
}

/// DNS names of the webhook Service.
pub fn webhook_names(ns: &str) -> Vec<String> {
    vec![
        format!("gitimage-webhook.{ns}.svc"),
        format!("gitimage-webhook.{ns}.svc.cluster.local"),
    ]
}

/// DNS names of the push Service: the configured host plus the standard ones.
pub fn push_names(ns: &str, host: &str) -> Vec<String> {
    let mut v = vec![
        host.to_string(),
        format!("gitimage-push.{ns}.svc"),
        format!("gitimage-push.{ns}.svc.cluster.local"),
    ];
    v.dedup();
    v
}

async fn ensure_cert(
    api: &Api<Secret>,
    name: &str,
    ca: &CertPair,
    names: &[String],
    renew: bool,
) -> anyhow::Result<()> {
    let existing = api.get_opt(name).await?;
    let current = existing.as_ref().and_then(pair_from);
    if current.is_some()
        && !(renew && needs_renewal(current.as_ref().map(|p| p.cert_pem.as_str()), names))
    {
        return Ok(());
    }
    let pair = issue_server_cert(ca, names)?;
    let secret = Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(name.into()),
            labels: Some(labels()),
            ..Default::default()
        },
        type_: Some("kubernetes.io/tls".into()),
        data: Some(
            pair_data(&pair, &ca.cert_pem)
                .into_iter()
                .map(|(k, v)| (k, ByteString(v)))
                .collect(),
        ),
        ..Default::default()
    };
    match existing {
        None => match api.create(&PostParams::default(), &secret).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            Err(e) => return Err(e.into()),
        },
        Some(old) => {
            let mut s = secret;
            s.metadata.resource_version = old.metadata.resource_version;
            api.replace(name, &PostParams::default(), &s).await?;
            tracing::info!(secret = name, "renewed serving certificate");
        }
    }
    Ok(())
}

/// Ensures every bootstrap object exists. With `renew`, also re-issues certificates that
/// expire within 30 days or no longer match the configured names.
pub async fn ensure(
    client: &Client,
    ns: &str,
    push_host: &str,
    renew: bool,
) -> anyhow::Result<Keys> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let ca_secret = get_or_create_secret(
        &api,
        CA_SECRET,
        || {
            let ca = generate_ca()?;
            Ok(BTreeMap::from([
                ("tls.crt".into(), ca.cert_pem.into_bytes()),
                ("tls.key".into(), ca.key_pem.into_bytes()),
            ]))
        },
        labels(),
    )
    .await?;
    let ca = pair_from(&ca_secret)
        .ok_or_else(|| anyhow::anyhow!("secret {CA_SECRET} lacks tls.crt/tls.key"))?;
    ensure_cert(&api, WEBHOOK_TLS_SECRET, &ca, &webhook_names(ns), renew).await?;
    ensure_cert(
        &api,
        PUSH_TLS_SECRET,
        &ca,
        &push_names(ns, push_host),
        renew,
    )
    .await?;
    let keys = get_or_create_secret(
        &api,
        KEYS_SECRET,
        || {
            use rand::RngExt as _;
            let mut rng = rand::rng();
            let a: [u8; 32] = rng.random();
            let b: [u8; 32] = rng.random();
            Ok(BTreeMap::from([
                ("push-token-key".into(), a.to_vec()),
                ("upload-key".into(), b.to_vec()),
            ]))
        },
        labels(),
    )
    .await?;
    let kd = secret_data(&keys);
    let push_token_key = kd
        .get("push-token-key")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{KEYS_SECRET}: no push-token-key"))?;
    let upload_key = kd
        .get("upload-key")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{KEYS_SECRET}: no upload-key"))?;
    patch_ca_bundle(client, &ca.cert_pem).await;
    Ok(Keys {
        ca,
        push_token_key,
        upload_key,
    })
}

/// Writes the CA into every entry of the `gitimage` MutatingWebhookConfiguration.
async fn patch_ca_bundle(client: &Client, ca_pem: &str) {
    let api: Api<MutatingWebhookConfiguration> = Api::all(client.clone());
    let Ok(Some(cfg)) = api.get_opt(WEBHOOK_CONFIG).await else {
        tracing::warn!("MutatingWebhookConfiguration {WEBHOOK_CONFIG} not found; caBundle not set");
        return;
    };
    let bundle = base64::engine::general_purpose::STANDARD.encode(ca_pem);
    let webhooks = cfg.webhooks.unwrap_or_default();
    if webhooks.iter().all(|w| {
        w.client_config
            .ca_bundle
            .as_ref()
            .map(|b| base64::engine::general_purpose::STANDARD.encode(&b.0))
            == Some(bundle.clone())
    }) {
        return;
    }
    let ops: Vec<serde_json::Value> = (0..webhooks.len())
        .map(|i| serde_json::json!({ "op": "add", "path": format!("/webhooks/{i}/clientConfig/caBundle"), "value": bundle }))
        .collect();
    let patch: json_patch::Patch = match serde_json::from_value(serde_json::Value::Array(ops)) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "caBundle patch build failed");
            return;
        }
    };
    match api
        .patch(
            WEBHOOK_CONFIG,
            &PatchParams::default(),
            &Patch::<()>::Json(patch),
        )
        .await
    {
        Ok(_) => tracing::info!("patched webhook caBundle"),
        Err(e) => tracing::warn!(error = %e, "caBundle patch failed"),
    }
}
