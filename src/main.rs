//! abuser: spends what's left of your AI subscriptions on background work,
//! without going over a daily limit.

/// `println!` that ends the process quietly when stdout is a closed pipe
/// (`abuser jobs | head`), where `println!` would panic. Rust ignores
/// SIGPIPE, and restoring it would also kill the daemon on a child's pipe.
macro_rules! outln {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout(), $($arg)*) {
            $crate::stdout_failed(e);
        }
    }};
}

/// `print!` with the same handling as `outln!`.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = write!(std::io::stdout(), $($arg)*) {
            $crate::stdout_failed(e);
        }
    }};
}

mod budget;
mod config;
mod control;
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
mod tui;

use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::{Local, Utc};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::daemon::{PAUSE_FLAG, Tick};
use crate::runner::Ctx;
use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "abuser",
    version,
    about = "Spend spare AI subscription quota on background coding work"
)]
struct Cli {
    /// Config file (default: $ABUSER_CONFIG or ~/.config/abuser/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the foreground: watch quota and work when there is budget.
    Run,
    /// Watch and steer abuser in a terminal UI.
    Tui,
    /// Quota, today's spend and the latest jobs.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// The work abuser would pick, in order, and what it skips.
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
    /// Queue a task in abuser's own queue.
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
    /// One job: what it did, what it spent, where its files are.
    Job {
        /// All or part of its id (default: the running job, else the latest).
        job: Option<String>,
        /// Show its changes.
        #[arg(long)]
        diff: bool,
        #[arg(long)]
        json: bool,
    },
    /// What a job's agent did, step by step.
    Logs {
        /// All or part of its id (default: the running job, else the latest).
        job: Option<String>,
        /// Keep following while the job runs.
        #[arg(short, long)]
        follow: bool,
    },
    /// abuser's own queue (`abuser add`).
    Queue {
        #[command(subcommand)]
        cmd: Option<QueueCmd>,
    },
    /// Leave a task out until `abuser retry`, or put a preset off a cooldown.
    Skip {
        /// Its number in `abuser tasks`, or part of its title.
        task: String,
    },
    /// Try a task again that was skipped, gave up or is waiting to retry.
    Retry {
        /// Part of its title, as `abuser tasks` lists it under skipped.
        task: String,
    },
    /// Run something next, whatever its attempts or cooldown say. The
    /// budget still decides when.
    RunNow {
        /// A number or title from `abuser tasks`, or a preset with --repo.
        what: String,
        /// Run the preset `what` in this repo (a path, remote or name).
        #[arg(long)]
        repo: Option<String>,
    },
    /// Stop starting jobs (a running one finishes).
    Pause,
    /// Start jobs again.
    Resume,
    /// Stop a running job; its worktree is discarded.
    Stop { job: String },
    /// Install abuser as a user service (systemd on Linux, launchd on macOS)
    /// and start it, or restart it on this binary.
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
enum QueueCmd {
    /// Remove a queued task.
    Rm { id: i64 },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Check the config, the CLIs and the logins.
    Check,
    /// Change one setting, keeping the file's comments. The daemon picks
    /// it up on its next round.
    Set {
        /// Dotted, e.g. providers.claude.daily_limit_pct or repo."github.com/o/r".verify.
        key: String,
        /// A TOML value; anything else is taken as a string.
        value: String,
    },
    /// Remove one setting, so its default applies.
    Unset { key: String },
    /// Open the config in $VISUAL or $EDITOR, then check it.
    Edit,
    /// Print a commented example config.
    Example,
    /// Print where the config and state live.
    Path,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ABUSER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("abuser=info")),
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

/// Whoever reads our output stopped reading: exit like a SIGPIPE'd tool
/// would, without a panic. Any other write error is a real failure.
fn stdout_failed(e: std::io::Error) -> ! {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    panic!("failed printing to stdout: {e}");
}

fn open(cli: &Cli) -> Result<Ctx> {
    let (cfg, path) = Config::load(cli.config.as_deref())?;
    let store = Store::open(&paths::state_dir().join("abuser.db"))?;
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    Ok(Ctx::new(cfg, dir, store))
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Config(ConfigCmd::Example) => {
            out!("{}", config::EXAMPLE);
            return Ok(());
        }
        Cmd::Config(ConfigCmd::Path) => {
            outln!(
                "config: {}",
                cli.config
                    .clone()
                    .unwrap_or_else(paths::config_file)
                    .display()
            );
            outln!("state:  {}", paths::state_dir().display());
            return Ok(());
        }
        Cmd::InstallService { print } => {
            let running: Vec<String> = Store::open(&paths::state_dir().join("abuser.db"))
                .and_then(|s| s.running_jobs())
                .map(|jobs| jobs.into_iter().map(|j| j.id).collect())
                .unwrap_or_default();
            return service::install(*print, cli.config.as_deref(), &running);
        }
        Cmd::Config(ConfigCmd::Set { key, value }) => {
            let path = cli.config.clone().unwrap_or_else(paths::config_file);
            config::set(&path, key, Some(value))?;
            outln!("set {key} = {value} in {}", path.display());
            return Ok(());
        }
        Cmd::Config(ConfigCmd::Unset { key }) => {
            let path = cli.config.clone().unwrap_or_else(paths::config_file);
            config::set(&path, key, None)?;
            outln!("removed {key} from {}", path.display());
            return Ok(());
        }
        Cmd::Config(ConfigCmd::Edit) => {
            let path = cli.config.clone().unwrap_or_else(paths::config_file);
            outln!("{}", control::edit_config(&path)?);
            return Ok(());
        }
        _ => {}
    }
    let ctx = open(&cli)?;
    match cli.cmd {
        Cmd::Run => daemon::run(ctx, cli.config.clone()).await,
        Cmd::Tui => {
            let path = cli.config.clone().unwrap_or_else(paths::config_file);
            tui::run(ctx, path).await
        }
        Cmd::Status { json } => status(&ctx, json).await,
        Cmd::Tasks { json } => tasks(&ctx, json).await,
        Cmd::Once { dry_run } => once(&ctx, dry_run).await,
        Cmd::Add { title, repo, body } => {
            outln!("{}", control::add(&ctx, &title, &repo, &body)?);
            Ok(())
        }
        Cmd::Jobs { limit } => {
            for j in ctx.store.recent_jobs(limit)? {
                print_job(&j);
            }
            Ok(())
        }
        Cmd::Job { job, diff, json } => control::job(&ctx, job.as_deref(), diff, json).await,
        Cmd::Logs { job, follow } => control::logs(&ctx, job.as_deref(), follow).await,
        Cmd::Queue { cmd: None } => control::queue(&ctx),
        Cmd::Queue {
            cmd: Some(QueueCmd::Rm { id }),
        } => control::unqueue(&ctx, id),
        Cmd::Skip { task } => control::skip(&ctx, &task).await,
        Cmd::Retry { task } => control::retry(&ctx, &task).await,
        Cmd::RunNow { what, repo } => control::run_now(&ctx, &what, repo.as_deref()).await,
        Cmd::Pause => {
            control::set_paused(&ctx, true)?;
            outln!("paused");
            Ok(())
        }
        Cmd::Resume => {
            control::set_paused(&ctx, false)?;
            outln!("resumed");
            Ok(())
        }
        Cmd::Stop { job } => {
            outln!("{}", control::stop(&ctx, &job)?);
            Ok(())
        }
        Cmd::Config(ConfigCmd::Check) => check(&ctx).await,
        Cmd::Config(_) | Cmd::InstallService { .. } => Ok(()),
    }
}

fn print_job(j: &store::JobRow) {
    outln!(
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
        outln!("    {d}");
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
    let heartbeat = daemon::heartbeat(&ctx.store);
    let alive = heartbeat.as_ref().is_some_and(|h| h.alive(Utc::now()));
    let next = ctx.store.flag(select::NEXT_FLAG)?;
    if json {
        outln!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "daemon": {"alive": alive, "heartbeat": heartbeat},
                "next": next,
                "paused": paused,
                "day_start": ctx.day_start(),
                "providers": standings,
                "running": running,
                "recent": recent,
            }))?
        );
        return Ok(());
    }
    match &heartbeat {
        Some(h) if alive => outln!("daemon running (pid {}): {}", h.pid, h.doing),
        Some(h) => outln!(
            "daemon not running (last seen {})",
            h.at.with_timezone(&Local).format("%a %H:%M")
        ),
        None => outln!("daemon not seen (not running, or older than this abuser)"),
    }
    if paused {
        outln!("paused (abuser resume to continue)");
    }
    if let Some(n) = &next {
        outln!("next: {n}");
    }
    outln!();
    for s in &standings {
        let snap = s.snapshot.as_ref();
        let plan = snap
            .and_then(|s| s.plan.as_deref())
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        outln!("{}{plan}", s.provider);
        outln!(
            "  {}  ·  {}",
            fmt_window("5h", snap.and_then(|s| s.session.as_ref())),
            fmt_window("week", snap.and_then(|s| s.weekly.as_ref()))
        );
        outln!(
            "  today: {:.1}% of {:.1}% used  ·  {}",
            s.used_today,
            s.daily_limit,
            match &s.verdict {
                budget::Verdict::Go => "may start a job".to_owned(),
                budget::Verdict::No(why) => format!("waiting: {why}"),
            }
        );
        if let Some(e) = &s.error {
            outln!("  quota: {e}");
        }
    }
    if !running.is_empty() {
        outln!("\nrunning");
        for j in &running {
            print_job(j);
        }
    }
    if !recent.is_empty() {
        outln!("\nrecent");
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
        outln!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    if plan.work.is_empty() {
        outln!("nothing to do");
    }
    let next = ctx.store.flag(select::NEXT_FLAG)?;
    for (i, w) in plan.work.iter().enumerate() {
        let kind = match &w.what {
            select::What::Task(t) => format!("task ({})", t.source),
            select::What::Preset(_) => "preset".to_owned(),
        };
        let forced = if next.as_deref() == Some(w.key().as_str()) {
            "  (next)"
        } else {
            ""
        };
        outln!(
            "{:>3}. {}  [{kind}]  {}{forced}",
            i + 1,
            w.title(),
            w.repo.key()
        );
    }
    if !plan.skipped.is_empty() {
        outln!("\nskipped");
        for s in &plan.skipped {
            outln!("  {}: {}", s.title, s.why);
        }
    }
    for e in &plan.errors {
        outln!("\nerror: {e}");
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
            outln!("interrupted; stopping the job");
            return daemon::stop_running(&ctx.store, |id| !before.iter().any(|b| b == id)).await;
        }
    };
    match tick {
        Tick::Paused => outln!("paused"),
        Tick::NoBudget(why) => {
            outln!("not starting:");
            for w in why {
                outln!("  {w}");
            }
        }
        Tick::NothingToDo => outln!("nothing to do"),
        Tick::Would(w, p) => outln!("would run on {p}: {}  ({})", w.title(), w.repo.key()),
        Tick::Ran(j) => print_job(&j),
    }
    Ok(())
}

async fn check(ctx: &Ctx) -> Result<()> {
    let mut ok = true;
    let mut line = |good: bool, what: &str, detail: String| {
        ok &= good;
        outln!("{} {what}: {detail}", if good { "ok  " } else { "FAIL" });
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
