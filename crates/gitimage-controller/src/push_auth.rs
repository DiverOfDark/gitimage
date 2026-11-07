//! Push-endpoint authorization: a build's token is valid only for its own `<ns>/<name>` and
//! only while its Job runs. Every replica checks against its own Job reflector.

use base64::Engine as _;
use gitimage_api::keys;
use gitimage_oci::tokens;
use gitimage_oci::{AuthCheck, OciError, Scope};
use k8s_openapi::api::batch::v1::Job;
use kube::runtime::reflector::Store;

pub struct JobTokenAuth {
    pub key: Vec<u8>,
    pub jobs: Store<Job>,
}

/// The token from `Basic base64(user:token)` or `Bearer token`.
pub fn token_from_header(header: &str) -> Option<String> {
    let (scheme, value) = header.trim().split_once(' ')?;
    match scheme.to_ascii_lowercase().as_str() {
        "bearer" => Some(value.trim().to_string()),
        "basic" => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(value.trim())
                .ok()?;
            let s = String::from_utf8(raw).ok()?;
            s.split_once(':').map(|(_, p)| p.to_string())
        }
        _ => None,
    }
}

fn job_running(job: &Job) -> bool {
    let Some(st) = &job.status else { return true };
    st.succeeded.unwrap_or(0) == 0
        && st.failed.unwrap_or(0) == 0
        && !st
            .conditions
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
}

impl JobTokenAuth {
    pub fn check_claims(
        &self,
        claims: &tokens::Claims,
        scope: Option<&Scope>,
    ) -> Result<(), OciError> {
        if let Some(scope) = scope
            && (scope.repo.namespace != claims.ns || scope.repo.name != claims.name)
        {
            return Err(OciError::Denied);
        }
        let alive = self.jobs.state().iter().any(|j| {
            j.metadata.uid.as_deref() == Some(claims.job_uid.as_str())
                && j.metadata.namespace.as_deref() == Some(claims.ns.as_str())
                && j.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(keys::NAME_LABEL))
                    == Some(&claims.name)
                && job_running(j)
        });
        if alive { Ok(()) } else { Err(OciError::Denied) }
    }
}

#[async_trait::async_trait]
impl AuthCheck for JobTokenAuth {
    async fn check(
        &self,
        authorization: Option<&str>,
        scope: Option<&Scope>,
    ) -> Result<(), OciError> {
        let token = authorization
            .and_then(token_from_header)
            .ok_or(OciError::Unauthorized)?;
        let claims = tokens::verify(&self.key, &token).map_err(|_| OciError::Unauthorized)?;
        self.check_claims(&claims, scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitimage_oci::{Access, Repo};
    use kube::runtime::reflector::store::Writer;
    use kube::runtime::watcher;

    fn job(uid: &str, ns: &str, name: &str, done: bool) -> Job {
        serde_yaml::from_str(&format!(
            "metadata: {{ uid: {uid}, name: {uid}, namespace: {ns}, labels: {{ gitimage.dev/name: {name} }} }}\nstatus: {{ succeeded: {} }}",
            if done { 1 } else { 0 }
        ))
        .unwrap()
    }

    fn auth(jobs: Vec<Job>) -> JobTokenAuth {
        let mut w: Writer<Job> = Writer::default();
        let store = w.as_reader();
        w.apply_watcher_event(&watcher::Event::Init);
        for j in jobs {
            w.apply_watcher_event(&watcher::Event::InitApply(j));
        }
        w.apply_watcher_event(&watcher::Event::InitDone);
        JobTokenAuth {
            key: b"k".to_vec(),
            jobs: store,
        }
    }

    fn header(ns: &str, name: &str, uid: &str) -> String {
        let t = tokens::sign(
            b"k",
            &tokens::Claims {
                ns: ns.into(),
                name: name.into(),
                job_uid: uid.into(),
                exp: 4_000_000_000,
            },
        )
        .unwrap();
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("gitimage:{t}"))
        )
    }

    fn scope(ns: &str, name: &str) -> Scope {
        Scope {
            repo: Repo::new(ns, name),
            access: Access::Push,
        }
    }

    #[tokio::test]
    async fn token_is_scoped_and_dies_with_the_job() {
        let a = auth(vec![
            job("u1", "team-a", "myapp", false),
            job("u2", "team-a", "old", true),
        ]);
        assert!(
            a.check(
                Some(&header("team-a", "myapp", "u1")),
                Some(&scope("team-a", "myapp"))
            )
            .await
            .is_ok()
        );
        assert!(
            a.check(Some(&header("team-a", "myapp", "u1")), None)
                .await
                .is_ok()
        );
        assert!(matches!(
            a.check(
                Some(&header("team-a", "myapp", "u1")),
                Some(&scope("team-b", "myapp"))
            )
            .await,
            Err(OciError::Denied)
        ));
        assert!(matches!(
            a.check(Some(&header("team-a", "old", "u2")), None).await,
            Err(OciError::Denied)
        ));
        assert!(matches!(
            a.check(None, None).await,
            Err(OciError::Unauthorized)
        ));
        assert!(matches!(
            a.check(Some("Basic Z2FyYmFnZQ=="), None).await,
            Err(OciError::Unauthorized)
        ));
    }
}
