//! The TUI's state and what keys do to it. No I/O here: the loop in
//! `mod.rs` fills [`Data`] and carries out the [`Action`]s keys return.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::daemon::{Heartbeat, Standing};
use crate::harness::LogLine;
use crate::select::{Plan, What, Work};
use crate::store::{JobRow, JobStatus};

/// One line of the plan: work abuser would pick, or something it skips.
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub title: String,
    pub repo: String,
    pub kind: String,
    pub body: String,
    /// The work, when it would run; `None` when it is skipped.
    pub work: Option<Work>,
    pub key: Option<String>,
    /// Why it is skipped.
    pub why: Option<String>,
}

impl PlanItem {
    /// The id of a task in abuser's own queue.
    pub fn native_id(&self) -> Option<i64> {
        match &self.work.as_ref()?.what {
            What::Task(t) if t.source == "native" => t.id.parse().ok(),
            _ => None,
        }
    }
}

pub fn plan_items(mut plan: Plan) -> Vec<PlanItem> {
    let mut items: Vec<PlanItem> = plan
        .work
        .into_iter()
        .map(|w| {
            let why = plan.held.remove(&w.key());
            let (kind, body) = match &w.what {
                What::Task(t) => (format!("task ({})", t.source), t.body.clone()),
                What::Preset(p) => ("preset".to_owned(), p.prompt.clone()),
            };
            PlanItem {
                title: w.title(),
                repo: w.repo.key(),
                kind,
                body,
                key: Some(w.key()),
                work: Some(w),
                why,
            }
        })
        .collect();
    items.extend(plan.skipped.into_iter().map(|s| PlanItem {
        title: s.title,
        repo: String::new(),
        kind: "skipped".to_owned(),
        body: String::new(),
        work: None,
        key: s.key,
        why: Some(s.why),
    }));
    items
}

/// What the TUI shows, as last read.
#[derive(Debug, Default)]
pub struct Data {
    pub heartbeat: Option<Heartbeat>,
    pub paused: bool,
    /// The key of the work asked to run next.
    pub next: Option<String>,
    /// `None` until the first reading.
    pub standings: Option<Vec<Standing>>,
    /// Running jobs first, then the latest.
    pub jobs: Vec<JobRow>,
    /// `None` until the first plan.
    pub plan: Option<Vec<PlanItem>>,
    pub plan_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Jobs,
    Plan,
}

/// What a key asks the loop to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Refresh,
    SetPaused(bool),
    Stop(String),
    RunNext { key: String, title: String },
    Skip(Box<Work>),
    Retry { key: String, title: String },
    Remove(i64),
    Add { title: String, repo: String },
    EditConfig,
    Open(String),
    LoadDiff(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddForm {
    /// Title, then repo.
    pub fields: [String; 2],
    pub at: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Normal,
    Help,
    Confirm { prompt: String, action: Action },
    Add(AddForm),
}

/// The detail pane's text for a job: its log, or its changes.
#[derive(Debug, Default)]
pub struct JobText {
    pub log: Vec<LogLine>,
    pub summary: Option<String>,
    pub diff: Option<Result<String, String>>,
}

#[derive(Debug)]
pub struct App {
    pub data: Data,
    pub focus: Focus,
    pub job_sel: usize,
    pub plan_sel: usize,
    /// Lines scrolled up from the end of the detail pane; 0 follows it.
    pub scroll_back: u16,
    pub show_diff: bool,
    pub texts: HashMap<String, JobText>,
    pub mode: Mode,
    /// The last thing an action said, and whether it was an error.
    pub message: Option<(String, bool)>,
    pub quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            data: Data::default(),
            focus: Focus::Jobs,
            job_sel: 0,
            plan_sel: 0,
            scroll_back: 0,
            show_diff: false,
            texts: HashMap::new(),
            mode: Mode::Normal,
            message: None,
            quit: false,
        }
    }
}

const SCROLL_STEP: u16 = 10;

impl App {
    pub fn selected_job(&self) -> Option<&JobRow> {
        self.data.jobs.get(self.job_sel)
    }

    pub fn selected_plan(&self) -> Option<&PlanItem> {
        self.data.plan.as_ref()?.get(self.plan_sel)
    }

    /// Replaces the jobs, keeping the same one selected.
    pub fn set_jobs(&mut self, jobs: Vec<JobRow>) {
        let id = self.selected_job().map(|j| j.id.clone());
        self.data.jobs = jobs;
        self.job_sel = id
            .and_then(|id| self.data.jobs.iter().position(|j| j.id == id))
            .unwrap_or(self.job_sel)
            .min(self.data.jobs.len().saturating_sub(1));
    }

    /// Replaces the plan, keeping the same item selected.
    pub fn set_plan(&mut self, items: Vec<PlanItem>) {
        let was = self
            .selected_plan()
            .map(|p| (p.key.clone(), p.title.clone()));
        self.plan_sel = was
            .and_then(|(key, title)| {
                items
                    .iter()
                    .position(|p| p.key.is_some() && p.key == key)
                    .or_else(|| items.iter().position(|p| p.title == title))
            })
            .unwrap_or(self.plan_sel)
            .min(items.len().saturating_sub(1));
        self.data.plan = Some(items);
    }

    /// The title of the work asked to run next, else its key.
    pub fn next_title(&self) -> Option<String> {
        let next = self.data.next.as_ref()?;
        let title = self
            .data
            .plan
            .iter()
            .flatten()
            .find(|p| p.key.as_ref() == Some(next))
            .map_or_else(|| next.clone(), |p| p.title.clone());
        Some(title)
    }

    fn move_sel(&mut self, by: isize) {
        let (sel, len) = match self.focus {
            Focus::Jobs => (&mut self.job_sel, self.data.jobs.len()),
            Focus::Plan => (
                &mut self.plan_sel,
                self.data.plan.as_ref().map_or(0, Vec::len),
            ),
        };
        let to = (*sel as isize + by).clamp(0, len.saturating_sub(1) as isize);
        if to as usize != *sel {
            *sel = to as usize;
            self.scroll_back = 0;
            self.show_diff = false;
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.normal_key(key),
            Mode::Help => None,
            Mode::Confirm { action, .. } => {
                matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter).then_some(action)
            }
            Mode::Add(form) => self.add_key(form, key),
        }
    }

    fn add_key(&mut self, mut form: AddForm, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Esc => return None,
            KeyCode::Tab | KeyCode::Down | KeyCode::BackTab | KeyCode::Up => form.at = 1 - form.at,
            KeyCode::Enter if form.at == 0 => form.at = 1,
            KeyCode::Enter => {
                let [title, repo] = form.fields;
                return Some(Action::Add { title, repo });
            }
            KeyCode::Backspace => {
                form.fields[form.at].pop();
            }
            KeyCode::Char(c) => form.fields[form.at].push(c),
            _ => {}
        }
        self.mode = Mode::Add(form);
        None
    }

    fn normal_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Jobs => Focus::Plan,
                    Focus::Plan => Focus::Jobs,
                };
                self.scroll_back = 0;
                self.show_diff = false;
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Home => self.move_sel(isize::MIN / 2),
            KeyCode::End => self.move_sel(isize::MAX / 2),
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(SCROLL_STEP),
            KeyCode::Char('u') if ctrl => {
                self.scroll_back = self.scroll_back.saturating_add(SCROLL_STEP);
            }
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(SCROLL_STEP),
            KeyCode::Char('d') if ctrl => {
                self.scroll_back = self.scroll_back.saturating_sub(SCROLL_STEP);
            }
            KeyCode::Char('f') => self.scroll_back = 0,
            KeyCode::Char('g') => return Some(Action::Refresh),
            KeyCode::Char('p') => return Some(Action::SetPaused(!self.data.paused)),
            KeyCode::Char('e') => return Some(Action::EditConfig),
            KeyCode::Char('a') => {
                let repo = match self.focus {
                    Focus::Jobs => self.selected_job().map(|j| j.repo.clone()),
                    Focus::Plan => self.selected_plan().map(|p| p.repo.clone()),
                };
                self.mode = Mode::Add(AddForm {
                    fields: [String::new(), repo.unwrap_or_default()],
                    at: 0,
                });
            }
            _ => {
                return match self.focus {
                    Focus::Jobs => self.job_key(key),
                    Focus::Plan => self.plan_key(key),
                };
            }
        }
        None
    }

    fn job_key(&mut self, key: KeyEvent) -> Option<Action> {
        let job = self.selected_job()?;
        match key.code {
            KeyCode::Char('s') if job.status == JobStatus::Running => {
                self.mode = Mode::Confirm {
                    prompt: format!("Stop {}? Its worktree is discarded. (y/n)", job.id),
                    action: Action::Stop(job.id.clone()),
                };
                None
            }
            KeyCode::Char('o') => job.pr_url.clone().map(Action::Open),
            KeyCode::Char('d') => {
                let id = job.id.clone();
                self.show_diff = !self.show_diff;
                self.scroll_back = 0;
                self.show_diff.then_some(Action::LoadDiff(id))
            }
            _ => None,
        }
    }

    fn plan_key(&mut self, key: KeyEvent) -> Option<Action> {
        let item = self.selected_plan()?;
        match key.code {
            KeyCode::Char('r') => Some(Action::RunNext {
                key: item.key.clone()?,
                title: item.title.clone(),
            }),
            KeyCode::Char('x') => item.work.clone().map(|w| Action::Skip(Box::new(w))),
            KeyCode::Char('R') if item.work.is_none() => Some(Action::Retry {
                key: item.key.clone()?,
                title: item.title.clone(),
            }),
            KeyCode::Char('D') => {
                let id = item.native_id()?;
                self.mode = Mode::Confirm {
                    prompt: format!("Remove #{id} \"{}\" from the queue? (y/n)", item.title),
                    action: Action::Remove(id),
                };
                None
            }
            _ => None,
        }
    }
}

/// The keys, for the help screen and the footer.
pub const KEYS: &[(&str, &str)] = &[
    ("tab", "switch between jobs and plan"),
    ("j/k ↑/↓", "move"),
    ("PgUp/PgDn ^u/^d", "scroll the detail"),
    ("f", "follow the end of the detail"),
    ("p", "pause or resume"),
    ("a", "add a task to the queue"),
    ("s", "stop the running job (jobs)"),
    ("d", "show the job's changes (jobs)"),
    ("o", "open the job's PR (jobs)"),
    ("r", "run next (plan)"),
    ("x", "skip (plan)"),
    ("R", "retry a skipped task (plan)"),
    ("D", "remove a queued task (plan)"),
    ("e", "edit the config"),
    ("g", "read quota and plan again"),
    ("q", "quit"),
];

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::repos::Repo;
    use crate::tasks::Task;

    pub fn job(id: &str, status: JobStatus) -> JobRow {
        JobRow {
            id: id.into(),
            provider: "claude".into(),
            repo: "github.com/o/r".into(),
            task_key: "k".into(),
            title: format!("title of {id}"),
            status,
            started: Utc::now(),
            ended: None,
            spent: 1.5,
            pr_url: Some("https://github.com/o/r/pull/1".into()),
            detail: None,
            state: None,
        }
    }

    pub fn native(id: &str, title: &str) -> PlanItem {
        let work = Work {
            what: What::Task(Task {
                source: "native".into(),
                id: id.into(),
                title: title.into(),
                body: String::new(),
                repo: None,
                meta: Default::default(),
            }),
            repo: Repo {
                path: "/r".into(),
                remote_url: None,
                remote: Some("github.com/o/r".into()),
                last_commit: None,
                base: None,
            },
        };
        PlanItem {
            title: title.into(),
            repo: "github.com/o/r".into(),
            kind: "task (native)".into(),
            body: String::new(),
            key: Some(work.key()),
            work: Some(work),
            why: None,
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    #[test]
    fn selection_survives_new_jobs() {
        let mut app = App::default();
        app.set_jobs(vec![job("a", JobStatus::Done), job("b", JobStatus::Done)]);
        app.on_key(key('j'));
        assert_eq!(app.selected_job().unwrap().id, "b");
        app.set_jobs(vec![
            job("c", JobStatus::Running),
            job("a", JobStatus::Done),
            job("b", JobStatus::Done),
        ]);
        assert_eq!(app.selected_job().unwrap().id, "b");
        app.set_jobs(vec![job("c", JobStatus::Running)]);
        assert_eq!(app.selected_job().unwrap().id, "c");
    }

    #[test]
    fn stopping_asks_first() {
        let mut app = App::default();
        app.set_jobs(vec![job("a", JobStatus::Running)]);
        assert_eq!(app.on_key(key('s')), None);
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert_eq!(app.on_key(key('y')), Some(Action::Stop("a".into())));
        assert_eq!(app.mode, Mode::Normal);

        app.on_key(key('s'));
        assert_eq!(app.on_key(key('n')), None);
        assert_eq!(app.mode, Mode::Normal);

        // Only running jobs stop.
        app.set_jobs(vec![job("b", JobStatus::Done)]);
        app.on_key(key('s'));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn plan_keys() {
        let mut app = App::default();
        let item = native("7", "Fix it");
        let item_key = item.key.clone().unwrap();
        app.set_plan(vec![
            item,
            PlanItem {
                title: "Old".into(),
                repo: String::new(),
                kind: "skipped".into(),
                body: String::new(),
                work: None,
                key: Some("task:old".into()),
                why: Some("gave up".into()),
            },
        ]);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(
            app.on_key(key('r')),
            Some(Action::RunNext {
                key: item_key,
                title: "Fix it".into()
            })
        );
        assert!(matches!(app.on_key(key('x')), Some(Action::Skip(_))));
        assert_eq!(app.on_key(key('R')), None, "only skipped items retry");
        app.on_key(key('D'));
        assert_eq!(app.on_key(key('y')), Some(Action::Remove(7)));

        app.on_key(key('j'));
        assert_eq!(app.on_key(key('x')), None, "skipped already");
        assert_eq!(
            app.on_key(key('R')),
            Some(Action::Retry {
                key: "task:old".into(),
                title: "Old".into()
            })
        );
    }

    #[test]
    fn adding_a_task() {
        let mut app = App::default();
        app.set_jobs(vec![job("a", JobStatus::Done)]);
        app.on_key(key('a'));
        for c in "Do it".chars() {
            app.on_key(key(c));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        // The repo starts as the selected job's.
        assert_eq!(
            app.on_key(KeyEvent::from(KeyCode::Enter)),
            Some(Action::Add {
                title: "Do it".into(),
                repo: "github.com/o/r".into()
            })
        );
        app.on_key(key('a'));
        app.on_key(key('x'));
        assert_eq!(app.on_key(KeyEvent::from(KeyCode::Esc)), None);
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.quit);
    }
}
