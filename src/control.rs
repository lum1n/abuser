//! Looking into and steering what subr does: one job's details and log,
//! the native queue, and skipping, retrying or forcing a piece of work.
//! The daemon hears about changes through the store.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Local, Utc};
use serde::Serialize;

use crate::daemon;
use crate::harness::{LogLine, LogReader};
use crate::quota::ProviderId;
use crate::runner::Ctx;
use crate::select::{NEXT_FLAG, What, Work};
use crate::store::{JobRow, JobStatus};

/// The job `query` names (all or part of its id; the latest of several),
/// else the running job, else the latest.
pub fn find_job(ctx: &Ctx, query: Option<&str>) -> Result<JobRow> {
    let Some(q) = query else {
        return ctx
            .store
            .running_jobs()?
            .into_iter()
            .next()
            .or(ctx.store.recent_jobs(1)?.into_iter().next())
            .context("no jobs yet");
    };
    let found = ctx.store.find_jobs(q)?;
    let first = found
        .first()
        .with_context(|| format!("no job matches {q:?}"))?;
    if found.len() > 1 && first.id != q {
        eprintln!("({q:?} matches {} jobs; showing the latest)", found.len());
    }
    Ok(first.clone())
}

/// Where a job's files are, when they are still there.
#[derive(Debug, Serialize)]
pub struct JobFiles {
    pub worktree: Option<PathBuf>,
    /// Transcript, usage and final message, kept after the job.
    pub dir: Option<PathBuf>,
    pub log: Option<PathBuf>,
}

pub fn job_files(id: &str) -> JobFiles {
    let worktree = crate::paths::acc_worktree(id);
    let dir = crate::paths::state_dir().join("jobs").join(id);
    let log = [
        crate::paths::acc_session(id).join("out.ndjson"),
        dir.join("out.ndjson"),
    ]
    .into_iter()
    .find(|p| p.is_file());
    JobFiles {
        worktree: worktree.is_dir().then_some(worktree),
        dir: dir.is_dir().then_some(dir),
        log,
    }
}

/// The agent's final message, without its `subr-result` block.
pub fn job_summary(files: &JobFiles) -> Option<String> {
    let text = std::fs::read_to_string(files.dir.as_ref()?.join("final.md")).ok()?;
    let text = crate::prompt::strip_result(&text);
    (!text.trim().is_empty()).then(|| text.trim().to_owned())
}

pub async fn job(ctx: &Ctx, query: Option<&str>, diff: bool, json: bool) -> Result<()> {
    let j = find_job(ctx, query)?;
    let files = job_files(&j.id);
    let summary = job_summary(&files);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "job": j,
                "files": files,
                "summary": summary,
            }))?
        );
        return Ok(());
    }
    if diff {
        return job_diff(&j, &files).await;
    }
    let ran = j.ended.unwrap_or_else(Utc::now) - j.started;
    println!("{}", j.id);
    println!("  title     {}", j.title);
    println!("  repo      {}", j.repo);
    println!(
        "  status    {}  ·  {}  ·  {:.1}% of the weekly window",
        j.status.as_str(),
        j.provider,
        j.spent
    );
    println!(
        "  started   {}  ·  {} {}m",
        j.started.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
        if j.status == JobStatus::Running {
            "running for"
        } else {
            "ran"
        },
        ran.num_minutes()
    );
    if let Some(pr) = &j.pr_url {
        println!("  pr        {pr}");
    }
    if let Some(d) = j.detail.as_deref().filter(|d| !d.is_empty()) {
        println!("  detail    {d}");
    }
    if let Some(w) = &files.worktree {
        println!("  worktree  {}", w.display());
    }
    if let Some(d) = &files.dir {
        println!("  files     {}", d.display());
    }
    if let Some(s) = summary {
        println!("\n{s}");
    }
    Ok(())
}

async fn job_diff(j: &JobRow, files: &JobFiles) -> Result<()> {
    let Some(worktree) = &files.worktree else {
        match &j.pr_url {
            Some(pr) => bail!("the worktree is gone; the changes are in {pr}"),
            None => bail!("the worktree is gone"),
        }
    };
    let base = crate::runner::read_base_commit(&j.id)
        .await
        .unwrap_or_else(|| "HEAD".to_owned());
    // What isn't committed yet (new files don't show in the diff), then
    // everything since the job started.
    for args in [&["status", "--short"][..], &["diff", &base]] {
        let status = tokio::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(args)
            .status()
            .await
            .context("running git")?;
        if !status.success() {
            bail!("git {} failed", args[0]);
        }
    }
    Ok(())
}

pub fn print_log_line(l: &LogLine) {
    let mut lines = l.text.lines();
    println!("{:>8}  {}", l.kind, lines.next().unwrap_or(""));
    for rest in lines {
        println!("{:>8}  {rest}", "");
    }
}

pub async fn logs(ctx: &Ctx, query: Option<&str>, follow: bool) -> Result<()> {
    let j = find_job(ctx, query)?;
    let harness = ProviderId::parse(&j.provider)
        .with_context(|| format!("unknown provider {}", j.provider))?
        .harness();
    let mut reader = LogReader::new(&j.id, harness)?;
    eprintln!("{}  ({})", j.id, j.status.as_str());
    loop {
        // The log moves to the job's files when the job ends.
        let running =
            ctx.store.find_jobs(&j.id)?.first().map(|r| r.status) == Some(JobStatus::Running);
        let Some(log) = job_files(&j.id).log else {
            if follow && running {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            bail!("no log for {}", j.id);
        };
        for l in reader.read(&log).await? {
            print_log_line(&l);
        }
        if !follow || !running {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

pub fn queue(ctx: &Ctx) -> Result<()> {
    let tasks = ctx.store.open_native()?;
    if tasks.is_empty() {
        println!("the queue is empty (subr add \"title\" --repo <repo>)");
    }
    for t in tasks {
        println!("#{:<4} {}  {}", t.id, t.repo, t.title);
    }
    Ok(())
}

pub fn unqueue(ctx: &Ctx, id: i64) -> Result<()> {
    if !ctx.store.remove_native(id)? {
        bail!("no queued task #{id}");
    }
    println!("removed #{id}");
    Ok(())
}

/// Something in `subr tasks` a command can point at.
struct Candidate {
    key: String,
    title: String,
    work: Option<Work>,
}

/// Picks the candidate `query` names: its number in `subr tasks`, its key,
/// or part of its title.
fn pick(candidates: Vec<Candidate>, numbered: usize, query: &str) -> Result<Candidate> {
    if let Ok(n) = query.parse::<usize>()
        && (1..=numbered).contains(&n)
    {
        return Ok(candidates.into_iter().nth(n - 1).expect("in range"));
    }
    if let Some(i) = candidates.iter().position(|c| c.key == query) {
        return Ok(candidates.into_iter().nth(i).expect("found"));
    }
    let q = query.to_lowercase();
    let mut hits: Vec<Candidate> = candidates
        .into_iter()
        .filter(|c| c.title.to_lowercase().contains(&q))
        .collect();
    match hits.len() {
        0 => bail!("nothing in `subr tasks` matches {query:?}"),
        1 => Ok(hits.remove(0)),
        n => {
            let titles: Vec<&str> = hits.iter().take(10).map(|c| c.title.as_str()).collect();
            bail!("{query:?} matches {n} things:\n  {}", titles.join("\n  "))
        }
    }
}

/// The planned work, then (with `skipped`) what was left out for its attempts.
async fn candidates(ctx: &Ctx, skipped: bool) -> Result<(Vec<Candidate>, usize)> {
    let plan = daemon::plan(ctx).await?;
    let numbered = plan.work.len();
    let mut out: Vec<Candidate> = plan
        .work
        .into_iter()
        .map(|w| Candidate {
            key: w.key(),
            title: w.title(),
            work: Some(w),
        })
        .collect();
    if skipped {
        out.extend(plan.skipped.into_iter().filter_map(|s| {
            Some(Candidate {
                key: s.key?,
                title: s.title,
                work: None,
            })
        }));
    }
    Ok((out, numbered))
}

pub async fn skip(ctx: &Ctx, query: &str) -> Result<()> {
    let (cands, numbered) = candidates(ctx, false).await?;
    let c = pick(cands, numbered, query)?;
    if ctx.store.flag(NEXT_FLAG)?.as_deref() == Some(c.key.as_str()) {
        ctx.store.set_flag(NEXT_FLAG, None)?;
    }
    match c.work.as_ref().map(|w| &w.what) {
        Some(What::Preset(p)) => {
            let repo = c.work.as_ref().map(|w| w.repo.key()).unwrap_or_default();
            ctx.store.preset_ran(&repo, &p.name)?;
            println!(
                "skipped {}; it is due again in {} days",
                c.title, p.cooldown_days
            );
        }
        _ => {
            ctx.store.end_attempt(&c.key, JobStatus::Skipped, None)?;
            println!("skipped {} (subr retry to undo)", c.title);
        }
    }
    Ok(())
}

pub async fn retry(ctx: &Ctx, query: &str) -> Result<()> {
    let (cands, _) = candidates(ctx, true).await?;
    let skipped: Vec<Candidate> = cands.into_iter().filter(|c| c.work.is_none()).collect();
    let c = pick(skipped, 0, query)?;
    ctx.store.clear_attempts(&c.key)?;
    println!("{} will be picked again", c.title);
    Ok(())
}

/// Asks the daemon to run something next, ahead of everything else and
/// whatever its attempts or cooldown say. The budget still decides when.
pub async fn run_now(ctx: &Ctx, query: &str, repo: Option<&str>) -> Result<()> {
    let (key, title) = match repo {
        Some(hint) => {
            let presets = crate::presets::all(&ctx.cfg, &ctx.cfg_dir)?;
            let Some(preset) = presets.iter().find(|p| p.name == query) else {
                let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
                bail!("no preset {query:?} (presets: {})", names.join(", "));
            };
            let repos = crate::repos::scan(&ctx.cfg.repos)?;
            let Some(repo) = repos.into_iter().find(|r| r.matches(hint)) else {
                bail!("no repo under repos.roots matches {hint:?}");
            };
            let w = Work {
                what: What::Preset(preset.clone()),
                repo,
            };
            (w.key(), w.title())
        }
        None => {
            let (cands, numbered) = candidates(ctx, true).await?;
            let c = pick(cands, numbered, query)?;
            (c.key, c.title)
        }
    };
    ctx.store.set_flag(NEXT_FLAG, Some(&key))?;
    println!("{title} runs next, as soon as the budget allows");
    if ctx.store.flag(daemon::PAUSE_FLAG)?.is_some() {
        println!("subr is paused: subr resume to let it start");
    } else if !daemon::heartbeat(&ctx.store).is_some_and(|h| h.alive(Utc::now())) {
        println!("the daemon isn't running: subr once runs it now");
    }
    Ok(())
}
