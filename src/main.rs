//! subr: spends what's left of your AI subscriptions on background work,
//! without going over a daily limit.

mod budget;
mod config;
mod daemon;
mod gh;
mod harness;
mod paths;
mod policy;
mod presets;
mod prompt;
mod publish;
mod quota;
mod repos;
mod runner;
mod select;
mod service;
mod store;
mod tasks;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{Local, Utc};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::daemon::{PAUSE_FLAG, Tick};
use crate::runner::Ctx;
use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "subr",
    version,
    about = "Spend spare AI subscription quota on background coding work"
)]
struct Cli {
    /// Config file (default: $SUBR_CONFIG or ~/.config/subr/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the foreground: watch quota and work when there is budget.
    Run,
    /// Quota, today's spend and the latest jobs.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// The work subr would pick, in order, and what it skips.
    Tasks {
        #[arg(long)]
        json: bool,
    },
    /// One round now: run the next piece of work if the budget allows.
    Once {
        /// Only say what would run.
        #[arg(long)]
        dry_run: bool,
    },
    /// Queue a task in subr's own queue.
    Add {
        title: String,
        /// The repo: a path, or a remote like github.com/you/project.
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Recent jobs.
    Jobs {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Stop starting jobs (a running one finishes).
    Pause,
    /// Start jobs again.
    Resume,
    /// Stop a running job; its worktree is discarded.
    Stop { job: String },
    /// Install subr as a user service (systemd on Linux, launchd on macOS).
    InstallService {
        /// Print the service file instead of installing it.
        #[arg(long)]
        print: bool,
    },
    /// Config helpers.
    #[command(subcommand)]
    Config(ConfigCmd),
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Check the config, the CLIs and the logins.
    Check,
    /// Print a commented example config.
    Example,
    /// Print where the config and state live.
    Path,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SUBR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("subr=info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let cli = Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

fn open(cli: &Cli) -> Result<Ctx> {
    let (cfg, path) = Config::load(cli.config.as_deref())?;
    let store = Store::open(&paths::state_dir().join("subr.db"))?;
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    Ok(Ctx::new(cfg, dir, store))
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Config(ConfigCmd::Example) => {
            print!("{}", config::EXAMPLE);
            return Ok(());
        }
        Cmd::Config(ConfigCmd::Path) => {
            println!(
                "config: {}",
                cli.config
                    .clone()
                    .unwrap_or_else(paths::config_file)
                    .display()
            );
            println!("state:  {}", paths::state_dir().display());
            return Ok(());
        }
        Cmd::InstallService { print } => return service::install(*print, cli.config.as_deref()),
        _ => {}
    }
    let ctx = open(&cli)?;
    match cli.cmd {
        Cmd::Run => daemon::run(ctx, cli.config.clone()).await,
        Cmd::Status { json } => status(&ctx, json).await,
        Cmd::Tasks { json } => tasks(&ctx, json).await,
        Cmd::Once { dry_run } => once(&ctx, dry_run).await,
        Cmd::Add { title, repo, body } => {
            let repo = match repos::normalize_remote(&repo) {
                Some(r) => r,
                None => std::fs::canonicalize(paths::expand_home(repo.as_ref()))
                    .with_context(|| format!("no such repo {repo:?}"))?
                    .display()
                    .to_string(),
            };
            let id = ctx.store.add_native(&repo, &title, &body)?;
            if !ctx
                .cfg
                .sources
                .iter()
                .any(|s| matches!(s, config::SourceCfg::Native {}))
            {
                eprintln!(
                    "note: the native source isn't in `sources`, so this task won't be picked up"
                );
            }
            println!("queued #{id} for {repo}");
            Ok(())
        }
        Cmd::Jobs { limit } => {
            for j in ctx.store.recent_jobs(limit)? {
                print_job(&j);
            }
            Ok(())
        }
        Cmd::Pause => {
            ctx.store
                .set_flag(PAUSE_FLAG, Some(&Utc::now().to_rfc3339()))?;
            println!("paused");
            Ok(())
        }
        Cmd::Resume => {
            ctx.store.set_flag(PAUSE_FLAG, None)?;
            println!("resumed");
            Ok(())
        }
        Cmd::Stop { job } => {
            if !ctx.store.running_jobs()?.iter().any(|j| j.id == job) {
                bail!("no running job {job}");
            }
            ctx.store.set_flag(&format!("stop:{job}"), Some("1"))?;
            println!("stopping {job} (the daemon acts on it within seconds)");
            Ok(())
        }
        Cmd::Config(ConfigCmd::Check) => check(&ctx).await,
        Cmd::Config(_) | Cmd::InstallService { .. } => Ok(()),
    }
}

fn print_job(j: &store::JobRow) {
    println!(
        "{}  {:<14} {:<6} {:>5.1}%  {}  {}{}",
        j.started.with_timezone(&Local).format("%m-%d %H:%M"),
        j.status.as_str(),
        j.provider,
        j.spent,
        j.repo,
        j.title,
        j.pr_url
            .as_deref()
            .map(|u| format!("  {u}"))
            .unwrap_or_default(),
    );
    if let Some(d) = j.detail.as_deref().filter(|d| !d.is_empty()) {
        println!("    {d}");
    }
}

fn fmt_window(label: &str, w: Option<&quota::Window>) -> String {
    match w {
        Some(w) => {
            let reset = w
                .resets_at
                .map(|t| format!(", resets {}", t.with_timezone(&Local).format("%a %H:%M")))
                .unwrap_or_default();
            format!("{label} {:.0}%{reset}", w.used_pct)
        }
        None => format!("{label} ?"),
    }
}

async fn status(ctx: &Ctx, json: bool) -> Result<()> {
    let standings = daemon::standings(ctx).await;
    let paused = ctx.store.flag(PAUSE_FLAG)?.is_some();
    let running = ctx.store.running_jobs()?;
    let recent = ctx.store.recent_jobs(5)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "paused": paused,
                "day_start": ctx.day_start(),
                "providers": standings,
                "running": running,
                "recent": recent,
            }))?
        );
        return Ok(());
    }
    if paused {
        println!("paused (subr resume to continue)\n");
    }
    for s in &standings {
        let snap = s.snapshot.as_ref();
        let plan = snap
            .and_then(|s| s.plan.as_deref())
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        println!("{}{plan}", s.provider);
        println!(
            "  {}  ·  {}",
            fmt_window("5h", snap.and_then(|s| s.session.as_ref())),
            fmt_window("week", snap.and_then(|s| s.weekly.as_ref()))
        );
        println!(
            "  today: {:.1}% of {:.1}% spent  ·  {}",
            s.spent_today,
            s.daily_limit,
            match &s.verdict {
                budget::Verdict::Go => "may start a job".to_owned(),
                budget::Verdict::No(why) => format!("waiting: {why}"),
            }
        );
        if let Some(e) = &s.error {
            println!("  quota: {e}");
        }
    }
    if !running.is_empty() {
        println!("\nrunning");
        for j in &running {
            print_job(j);
        }
    }
    if !recent.is_empty() {
        println!("\nrecent");
        for j in recent
            .iter()
            .filter(|j| j.status != store::JobStatus::Running)
        {
            print_job(j);
        }
    }
    Ok(())
}

async fn tasks(ctx: &Ctx, json: bool) -> Result<()> {
    let plan = daemon::plan(ctx).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    if plan.work.is_empty() {
        println!("nothing to do");
    }
    for (i, w) in plan.work.iter().enumerate() {
        let kind = match &w.what {
            select::What::Task(t) => format!("task ({})", t.source),
            select::What::Preset(_) => "preset".to_owned(),
        };
        println!("{:>3}. {}  [{kind}]  {}", i + 1, w.title(), w.repo.key());
    }
    if !plan.skipped.is_empty() {
        println!("\nskipped");
        for s in &plan.skipped {
            println!("  {}: {}", s.title, s.why);
        }
    }
    for e in &plan.errors {
        println!("\nerror: {e}");
    }
    Ok(())
}

async fn once(ctx: &Ctx, dry_run: bool) -> Result<()> {
    // On Ctrl-C, stop the job this run started (not the service's), and
    // count it as a failed attempt so the next run doesn't pick it again.
    let before: Vec<String> = ctx
        .store
        .running_jobs()?
        .into_iter()
        .map(|j| j.id)
        .collect();
    let tick = tokio::select! {
        t = daemon::tick(ctx, dry_run) => t?,
        _ = tokio::signal::ctrl_c() => {
            println!("interrupted; stopping the job");
            return daemon::stop_running(&ctx.store, |id| !before.iter().any(|b| b == id)).await;
        }
    };
    match tick {
        Tick::Paused => println!("paused"),
        Tick::NoBudget(why) => {
            println!("not starting:");
            for w in why {
                println!("  {w}");
            }
        }
        Tick::NothingToDo => println!("nothing to do"),
        Tick::Would(w, p) => println!("would run on {p}: {}  ({})", w.title(), w.repo.key()),
        Tick::Ran(j) => print_job(&j),
    }
    Ok(())
}

async fn check(ctx: &Ctx) -> Result<()> {
    let mut ok = true;
    let mut line = |good: bool, what: &str, detail: String| {
        ok &= good;
        println!("{} {what}: {detail}", if good { "ok  " } else { "FAIL" });
    };
    line(true, "config", "valid".to_owned());
    line(
        !ctx.cfg.repos.roots.is_empty(),
        "repos.roots",
        if ctx.cfg.repos.roots.is_empty() {
            "empty; set it to where your repositories are".to_owned()
        } else {
            let n = repos::scan(&ctx.cfg.repos).map(|r| r.len()).unwrap_or(0);
            format!("{n} repositories")
        },
    );
    for (name, cmd) in [("git", "git"), ("gh", "gh")] {
        let found = which(cmd);
        line(
            found.is_some() || name != "git",
            name,
            found.unwrap_or_else(|| "not found".to_owned()),
        );
    }
    if ctx.cfg.publish.push {
        let authed = tokio::process::Command::new("gh")
            .args(["auth", "status"])
            .output()
            .await
            .is_ok_and(|o| o.status.success());
        line(
            authed,
            "gh auth",
            if authed {
                "signed in"
            } else {
                "not signed in (gh auth login)"
            }
            .to_owned(),
        );
    }
    for (&p, q) in &ctx.quota {
        if !ctx.provider_cfg(p).enabled {
            continue;
        }
        let bin = p.harness().program();
        line(
            which(bin).is_some(),
            bin,
            which(bin).unwrap_or_else(|| "not found".to_owned()),
        );
        match q.usage().await {
            Ok(s) => line(
                true,
                &format!("{p} quota"),
                format!(
                    "{}, {}",
                    fmt_window("5h", s.session.as_ref()),
                    fmt_window("week", s.weekly.as_ref())
                ),
            ),
            Err(e) => line(false, &format!("{p} quota"), e.to_string()),
        }
    }
    if !ok {
        bail!("some checks failed");
    }
    Ok(())
}

fn which(cmd: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}
