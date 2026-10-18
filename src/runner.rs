//! One job from start to finish: worktree and agent through acc, the live
//! budget watch, the result, publishing and telling the source.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::budget::{self, Meter, Verdict};
use crate::config::{Config, ProviderCfg};
use crate::harness::{self, Agent};
use crate::presets::Mode;
use crate::prompt::{self, AgentResult, ResultStatus};
use crate::publish::{self, Published};
use crate::quota::{ProviderId, QuotaProvider, Snapshot};
use crate::select::{What, Work};
use crate::store::{JobRow, JobStatus, Store};
use crate::tasks::{Outcome, Source};

/// How often the agent's log is read and approvals are answered.
const PUMP_EVERY: Duration = Duration::from_secs(3);
/// How long an interrupted agent gets to wind down before it is killed.
const INTERRUPT_GRACE: Duration = Duration::from_secs(20);

/// Everything a job needs from the daemon.
pub struct Ctx {
    pub cfg: Config,
    pub cfg_dir: PathBuf,
    pub store: Store,
    pub quota: BTreeMap<ProviderId, Box<dyn QuotaProvider>>,
    pub sources: Vec<Box<dyn Source>>,
}

impl Ctx {
    pub fn new(cfg: Config, cfg_dir: PathBuf, store: Store) -> Self {
        let quota = cfg
            .providers
            .iter()
            .filter_map(|(id, p)| ProviderId::parse(id).map(|i| (i, crate::quota::provider(i, p))))
            .collect();
        let sources = crate::tasks::build(&cfg.sources, &store);
        Self {
            cfg,
            cfg_dir,
            store,
            quota,
            sources,
        }
    }

    pub fn provider_cfg(&self, p: ProviderId) -> ProviderCfg {
        self.cfg
            .providers
            .get(p.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// A fresh reading when the endpoint gives one (saved), else the last saved.
    pub async fn reading(&self, p: ProviderId) -> (Option<Snapshot>, Option<String>) {
        let Some(q) = self.quota.get(&p) else {
            return (None, Some("not configured".to_owned()));
        };
        match q.usage().await {
            Ok(s) => {
                let _ = self.store.save_snapshot(&s);
                (Some(s), None)
            }
            Err(e) => (self.store.snapshot(p).ok().flatten(), Some(e.to_string())),
        }
    }

    pub fn day_start(&self) -> DateTime<Utc> {
        let starts = budget::parse_hhmm(&self.cfg.schedule.day_starts).unwrap_or_default();
        budget::day_start(Local::now(), starts)
    }

    pub fn spent_today(&self, p: ProviderId) -> f64 {
        self.store
            .spent_since(p, self.day_start())
            .unwrap_or(f64::INFINITY)
    }

    fn source(&self, name: &str) -> Option<&dyn Source> {
        self.sources
            .iter()
            .find(|s| s.name() == name)
            .map(|s| s.as_ref())
    }
}

/// What the runner saves to pick a job up after a restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobState {
    name: String,
    provider: ProviderId,
    work: Work,
    meter: Meter,
    base_commit: String,
    started: DateTime<Utc>,
    snapshot: Option<String>,
}

/// How the agent's run ended.
#[derive(Debug)]
enum End {
    /// The turn finished; look at the result.
    Finished,
    Stopped(JobStatus, String),
}

/// Runs `work` on `provider` to the end. Returns the finished job.
pub async fn run(ctx: &Ctx, work: Work, provider: ProviderId) -> Result<JobRow> {
    let pcfg = ctx.provider_cfg(provider);
    let now = Utc::now();
    let (snap, err) = ctx.reading(provider).await;
    let Some(weekly) = snap.as_ref().and_then(|s| s.weekly.clone()) else {
        bail!(
            "no weekly quota reading for {provider}: {}",
            err.unwrap_or_default()
        );
    };
    let name = job_name(now, &work.slug());
    let over = ctx
        .cfg
        .repo_override(work.repo.remote.as_deref(), &work.repo.path);
    let hint = match &work.what {
        What::Task(t) => ctx.source(&t.source).and_then(|s| s.agent_hint(t)),
        What::Preset(_) => None,
    };
    let (title, body) = match &work.what {
        What::Task(t) => (t.title.clone(), t.body.clone()),
        What::Preset(p) => (format!("Preset: {}", p.name), p.prompt.clone()),
    };
    let local_branch = harness::branch(&name);
    let text = prompt::build(&prompt::PromptInput {
        title: &title,
        body: &body,
        mode: work.mode(),
        branch: &local_branch,
        hint,
        verify: over.verify.as_deref(),
    });

    // Start from what the remote has now, when it can be reached.
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&work.repo.path)
        .args(["fetch", "-q", &ctx.cfg.publish.remote])
        .output()
        .await;

    tracing::info!(job = %name, %provider, repo = %work.repo.key(), "starting: {}", work.title());
    let agent = Agent::launch(harness::Spec {
        harness: provider.harness(),
        repo: work.repo.path.clone(),
        base: work.repo.base.clone(),
        name: name.clone(),
        prompt: text,
        model: pcfg.model.clone(),
        permission: pcfg
            .permission
            .clone()
            .or_else(|| default_permission(provider)),
        launcher: Some(subscription_launcher(provider, pcfg.launcher.as_ref())),
        policy: crate::policy::rules(&pcfg.policy, pcfg.default_policy),
        labels: vec![
            ("subr_job".to_owned(), name.clone()),
            ("subr_work".to_owned(), work.key()),
        ],
    })
    .await?;
    let base_commit = read_base_commit(&name)
        .await
        .unwrap_or_else(|| "HEAD".to_owned());
    let state = JobState {
        name: name.clone(),
        provider,
        work,
        meter: Meter::start(&weekly),
        base_commit,
        started: now,
        snapshot: agent.snapshot(),
    };
    ctx.store.insert_job(&JobRow {
        id: name.clone(),
        provider: provider.as_str().to_owned(),
        repo: state.work.repo.key(),
        task_key: state.work.key(),
        title: state.work.title(),
        status: JobStatus::Running,
        started: now,
        ended: None,
        spent: 0.0,
        pr_url: None,
        detail: None,
        state: Some(serde_json::to_string(&state)?),
    })?;
    drive(
        ctx,
        agent,
        state,
        snap.unwrap_or_else(|| unreachable_snapshot(provider)),
    )
    .await
}

/// Picks up jobs that were running when subr stopped.
pub async fn resume_all(ctx: &Ctx) -> Result<()> {
    for row in ctx.store.running_jobs()? {
        let Some(state) = row
            .state
            .as_deref()
            .and_then(|s| serde_json::from_str::<JobState>(s).ok())
        else {
            ctx.store
                .finish_job(&row.id, JobStatus::Failed, None, Some("lost its state"))?;
            continue;
        };
        let pcfg = ctx.provider_cfg(state.provider);
        let policy = crate::policy::rules(&pcfg.policy, pcfg.default_policy);
        let agent = match state.snapshot.as_deref() {
            Some(snap) => Agent::restore(snap, &state.name, policy).await,
            None => Err(anyhow::anyhow!("no session snapshot")),
        };
        match agent {
            Ok(agent) => {
                tracing::info!(job = %row.id, "reattached");
                let latest = ctx
                    .store
                    .snapshot(state.provider)?
                    .unwrap_or_else(|| unreachable_snapshot(state.provider));
                drive(ctx, agent, state, latest).await?;
            }
            Err(e) => {
                tracing::warn!(job = %row.id, "can't reattach: {e:#}");
                ctx.store.finish_job(
                    &row.id,
                    JobStatus::Failed,
                    None,
                    Some(&format!("can't reattach: {e:#}")),
                )?;
            }
        }
    }
    Ok(())
}

/// A reading that is too old to allow anything: used when there is none.
fn unreachable_snapshot(provider: ProviderId) -> Snapshot {
    Snapshot {
        provider,
        session: None,
        weekly: None,
        plan: None,
        fetched: DateTime::<Utc>::UNIX_EPOCH,
        origin: crate::quota::Origin::Endpoint,
    }
}

fn default_permission(p: ProviderId) -> Option<String> {
    Some(
        match p {
            ProviderId::Claude => "acceptEdits",
            ProviderId::Codex => ":workspace",
        }
        .to_owned(),
    )
}

/// Environment variables that make a harness bill an API account instead of
/// the subscription. Headless Claude Code prefers `ANTHROPIC_API_KEY` over
/// its login without asking, so a key left in the environment fails a job
/// with "Credit balance is too low" while the subscription has quota.
fn api_key_vars(p: ProviderId) -> &'static [&'static str] {
    match p {
        ProviderId::Claude => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
        ProviderId::Codex => &["OPENAI_API_KEY", "CODEX_API_KEY"],
    }
}

/// The configured launcher, run under `env -u ...` so the harness never
/// sees an API key and uses the subscription login.
fn subscription_launcher(
    p: ProviderId,
    configured: Option<&acc_core::harness::Launcher>,
) -> acc_core::harness::Launcher {
    let mut launcher = configured.cloned().unwrap_or_default();
    let mut prefix = vec!["env".to_owned()];
    for var in api_key_vars(p) {
        prefix.push("-u".to_owned());
        prefix.push((*var).to_owned());
    }
    prefix.append(&mut launcher.prefix);
    launcher.prefix = prefix;
    launcher
}

fn job_name(now: DateTime<Utc>, slug: &str) -> String {
    let mut name = format!(
        "subr-{}-{slug}",
        now.with_timezone(&Local).format("%Y%m%d-%H%M%S")
    );
    name.truncate(64);
    name.trim_end_matches('-').to_owned()
}

async fn read_base_commit(name: &str) -> Option<String> {
    let meta = crate::paths::acc_worktree(name).with_extension("meta");
    let text = tokio::fs::read_to_string(meta).await.ok()?;
    text.lines().next().map(str::to_owned)
}

/// Charges a reading to the job and keeps it if it's newer.
fn take_reading(ctx: &Ctx, state: &mut JobState, latest: &mut Snapshot, snap: Snapshot) {
    let snap = snap.merged_over(latest);
    if let Some(w) = &snap.weekly {
        let charge = state.meter.observe(w);
        if charge > 0.0 {
            tracing::info!(job = %state.name, "charged {charge:.1}% of the weekly window");
            let _ = ctx
                .store
                .charge(state.provider, Utc::now(), charge, &state.name);
        }
    }
    if snap.fetched >= latest.fetched {
        let _ = ctx.store.save_snapshot(&snap);
        *latest = snap;
    }
}

async fn drive(
    ctx: &Ctx,
    mut agent: Agent,
    mut state: JobState,
    mut latest: Snapshot,
) -> Result<JobRow> {
    let pcfg = ctx.provider_cfg(state.provider);
    let jobs = &ctx.cfg.jobs;
    let watch = Duration::from_secs(jobs.watch_secs.max(10));
    let stale = Duration::from_secs(jobs.stale_kill_after_secs);
    let deadline = state.started + chrono::Duration::minutes(jobs.timeout_mins as i64);
    let mut last_check = Instant::now();
    let stop_key = format!("stop:{}", state.name);

    let end = loop {
        tokio::time::sleep(PUMP_EVERY).await;
        let p = match agent.pump().await {
            Ok(p) => p,
            Err(e) => break End::Stopped(JobStatus::Failed, format!("lost the agent: {e:#}")),
        };
        for r in &p.rejected {
            tracing::info!(job = %state.name, "rejected (no policy rule): {r}");
        }
        if let Some(q) = &p.quota
            && let Some(s) = crate::quota::from_acc(state.provider, q, Utc::now())
        {
            take_reading(ctx, &mut state, &mut latest, s);
        }
        if last_check.elapsed() >= watch {
            last_check = Instant::now();
            agent.refresh().await;
            if let Some(q) = ctx.quota.get(&state.provider) {
                match q.usage().await {
                    Ok(s) => take_reading(ctx, &mut state, &mut latest, s),
                    Err(e) => tracing::warn!(job = %state.name, "quota: {e}"),
                }
            }
            state.snapshot = agent.snapshot();
            let _ = ctx
                .store
                .save_job_state(&state.name, &serde_json::to_string(&state)?);
        }
        let spent = ctx.spent_today(state.provider);
        if ctx.store.flag(&stop_key)?.is_some() {
            ctx.store.set_flag(&stop_key, None)?;
            break End::Stopped(JobStatus::Failed, "stopped with `subr stop`".to_owned());
        }
        if let Verdict::No(why) = budget::must_stop(&pcfg, Some(&latest), spent, Utc::now(), stale)
        {
            break End::Stopped(JobStatus::BudgetStopped, why);
        }
        if p.needs_auth {
            break End::Stopped(
                JobStatus::Failed,
                "the harness needs you to sign in".to_owned(),
            );
        }
        if let Some(e) = p.error {
            break End::Stopped(JobStatus::Failed, e);
        }
        if p.turn_done {
            break End::Finished;
        }
        if p.exited {
            break End::Stopped(
                JobStatus::Failed,
                "the harness exited before finishing".to_owned(),
            );
        }
        if Utc::now() > deadline {
            break End::Stopped(
                JobStatus::TimedOut,
                format!("ran past {} minutes", jobs.timeout_mins),
            );
        }
    };

    if let End::Stopped(status, why) = &end {
        tracing::warn!(job = %state.name, "stopping ({}): {why}", status.as_str());
        agent.interrupt().await;
        let until = Instant::now() + INTERRUPT_GRACE;
        while Instant::now() < until {
            tokio::time::sleep(PUMP_EVERY).await;
            if agent
                .pump()
                .await
                .map(|p| p.turn_done || p.exited)
                .unwrap_or(true)
            {
                break;
            }
        }
    }
    let _ = agent.kill().await;

    // What the job used after the last reading shows up a little later.
    let wait = Duration::from_secs(pcfg.min_fetch_secs).saturating_sub(last_check.elapsed());
    tokio::time::sleep(wait).await;
    if let Some(q) = ctx.quota.get(&state.provider)
        && let Ok(s) = q.usage().await
    {
        take_reading(ctx, &mut state, &mut latest, s);
    }

    finish(ctx, &agent, &state, end).await
}

async fn finish(ctx: &Ctx, agent: &Agent, state: &JobState, end: End) -> Result<JobRow> {
    let work = &state.work;
    let message = agent.last_message().unwrap_or_default();
    let result = prompt::parse_result(&message);
    let summary = result
        .as_ref()
        .map(|r| r.summary.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| prompt::strip_result(&message));
    save_transcript(agent, state, &message).await;

    let (status, published, detail) = match end {
        End::Stopped(status, why) => {
            match status {
                // Keep the work for a look; the session itself can go.
                JobStatus::BudgetStopped | JobStatus::TimedOut => {
                    let _ = agent.remove_session().await;
                }
                _ => {
                    let _ = agent.discard().await;
                }
            }
            (status, None, Some(why))
        }
        End::Finished => match work.mode() {
            Mode::Report => {
                let filed = file_report(ctx, state, &summary).await;
                let _ = agent.discard().await;
                (JobStatus::Done, None, Some(filed))
            }
            Mode::Pr => publish_work(ctx, agent, state, result.as_ref(), &summary).await,
        },
    };

    let pr_url = published.as_ref().and_then(|p| p.url().map(str::to_owned));
    let detail = detail.unwrap_or_else(|| summary.lines().next().unwrap_or("").to_owned());
    ctx.store
        .record_attempt(&work.key(), status, pr_url.as_deref())?;
    if let What::Preset(p) = &work.what
        && status != JobStatus::BudgetStopped
    {
        ctx.store.preset_ran(&work.repo.key(), &p.name)?;
    }
    if let What::Task(task) = &work.what
        && let Some(source) = ctx.source(&task.source)
    {
        let outcome = Outcome {
            status,
            summary: if summary.trim().is_empty() {
                detail.clone()
            } else {
                summary.clone()
            },
            pr_url: pr_url.clone(),
        };
        let gave_up = ctx
            .store
            .attempts(&work.key())?
            .is_some_and(|a| a.count >= ctx.cfg.jobs.max_attempts);
        let told = match status {
            JobStatus::Done => source.complete(task, &work.repo, &outcome).await,
            JobStatus::Blocked | JobStatus::NoChange => {
                source.fail(task, &work.repo, &outcome).await
            }
            _ if gave_up => source.fail(task, &work.repo, &outcome).await,
            _ => Ok(()),
        };
        if let Err(e) = told {
            tracing::warn!(job = %state.name, "telling {}: {e:#}", task.source);
        }
    }
    ctx.store
        .finish_job(&state.name, status, pr_url.as_deref(), Some(&detail))?;
    tracing::info!(job = %state.name, "finished: {} ({detail})", status.as_str());
    let rows = ctx.store.recent_jobs(50)?;
    rows.into_iter()
        .find(|r| r.id == state.name)
        .context("the job vanished from the store")
}

async fn publish_work(
    ctx: &Ctx,
    agent: &Agent,
    state: &JobState,
    result: Option<&AgentResult>,
    summary: &str,
) -> (JobStatus, Option<Published>, Option<String>) {
    let work = &state.work;
    match result.map(|r| r.status) {
        Some(ResultStatus::Blocked) => {
            let _ = agent.discard().await;
            return (JobStatus::Blocked, None, None);
        }
        Some(ResultStatus::NoChange) => {
            let _ = agent.discard().await;
            return (JobStatus::NoChange, None, None);
        }
        _ => {}
    }
    let over = ctx
        .cfg
        .repo_override(work.repo.remote.as_deref(), &work.repo.path);
    let title = result
        .and_then(|r| r.pr_title.clone())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| work.title());
    let body = pr_body(ctx, state, result, summary);
    let branch = format!(
        "{}{}",
        ctx.cfg.publish.branch_prefix,
        state.name.trim_start_matches("subr-")
    );
    let local_branch = harness::branch(&state.name);
    let worktree = agent.worktree_dir();
    let push = over.push.unwrap_or(ctx.cfg.publish.push);
    let req = publish::Request {
        repo: &work.repo,
        worktree: &worktree,
        local_branch: &local_branch,
        branch: &branch,
        base_commit: &state.base_commit,
        title: &title,
        body: &body,
        push,
        verify: over.verify.as_deref(),
    };
    match publish::publish(&req, &ctx.cfg.publish).await {
        Ok(Published::Nothing) => {
            let _ = agent.discard().await;
            (
                JobStatus::NoChange,
                Some(Published::Nothing),
                Some("no changes".to_owned()),
            )
        }
        Ok(Published::Local(_)) => {
            let _ = agent.remove_session().await;
            let kept =
                publish::keep_local(&work.repo.path, &worktree, &local_branch, &branch).await;
            let detail = match kept {
                Ok(()) => format!("kept as local branch {branch}"),
                Err(e) => format!("left in worktree {}: {e:#}", worktree.display()),
            };
            (
                JobStatus::Done,
                Some(Published::Local(branch)),
                Some(detail),
            )
        }
        Ok(p) => {
            let _ = agent.discard().await;
            let d = p.describe();
            (JobStatus::Done, Some(p), Some(d))
        }
        Err(e) => {
            let _ = agent.remove_session().await;
            (
                JobStatus::Failed,
                None,
                Some(format!(
                    "publishing failed (worktree kept at {}): {e:#}",
                    worktree.display()
                )),
            )
        }
    }
}

fn pr_body(ctx: &Ctx, state: &JobState, result: Option<&AgentResult>, summary: &str) -> String {
    let mut body = result
        .and_then(|r| r.pr_body.clone())
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| summary.to_owned());
    if let What::Task(t) = &state.work.what
        && let Some(n) = t.meta.get("number")
        && t.source == "github"
    {
        body.push_str(&format!("\n\nCloses #{n}"));
    }
    let spent = ctx
        .store
        .recent_jobs(50)
        .ok()
        .and_then(|j| j.into_iter().find(|r| r.id == state.name))
        .map_or(0.0, |r| r.spent);
    let source = match &state.work.what {
        What::Task(t) => format!("task from {}", t.source),
        What::Preset(p) => format!("preset `{}`", p.name),
    };
    body.push_str(&format!(
        "\n\n---\nOpened by [subr](https://github.com/lum1n/subr) ({source}, job `{}`, {} via {:.1}% of the weekly quota).",
        state.name,
        state.provider,
        spent
    ));
    body
}

async fn file_report(ctx: &Ctx, state: &JobState, summary: &str) -> String {
    let title = format!(
        "{} ({})",
        state.work.title(),
        Local::now().format("%Y-%m-%d")
    );
    for s in &ctx.sources {
        match s.report(&state.work.repo, &title, summary).await {
            Ok(true) => return format!("report filed in {}", s.name()),
            Ok(false) => {}
            Err(e) => tracing::warn!("report to {}: {e:#}", s.name()),
        }
    }
    let path = crate::paths::state_dir()
        .join("reports")
        .join(format!("{}.md", state.name));
    let text = format!("# {title}\n\n{summary}\n");
    let ok = async {
        tokio::fs::create_dir_all(path.parent().unwrap_or(&path)).await?;
        tokio::fs::write(&path, text).await
    }
    .await;
    match ok {
        Ok(()) => format!("report written to {}", path.display()),
        Err(e) => format!("report lost: {e}"),
    }
}

async fn save_transcript(agent: &Agent, state: &JobState, message: &str) {
    let dir = crate::paths::state_dir().join("jobs").join(&state.name);
    let _ = tokio::fs::create_dir_all(&dir).await;
    let _ = tokio::fs::write(dir.join("final.md"), message).await;
    if let Ok(usage) = serde_json::to_string_pretty(agent.usage()) {
        let _ = tokio::fs::write(dir.join("usage.json"), usage).await;
    }
    // The full harness log, for when something needs explaining.
    let log = crate::paths::acc_session(&state.name).join("out.ndjson");
    let _ = tokio::fs::copy(&log, dir.join("out.ndjson")).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_names_are_valid_acc_names() {
        let n = job_name(Utc::now(), &"x".repeat(80));
        assert!(harness::valid_name(&n), "{n}");
        assert!(n.len() <= 64);
    }

    #[test]
    fn launcher_drops_api_keys_before_the_configured_prefix() {
        let configured = acc_core::harness::Launcher {
            prefix: vec!["nice".to_owned(), "-n".to_owned(), "10".to_owned()],
            ..Default::default()
        };
        let l = subscription_launcher(ProviderId::Claude, Some(&configured));
        assert_eq!(
            l.prefix,
            [
                "env",
                "-u",
                "ANTHROPIC_API_KEY",
                "-u",
                "ANTHROPIC_AUTH_TOKEN",
                "nice",
                "-n",
                "10"
            ]
        );
        let l = subscription_launcher(ProviderId::Codex, None);
        assert_eq!(
            l.prefix,
            ["env", "-u", "OPENAI_API_KEY", "-u", "CODEX_API_KEY"]
        );
    }
}
