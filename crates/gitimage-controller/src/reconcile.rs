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
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
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

/// Records kept in status. Matches GC's `KEEP_BUILDS`, so every listed build that has a digest
/// still has its image (the UI says "the last five builds are kept").
const MAX_RECORDS: usize = crate::gc::KEEP_BUILDS;
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
    if namespace_terminating(err) {
        // The namespace is being deleted; nothing to do.
        return Action::await_change();
    }
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
        .get(keys::ATTEMPT_ANNOTATION)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0)
}

/// Job annotation: the attempt this Job's series of pod-failure retries started at. A rebuild
/// starts a new series with its own [`MAX_ATTEMPTS`]; Jobs without it started at 0.
const FIRST_ATTEMPT_ANNOTATION: &str = "gitimage.dev/first-attempt";

/// Job annotation: the build empties its persistent state volume first. Set on the one retry of
/// a build that ran out of space.
const FRESH_STATE_ANNOTATION: &str = "gitimage.dev/fresh-state";

fn job_fresh_state(job: &Job) -> bool {
    job.annotations()
        .get(FRESH_STATE_ANNOTATION)
        .is_some_and(|v| v == "true")
}

/// Pod-failure retries so far in this Job's series.
fn job_retries(job: &Job) -> u32 {
    let first = job
        .annotations()
        .get(FIRST_ATTEMPT_ANNOTATION)
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    job_attempt(job).saturating_sub(first)
}

/// The `gitimage.dev/rebuild` value a rebuild Job was started for; `None` for other builds.
fn job_rebuild(job: &Job) -> Option<&str> {
    job.annotations()
        .get(keys::REBUILD_ANNOTATION)
        .map(String::as_str)
}

/// Which build of a commit [`start_build`] starts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Attempt {
    number: u32,
    /// The attempt this series of pod-failure retries started at.
    first: u32,
    /// F7 fallback: the clone exits early when the context tree equals this.
    skip_if_tree: Option<String>,
    /// For a requested rebuild, the `gitimage.dev/rebuild` value it is for.
    rebuild: Option<String>,
    /// Empty the persistent state volume first.
    fresh_state: bool,
}

impl Attempt {
    /// The first build of a new commit.
    fn new_commit(skip_if_tree: Option<String>) -> Self {
        Self {
            number: 0,
            first: 0,
            skip_if_tree,
            rebuild: None,
            fresh_state: false,
        }
    }

    /// A requested rebuild: never skipped as unchanged, so it never ends as `Reused`.
    fn rebuild(number: u32, value: &str) -> Self {
        Self {
            number,
            first: number,
            skip_if_tree: None,
            rebuild: Some(value.into()),
            fresh_state: false,
        }
    }

    /// The next attempt after `job`'s pod failed: the same kind of build.
    fn retry_of(job: &Job) -> Self {
        Self {
            number: job_attempt(job) + 1,
            first: job_attempt(job) - job_retries(job),
            skip_if_tree: job.annotations().get("gitimage.dev/skip-if-tree").cloned(),
            rebuild: job_rebuild(job).map(str::to_string),
            fresh_state: job_fresh_state(job),
        }
    }

    /// The retry of `job` after it ran out of space: on an emptied state volume, with its own
    /// pod-failure retries.
    fn fresh_retry_of(job: &Job) -> Self {
        let number = job_attempt(job) + 1;
        Self {
            first: number,
            fresh_state: true,
            ..Self::retry_of(job)
        }
    }
}

/// The current commit's newest build Job, as [`next_step`] sees it.
#[derive(Debug, Clone, Copy)]
struct JobView<'a> {
    attempt: u32,
    finished: bool,
    /// See [`job_rebuild`].
    rebuild: Option<&'a str>,
}

impl<'a> JobView<'a> {
    fn of(job: &'a Job) -> Self {
        Self {
            attempt: job_attempt(job),
            finished: job_finished(job),
            rebuild: job_rebuild(job),
        }
    }
}

/// What the reconciler does for the current commit (once no build of an older commit holds it
/// back).
#[derive(Debug, PartialEq, Eq)]
enum Next<J> {
    /// Nothing: the commit's newest Job is recorded and no rebuild is pending.
    Idle,
    /// Follow this Job of the commit, which still runs and is not recorded yet.
    Follow(J),
    /// Record the result of this finished Job of the commit.
    Record(J),
    /// A new commit without a Job: reuse the last image if the context is unchanged, else build.
    Build,
    /// Start the requested rebuild as this attempt.
    Rebuild { attempt: u32 },
}

/// Decides [`Next`] from the commit's newest Job, the commit's record in status (`None`: no
/// record; `Some(attempt)`: its [`BuildRecord::attempt`]), and the pending `gitimage.dev/rebuild`
/// value (one that differs from `handledRebuild`).
///
/// The newest Job is recorded when the record came from its attempt (or a later one, whose Job
/// is gone). A record without an attempt (one without a Job, or written before records had one)
/// counts as the newest Job's unless that Job is a rebuild: rebuild Jobs are newer than the
/// field, so theirs carry one.
///
/// A Job that is not recorded yet is followed, then recorded, whatever the annotation: removing
/// or resetting the annotation does not abandon a running rebuild, and a build that finished
/// before a request is recorded first. Once it is recorded, a pending rebuild builds the commit
/// again whatever its last outcome, even when its Jobs are gone (TTL). The request is handled
/// once the Job started for that value is recorded.
fn next_step<J>(
    latest: Option<(J, JobView<'_>)>,
    record: Option<Option<u32>>,
    rebuild: Option<&str>,
) -> Next<J> {
    let recorded = |v: &JobView<'_>| match record {
        None => false,
        Some(Some(attempt)) => attempt >= v.attempt,
        Some(None) => v.rebuild.is_none(),
    };
    match (latest, rebuild) {
        (Some((j, v)), _) if !recorded(&v) => {
            if v.finished {
                Next::Record(j)
            } else {
                Next::Follow(j)
            }
        }
        (_, None) if record.is_some() => Next::Idle,
        // Without a record the newest Job, if any, is not recorded: handled above.
        (_, None) => Next::Build,
        // That request's Job is recorded; only a lost `handledRebuild` update gets here.
        (Some((_, v)), Some(r)) if v.rebuild == Some(r) => Next::Idle,
        // After both the newest Job and the record: the new Job never looks recorded.
        (latest, Some(_)) => Next::Rebuild {
            attempt: latest
                .map(|(_, v)| v.attempt)
                .max(record.flatten())
                .map_or(0, |a| a + 1),
        },
    }
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

/// Records a build result. A failed rebuild of a commit that has an image keeps serving that
/// image: the failure is recorded with its digest, so `status.image` and the pods stay on it.
fn push_result(status: &mut GitImageStatus, mut rec: BuildRecord) {
    if rec.outcome == BuildOutcome::Failed
        && let Some(prev) = status.record(&rec.revision)
        && prev.digest.is_some()
    {
        rec.digest = prev.digest.clone();
        rec.context_tree = prev.context_tree.clone();
        rec.source_revision = prev.source_revision.clone();
    }
    push_record(status, rec);
}

fn record(revision: &str, outcome: BuildOutcome) -> BuildRecord {
    BuildRecord {
        revision: revision.into(),
        outcome,
        digest: None,
        context_tree: None,
        source_revision: None,
        reason: None,
        message: None,
        attempt: None,
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

/// If a running Job has no pod and its controller reported `FailedCreate`, the reason.
async fn pod_creation_failure(ctx: &Ctx, ns: &str, job: &Job) -> Option<String> {
    let age = job
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| chrono::Utc::now().timestamp() - t.0.as_second())
        .unwrap_or(0);
    if age < 5 || job_pod(ctx, ns, &job.name_any()).await.is_some() {
        return None;
    }
    let events: Api<k8s_openapi::api::core::v1::Event> = Api::namespaced(ctx.client.clone(), ns);
    let lp = ListParams::default().fields(&format!(
        "involvedObject.kind=Job,involvedObject.name={},reason=FailedCreate",
        job.name_any()
    ));
    let list = match events.list(&lp).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(job = job.name_any(), error = %e, "cannot list events for a pod-less build Job");
            return None;
        }
    };
    let uid = job.metadata.uid.as_deref();
    list.items
        .into_iter()
        .filter(|e| e.involved_object.uid.as_deref() == uid)
        .filter_map(|e| e.message)
        .next_back()
        .map(|m| {
            if m.contains("PodSecurity") {
                let level = ctx.config.build_policy.isolation.pod_security_level();
                // Hint first: the message is truncated, and admission messages can be long.
                format!("Build pods need the namespace label pod-security.kubernetes.io/enforce={level}: {m}")
            } else {
                m
            }
        })
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
            let mut state = None;
            while let Ok(Some(line)) = lines.try_next().await {
                if let Some(json) = line.trim().strip_prefix("gitimage-state ") {
                    state = serde_json::from_str(json).ok();
                    let _ = ctx.trigger.send(ObjectRef::new(&name).within(&ns));
                }
                let changed = progress.feed(&line);
                // Every line: the failure detail is the last output line when BuildKit
                // stops without reporting an error.
                let snapshot = JobProgress {
                    step: progress.current(),
                    error: progress.failure(),
                    state: state.clone(),
                };
                ctx.progress
                    .lock()
                    .unwrap()
                    .insert(uid.clone(), snapshot.clone());
                if changed || progress.error().is_some() {
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
/// What [`start_build`] did.
enum Started {
    Job(Box<Job>),
    /// A build secret is missing or has no such key.
    SecretMissing(String),
    /// The persistent state volume is being replaced; try again shortly.
    WaitingForStateVolume,
}

/// Updates `status` for a [`start_build`] result; whether a build is (about to be) running.
fn on_start(
    ctx: &Ctx,
    status: &mut GitImageStatus,
    started: Started,
    generation: Option<i64>,
    sha: &str,
) -> bool {
    match started {
        Started::Job(job) => {
            set_building(ctx, status, &job, generation, sha);
            true
        }
        Started::SecretMissing(msg) => {
            set_condition(
                &mut status.conditions,
                cond::FAILED,
                true,
                fail::SECRET_MISSING,
                &msg,
                generation,
            );
            false
        }
        Started::WaitingForStateVolume => {
            set_condition(
                &mut status.conditions,
                cond::BUILDING,
                true,
                "Waiting",
                &format!("{}: waiting for the BuildKit state volume", short(sha)),
                generation,
            );
            true
        }
    }
}

/// Creates the GitImage's persistent BuildKit state PVC (`build.stateVolume.persistent`).
/// `false` while one made for other settings, or deleted by the user to reset the cache, is
/// going away: the build waits for it rather than reuse it.
async fn ensure_state_claim(ctx: &Ctx, gi: &GitImage) -> Result<bool, Error> {
    let Some(want) = job::state_claim(gi, &ctx.config) else {
        return Ok(true);
    };
    let ns = gi.namespace().unwrap_or_default();
    let name = want.name_any();
    let api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), &ns);
    let Some(have) = api.get_opt(&name).await? else {
        return match api.create(&PostParams::default(), &want).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e.into()),
        };
    };
    if have.metadata.deletion_timestamp.is_some() {
        return Ok(false);
    }
    let owners = have.owner_references();
    let ours = owners
        .iter()
        .any(|o| Some(&o.uid) == gi.meta().uid.as_ref());
    // Restored from a backup, or left by a deleted and recreated GitImage of the same name.
    let stale = !ours
        && owners
            .iter()
            .any(|o| o.kind == "GitImage" && o.name == gi.name_any());
    if !ours && !stale {
        return Err(Error::Other(format!(
            "PersistentVolumeClaim {name} exists and does not belong to this GitImage; \
             delete or rename it to use build.stateVolume.persistent"
        )));
    }
    let key = |p: &PersistentVolumeClaim| p.annotations().get(job::STATE_KEY_ANNOTATION).cloned();
    if ours && key(&have) == key(&want) {
        // Labels and annotations added to the config since (backup exclusions) reach
        // existing PVCs too.
        let missing = |want: &std::collections::BTreeMap<String, String>,
                       have: &std::collections::BTreeMap<String, String>| {
            want.iter()
                .filter(|(k, v)| have.get(*k) != Some(*v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let labels = missing(want.labels(), have.labels());
        let annotations = missing(want.annotations(), have.annotations());
        if !labels.is_empty() || !annotations.is_empty() {
            let patch = serde_json::json!({
                "metadata": { "labels": labels, "annotations": annotations }
            });
            api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
        }
        return Ok(true);
    }
    let why = if stale {
        "it belongs to an earlier GitImage of this name".to_string()
    } else {
        format!(
            "made for {}, now {}",
            key(&have).unwrap_or_default(),
            key(&want).unwrap_or_default()
        )
    };
    tracing::info!(namespace = %ns, pvc = %name, reason = %why, "recreating BuildKit state volume");
    publish(
        ctx,
        gi,
        EventType::Normal,
        "StateVolumeRecreated",
        format!("Recreating {name}: {why}"),
    )
    .await;
    match api.delete(&name, &DeleteParams::default()).await {
        Ok(_) => Ok(false),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn start_build(
    ctx: &Arc<Ctx>,
    gi: &GitImage,
    sha: &str,
    attempt: Attempt,
) -> Result<Started, Error> {
    let ns = gi.namespace().unwrap_or_default();
    let mut gi_resolved = gi.clone();
    let secrets = match resolve_secret_keys(ctx, &mut gi_resolved).await {
        Ok(s) => s,
        Err(msg) => return Ok(Started::SecretMissing(msg)),
    };
    if !ensure_state_claim(ctx, gi).await? {
        return Ok(Started::WaitingForStateVolume);
    }
    let name = job::job_name(&gi.name_any(), sha, attempt.number);
    let params = BuildParams {
        sha: sha.into(),
        attempt: attempt.number,
        skip_if_tree: attempt.skip_if_tree.clone(),
        push_auth_secret: job::push_auth_secret_name(&name),
        gitimage_image: ctx.gitimage_image.clone(),
        secrets,
        rebuild: gi
            .annotations()
            .get(keys::REBUILD_ANNOTATION)
            .cloned()
            .unwrap_or_default(),
        fresh_state: attempt.fresh_state,
    };
    let mut spec = job::build_job(&gi_resolved, &ctx.config, &params);
    let annotations = spec
        .metadata
        .annotations
        .get_or_insert_with(Default::default);
    annotations.insert(keys::ATTEMPT_ANNOTATION.into(), attempt.number.to_string());
    annotations.insert(FIRST_ATTEMPT_ANNOTATION.into(), attempt.first.to_string());
    if let Some(r) = &attempt.rebuild {
        annotations.insert(keys::REBUILD_ANNOTATION.into(), r.clone());
    }
    if attempt.fresh_state {
        annotations.insert(FRESH_STATE_ANNOTATION.into(), "true".into());
    }
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
    let rebuild = attempt.rebuild.as_deref();
    tracing::info!(namespace = %ns, name = gi.name_any(), sha, attempt = attempt.number, rebuild, job = %name, "build started");
    let verb = if rebuild.is_some() {
        "Rebuilding"
    } else {
        "Building"
    };
    publish(
        ctx,
        gi,
        EventType::Normal,
        "BuildStarted",
        format!("{verb} {} (job {name})", &sha[..sha.len().min(12)]),
    )
    .await;
    Ok(Started::Job(Box::new(created)))
}

/// Outcome of looking at a finished Job.
enum Finished {
    Record(BuildRecord),
    Retry,
    /// It ran out of space on a persistent state volume: build again on an emptied one.
    RetryFresh(String),
}

/// The error line of a build's output (the BuildKit container's termination message holds its
/// last lines): the "no space left" one, else the last naming an error, else the last line.
/// Progress JSON and `state.sh` reports are skipped.
fn error_line(log: &str) -> Option<String> {
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('{') && !l.starts_with("gitimage-state "))
        .collect();
    lines
        .iter()
        .rev()
        .find(|l| is_no_space(l))
        .or_else(|| {
            lines
                .iter()
                .rev()
                .find(|l| l.to_ascii_lowercase().contains("error"))
        })
        .or_else(|| lines.last())
        .map(|l| l.to_string())
}

/// ENOSPC, as Go ("no space left on device"), libc ("No space left on device") and `state.sh`
/// (after a build that filled the volume) print it.
fn is_no_space(line: &str) -> bool {
    line.to_ascii_lowercase()
        .contains("no space left on device")
}

/// `Failed`/`OutOfSpace` record for a build that ran out of space, saying what to change.
fn out_of_space(ctx: &Ctx, sha: &str, fresh: bool, line: &str) -> BuildRecord {
    let sv = &ctx.config.build.state_volume;
    let message = match &sv.persistent {
        Some(p) if fresh => format!(
            "ran out of space on an emptied BuildKit state volume: the build needs more than \
             {}. Raise build.stateVolume.persistent.size. {line}",
            p.size
        ),
        Some(_) => format!("ran out of space on the BuildKit state volume. {line}"),
        None => format!(
            "BuildKit's state volume ran out of space: raise build.stateVolume.sizeLimit or \
             block.size, or use build.stateVolume.persistent. {line}"
        ),
    };
    BuildRecord {
        reason: Some(fail::OUT_OF_SPACE.into()),
        message: Some(truncate(&message, 512)),
        ..record(sha, BuildOutcome::Failed)
    }
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
                    let reported = job.metadata.uid.as_ref().and_then(|u| {
                        ctx.progress
                            .lock()
                            .unwrap()
                            .get(u)
                            .and_then(|p| p.error.clone())
                    });
                    if let Some(line) = [Some(msg.as_str()), reported.as_deref()]
                        .into_iter()
                        .flatten()
                        .flat_map(str::lines)
                        .find(|l| is_no_space(l))
                    {
                        let line = line.trim().to_string();
                        let fresh = job_fresh_state(job);
                        return if ctx.config.build.state_volume.persistent.is_some() && !fresh {
                            Finished::RetryFresh(line)
                        } else {
                            Finished::Record(out_of_space(ctx, sha, fresh, &line))
                        };
                    }
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
                        // BuildKit's own report, if any, replaces it (see `reconcile_inner`).
                        message: error_line(&msg).map(|l| truncate(&l, 512)),
                        ..record(sha, BuildOutcome::Failed)
                    })
                }
                // No container result: the pod was evicted or its node was lost.
                None if job_retries(job) < MAX_ATTEMPTS => Finished::Retry,
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
    let result = reconcile_inner(gi.clone(), ctx.clone()).await;
    if let Err(err) = &result
        && !namespace_terminating(err)
    {
        record_error(&ctx, &gi, err).await;
    }
    result
}

fn namespace_terminating(err: &Error) -> bool {
    matches!(err, Error::Kube(kube::Error::Api(e)) if e.message.contains("being terminated"))
}

/// The error as a user should read it: the API server's message, not the whole `Status`.
fn error_message(err: &Error) -> String {
    let msg = match err {
        Error::Kube(kube::Error::Api(e)) => format!("kubernetes: {}", e.message),
        other => other.to_string(),
    };
    truncate(&msg, 1024)
}

/// Puts a failed reconcile into status, so `kubectl get`/`describe` and the UI show it
/// instead of only the controller log. The next successful reconcile clears it.
async fn record_error(ctx: &Ctx, gi: &GitImage, err: &Error) {
    let old = gi.status.clone().unwrap_or_default();
    let new = with_error(&old, gi.metadata.generation, &error_message(err));
    if new == old {
        return;
    }
    let api: Api<GitImage> =
        Api::namespaced(ctx.client.clone(), &gi.namespace().unwrap_or_default());
    let patch = serde_json::json!({ "status": status_patch(&old, &new) });
    if let Err(e) = api
        .patch_status(
            &gi.name_any(),
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
    {
        tracing::warn!(namespace = gi.namespace().unwrap_or_default(), name = gi.name_any(), error = %e, "could not record reconcile error in status");
    }
}

fn with_error(old: &GitImageStatus, generation: Option<i64>, msg: &str) -> GitImageStatus {
    let mut new = old.clone();
    set_condition(
        &mut new.conditions,
        cond::FAILED,
        true,
        fail::RECONCILE_ERROR,
        msg,
        generation,
    );
    // Ready stays as it was if an image exists (it still serves); a GitImage that never
    // got this far says why it has none.
    if new
        .condition(cond::READY)
        .is_none_or(|c| c.status != "True")
    {
        set_condition(
            &mut new.conditions,
            cond::READY,
            false,
            fail::RECONCILE_ERROR,
            msg,
            generation,
        );
    }
    new
}

async fn reconcile_inner(gi: Arc<GitImage>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    if gi.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }
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

    // A `gitimage.dev/rebuild` value not handled yet: rebuild the current commit (see
    // `next_step`). It stays pending until that rebuild's result is recorded.
    let rebuild = gi.annotations().get(keys::REBUILD_ANNOTATION).cloned();
    let rebuild_pending = rebuild
        .clone()
        .filter(|r| Some(r) != status.handled_rebuild.as_ref());
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
                if j.metadata.deletion_timestamp.is_none() {
                    let dp = DeleteParams {
                        propagation_policy: Some(PropagationPolicy::Foreground),
                        ..Default::default()
                    };
                    let _ = jobs_api.delete(&j.name_any(), &dp).await;
                    if status.record(&rev).is_none() {
                        push_record(
                            &mut status,
                            BuildRecord {
                                attempt: Some(job_attempt(j)),
                                ..record(&rev, BuildOutcome::Superseded)
                            },
                        );
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
                // Until its pod is gone it holds the persistent state volume, and with it
                // buildkitd's lock: a second buildkitd on the same node would fail to start.
                // Foreground deletion keeps the Job until then.
                if ctx.config.build.state_volume.persistent.is_some() {
                    waiting_on_other = true;
                    building = true;
                    set_condition(
                        &mut status.conditions,
                        cond::BUILDING,
                        true,
                        "Waiting",
                        &format!(
                            "{}: waiting for the cancelled build of {} to stop",
                            short(&sha),
                            short(&rev)
                        ),
                        generation,
                    );
                }
            }
            OnNewCommit::Wait => {
                follow_job(&ctx, &gi, j);
                waiting_on_other = true;
                building = true;
                set_building(&ctx, &mut status, j, generation, &rev);
            }
        }
    }

    if !waiting_on_other {
        let mut sha_jobs: Vec<&Job> = jobs
            .iter()
            .filter(|j| job_label(j, keys::REVISION_LABEL).as_deref() == Some(sha.as_str()))
            .collect();
        sha_jobs.sort_by_key(|j| job_attempt(j));
        let latest = sha_jobs.last().map(|j| (*j, JobView::of(j)));
        let record_attempt = status.record(&sha).map(|r| r.attempt);
        // Whether `j` is the Job of the pending rebuild, whose result handles the request. The
        // result of a rebuild whose value is no longer pending leaves `handledRebuild` alone.
        let is_requested =
            |j: &Job| rebuild_pending.is_some() && job_rebuild(j) == rebuild_pending.as_deref();
        match next_step(latest, record_attempt, rebuild_pending.as_deref()) {
            Next::Idle => {}
            // The Job cannot create its pod (Pod Security, quota, ...): fail now instead of
            // waiting for activeDeadlineSeconds. Its record carries its attempt, so once deleted
            // for that it is not followed again.
            Next::Follow(j) => {
                let creation_failure = match j.metadata.deletion_timestamp {
                    None => pod_creation_failure(&ctx, &ns, j).await,
                    Some(_) => None,
                };
                if let Some(msg) = creation_failure {
                    let dp = DeleteParams {
                        propagation_policy: Some(PropagationPolicy::Foreground),
                        ..Default::default()
                    };
                    let _ = jobs_api.delete(&j.name_any(), &dp).await;
                    ctx.metrics.build("Failed");
                    publish(
                        &ctx,
                        &gi,
                        EventType::Warning,
                        fail::POD_CREATION_FAILED,
                        msg.clone(),
                    )
                    .await;
                    push_result(
                        &mut status,
                        BuildRecord {
                            reason: Some(fail::POD_CREATION_FAILED.into()),
                            message: Some(truncate(&msg, 512)),
                            attempt: Some(job_attempt(j)),
                            ..record(&sha, BuildOutcome::Failed)
                        },
                    );
                    if is_requested(j) {
                        status.handled_rebuild = rebuild_pending.clone();
                    }
                    set_condition(
                        &mut status.conditions,
                        cond::FAILED,
                        true,
                        fail::POD_CREATION_FAILED,
                        &msg,
                        generation,
                    );
                } else {
                    follow_job(&ctx, &gi, j);
                    building = true;
                    set_building(&ctx, &mut status, j, generation, &sha);
                }
            }
            // A rebuild request: a new attempt, after the newest Job and the recorded one.
            Next::Rebuild { attempt } => {
                let value = rebuild_pending.as_deref().unwrap_or_default();
                let started =
                    start_build(&ctx, &gi, &sha, Attempt::rebuild(attempt, value)).await?;
                if matches!(started, Started::Job(_)) {
                    // A failure is cleared while its retry runs. One that still serves an image
                    // stays: dropping it would drop the image.
                    status
                        .builds
                        .retain(|b| b.outcome != BuildOutcome::Failed || b.digest.is_some());
                }
                building |= on_start(&ctx, &mut status, started, generation, &sha);
            }
            Next::Record(j) => match inspect_finished(&ctx, &gi, &status, j, &sha).await {
                Finished::Record(mut rec) => {
                    rec.attempt = Some(job_attempt(j));
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
                            if !detail.is_empty()
                                && rec.reason.as_deref() != Some(fail::OUT_OF_SPACE)
                            {
                                rec.message = Some(truncate(&detail, 512));
                            }
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
                    let reason = if rec.reason.as_deref() == Some(fail::OUT_OF_SPACE) {
                        fail::OUT_OF_SPACE
                    } else {
                        reason
                    };
                    publish(&ctx, &gi, ev_type, reason, note).await;
                    take_state_report(&ctx, &mut status, j, &sha, true);
                    push_result(&mut status, rec);
                    if is_requested(j) {
                        status.handled_rebuild = rebuild_pending.clone();
                    }
                }
                Finished::Retry
                    if job_finished(j)
                        && job_retries(j) < MAX_ATTEMPTS
                        && j.status.as_ref().and_then(|s| s.failed).unwrap_or(0) > 0 =>
                {
                    let started = start_build(&ctx, &gi, &sha, Attempt::retry_of(j)).await?;
                    building |= on_start(&ctx, &mut status, started, generation, &sha);
                }
                Finished::RetryFresh(line) => {
                    take_state_report(&ctx, &mut status, j, &sha, true);
                    publish(
                        &ctx,
                        &gi,
                        EventType::Warning,
                        fail::OUT_OF_SPACE,
                        format!(
                            "{} ran out of space on its BuildKit state volume; building it again on an emptied volume: {line}",
                            short(&sha)
                        ),
                    )
                    .await;
                    let started = start_build(&ctx, &gi, &sha, Attempt::fresh_retry_of(j)).await?;
                    building |= on_start(&ctx, &mut status, started, generation, &sha);
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
            Next::Build => {
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
                    let started =
                        start_build(&ctx, &gi, &sha, Attempt::new_commit(last_tree)).await?;
                    building |= on_start(&ctx, &mut status, started, generation, &sha);
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
    match status.record(&sha).cloned() {
        Some(r) if r.outcome == BuildOutcome::Failed => {
            let reason = r
                .reason
                .clone()
                .unwrap_or_else(|| fail::BUILD_FAILED.into());
            let detail = r
                .message
                .as_ref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default();
            // A failed rebuild of a commit that had an image.
            let serving = if r.digest.is_some() {
                "; the previous image still serves"
            } else {
                ""
            };
            set_condition(
                &mut status.conditions,
                cond::FAILED,
                true,
                &reason,
                &format!(
                    "build of {} failed{detail}{serving}; push a new commit or change {}",
                    short(&sha),
                    keys::REBUILD_ANNOTATION
                ),
                generation,
            );
        }
        _ => {
            // A missing build secret stays visible while the commit has no record, or while
            // the rebuild it blocks is still pending.
            let still_pending = rebuild.is_some() && rebuild != status.handled_rebuild;
            if status.condition(cond::FAILED).is_none_or(|c| {
                c.reason != fail::SECRET_MISSING
                    || (status.record(&sha).is_some() && !still_pending)
            }) {
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

    // With pinTemplateImage the roller runs on every reconcile, not just on a new revision:
    // it also pins workloads created since the last build, and re-pins templates an apply
    // reset to the placeholder. Workloads already pinned are not patched. A rebuild changes
    // only the digest; pinning it too rolls the workloads onto it.
    let pin_image = ctx.config.controller.pin_template_image;
    let digest = status.last_good().and_then(|b| b.digest.clone());
    let changed =
        status.revision != old.revision || digest != old.last_good().and_then(|b| b.digest.clone());
    if let Some(revision) = status.revision.as_deref()
        && (pin_image || changed)
    {
        let image = status.image.as_deref().filter(|_| pin_image);
        let digest = digest.as_deref().filter(|_| changed);
        roller::roll(&ctx, &gi, revision, image, digest).await;
    }
    let requeue = if building {
        // A Job without a pod produces no events we watch; look again soon.
        Duration::from_secs(15)
    } else if resolved.movable {
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
    take_state_report(ctx, status, job, sha, false);
}

/// Copies the state volume report of `job`'s build into status, and with `done` forgets the
/// build's progress.
fn take_state_report(ctx: &Ctx, status: &mut GitImageStatus, job: &Job, sha: &str, done: bool) {
    let Some(uid) = &job.metadata.uid else { return };
    let mut progress = ctx.progress.lock().unwrap();
    let report = progress.get(uid).and_then(|p| p.state.clone());
    if done {
        progress.remove(uid);
    }
    let Some(mut report) = report else { return };
    // Only the first report of a build: re-reading its log must not move `observedAt`.
    if status
        .state_volume
        .as_ref()
        .and_then(|s| s.revision.as_deref())
        == Some(sha)
        && status
            .state_volume
            .as_ref()
            .map(|s| (s.used_bytes, s.free_bytes))
            == Some((report.used_bytes, report.free_bytes))
    {
        return;
    }
    report.revision = Some(sha.into());
    report.observed_at = Some(now_rfc3339());
    report.emptied = report.emptied.filter(|e| !e.is_empty());
    status.state_volume = Some(report);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
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
        let patch = serde_json::json!({ "status": status_patch(old, &status) });
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

/// The full new status as a JSON merge patch. Fields that became empty are skipped when
/// serializing (`skip_serializing_if`), and a merge patch leaves absent keys untouched, so
/// every key `old` had and `new` lacks is sent as an explicit `null` (e.g. a rebuild that drops
/// the only, failed, build record must clear `builds` on the server).
fn status_patch(old: &GitImageStatus, new: &GitImageStatus) -> serde_json::Value {
    fn null_removed(old: &serde_json::Value, new: &mut serde_json::Value) {
        if let (Some(o), Some(n)) = (old.as_object(), new.as_object_mut()) {
            for (k, ov) in o {
                match n.get_mut(k) {
                    None => {
                        n.insert(k.clone(), serde_json::Value::Null);
                    }
                    Some(nv) => null_removed(ov, nv),
                }
            }
        }
    }
    let old = serde_json::to_value(old).unwrap_or_default();
    let mut new = serde_json::to_value(new).unwrap_or_default();
    null_removed(&old, &mut new);
    new
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_patch_clears_fields_that_became_empty() {
        let old = GitImageStatus {
            builds: vec![record("cf5480d", BuildOutcome::Failed)],
            image: Some("localhost:25050/a/b@sha256:1".into()),
            latest_revision: Some("cf5480d".into()),
            ..Default::default()
        };
        let new = GitImageStatus {
            latest_revision: Some("cf5480d".into()),
            handled_rebuild: Some("1".into()),
            ..Default::default()
        };
        let p = status_patch(&old, &new);
        assert_eq!(p["builds"], serde_json::Value::Null);
        assert!(p.as_object().unwrap().contains_key("builds"));
        assert_eq!(p["image"], serde_json::Value::Null);
        assert_eq!(p["latestRevision"], "cf5480d");
        assert_eq!(p["handledRebuild"], "1");
        assert!(!p.as_object().unwrap().contains_key("revision"));
    }

    #[test]
    fn reconcile_error_is_shown_without_hiding_a_ready_image() {
        let fresh = with_error(
            &GitImageStatus::default(),
            Some(1),
            "kubernetes: Job is invalid",
        );
        let failed = fresh.condition(cond::FAILED).unwrap();
        assert_eq!(
            (failed.status.as_str(), failed.reason.as_str()),
            ("True", fail::RECONCILE_ERROR)
        );
        assert_eq!(failed.message, "kubernetes: Job is invalid");
        let ready = fresh.condition(cond::READY).unwrap();
        assert_eq!(
            (ready.status.as_str(), ready.reason.as_str()),
            ("False", fail::RECONCILE_ERROR)
        );

        // An image that already serves stays Ready.
        let mut built = GitImageStatus::default();
        set_condition(
            &mut built.conditions,
            cond::READY,
            true,
            "Built",
            "abc is ready",
            Some(1),
        );
        let after = with_error(&built, Some(1), "boom");
        assert_eq!(after.condition(cond::READY).unwrap().status, "True");
        assert_eq!(
            after.condition(cond::FAILED).unwrap().reason,
            fail::RECONCILE_ERROR
        );

        // The same error again changes nothing, so it is not patched every 30 seconds.
        assert_eq!(with_error(&after, Some(1), "boom"), after);
    }

    fn view(attempt: u32, finished: bool, rebuild: Option<&str>) -> JobView<'_> {
        JobView {
            attempt,
            finished,
            rebuild,
        }
    }

    fn running(attempt: u32, rebuild: Option<&str>) -> Option<JobView<'_>> {
        Some(view(attempt, false, rebuild))
    }

    fn done(attempt: u32, rebuild: Option<&str>) -> Option<JobView<'_>> {
        Some(view(attempt, true, rebuild))
    }

    /// A record of the commit from this Job attempt.
    fn rec(attempt: u32) -> Option<Option<u32>> {
        Some(Some(attempt))
    }

    /// A record without an attempt: from before the field, or `Reused` without a Job.
    const LEGACY: Option<Option<u32>> = Some(None);

    fn step(
        latest: Option<JobView<'_>>,
        record: Option<Option<u32>>,
        rebuild: Option<&str>,
    ) -> Next<()> {
        next_step(latest.map(|v| ((), v)), record, rebuild)
    }

    #[test]
    fn commits_without_a_rebuild_request_build_once() {
        // A new commit builds (or reuses), follows its Job, then records it; after that nothing.
        assert_eq!(step(None, None, None), Next::Build);
        assert_eq!(step(running(0, None), None, None), Next::Follow(()));
        assert_eq!(step(done(0, None), None, None), Next::Record(()));
        assert_eq!(step(done(0, None), rec(0), None), Next::Idle);
        // Reused without a Job.
        assert_eq!(step(None, LEGACY, None), Next::Idle);
        // A failed build is not retried automatically, even once its Job is gone.
        assert_eq!(step(None, rec(0), None), Next::Idle);
        // A pod-failure retry is a new attempt, followed like the first.
        assert_eq!(step(running(1, None), None, None), Next::Follow(()));
        // An earlier rebuild's Job, already handled, changes nothing.
        assert_eq!(step(done(1, Some("1")), rec(1), None), Next::Idle);
        // An older Job than the record's (the newer ones deleted) is not recorded again.
        assert_eq!(step(done(0, None), rec(2), None), Next::Idle);
    }

    #[test]
    fn records_without_an_attempt_cover_all_but_rebuild_jobs() {
        assert_eq!(step(done(0, None), LEGACY, None), Next::Idle);
        assert_eq!(step(running(0, None), LEGACY, None), Next::Idle);
        assert_eq!(
            step(done(0, None), LEGACY, Some("1")),
            Next::Rebuild { attempt: 1 }
        );
        assert_eq!(step(None, LEGACY, Some("1")), Next::Rebuild { attempt: 0 });
        // Rebuild Jobs are newer than records without an attempt: theirs is still to record.
        assert_eq!(step(running(1, Some("1")), LEGACY, None), Next::Follow(()));
        assert_eq!(step(done(1, Some("1")), LEGACY, None), Next::Record(()));
    }

    #[test]
    fn rebuild_request_rebuilds_whatever_the_last_outcome() {
        // Built or reused: a new attempt after the newest Job, which still exists.
        assert_eq!(
            step(done(0, None), rec(0), Some("1")),
            Next::Rebuild { attempt: 1 }
        );
        // Failed: the failure is recorded, its Job failed.
        assert_eq!(
            step(done(2, None), rec(2), Some("1")),
            Next::Rebuild { attempt: 3 }
        );
        // Every Job of the commit is gone (TTL), e.g. a daily rebuild of an old commit: an
        // attempt after the recorded one, so the new Job does not look recorded.
        assert_eq!(step(None, rec(0), Some("1")), Next::Rebuild { attempt: 1 });
        assert_eq!(step(None, rec(4), Some("1")), Next::Rebuild { attempt: 5 });
        // A new commit and a request at once: no reuse, the forced build is its first build.
        assert_eq!(step(None, None, Some("1")), Next::Rebuild { attempt: 0 });
        // An older request's rebuild finished; this value is new.
        assert_eq!(
            step(done(1, Some("1")), rec(1), Some("2")),
            Next::Rebuild { attempt: 2 }
        );
        // This value's rebuild is recorded but `handledRebuild` was not updated: no loop.
        assert_eq!(step(done(1, Some("1")), rec(1), Some("1")), Next::Idle);
    }

    #[test]
    fn rebuild_request_waits_for_a_running_build_and_records_its_own() {
        // A build of the commit runs (a new-commit build, or an older request's rebuild): the
        // request stays pending until it finishes.
        assert_eq!(step(running(0, None), None, Some("1")), Next::Follow(()));
        assert_eq!(
            step(running(1, Some("1")), rec(0), Some("2")),
            Next::Follow(())
        );
        // The rebuild itself runs, then is recorded (which handles the request), even though
        // status still has the previous record of the commit.
        assert_eq!(
            step(running(1, Some("1")), rec(0), Some("1")),
            Next::Follow(())
        );
        assert_eq!(
            step(done(1, Some("1")), rec(0), Some("1")),
            Next::Record(())
        );
        // A failed record is dropped when its rebuild starts.
        assert_eq!(step(done(1, Some("1")), None, Some("1")), Next::Record(()));
        // A build (success or failure) that finished before the request is recorded first;
        // the rebuild follows.
        assert_eq!(step(done(0, None), None, Some("1")), Next::Record(()));
        assert_eq!(
            step(done(0, None), rec(0), Some("1")),
            Next::Rebuild { attempt: 1 }
        );
        // A rebuild superseded by a newer value is recorded first, then the newer one starts.
        assert_eq!(
            step(done(1, Some("1")), rec(0), Some("2")),
            Next::Record(())
        );
        assert_eq!(
            step(done(1, Some("1")), rec(1), Some("2")),
            Next::Rebuild { attempt: 2 }
        );
    }

    #[test]
    fn a_running_rebuild_is_recorded_whatever_happens_to_the_annotation() {
        // The annotation is removed while the rebuild runs: no request is pending, but the
        // rebuild's result is still followed and recorded, then nothing.
        assert_eq!(step(running(1, Some("1")), rec(0), None), Next::Follow(()));
        assert_eq!(step(done(1, Some("1")), rec(0), None), Next::Record(()));
        assert_eq!(step(done(1, Some("1")), rec(1), None), Next::Idle);
        // Its pod-failure retry too.
        assert_eq!(step(running(2, Some("1")), rec(0), None), Next::Follow(()));
        // A sync resets it to the handled value "1" while "2" runs: the same, and no extra
        // rebuild afterwards.
        assert_eq!(step(running(2, Some("2")), rec(1), None), Next::Follow(()));
        assert_eq!(step(done(2, Some("2")), rec(1), None), Next::Record(()));
        assert_eq!(step(done(2, Some("2")), rec(2), None), Next::Idle);
    }

    #[test]
    fn a_job_failed_for_pod_creation_is_not_followed_again() {
        // Recorded at once and deleted; it still exists (running) until its deletion finishes.
        assert_eq!(step(running(0, None), rec(0), None), Next::Idle);
        // A rebuild that failed so handled its request.
        assert_eq!(step(running(1, Some("1")), rec(1), None), Next::Idle);
        // A newer request does not wait for it: it never had a pod.
        assert_eq!(
            step(running(1, Some("1")), rec(1), Some("2")),
            Next::Rebuild { attempt: 2 }
        );
    }

    fn job(annotations: &[(&str, &str)]) -> Job {
        let mut j = Job::default();
        j.metadata.annotations = Some(
            annotations
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        j
    }

    #[test]
    fn rebuilds_get_their_own_pod_failure_retries() {
        // Jobs from before the first-attempt annotation count from 0.
        let old = job(&[("gitimage.dev/attempt", "1")]);
        assert_eq!(job_retries(&old), 1);
        assert_eq!(
            Attempt::retry_of(&old),
            Attempt {
                number: 2,
                first: 0,
                skip_if_tree: None,
                rebuild: None,
                fresh_state: false,
            }
        );
        let with_skip = job(&[
            ("gitimage.dev/attempt", "0"),
            ("gitimage.dev/skip-if-tree", "tree1"),
        ]);
        assert_eq!(
            Attempt::retry_of(&with_skip).skip_if_tree.as_deref(),
            Some("tree1")
        );

        // A rebuild as attempt 3 starts a new series: never skipped, retried like a first build.
        let rebuild = Attempt::rebuild(3, "1");
        assert_eq!((rebuild.first, rebuild.skip_if_tree.as_deref()), (3, None));
        let j = job(&[
            ("gitimage.dev/attempt", "3"),
            (FIRST_ATTEMPT_ANNOTATION, "3"),
            (keys::REBUILD_ANNOTATION, "1"),
        ]);
        assert_eq!(job_retries(&j), 0);
        assert_eq!(job_rebuild(&j), Some("1"));
        let retry = Attempt::retry_of(&j);
        assert_eq!(
            retry,
            Attempt {
                number: 4,
                first: 3,
                skip_if_tree: None,
                rebuild: Some("1".into()),
                fresh_state: false,
            }
        );
        let j = job(&[
            ("gitimage.dev/attempt", "4"),
            (FIRST_ATTEMPT_ANNOTATION, "3"),
        ]);
        assert_eq!(job_retries(&j), 1);
        assert!(job_retries(&j) < MAX_ATTEMPTS);
    }

    #[test]
    fn out_of_space_retry_empties_the_state_once() {
        let j = job(&[
            ("gitimage.dev/attempt", "1"),
            (FIRST_ATTEMPT_ANNOTATION, "0"),
            (keys::REBUILD_ANNOTATION, "7"),
            ("gitimage.dev/skip-if-tree", "tree1"),
        ]);
        assert!(!job_fresh_state(&j));
        // Same kind of build, a new series of pod-failure retries, on an emptied volume.
        let fresh = Attempt::fresh_retry_of(&j);
        assert_eq!(
            fresh,
            Attempt {
                number: 2,
                first: 2,
                skip_if_tree: Some("tree1".into()),
                rebuild: Some("7".into()),
                fresh_state: true,
            }
        );
        // A pod-failure retry of the fresh build stays fresh.
        let f = job(&[
            ("gitimage.dev/attempt", "2"),
            (FRESH_STATE_ANNOTATION, "true"),
        ]);
        assert!(job_fresh_state(&f));
        assert!(Attempt::retry_of(&f).fresh_state);
    }

    #[test]
    fn error_line_finds_the_cause_in_the_log_tail() {
        // What phos printed when its state volume was full: buildkitd never started.
        let full = "gitimage: pruning the BuildKit state failed\n\
            could not connect to unix:///run/buildkit/buildkitd.sock after 10 trials\n\
            ========== log ==========\n\
            time=\"2026-11-27T13:10:39Z\" level=info msg=\"auto snapshotter: using native\"\n\
            buildkitd: mkdir /var/lib/buildkit/runc-native: no space left on device\n\
            sh: can't kill pid 6: No such process\n";
        assert_eq!(
            error_line(full).as_deref(),
            Some("buildkitd: mkdir /var/lib/buildkit/runc-native: no space left on device")
        );
        let failed = "{\"vertexes\":[]}\n\
            error: failed to solve: process \"/bin/sh -c cargo build\" did not complete successfully: exit code: 101\n\
            gitimage-state {\"sizeBytes\":1}\n";
        assert_eq!(
            error_line(failed).as_deref(),
            Some(
                "error: failed to solve: process \"/bin/sh -c cargo build\" did not complete successfully: exit code: 101"
            )
        );
        assert_eq!(error_line("one\ntwo\n").as_deref(), Some("two"));
        assert_eq!(error_line("{}\n\n"), None);
    }

    fn built(revision: &str, digest: &str) -> BuildRecord {
        BuildRecord {
            digest: Some(digest.into()),
            context_tree: Some(format!("tree-{revision}")),
            ..record(revision, BuildOutcome::Built)
        }
    }

    #[test]
    fn failed_rebuild_keeps_serving_the_previous_image() {
        let mut status = GitImageStatus {
            builds: vec![built("sha2", "sha256:2"), built("sha1", "sha256:1")],
            ..Default::default()
        };
        push_result(
            &mut status,
            BuildRecord {
                reason: Some(fail::BUILD_FAILED.into()),
                ..record("sha2", BuildOutcome::Failed)
            },
        );
        let rec = status.record("sha2").unwrap();
        assert_eq!(rec.outcome, BuildOutcome::Failed);
        assert_eq!(rec.reason.as_deref(), Some(fail::BUILD_FAILED));
        assert_eq!(rec.digest.as_deref(), Some("sha256:2"));
        assert_eq!(rec.context_tree.as_deref(), Some("tree-sha2"));
        // Still the current image: not sha1's, and the webhook still resolves sha2.
        assert_eq!(status.last_good().unwrap().revision, "sha2");
        assert_eq!(status.digest_for("sha2"), Some("sha256:2"));
        assert_eq!(status.builds.len(), 2);

        // A successful rebuild replaces it with the new digest.
        push_result(&mut status, built("sha2", "sha256:2b"));
        assert_eq!(status.record("sha2").unwrap().outcome, BuildOutcome::Built);
        assert_eq!(status.digest_for("sha2"), Some("sha256:2b"));

        // A commit without an image fails as before: no digest.
        push_result(&mut status, record("sha3", BuildOutcome::Failed));
        assert_eq!(status.record("sha3").unwrap().digest, None);
        assert_eq!(status.last_good().unwrap().revision, "sha2");
    }

    #[test]
    fn api_errors_are_reported_by_their_message() {
        let err = Error::Kube(kube::Error::Api(
            kube::core::Status::failure("Job.batch \"x\" is invalid", "Invalid").boxed(),
        ));
        assert_eq!(
            error_message(&err),
            "kubernetes: Job.batch \"x\" is invalid"
        );
        let long = Error::Other("x".repeat(5000));
        assert_eq!(error_message(&long).chars().count(), 1024);
    }
}
