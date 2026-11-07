//! The GitImage reconciler (design section 5): resolve the ref, decide whether to build,
//! run and follow the build Job, record the result, and roll workloads.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{AsyncBufReadExt, TryStreamExt};
use gitimage_api::gitimage::{condition_types as cond, failure_reasons as fail};
use gitimage_api::{BuildOutcome, BuildRecord, GitImage, GitImageStatus, OnNewCommit, keys};
use gitimage_git::{Credentials, GitClient, Resolved};
use gitimage_oci::{Reference, Repo};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::api::{
    Api, DeleteParams, ListParams, LogParams, Patch, PatchParams, PostParams, PropagationPolicy,
};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType};
use kube::runtime::reflector::ObjectRef;
use kube::{Resource, ResourceExt};
use sha2::Digest as _;

use crate::context::{Ctx, JobProgress};
use crate::job::{self, BuildParams};
use crate::util::{self, now_rfc3339, set_condition};
use crate::{namespace_setup, roller};

/// Records kept in status (the newest `KEEP_BUILDS` with digests are protected by GC).
const MAX_RECORDS: usize = 10;
/// Pod-level failures (eviction, node loss) retried per commit.
const MAX_ATTEMPTS: u32 = 2;
/// How long a ref resolution is reused for reconciles not caused by time or hooks.
const RESOLVE_TTL: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kubernetes: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(format!("{e:#}"))
    }
}

/// Requeue after an error.
pub fn error_policy(gi: Arc<GitImage>, err: &Error, ctx: Arc<Ctx>) -> Action {
    tracing::warn!(namespace = gi.namespace().unwrap_or_default(), name = gi.name_any(), error = %err, "reconcile failed");
    ctx.metrics.error("gitimage");
    Action::requeue(Duration::from_secs(30))
}

/// What the latest commit's build looks like right now.
enum JobState {
    Running,
    Succeeded,
    Failed { reason: String },
}

fn job_state(job: &Job) -> JobState {
    let Some(st) = job.status.as_ref() else {
        return JobState::Running;
    };
    let conds = st.conditions.as_deref().unwrap_or_default();
    if st.succeeded.unwrap_or(0) > 0
        || conds
            .iter()
            .any(|c| c.type_ == "Complete" && c.status == "True")
    {
        return JobState::Succeeded;
    }
    if let Some(c) = conds
        .iter()
        .find(|c| c.type_ == "Failed" && c.status == "True")
    {
        return JobState::Failed {
            reason: c.reason.clone().unwrap_or_default(),
        };
    }
    if st.failed.unwrap_or(0) > 0 {
        return JobState::Failed {
            reason: String::new(),
        };
    }
    JobState::Running
}

fn job_finished(job: &Job) -> bool {
    !matches!(job_state(job), JobState::Running)
}

fn job_label(job: &Job, key: &str) -> Option<String> {
    job.metadata.labels.as_ref()?.get(key).cloned()
}

fn job_attempt(job: &Job) -> u32 {
    job.annotations()
        .get("gitimage.dev/attempt")
        .and_then(|a| a.parse().ok())
        .unwrap_or(0)
}

/// Reads the GitImage's git credentials (design D7: `get` only, never stored).
async fn load_credentials(ctx: &Ctx, gi: &GitImage) -> Result<Credentials, String> {
    let Some(sr) = &gi.spec.secret_ref else {
        return Ok(Credentials::None);
    };
    let ns = gi.namespace().unwrap_or_default();
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
    match api.get_opt(&sr.name).await {
        Ok(Some(s)) => Credentials::from_secret_data(&util::secret_data(&s))
            .map_err(|e| format!("secret {}/{}: {e}", ns, sr.name)),
        Ok(None) => Err(format!("secret {}/{} not found", ns, sr.name)),
        Err(e) => Err(format!("secret {}/{}: {e}", ns, sr.name)),
    }
}

/// Resolves the build secrets' and secret build args' keys (a Secret with one key may omit it).
async fn resolve_secret_keys(
    ctx: &Ctx,
    gi: &mut GitImage,
) -> Result<Vec<(String, String, String)>, String> {
    let ns = gi.namespace().unwrap_or_default();
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
    let only_key = |s: Option<Secret>, name: &str| -> Result<String, String> {
        let s = s.ok_or_else(|| format!("secret {ns}/{name} not found"))?;
        let keys: Vec<String> = util::secret_data(&s).into_keys().collect();
        match keys.as_slice() {
            [k] => Ok(k.clone()),
            _ => Err(format!(
                "secret {ns}/{name} has {} keys; set secretRef.key",
                keys.len()
            )),
        }
    };
    let mut out = Vec::new();
    for bs in &gi.spec.build.secrets {
        let key = match &bs.secret_ref.key {
            Some(k) => k.clone(),
            None => only_key(
                api.get_opt(&bs.secret_ref.name)
                    .await
                    .map_err(|e| e.to_string())?,
                &bs.secret_ref.name,
            )?,
        };
        out.push((bs.id.clone(), bs.secret_ref.name.clone(), key));
    }
    for arg in gi.spec.build.args.values_mut() {
        if let Some(src) = arg.value_from.as_mut()
            && src.secret_key_ref.key.is_none()
        {
            let name = src.secret_key_ref.name.clone();
            src.secret_key_ref.key = Some(only_key(
                api.get_opt(&name).await.map_err(|e| e.to_string())?,
                &name,
            )?);
        }
    }
    Ok(out)
}

async fn resolve_ref(
    ctx: &Ctx,
    gi: &GitImage,
    creds: &Credentials,
    force: bool,
) -> Result<Resolved, String> {
    let key = format!("{}/{}", gi.namespace().unwrap_or_default(), gi.name_any());
    if !force
        && let Some((r, at)) = ctx.resolve_cache.lock().unwrap().get(&key)
        && at.elapsed() < RESOLVE_TTL
    {
        return Ok(r.clone());
    }
    let refs = ctx
        .git
        .list_refs(&gi.spec.url, creds, &gi.spec.git_ref)
        .await
        .map_err(|e| e.to_string())?;
    let r = gitimage_git::resolve(&refs, &gi.spec.git_ref).map_err(|e| e.to_string())?;
    ctx.resolve_cache
        .lock()
        .unwrap()
        .insert(key, (r.clone(), Instant::now()));
    Ok(r)
}

fn push_record(status: &mut GitImageStatus, rec: BuildRecord) {
    status.builds.retain(|b| b.revision != rec.revision);
    status.builds.insert(0, rec);
    status.builds.truncate(MAX_RECORDS);
}

fn record(revision: &str, outcome: BuildOutcome) -> BuildRecord {
    BuildRecord {
        revision: revision.into(),
        outcome,
        digest: None,
        context_tree: None,
        source_revision: None,
        reason: None,
        finished_at: now_rfc3339(),
    }
}

/// Parses `cloned <tree>` / `unchanged <tree>` from the clone container's termination message.
fn clone_message(pod: &Pod) -> (Option<i32>, Option<String>) {
    let st = pod
        .status
        .as_ref()
        .and_then(|s| s.init_container_statuses.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.name == "clone"))
        .and_then(|c| c.state.as_ref())
        .and_then(|s| s.terminated.as_ref());
    let Some(t) = st else { return (None, None) };
    let tree = t
        .message
        .as_deref()
        .and_then(|m| m.split_whitespace().nth(1))
        .map(str::to_string);
    (Some(t.exit_code), tree)
}

fn buildkit_terminated(pod: &Pod) -> Option<(i32, String)> {
    pod.status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == "buildkit")?
        .state
        .as_ref()?
        .terminated
        .as_ref()
        .map(|t| (t.exit_code, t.message.clone().unwrap_or_default()))
}

async fn job_pod(ctx: &Ctx, ns: &str, job: &str) -> Option<Pod> {
    let api: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let lp = ListParams::default().labels(&format!("batch.kubernetes.io/job-name={job}"));
    let mut pods = api.list(&lp).await.ok()?.items;
    pods.sort_by_key(|p| p.metadata.creation_timestamp.clone());
    pods.pop()
}

/// Starts a task that follows the BuildKit log of `job` and reports progress.
fn follow_job(ctx: &Arc<Ctx>, gi: &GitImage, job: &Job) {
    let Some(uid) = job.metadata.uid.clone() else {
        return;
    };
    if !ctx.followers.lock().unwrap().insert(uid.clone()) {
        return;
    }
    let ctx = ctx.clone();
    let ns = gi.namespace().unwrap_or_default();
    let name = gi.name_any();
    let job_name = job.name_any();
    tokio::spawn(async move {
        let api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
        let mut progress = crate::progress::Progress::default();
        let mut last_sent = Instant::now() - Duration::from_secs(60);
        let mut last_event = Instant::now() - Duration::from_secs(60);
        let deadline = Instant::now()
            + Duration::from_secs(ctx.config.build.active_deadline_seconds() as u64 + 300);
        'outer: while Instant::now() < deadline {
            let Some(pod) = job_pod(&ctx, &ns, &job_name).await else {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            };
            let running = pod
                .status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .and_then(|cs| cs.iter().find(|c| c.name == "buildkit"))
                .and_then(|c| c.state.as_ref())
                .is_some_and(|s| s.running.is_some() || s.terminated.is_some());
            if !running {
                if pod
                    .status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .is_some_and(|p| p == "Failed" || p == "Succeeded")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            let lp = LogParams {
                container: Some("buildkit".into()),
                follow: true,
                ..Default::default()
            };
            let Ok(stream) = api.log_stream(&pod.name_any(), &lp).await else {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            };
            let mut lines = stream.lines();
            while let Ok(Some(line)) = lines.try_next().await {
                if progress.feed(&line) || progress.error().is_some() {
                    let snapshot = JobProgress {
                        step: progress.current(),
                        error: progress.error().map(str::to_string),
                    };
                    ctx.progress
                        .lock()
                        .unwrap()
                        .insert(uid.clone(), snapshot.clone());
                    if last_sent.elapsed() >= Duration::from_secs(5) {
                        last_sent = Instant::now();
                        let _ = ctx.trigger.send(ObjectRef::new(&name).within(&ns));
                    }
                    if let Some(step) = snapshot.step
                        && last_event.elapsed() >= Duration::from_secs(10)
                    {
                        last_event = Instant::now();
                        crate::gates::progress_events(&ctx, &ns, &name, &step).await;
                    }
                }
            }
            break 'outer;
        }
        let _ = ctx.trigger.send(ObjectRef::new(&name).within(&ns));
        ctx.followers.lock().unwrap().remove(&uid);
    });
}

async fn publish(ctx: &Ctx, gi: &GitImage, type_: EventType, reason: &str, note: String) {
    let ev = Event {
        type_,
        reason: reason.into(),
        note: Some(note),
        action: "Build".into(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&ev, &gi.object_ref(&())).await {
        tracing::debug!(error = %e, "event publish failed");
    }
}

/// Creates the build Job and its push-auth Secret.
async fn start_build(
    ctx: &Arc<Ctx>,
    gi: &GitImage,
    sha: &str,
    attempt: u32,
    skip_if_tree: Option<String>,
) -> Result<Result<Job, String>, Error> {
    let ns = gi.namespace().unwrap_or_default();
    let mut gi_resolved = gi.clone();
    let secrets = match resolve_secret_keys(ctx, &mut gi_resolved).await {
        Ok(s) => s,
        Err(msg) => return Ok(Err(msg)),
    };
    let name = job::job_name(&gi.name_any(), sha, attempt);
    let params = BuildParams {
        sha: sha.into(),
        attempt,
        skip_if_tree,
        push_auth_secret: job::push_auth_secret_name(&name),
        gitimage_image: ctx.gitimage_image.clone(),
        secrets,
    };
    let mut spec = job::build_job(&gi_resolved, &ctx.config, &params);
    spec.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert("gitimage.dev/attempt".into(), attempt.to_string());
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
    let created = match jobs.create(&PostParams::default(), &spec).await {
        Ok(j) => j,
        Err(kube::Error::Api(e)) if e.code == 409 => jobs.get(&name).await?,
        Err(e) => return Err(e.into()),
    };
    let uid = created.metadata.uid.clone().unwrap_or_default();
    let exp = chrono::Utc::now().timestamp() as u64
        + ctx.config.build.active_deadline_seconds() as u64
        + 300;
    let token = gitimage_oci::tokens::sign(
        &ctx.push_key,
        &gitimage_oci::tokens::Claims {
            ns: ns.clone(),
            name: gi.name_any(),
            job_uid: uid,
            exp,
        },
    )
    .map_err(|e| Error::Other(e.to_string()))?;
    let secret = job::push_auth_secret(&created, &ctx.config, &token);
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
    match secrets.create(&PostParams::default(), &secret).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 409 => {
            secrets
                .replace(
                    &job::push_auth_secret_name(&name),
                    &PostParams::default(),
                    &secret,
                )
                .await?;
        }
        Err(e) => return Err(e.into()),
    }
    tracing::info!(namespace = %ns, name = gi.name_any(), sha, attempt, job = %name, "build started");
    publish(
        ctx,
        gi,
        EventType::Normal,
        "BuildStarted",
        format!("Building {} (job {name})", &sha[..sha.len().min(12)]),
    )
    .await;
    Ok(Ok(created))
}

/// Outcome of looking at a finished Job.
enum Finished {
    Record(BuildRecord),
    Retry,
}

async fn inspect_finished(
    ctx: &Ctx,
    gi: &GitImage,
    status: &GitImageStatus,
    job: &Job,
    sha: &str,
) -> Finished {
    let ns = gi.namespace().unwrap_or_default();
    let pod = job_pod(ctx, &ns, &job.name_any()).await;
    let (clone_exit, tree) = pod.as_ref().map(clone_message).unwrap_or((None, None));
    match job_state(job) {
        JobState::Succeeded => {
            let repo = Repo::new(&ns, gi.name_any());
            match ctx
                .storage
                .head_manifest(&repo, &Reference::Tag(sha.into()))
                .await
            {
                Ok(Some(d)) => Finished::Record(BuildRecord {
                    digest: Some(d.to_string()),
                    context_tree: tree,
                    ..record(sha, BuildOutcome::Built)
                }),
                Ok(None) => Finished::Record(BuildRecord {
                    reason: Some(fail::PUSH_FAILED.into()),
                    ..record(sha, BuildOutcome::Failed)
                }),
                Err(e) => {
                    tracing::warn!(error = %e, "storage HEAD after build failed; retrying later");
                    Finished::Retry
                }
            }
        }
        JobState::Failed { reason } => {
            if clone_exit == Some(keys::CLONE_EXIT_UNCHANGED)
                && let Some(last) = status.last_good()
            {
                return Finished::Record(BuildRecord {
                    digest: last.digest.clone(),
                    context_tree: tree.or_else(|| last.context_tree.clone()),
                    source_revision: Some(
                        last.source_revision
                            .clone()
                            .unwrap_or_else(|| last.revision.clone()),
                    ),
                    ..record(sha, BuildOutcome::Reused)
                });
            }
            if reason == "DeadlineExceeded" {
                return Finished::Record(BuildRecord {
                    reason: Some(fail::DEADLINE_EXCEEDED.into()),
                    ..record(sha, BuildOutcome::Failed)
                });
            }
            if let Some(code) = clone_exit
                && code != 0
            {
                return Finished::Record(BuildRecord {
                    reason: Some(fail::CLONE_FAILED.into()),
                    ..record(sha, BuildOutcome::Failed)
                });
            }
            match pod.as_ref().and_then(buildkit_terminated) {
                Some((_, msg)) => {
                    let push = msg.contains("failed to push") || msg.contains("failed to export");
                    Finished::Record(BuildRecord {
                        reason: Some(
                            if push {
                                fail::PUSH_FAILED
                            } else {
                                fail::BUILD_FAILED
                            }
                            .into(),
                        ),
                        ..record(sha, BuildOutcome::Failed)
                    })
                }
                // No container result: the pod was evicted or its node was lost.
                None if job_attempt(job) < MAX_ATTEMPTS => Finished::Retry,
                None => Finished::Record(BuildRecord {
                    reason: Some(fail::BUILD_FAILED.into()),
                    ..record(sha, BuildOutcome::Failed)
                }),
            }
        }
        JobState::Running => Finished::Retry,
    }
}

pub async fn reconcile(gi: Arc<GitImage>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = gi.namespace().unwrap_or_default();
    let name = gi.name_any();
    let api: Api<GitImage> = Api::namespaced(ctx.client.clone(), &ns);
    let generation = gi.metadata.generation;
    let old = gi.status.clone().unwrap_or_default();
    let mut status = old.clone();
    status.observed_generation = generation;
    let interval = gi.spec.interval();

    namespace_setup::ensure(&ctx, &ns).await?;
    ensure_hook(&ctx, &gi, &mut status).await?;

    // Retry of a failed build.
    let rebuild = gi.annotations().get(keys::REBUILD_ANNOTATION).cloned();
    let rebuild_requested = rebuild.is_some() && rebuild != status.handled_rebuild;
    if rebuild_requested {
        status.builds.retain(|b| b.outcome != BuildOutcome::Failed);
        status.handled_rebuild = rebuild;
    }
    let hook_at = gi.annotations().get(keys::HOOK_AT_ANNOTATION).cloned();
    let hook_new = hook_at.is_some() && hook_at != status.handled_hook_at;
    if hook_new {
        status.handled_hook_at = hook_at;
    }

    let creds = match load_credentials(&ctx, &gi).await {
        Ok(c) => c,
        Err(msg) => {
            set_condition(
                &mut status.conditions,
                cond::FAILED,
                true,
                fail::SECRET_MISSING,
                &msg,
                generation,
            );
            return finish(
                &ctx,
                &api,
                &gi,
                &old,
                status,
                Action::requeue(Duration::from_secs(60)),
            )
            .await;
        }
    };
    let resolved = match resolve_ref(
        &ctx,
        &gi,
        &creds,
        hook_new || generation != old.observed_generation,
    )
    .await
    {
        Ok(r) => r,
        Err(msg) => {
            set_condition(
                &mut status.conditions,
                cond::FAILED,
                true,
                "ResolveFailed",
                &msg,
                generation,
            );
            return finish(
                &ctx,
                &api,
                &gi,
                &old,
                status,
                Action::requeue(Duration::from_secs(60)),
            )
            .await;
        }
    };
    let sha = resolved.sha.clone();
    status.latest_revision = Some(sha.clone());

    // Jobs of this GitImage.
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
    let lp = ListParams::default().labels(&format!(
        "{}={},{}={}",
        keys::NAME_LABEL,
        name,
        keys::ROLE_LABEL,
        keys::ROLE_BUILD
    ));
    let jobs: Vec<Job> = jobs_api
        .list(&lp)
        .await?
        .items
        .into_iter()
        .filter(|j| {
            j.owner_references()
                .iter()
                .any(|o| Some(&o.uid) == gi.meta().uid.as_ref())
        })
        .collect();

    // A build of an older commit still running.
    let mut waiting_on_other = false;
    let mut building = false;
    for j in jobs.iter().filter(|j| !job_finished(j)) {
        let rev = job_label(j, keys::REVISION_LABEL).unwrap_or_default();
        if rev == sha {
            continue;
        }
        match gi.spec.build.on_new_commit {
            OnNewCommit::Cancel => {
                let dp = DeleteParams {
                    propagation_policy: Some(PropagationPolicy::Foreground),
                    ..Default::default()
                };
                let _ = jobs_api.delete(&j.name_any(), &dp).await;
                if status.record(&rev).is_none() {
                    push_record(&mut status, record(&rev, BuildOutcome::Superseded));
                    ctx.metrics.build("Superseded");
                }
                publish(
                    &ctx,
                    &gi,
                    EventType::Normal,
                    "BuildSuperseded",
                    format!(
                        "Cancelled build of {} for newer commit {}",
                        short(&rev),
                        short(&sha)
                    ),
                )
                .await;
            }
            OnNewCommit::Wait => {
                follow_job(&ctx, &gi, j);
                waiting_on_other = true;
                building = true;
                set_building(&ctx, &mut status, j, generation, &rev);
            }
        }
    }

    if !waiting_on_other && status.record(&sha).is_none() {
        let mut sha_jobs: Vec<&Job> = jobs
            .iter()
            .filter(|j| job_label(j, keys::REVISION_LABEL).as_deref() == Some(sha.as_str()))
            .collect();
        sha_jobs.sort_by_key(|j| job_attempt(j));
        match sha_jobs.last() {
            Some(j) if !job_finished(j) => {
                follow_job(&ctx, &gi, j);
                building = true;
                set_building(&ctx, &mut status, j, generation, &sha);
            }
            // A rebuild request: the finished Job stays until its TTL, so start a new attempt.
            Some(j) if rebuild_requested => {
                let skip = j.annotations().get("gitimage.dev/skip-if-tree").cloned();
                match start_build(&ctx, &gi, &sha, job_attempt(j) + 1, skip).await? {
                    Ok(job) => {
                        building = true;
                        set_building(&ctx, &mut status, &job, generation, &sha)
                    }
                    Err(msg) => set_condition(
                        &mut status.conditions,
                        cond::FAILED,
                        true,
                        fail::SECRET_MISSING,
                        &msg,
                        generation,
                    ),
                }
            }
            Some(j) => match inspect_finished(&ctx, &gi, &status, j, &sha).await {
                Finished::Record(rec) => {
                    let outcome = format!("{:?}", rec.outcome);
                    ctx.metrics.build(&outcome);
                    let (ev_type, reason, note) = match rec.outcome {
                        BuildOutcome::Built => (
                            EventType::Normal,
                            "BuildSucceeded",
                            format!(
                                "Built {} as {}",
                                short(&sha),
                                rec.digest.clone().unwrap_or_default()
                            ),
                        ),
                        BuildOutcome::Reused => (
                            EventType::Normal,
                            "BuildReused",
                            format!(
                                "{}: no change under {}; reusing {}",
                                short(&sha),
                                gi.spec.build.context,
                                rec.source_revision
                                    .as_deref()
                                    .map(short)
                                    .unwrap_or_default()
                            ),
                        ),
                        _ => {
                            let detail = j
                                .metadata
                                .uid
                                .as_ref()
                                .and_then(|u| {
                                    ctx.progress
                                        .lock()
                                        .unwrap()
                                        .get(u)
                                        .and_then(|p| p.error.clone())
                                })
                                .unwrap_or_default();
                            (
                                EventType::Warning,
                                "BuildFailed",
                                format!(
                                    "{} failed: {} {detail}",
                                    short(&sha),
                                    rec.reason.clone().unwrap_or_default()
                                ),
                            )
                        }
                    };
                    publish(&ctx, &gi, ev_type, reason, note).await;
                    if let Some(uid) = &j.metadata.uid {
                        ctx.progress.lock().unwrap().remove(uid);
                    }
                    push_record(&mut status, rec);
                }
                Finished::Retry
                    if job_finished(j)
                        && job_attempt(j) < MAX_ATTEMPTS
                        && j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0 =>
                {
                    let attempt = job_attempt(j) + 1;
                    let skip = j.annotations().get("gitimage.dev/skip-if-tree").cloned();
                    match start_build(&ctx, &gi, &sha, attempt, skip).await? {
                        Ok(job) => {
                            building = true;
                            set_building(&ctx, &mut status, &job, generation, &sha)
                        }
                        Err(msg) => set_condition(
                            &mut status.conditions,
                            cond::FAILED,
                            true,
                            fail::SECRET_MISSING,
                            &msg,
                            generation,
                        ),
                    }
                }
                Finished::Retry => {
                    return finish(
                        &ctx,
                        &api,
                        &gi,
                        &old,
                        status,
                        Action::requeue(Duration::from_secs(15)),
                    )
                    .await;
                }
            },
            None => {
                let last_tree = status.last_good().and_then(|b| b.context_tree.clone());
                let reused = match (&last_tree, status.last_good()) {
                    (Some(tree), Some(last)) => ctx
                        .git
                        .context_tree_via_host(&gi.spec.url, &creds, &sha, &gi.spec.build.context)
                        .await
                        .ok()
                        .flatten()
                        .filter(|t| t == tree)
                        .map(|t| BuildRecord {
                            digest: last.digest.clone(),
                            context_tree: Some(t),
                            source_revision: Some(
                                last.source_revision
                                    .clone()
                                    .unwrap_or_else(|| last.revision.clone()),
                            ),
                            ..record(&sha, BuildOutcome::Reused)
                        }),
                    _ => None,
                };
                if let Some(rec) = reused {
                    ctx.metrics.build("Reused");
                    publish(
                        &ctx,
                        &gi,
                        EventType::Normal,
                        "BuildReused",
                        format!("{}: no change under {}", short(&sha), gi.spec.build.context),
                    )
                    .await;
                    push_record(&mut status, rec);
                } else {
                    match start_build(&ctx, &gi, &sha, 0, last_tree).await? {
                        Ok(job) => {
                            building = true;
                            set_building(&ctx, &mut status, &job, generation, &sha)
                        }
                        Err(msg) => {
                            set_condition(
                                &mut status.conditions,
                                cond::FAILED,
                                true,
                                fail::SECRET_MISSING,
                                &msg,
                                generation,
                            );
                        }
                    }
                }
            }
        }
    }

    // Conditions and the current image.
    if !building {
        set_condition(
            &mut status.conditions,
            cond::BUILDING,
            false,
            "Idle",
            "",
            generation,
        );
    }
    match status.record(&sha) {
        Some(r) if r.outcome == BuildOutcome::Failed => {
            let reason = r
                .reason
                .clone()
                .unwrap_or_else(|| fail::BUILD_FAILED.into());
            set_condition(
                &mut status.conditions,
                cond::FAILED,
                true,
                &reason,
                &format!(
                    "build of {} failed; push a new commit or change {}",
                    short(&sha),
                    keys::REBUILD_ANNOTATION
                ),
                generation,
            );
        }
        _ => {
            if status
                .condition(cond::FAILED)
                .is_none_or(|c| c.reason != fail::SECRET_MISSING || status.record(&sha).is_some())
            {
                set_condition(
                    &mut status.conditions,
                    cond::FAILED,
                    false,
                    "",
                    "",
                    generation,
                );
            }
        }
    }
    if let Some(good) = status.last_good().cloned() {
        status.revision = Some(good.revision.clone());
        status.image = good.digest.as_ref().map(|d| ctx.image_ref(&ns, &name, d));
        set_condition(
            &mut status.conditions,
            cond::READY,
            true,
            "Built",
            &format!("{} is ready", short(&good.revision)),
            generation,
        );
    } else {
        status.revision = None;
        status.image = None;
        set_condition(
            &mut status.conditions,
            cond::READY,
            false,
            "NoImage",
            "no successful build yet",
            generation,
        );
    }

    if status.revision.is_some() && status.revision != old.revision {
        roller::roll(&ctx, &gi, status.revision.as_deref().unwrap_or_default()).await;
    }
    let requeue = if resolved.movable {
        interval
    } else {
        interval.max(Duration::from_secs(600))
    };
    finish(&ctx, &api, &gi, &old, status, Action::requeue(requeue)).await
}

fn set_building(
    ctx: &Ctx,
    status: &mut GitImageStatus,
    job: &Job,
    generation: Option<i64>,
    sha: &str,
) {
    let step = job
        .metadata
        .uid
        .as_ref()
        .and_then(|u| {
            ctx.progress
                .lock()
                .unwrap()
                .get(u)
                .and_then(|p| p.step.clone())
        })
        .unwrap_or_else(|| "starting".into());
    set_condition(
        &mut status.conditions,
        cond::BUILDING,
        true,
        "Started",
        &format!("{}: {step}", short(sha)),
        generation,
    );
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

async fn finish(
    ctx: &Ctx,
    api: &Api<GitImage>,
    gi: &GitImage,
    old: &GitImageStatus,
    status: GitImageStatus,
    action: Action,
) -> Result<Action, Error> {
    if &status != old {
        let patch = serde_json::json!({ "status": status });
        api.patch_status(
            &gi.name_any(),
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await?;
    }
    let _ = ctx;
    Ok(action)
}

/// Ensures the hook token Secret and writes its hash and the hook URL into status.
async fn ensure_hook(ctx: &Ctx, gi: &GitImage, status: &mut GitImageStatus) -> Result<(), Error> {
    let ns = gi.namespace().unwrap_or_default();
    let secret_name = format!("{}-gitimage-hook", gi.name_any());
    let api: Api<Secret> = Api::namespaced(ctx.client.clone(), &ns);
    let secret = match api.get_opt(&secret_name).await? {
        Some(s) => s,
        None => {
            let token: String = {
                use rand::RngExt as _;
                let bytes: [u8; 32] = rand::rng().random();
                hex::encode(bytes)
            };
            let mut s = Secret {
                metadata: kube::core::ObjectMeta {
                    name: Some(secret_name.clone()),
                    namespace: Some(ns.clone()),
                    owner_references: Some(vec![job::owner_ref(gi)]),
                    labels: Some(
                        [(
                            "app.kubernetes.io/managed-by".to_string(),
                            "gitimage".to_string(),
                        )]
                        .into(),
                    ),
                    ..Default::default()
                },
                string_data: Some([("token".to_string(), token)].into()),
                ..Default::default()
            };
            s = match api.create(&PostParams::default(), &s).await {
                Ok(s) => s,
                Err(kube::Error::Api(e)) if e.code == 409 => api.get(&secret_name).await?,
                Err(e) => return Err(e.into()),
            };
            s
        }
    };
    let token = util::secret_data(&secret)
        .get("token")
        .map(|t| String::from_utf8_lossy(t).trim().to_string())
        .unwrap_or_default();
    if token.is_empty() {
        return Ok(());
    }
    status.hook_token_hash = Some(hex::encode(sha2::Sha256::digest(token.as_bytes())));
    status.webhook_url = ctx
        .config
        .hooks
        .base_url
        .as_ref()
        .map(|b| format!("{}/hooks/{token}", b.trim_end_matches('/')));
    Ok(())
}
