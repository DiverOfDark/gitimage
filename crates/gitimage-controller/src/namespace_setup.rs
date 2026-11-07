//! Per-tenant-namespace objects: the build ConfigMap (CA + buildkitd.toml), the build egress
//! NetworkPolicy, and a copy of the registry pull Secret.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::Api;

use crate::context::Ctx;
use crate::job;
use crate::util::apply;

/// Name of the pull Secret copy in tenant namespaces (what the webhook references).
pub const PULL_SECRET: &str = "gitimage-pull";

static DONE: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
const REFRESH: Duration = Duration::from_secs(600);

/// Applies the namespace objects at most every 10 minutes per namespace.
pub async fn ensure(ctx: &Ctx, ns: &str) -> Result<(), kube::Error> {
    {
        let mut done = DONE.lock().unwrap();
        let map = done.get_or_insert_with(HashMap::new);
        if map.get(ns).is_some_and(|t| t.elapsed() < REFRESH) {
            return Ok(());
        }
    }
    let cms: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    apply(
        &cms,
        job::BUILD_CONFIGMAP,
        &job::build_configmap(ns, &ctx.config, &ctx.ca_pem),
    )
    .await?;
    let nps: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), ns);
    apply(
        &nps,
        job::EGRESS_POLICY,
        &job::egress_policy(ns, &ctx.config, &ctx.namespace),
    )
    .await?;
    if let Some(src) = ctx
        .config
        .storage
        .registry
        .as_ref()
        .and_then(|r| r.pull_secret.clone())
    {
        let from: Api<Secret> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
        if let Some(s) = from.get_opt(&src).await? {
            let copy = Secret {
                metadata: kube::core::ObjectMeta {
                    name: Some(PULL_SECRET.into()),
                    namespace: Some(ns.into()),
                    labels: Some(BTreeMap::from([(
                        "app.kubernetes.io/managed-by".into(),
                        "gitimage".into(),
                    )])),
                    ..Default::default()
                },
                data: s.data.clone(),
                type_: s.type_.clone(),
                ..Default::default()
            };
            let to: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
            apply(&to, PULL_SECRET, &copy).await?;
        } else {
            tracing::warn!(secret = %src, "storage.registry.pullSecret not found in the controller namespace");
        }
    }
    DONE.lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(ns.into(), Instant::now());
    Ok(())
}
