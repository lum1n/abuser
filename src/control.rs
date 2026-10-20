//! Looking into and steering what abuser does: one job's details and log,
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

/// The agent's final message, without its `abuser-result` block.
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
        outln!(
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
    outln!("{}", j.id);
    outln!("  title     {}", j.title);
    outln!("  repo      {}", j.repo);
    outln!(
        "  status    {}  ·  {}  ·  {:.1}% of the weekly window",
        j.status.as_str(),
        j.provider,
        j.spent
    );
    outln!(
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
        outln!("  pr        {pr}");
    }
    if let Some(d) = j.detail.as_deref().filter(|d| !d.is_empty()) {
        outln!("  detail    {d}");
    }
    if let Some(w) = &files.worktree {
        outln!("  worktree  {}", w.display());
    }
    if let Some(d) = &files.dir {
        outln!("  files     {}", d.display());
    }
    if let Some(s) = summary {
        outln!("\n{s}");
    }
    Ok(())
}

/// The git commands that show a job's changes: what isn't committed yet
/// (new files don't show in the diff), then everything since it started.
async fn diff_commands(j: &JobRow, files: &JobFiles) -> Result<Vec<tokio::process::Command>> {
    let Some(worktree) = &files.worktree else {
        match &j.pr_url {
            Some(pr) => bail!("the worktree is gone; the changes are in {pr}"),
            None => bail!("the worktree is gone"),
        }
    };
    let base = crate::runner::read_base_commit(&j.id)
        .await
        .unwrap_or_else(|| "HEAD".to_owned());
    Ok([vec!["status", "--short"], vec!["diff", &base]]
        .into_iter()
        .map(|args| {
            let mut c = tokio::process::Command::new("git");
            c.arg("-C").arg(worktree).args(args);
            c
        })
        .collect())
}

async fn job_diff(j: &JobRow, files: &JobFiles) -> Result<()> {
    for mut c in diff_commands(j, files).await? {
        if !c.status().await.context("running git")?.success() {
            bail!("git failed");
        }
    }
    Ok(())
}

/// A job's changes as text, as `abuser job --diff` shows them.
pub async fn diff_text(j: &JobRow) -> Result<String> {
    let mut text = String::new();
    for mut c in diff_commands(j, &job_files(&j.id)).await? {
        let out = c.output().await.context("running git")?;
        if !out.status.success() {
            bail!("git: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        text.push_str(&String::from_utf8_lossy(&out.stdout));
    }
    Ok(text)
}

/// A log line as people read it: the agent's `abuser-result` block is for
/// abuser, and its summary shows elsewhere.
pub fn readable(mut l: LogLine) -> Option<LogLine> {
    if l.kind == "agent" {
        l.text = crate::prompt::strip_result(&l.text).trim().to_owned();
    }
    (!l.text.is_empty()).then_some(l)
}

pub fn print_log_line(l: &LogLine) {
    let mut lines = l.text.lines();
    outln!("{:>8}  {}", l.kind, lines.next().unwrap_or(""));
    for rest in lines {
        outln!("{:>8}  {rest}", "");
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
        for l in reader.read(&log).await?.into_iter().filter_map(readable) {
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
        outln!("the queue is empty (abuser add \"title\" --repo <repo>)");
    }
    for t in tasks {
        outln!("#{:<4} {}  {}", t.id, t.repo, t.title);
    }
    Ok(())
}

pub fn unqueue(ctx: &Ctx, id: i64) -> Result<()> {
    remove(ctx, id)?;
    outln!("removed #{id}");
    Ok(())
}

pub fn remove(ctx: &Ctx, id: i64) -> Result<()> {
    if !ctx.store.remove_native(id)? {
        bail!("no queued task #{id}");
    }
    Ok(())
}

/// Something in `abuser tasks` a command can point at.
struct Candidate {
    key: String,
    title: String,
    work: Option<Work>,
}

/// Picks the candidate `query` names: its number in `abuser tasks`, its key,
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
        0 => bail!("nothing in `abuser tasks` matches {query:?}"),
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
    let Some(work) = &c.work else {
        bail!("{} is skipped already", c.title);
    };
    outln!("{}", skip_work(ctx, work)?);
    Ok(())
}

/// Leaves a task out until it is retried, or puts a preset off for a cooldown.
pub fn skip_work(ctx: &Ctx, work: &Work) -> Result<String> {
    let key = work.key();
    if ctx.store.flag(NEXT_FLAG)?.as_deref() == Some(key.as_str()) {
        ctx.store.set_flag(NEXT_FLAG, None)?;
    }
    Ok(match &work.what {
        What::Preset(p) => {
            ctx.store.preset_ran(&work.repo.key(), &p.name)?;
            format!(
                "skipped {}; it is due again in {} days",
                work.title(),
                p.cooldown_days
            )
        }
        What::Task(_) => {
            ctx.store.end_attempt(&key, JobStatus::Skipped, None)?;
            format!("skipped {} (retry to undo)", work.title())
        }
    })
}

pub async fn retry(ctx: &Ctx, query: &str) -> Result<()> {
    let (cands, _) = candidates(ctx, true).await?;
    let skipped: Vec<Candidate> = cands.into_iter().filter(|c| c.work.is_none()).collect();
    let c = pick(skipped, 0, query)?;
    ctx.store.clear_attempts(&c.key)?;
    outln!("{} will be picked again", c.title);
    Ok(())
}

/// Queues a task for `repo`: a remote, or a path to a checkout.
pub fn add(ctx: &Ctx, title: &str, repo: &str, body: &str) -> Result<String> {
    if title.trim().is_empty() {
        bail!("the task needs a title");
    }
    let repo = match crate::repos::normalize_remote(repo) {
        Some(r) => r,
        None => std::fs::canonicalize(crate::paths::expand_home(repo.as_ref()))
            .with_context(|| format!("no such repo {repo:?}"))?
            .display()
            .to_string(),
    };
    let id = ctx.store.add_native(&repo, title.trim(), body)?;
    let mut said = format!("queued #{id} for {repo}");
    if !ctx
        .cfg
        .sources
        .iter()
        .any(|s| matches!(s, crate::config::SourceCfg::Native {}))
    {
        said.push_str(
            "; note: the native source isn't in `sources`, so this task won't be picked up",
        );
    }
    Ok(said)
}

/// Asks the daemon to stop a running job.
pub fn stop(ctx: &Ctx, job: &str) -> Result<String> {
    if !ctx.store.running_jobs()?.iter().any(|j| j.id == job) {
        bail!("no running job {job}");
    }
    ctx.store.set_flag(&format!("stop:{job}"), Some("1"))?;
    Ok(format!(
        "stopping {job} (the daemon acts on it within seconds)"
    ))
}

pub fn set_paused(ctx: &Ctx, paused: bool) -> Result<()> {
    let at = Utc::now().to_rfc3339();
    ctx.store
        .set_flag(daemon::PAUSE_FLAG, paused.then_some(at.as_str()))
}

/// Opens the config in `$VISUAL` or `$EDITOR`, then checks it.
pub fn edit_config(path: &std::path::Path) -> Result<String> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, crate::config::EXAMPLE)?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned());
    // The editor may carry flags (`code --wait`).
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("starting {editor}"))?;
    if !status.success() {
        bail!("{editor} failed");
    }
    crate::config::Config::load(Some(path))
        .context("the config has a problem; abuser keeps the old one")?;
    Ok(format!("{} is valid", path.display()))
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
    outln!("{}", run_next(ctx, &key, &title)?);
    Ok(())
}

/// Puts the work with `key` first in line.
pub fn run_next(ctx: &Ctx, key: &str, title: &str) -> Result<String> {
    ctx.store.set_flag(NEXT_FLAG, Some(key))?;
    let mut said = format!("{title} runs next, as soon as the budget allows");
    if ctx.store.flag(daemon::PAUSE_FLAG)?.is_some() {
        said.push_str("\nabuser is paused: resume to let it start");
    } else if !daemon::heartbeat(&ctx.store).is_some_and(|h| h.alive(Utc::now())) {
        said.push_str("\nthe daemon isn't running: abuser once runs it now");
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<Candidate> {
        [
            ("gh:a/b#1", "Fix the login bug"),
            ("gh:a/b#2", "Add tests"),
            ("tests:repo", "Tests for repo"),
        ]
        .into_iter()
        .map(|(key, title)| Candidate {
            key: key.to_owned(),
            title: title.to_owned(),
            work: None,
        })
        .collect()
    }

    fn picked(numbered: usize, query: &str) -> Result<String> {
        pick(candidates(), numbered, query).map(|c| c.key)
    }

    #[test]
    fn pick_by_number_key_or_title() {
        assert_eq!(picked(2, "1").unwrap(), "gh:a/b#1");
        assert_eq!(picked(2, "2").unwrap(), "gh:a/b#2");
        // Only the planned work is numbered; the skipped can't be picked so.
        assert!(picked(2, "3").is_err());
        assert!(picked(2, "0").is_err());
        assert_eq!(picked(2, "tests:repo").unwrap(), "tests:repo");
        assert_eq!(picked(2, "LOGIN").unwrap(), "gh:a/b#1");
        assert_eq!(picked(2, "for repo").unwrap(), "tests:repo");
    }

    #[test]
    fn pick_refuses_no_or_several_matches() {
        let e = picked(2, "deploy").unwrap_err().to_string();
        assert!(e.contains("nothing in `abuser tasks` matches"), "{e}");
        let e = picked(2, "tests").unwrap_err().to_string();
        assert!(e.contains("matches 2 things"), "{e}");
        assert!(
            e.contains("Add tests") && e.contains("Tests for repo"),
            "{e}"
        );
    }

    #[test]
    fn readable_hides_the_result_block() {
        let line = |kind, text: &str| LogLine {
            kind,
            text: text.to_owned(),
        };
        let r = readable(line("agent", "Done.\n\n```abuser-result\n{}\n```"));
        assert_eq!(r, Some(line("agent", "Done.")));
        assert_eq!(readable(line("agent", "```abuser-result\n{}\n```")), None);
        assert_eq!(readable(line("run", "")), None);
        // Only the agent's own messages carry the block.
        let run = line("run", "cat x\n```abuser-result");
        assert_eq!(readable(run.clone()), Some(run));
    }
}
