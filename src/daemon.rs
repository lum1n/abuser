//! The background loop: read quota, pick work, run it, repeat.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;

use crate::budget::{self, Verdict};
use crate::config::Config;
use crate::quota::{ProviderId, Snapshot};
use crate::runner::{self, Ctx};
use crate::select::{self, Plan, Work};
use crate::store::Store;

pub const PAUSE_FLAG: &str = "paused";

/// One provider's standing at the moment.
#[derive(Debug, Clone, Serialize)]
pub struct Standing {
    pub provider: ProviderId,
    pub snapshot: Option<Snapshot>,
    pub error: Option<String>,
    pub spent_today: f64,
    pub daily_limit: f64,
    pub verdict: Verdict,
}

pub async fn standings(ctx: &Ctx) -> Vec<Standing> {
    let mut out = Vec::new();
    for &p in ctx.quota.keys() {
        let cfg = ctx.provider_cfg(p);
        let (snapshot, error) = if cfg.enabled {
            ctx.reading(p).await
        } else {
            (ctx.store.snapshot(p).ok().flatten(), None)
        };
        let spent_today = ctx.spent_today(p);
        let verdict = budget::can_start(
            &cfg,
            &ctx.cfg.schedule,
            snapshot.as_ref(),
            spent_today,
            Utc::now(),
        );
        out.push(Standing {
            provider: p,
            snapshot,
            error,
            spent_today,
            daily_limit: cfg.daily_limit_pct,
            verdict,
        });
    }
    out
}

/// The provider to run `work` on: the repo's pick if it may start, else
/// the one with the most budget left today.
pub fn choose_provider(ctx: &Ctx, standings: &[Standing], work: &Work) -> Option<ProviderId> {
    let over = ctx
        .cfg
        .repo_override(work.repo.remote.as_deref(), &work.repo.path);
    let ok = |p: &&Standing| p.verdict.ok();
    if let Some(want) = over.provider.as_deref().and_then(ProviderId::parse) {
        return standings
            .iter()
            .filter(ok)
            .find(|s| s.provider == want)
            .map(|s| s.provider);
    }
    standings
        .iter()
        .filter(ok)
        .max_by(|a, b| {
            (a.daily_limit - a.spent_today)
                .partial_cmp(&(b.daily_limit - b.spent_today))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|s| s.provider)
}

pub async fn plan(ctx: &Ctx) -> Result<Plan> {
    let repos = crate::repos::scan(&ctx.cfg.repos)?;
    let presets = crate::presets::all(&ctx.cfg, &ctx.cfg_dir)?;
    select::plan(
        &ctx.cfg,
        &ctx.store,
        &ctx.sources,
        &repos,
        &presets,
        Utc::now(),
    )
    .await
}

/// What one tick decided.
#[derive(Debug)]
pub enum Tick {
    Paused,
    NoBudget(Vec<String>),
    NothingToDo,
    Ran(crate::store::JobRow),
    Would(Work, ProviderId),
}

/// One round: if any provider may start a job, run the first runnable piece
/// of work. With `dry_run`, only say what would run.
pub async fn tick(ctx: &Ctx, dry_run: bool) -> Result<Tick> {
    if ctx.store.flag(PAUSE_FLAG)?.is_some() {
        return Ok(Tick::Paused);
    }
    let standings = standings(ctx).await;
    if !standings.iter().any(|s| s.verdict.ok()) {
        let why = standings
            .iter()
            .map(|s| match &s.verdict {
                Verdict::No(w) => format!("{}: {w}", s.provider),
                Verdict::Go => format!("{}: ok", s.provider),
            })
            .collect();
        return Ok(Tick::NoBudget(why));
    }
    let plan = plan(ctx).await?;
    for e in &plan.errors {
        tracing::warn!("task source: {e}");
    }
    for work in plan.work {
        let Some(provider) = choose_provider(ctx, &standings, &work) else {
            continue;
        };
        if work.mode() == crate::presets::Mode::Pr
            && let Some(open) = crate::publish::open_prs(&work.repo, &ctx.cfg.publish.label).await
            && open >= ctx.cfg.publish.max_open_prs
        {
            tracing::debug!(repo = %work.repo.key(), "{open} open subr PRs; skipping");
            continue;
        }
        if dry_run {
            return Ok(Tick::Would(work, provider));
        }
        return Ok(Tick::Ran(runner::run(ctx, work, provider).await?));
    }
    Ok(Tick::NothingToDo)
}

/// Runs until SIGTERM or Ctrl-C. A running job's agent is stopped then, so
/// nothing spends quota while subr isn't watching; its worktree is kept.
/// (After a crash, the next start reattaches to it instead.) The config
/// file is read again between ticks when it changes.
pub async fn run(mut ctx: Ctx, config: Option<PathBuf>) -> Result<()> {
    tracing::info!("subr {} running", env!("CARGO_PKG_VERSION"));
    let path = config.unwrap_or_else(crate::paths::config_file);
    warn_unconfigured(&ctx);
    let mut cfg_mtime = mtime(&path);
    let stopping = ctx.store.clone();
    let work = async {
        if let Err(e) = runner::resume_all(&ctx).await {
            tracing::error!("resuming jobs: {e:#}");
        }
        // The last thing said about an idle tick, so it isn't repeated each time.
        let mut said = String::new();
        loop {
            let now = mtime(&path);
            if now != cfg_mtime {
                cfg_mtime = now;
                match Config::load(Some(&path)) {
                    Ok((cfg, _)) => {
                        ctx = Ctx::new(cfg, ctx.cfg_dir.clone(), ctx.store.clone());
                        tracing::info!("reloaded {}", path.display());
                        warn_unconfigured(&ctx);
                        said.clear();
                    }
                    Err(e) => tracing::error!("keeping the old config: {e:#}"),
                }
            }
            let idle = match tick(&ctx, false).await {
                Ok(Tick::Ran(job)) => {
                    tracing::info!(job = %job.id, "{}: {}", job.status.as_str(), job.detail.unwrap_or_default());
                    said.clear();
                    // Look again soon: there may be budget for more.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                Ok(Tick::NoBudget(why)) => format!("not starting: {}", why.join("; ")),
                Ok(Tick::NothingToDo) => "nothing to do".to_owned(),
                Ok(Tick::Paused) => "paused".to_owned(),
                Ok(Tick::Would(..)) => String::new(),
                Err(e) => {
                    tracing::error!("{e:#}");
                    String::new()
                }
            };
            if idle != said {
                tracing::info!("{idle}");
                said = idle;
            }
            tokio::time::sleep(Duration::from_secs(ctx.cfg.schedule.tick_secs.max(30))).await;
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        () = work => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
        _ = term.recv() => tracing::info!("terminated"),
    }
    stop_running(&stopping, |_| true).await
}

fn warn_unconfigured(ctx: &Ctx) {
    if ctx.cfg.repos.roots.is_empty() {
        tracing::warn!("repos.roots is empty: no repos to work in (see `subr config example`)");
    }
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Stops the running jobs `which` picks; their worktrees are kept.
pub async fn stop_running(store: &Store, which: impl Fn(&str) -> bool) -> Result<()> {
    for job in store.running_jobs()? {
        if !which(&job.id) {
            continue;
        }
        tracing::info!(job = %job.id, "stopping the agent; worktree kept");
        if let Err(e) = crate::harness::kill_session(&job.id).await {
            tracing::warn!(job = %job.id, "{e:#}");
        }
        store.finish_job(
            &job.id,
            crate::store::JobStatus::Failed,
            None,
            Some("subr was stopped while it ran; the worktree is kept"),
        )?;
        store.end_attempt(&job.task_key, crate::store::JobStatus::Failed, None)?;
    }
    Ok(())
}
