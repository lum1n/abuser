//! `subr tui`: what subr is doing, and the controls the CLI has, in one
//! screen. It reads the same store as the daemon and steers it the same
//! way the CLI does (through [`crate::control`]), so the daemon needn't be
//! running for it to work.

mod state;
mod view;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::control;
use crate::daemon::{self, Standing};
use crate::harness::LogReader;
use crate::quota::ProviderId;
use crate::runner::Ctx;
use crate::select::{NEXT_FLAG, Plan};
use crate::store::JobStatus;
use state::{Action, App, Focus};

/// How often the store (jobs, flags, the selected log) is read.
const STORE_EVERY: Duration = Duration::from_secs(1);
/// How often quota is read; the providers pace themselves too.
const QUOTA_EVERY: Duration = Duration::from_secs(60);
/// How often the task sources are asked again.
const PLAN_EVERY: Duration = Duration::from_secs(120);

enum Loaded {
    Standings(Vec<Standing>),
    Plan(Result<Plan, String>),
}

pub async fn run(ctx: Ctx, config: PathBuf) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = Tui::new(ctx, config).run(&mut terminal).await;
    ratatui::restore();
    result
}

struct Tui {
    ctx: Arc<Ctx>,
    config: PathBuf,
    cfg_mtime: Option<std::time::SystemTime>,
    app: App,
    readers: HashMap<String, LogReader>,
    tx: mpsc::UnboundedSender<Loaded>,
    rx: mpsc::UnboundedReceiver<Loaded>,
    store_at: Option<Instant>,
    quota_at: Option<Instant>,
    plan_at: Option<Instant>,
    loading_quota: bool,
    loading_plan: bool,
    /// The plan being read is already out of date.
    plan_stale: bool,
    message_at: Option<Instant>,
}

impl Tui {
    fn new(ctx: Ctx, config: PathBuf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            ctx: Arc::new(ctx),
            cfg_mtime: mtime(&config),
            config,
            app: App::default(),
            readers: HashMap::new(),
            tx,
            rx,
            store_at: None,
            quota_at: None,
            plan_at: None,
            loading_quota: false,
            loading_plan: false,
            plan_stale: false,
            message_at: None,
        }
    }

    async fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.app.quit {
            self.reload_config();
            if self.store_at.is_none_or(|t| t.elapsed() >= STORE_EVERY) {
                self.store_at = Some(Instant::now());
                if let Err(e) = self.read_store().await {
                    self.say(format!("{e:#}"), true);
                }
            }
            self.load_in_background();
            while let Ok(loaded) = self.rx.try_recv() {
                self.take(loaded);
            }
            if self
                .message_at
                .is_some_and(|t| t.elapsed() > Duration::from_secs(8))
            {
                self.app.message = None;
                self.message_at = None;
            }
            terminal.draw(|f| view::draw(f, &self.app))?;

            let ready = tokio::task::block_in_place(|| event::poll(Duration::from_millis(250)))?;
            if !ready {
                continue;
            }
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && let Some(action) = self.app.on_key(key)
                && let Err(e) = self.act(action, terminal).await
            {
                self.say(format!("{e:#}"), true);
            }
        }
        Ok(())
    }

    fn say(&mut self, message: String, error: bool) {
        let message = message.lines().collect::<Vec<_>>().join(" · ");
        self.app.message = Some((message, error));
        self.message_at = Some(Instant::now());
    }

    /// Picks up config changes, made here or elsewhere.
    fn reload_config(&mut self) {
        let now = mtime(&self.config);
        if now == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = now;
        match Config::load(Some(&self.config)) {
            Ok((cfg, _)) => {
                self.ctx = Arc::new(Ctx::new(
                    cfg,
                    self.ctx.cfg_dir.clone(),
                    self.ctx.store.clone(),
                ));
                self.quota_at = None;
                self.replan();
            }
            Err(e) => self.say(format!("config: {e:#}"), true),
        }
    }

    async fn read_store(&mut self) -> Result<()> {
        let store = &self.ctx.store;
        let d = &mut self.app.data;
        d.heartbeat = daemon::heartbeat(store);
        d.paused = store.flag(daemon::PAUSE_FLAG)?.is_some();
        d.next = store.flag(NEXT_FLAG)?;
        let mut jobs = store.running_jobs()?;
        jobs.extend(
            store
                .recent_jobs(100)?
                .into_iter()
                .filter(|j| j.status != JobStatus::Running),
        );
        self.app.set_jobs(jobs);
        if self.app.focus == Focus::Jobs
            && let Some(j) = self.app.selected_job().cloned()
        {
            self.read_log(&j.id, &j.provider).await;
        }
        Ok(())
    }

    /// Reads what is new in a job's log into its text.
    async fn read_log(&mut self, id: &str, provider: &str) {
        let files = control::job_files(id);
        let text = self.app.texts.entry(id.to_owned()).or_default();
        if text.summary.is_none() {
            text.summary = control::job_summary(&files);
        }
        let Some(log) = files.log else { return };
        let Some(harness) = ProviderId::parse(provider).map(ProviderId::harness) else {
            return;
        };
        let reader = match self.readers.entry(id.to_owned()) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => match LogReader::new(id, harness) {
                Ok(r) => e.insert(r),
                Err(_) => return,
            },
        };
        if let Ok(lines) = reader.read(&log).await {
            text.log
                .extend(lines.into_iter().filter_map(control::readable));
        }
    }

    fn load_in_background(&mut self) {
        if !self.loading_quota && self.quota_at.is_none_or(|t| t.elapsed() >= QUOTA_EVERY) {
            self.loading_quota = true;
            self.quota_at = Some(Instant::now());
            let (ctx, tx) = (self.ctx.clone(), self.tx.clone());
            tokio::spawn(async move {
                let _ = tx.send(Loaded::Standings(daemon::standings(&ctx).await));
            });
        }
        if !self.loading_plan && self.plan_at.is_none_or(|t| t.elapsed() >= PLAN_EVERY) {
            self.loading_plan = true;
            self.plan_at = Some(Instant::now());
            let (ctx, tx) = (self.ctx.clone(), self.tx.clone());
            tokio::spawn(async move {
                let plan = daemon::plan(&ctx).await.map_err(|e| format!("{e:#}"));
                let _ = tx.send(Loaded::Plan(plan));
            });
        }
    }

    fn take(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::Standings(s) => {
                self.loading_quota = false;
                self.app.data.standings = Some(s);
            }
            Loaded::Plan(Ok(plan)) => {
                self.plan_loaded();
                if let Some(e) = plan.errors.first() {
                    self.say(format!("task source: {e}"), true);
                }
                self.app.data.plan_at = Some(Utc::now());
                self.app.set_plan(state::plan_items(plan));
            }
            Loaded::Plan(Err(e)) => {
                self.plan_loaded();
                self.say(format!("plan: {e}"), true);
            }
        }
    }

    fn plan_loaded(&mut self) {
        self.loading_plan = false;
        if std::mem::take(&mut self.plan_stale) {
            self.plan_at = None;
        }
    }

    /// Reads the plan again soon, after something changed it.
    fn replan(&mut self) {
        if self.loading_plan {
            self.plan_stale = true;
        } else {
            self.plan_at = None;
        }
    }

    async fn act(&mut self, action: Action, terminal: &mut DefaultTerminal) -> Result<()> {
        let ctx = self.ctx.clone();
        match action {
            Action::Refresh => {
                self.quota_at = None;
                self.replan();
                self.say("reading quota and tasks again".to_owned(), false);
            }
            Action::SetPaused(paused) => {
                control::set_paused(&ctx, paused)?;
                self.app.data.paused = paused;
                self.say(
                    if paused {
                        "paused: no new jobs start"
                    } else {
                        "resumed"
                    }
                    .to_owned(),
                    false,
                );
            }
            Action::Stop(id) => {
                let said = control::stop(&ctx, &id)?;
                self.say(said, false);
            }
            Action::RunNext { key, title } => {
                let said = control::run_next(&ctx, &key, &title)?;
                self.app.data.next = Some(key);
                self.say(said, false);
            }
            Action::Skip(work) => {
                let said = control::skip_work(&ctx, &work)?;
                self.say(said, false);
                self.replan();
            }
            Action::Retry { key, title } => {
                ctx.store.clear_attempts(&key)?;
                self.say(format!("{title} will be picked again"), false);
                self.replan();
            }
            Action::Remove(id) => {
                control::remove(&ctx, id)?;
                self.say(format!("removed #{id}"), false);
                self.replan();
            }
            Action::Add { title, repo } => {
                let said = control::add(&ctx, &title, &repo, "")?;
                self.say(said, false);
                self.replan();
            }
            Action::EditConfig => {
                ratatui::restore();
                let edited = control::edit_config(&self.config);
                *terminal = ratatui::init();
                terminal.clear()?;
                self.say(edited?, false);
            }
            Action::Open(url) => {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                std::process::Command::new(opener)
                    .arg(&url)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                self.say(format!("opened {url}"), false);
            }
            Action::LoadDiff(id) => {
                let diff = match ctx.store.find_jobs(&id)?.into_iter().next() {
                    Some(j) => control::diff_text(&j).await.map_err(|e| format!("{e:#}")),
                    None => Err("no such job".to_owned()),
                };
                self.app.texts.entry(id).or_default().diff = Some(diff);
            }
        }
        Ok(())
    }
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}
