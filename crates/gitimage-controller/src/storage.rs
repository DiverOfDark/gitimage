//! Builds the configured storage backend. Credentials come from Secrets in the controller
//! namespace and never leave it (PRD: builds never see storage credentials).

use gitimage_api::Config;
use gitimage_api::config::StorageType;
use gitimage_oci::DynStorage;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::Api;

use crate::util::secret_data;

async fn secret(
    client: &Client,
    ns: &str,
    name: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, Vec<u8>>> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let s = api
        .get_opt(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("storage secret {ns}/{name} not found"))?;
    Ok(secret_data(&s))
}

fn utf8(d: &std::collections::BTreeMap<String, Vec<u8>>, keys: &[&str]) -> anyhow::Result<String> {
    for k in keys {
        if let Some(v) = d.get(*k) {
            return Ok(String::from_utf8(v.clone())?.trim().to_string());
        }
    }
    anyhow::bail!("secret lacks any of {keys:?}")
}

/// S3 credentials from a Secret: `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`
/// (also accepts `accessKey`/`secretKey`, as Rook's ObjectBucketClaim Secrets are named).
pub async fn s3_credentials(
    client: &Client,
    ns: &str,
    name: &str,
) -> anyhow::Result<(String, String)> {
    let d = secret(client, ns, name).await?;
    Ok((
        utf8(&d, &["AWS_ACCESS_KEY_ID", "accessKey", "access_key"])?,
        utf8(&d, &["AWS_SECRET_ACCESS_KEY", "secretKey", "secret_key"])?,
    ))
}

pub async fn build(cfg: &Config, client: &Client, ns: &str) -> anyhow::Result<DynStorage> {
    match cfg.storage.kind {
        StorageType::S3 => {
            let s3 = cfg
                .storage
                .s3
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("storage.s3 missing"))?;
            let (access, secret_key) = s3_credentials(client, ns, &s3.secret).await?;
            let store = gitimage_oci::S3Storage::new(s3, access, secret_key)?;
            let _ = store; // MERGE-TODO: Ok(Arc::new(store)) once feat/oci lands
            anyhow::bail!("s3 storage not built yet")
        }
        StorageType::Registry => {
            let r = cfg
                .storage
                .registry
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("storage.registry missing"))?;
            let d = secret(client, ns, &r.push_secret).await?;
            let json = d
                .get(".dockerconfigjson")
                .or_else(|| d.get("config.json"))
                .ok_or_else(|| {
                    anyhow::anyhow!("push secret {} lacks .dockerconfigjson", r.push_secret)
                })?;
            let store =
                gitimage_oci::RegistryStorage::from_dockerconfigjson(&r.url, json, r.insecure)?;
            let _ = store; // MERGE-TODO: Ok(Arc::new(store)) once feat/oci lands
            anyhow::bail!("registry storage not built yet")
        }
    }
}
