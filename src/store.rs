//! subr's SQLite state: quota readings, budget charges, jobs, task attempts,
//! preset cooldowns, the native task queue and the pause flag.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::quota::{ProviderId, Snapshot};

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS quota (provider TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS charges (
  id INTEGER PRIMARY KEY, provider TEXT NOT NULL, at INTEGER NOT NULL, pct REAL NOT NULL, job TEXT);
CREATE INDEX IF NOT EXISTS charges_at ON charges(provider, at);
CREATE TABLE IF NOT EXISTS jobs (
  id TEXT PRIMARY KEY, provider TEXT NOT NULL, repo TEXT NOT NULL, task_key TEXT NOT NULL,
  title TEXT NOT NULL, status TEXT NOT NULL, started INTEGER NOT NULL, ended INTEGER,
  spent REAL NOT NULL DEFAULT 0, pr_url TEXT, detail TEXT, state TEXT);
CREATE TABLE IF NOT EXISTS attempts (
  task_key TEXT PRIMARY KEY, count INTEGER NOT NULL, last_at INTEGER NOT NULL,
  last_status TEXT NOT NULL, pr_url TEXT);
CREATE TABLE IF NOT EXISTS cooldowns (
  repo TEXT NOT NULL, preset TEXT NOT NULL, last_at INTEGER NOT NULL, PRIMARY KEY (repo, preset));
CREATE TABLE IF NOT EXISTS native_tasks (
  id INTEGER PRIMARY KEY, repo TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL DEFAULT '',
  created INTEGER NOT NULL, done INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    /// Published, or finished with a report.
    Done,
    /// The agent changed nothing or said there was nothing to do.
    NoChange,
    /// The agent couldn't do it.
    Blocked,
    /// Stopped to stay within the budget; the worktree is kept.
    BudgetStopped,
    TimedOut,
    Failed,
    /// Set aside with `subr skip` (attempts only; never a job's status).
    Skipped,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::NoChange => "no_change",
            Self::Blocked => "blocked",
            Self::BudgetStopped => "budget_stopped",
            Self::TimedOut => "timed_out",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "done" => Self::Done,
            "no_change" => Self::NoChange,
            "blocked" => Self::Blocked,
            "budget_stopped" => Self::BudgetStopped,
            "timed_out" => Self::TimedOut,
            "skipped" => Self::Skipped,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRow {
    pub id: String,
    pub provider: String,
    pub repo: String,
    pub task_key: String,
    pub title: String,
    pub status: JobStatus,
    pub started: DateTime<Utc>,
    pub ended: Option<DateTime<Utc>>,
    pub spent: f64,
    pub pr_url: Option<String>,
    pub detail: Option<String>,
    /// Runner state for reattaching (JSON), while running.
    #[serde(skip)]
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attempts {
    pub count: u32,
    pub last_at: DateTime<Utc>,
    pub last_status: JobStatus,
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeTask {
    pub id: i64,
    pub repo: String,
    pub title: String,
    pub body: String,
}

fn ts(t: DateTime<Utc>) -> i64 {
    t.timestamp()
}
fn from_ts(s: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(s, 0).unwrap_or_default()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ---- quota ---------------------------------------------------------------

    pub fn save_snapshot(&self, s: &Snapshot) -> Result<()> {
        self.db().execute(
            "INSERT INTO quota (provider, json) VALUES (?1, ?2)
             ON CONFLICT(provider) DO UPDATE SET json = excluded.json",
            params![s.provider.as_str(), serde_json::to_string(s)?],
        )?;
        Ok(())
    }

    pub fn snapshot(&self, p: ProviderId) -> Result<Option<Snapshot>> {
        let json: Option<String> = self
            .db()
            .query_row(
                "SELECT json FROM quota WHERE provider = ?1",
                [p.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    // ---- budget --------------------------------------------------------------

    pub fn charge(&self, p: ProviderId, at: DateTime<Utc>, pct: f64, job: &str) -> Result<()> {
        if pct <= 0.0 {
            return Ok(());
        }
        let db = self.db();
        db.execute(
            "INSERT INTO charges (provider, at, pct, job) VALUES (?1, ?2, ?3, ?4)",
            params![p.as_str(), ts(at), pct, job],
        )?;
        db.execute(
            "UPDATE jobs SET spent = spent + ?1 WHERE id = ?2",
            params![pct, job],
        )?;
        Ok(())
    }

    /// Percentage points charged to `p` since `since`.
    pub fn spent_since(&self, p: ProviderId, since: DateTime<Utc>) -> Result<f64> {
        Ok(self.db().query_row(
            "SELECT COALESCE(SUM(pct), 0) FROM charges WHERE provider = ?1 AND at >= ?2",
            params![p.as_str(), ts(since)],
            |r| r.get(0),
        )?)
    }

    // ---- jobs ----------------------------------------------------------------

    pub fn insert_job(&self, j: &JobRow) -> Result<()> {
        self.db().execute(
            "INSERT INTO jobs (id, provider, repo, task_key, title, status, started, spent, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
            params![
                j.id,
                j.provider,
                j.repo,
                j.task_key,
                j.title,
                j.status.as_str(),
                ts(j.started),
                j.state
            ],
        )?;
        Ok(())
    }

    pub fn save_job_state(&self, id: &str, state: &str) -> Result<()> {
        self.db().execute(
            "UPDATE jobs SET state = ?1 WHERE id = ?2",
            params![state, id],
        )?;
        Ok(())
    }

    pub fn finish_job(
        &self,
        id: &str,
        status: JobStatus,
        pr_url: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        self.db().execute(
            "UPDATE jobs SET status = ?1, ended = ?2, pr_url = ?3, detail = ?4, state = NULL WHERE id = ?5",
            params![status.as_str(), ts(Utc::now()), pr_url, detail, id],
        )?;
        Ok(())
    }

    fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
        Ok(JobRow {
            id: r.get(0)?,
            provider: r.get(1)?,
            repo: r.get(2)?,
            task_key: r.get(3)?,
            title: r.get(4)?,
            status: JobStatus::parse(&r.get::<_, String>(5)?),
            started: from_ts(r.get(6)?),
            ended: r.get::<_, Option<i64>>(7)?.map(from_ts),
            spent: r.get(8)?,
            pr_url: r.get(9)?,
            detail: r.get(10)?,
            state: r.get(11)?,
        })
    }

    const JOB_COLS: &str =
        "id, provider, repo, task_key, title, status, started, ended, spent, pr_url, detail, state";

    pub fn recent_jobs(&self, limit: usize) -> Result<Vec<JobRow>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "SELECT {} FROM jobs ORDER BY started DESC LIMIT ?1",
            Self::JOB_COLS
        ))?;
        let rows = st.query_map([limit as i64], Self::job_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn running_jobs(&self) -> Result<Vec<JobRow>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "SELECT {} FROM jobs WHERE status = 'running' ORDER BY started",
            Self::JOB_COLS
        ))?;
        let rows = st.query_map([], Self::job_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Jobs whose id is `pattern` or contains it, newest first.
    pub fn find_jobs(&self, pattern: &str) -> Result<Vec<JobRow>> {
        let db = self.db();
        let mut st = db.prepare(&format!(
            "SELECT {} FROM jobs WHERE id = ?1 OR instr(id, ?1) > 0
             ORDER BY id = ?1 DESC, started DESC",
            Self::JOB_COLS
        ))?;
        let rows = st.query_map([pattern], Self::job_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- task attempts -------------------------------------------------------

    /// Counts an attempt as it starts, so one that never finishes (subr was
    /// killed, the agent couldn't start) still counts and waits its turn.
    pub fn start_attempt(&self, key: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO attempts (task_key, count, last_at, last_status) VALUES (?1, 1, ?2, 'running')
             ON CONFLICT(task_key) DO UPDATE SET count = count + 1, last_at = excluded.last_at,
               last_status = excluded.last_status",
            params![key, ts(Utc::now())],
        )?;
        Ok(())
    }

    /// Records how the latest attempt ended (one attempt, when none was started).
    pub fn end_attempt(&self, key: &str, status: JobStatus, pr_url: Option<&str>) -> Result<()> {
        self.db().execute(
            "INSERT INTO attempts (task_key, count, last_at, last_status, pr_url) VALUES (?1, 1, ?2, ?3, ?4)
             ON CONFLICT(task_key) DO UPDATE SET last_at = excluded.last_at,
               last_status = excluded.last_status, pr_url = COALESCE(excluded.pr_url, pr_url)",
            params![key, ts(Utc::now()), status.as_str(), pr_url],
        )?;
        Ok(())
    }

    pub fn attempts(&self, key: &str) -> Result<Option<Attempts>> {
        Ok(self
            .db()
            .query_row(
                "SELECT count, last_at, last_status, pr_url FROM attempts WHERE task_key = ?1",
                [key],
                |r| {
                    Ok(Attempts {
                        count: r.get(0)?,
                        last_at: from_ts(r.get(1)?),
                        last_status: JobStatus::parse(&r.get::<_, String>(2)?),
                        pr_url: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Forgets a task's attempts, so it is picked again.
    pub fn clear_attempts(&self, key: &str) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM attempts WHERE task_key = ?1", [key])?
            > 0)
    }

    // ---- preset cooldowns ----------------------------------------------------

    pub fn preset_ran(&self, repo: &str, preset: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO cooldowns (repo, preset, last_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo, preset) DO UPDATE SET last_at = excluded.last_at",
            params![repo, preset, ts(Utc::now())],
        )?;
        Ok(())
    }

    pub fn preset_last_run(&self, repo: &str, preset: &str) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .db()
            .query_row(
                "SELECT last_at FROM cooldowns WHERE repo = ?1 AND preset = ?2",
                [repo, preset],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(from_ts))
    }

    // ---- native queue --------------------------------------------------------

    pub fn add_native(&self, repo: &str, title: &str, body: &str) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO native_tasks (repo, title, body, created) VALUES (?1, ?2, ?3, ?4)",
            params![repo, title, body, ts(Utc::now())],
        )?;
        Ok(db.last_insert_rowid())
    }

    pub fn open_native(&self) -> Result<Vec<NativeTask>> {
        let db = self.db();
        let mut st = db
            .prepare("SELECT id, repo, title, body FROM native_tasks WHERE done = 0 ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(NativeTask {
                id: r.get(0)?,
                repo: r.get(1)?,
                title: r.get(2)?,
                body: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn close_native(&self, id: i64) -> Result<()> {
        self.db()
            .execute("UPDATE native_tasks SET done = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Drops an open task from the queue; false when there is none.
    pub fn remove_native(&self, id: i64) -> Result<bool> {
        Ok(self
            .db()
            .execute("DELETE FROM native_tasks WHERE id = ?1 AND done = 0", [id])?
            > 0)
    }

    // ---- flags ---------------------------------------------------------------

    pub fn set_flag(&self, key: &str, value: Option<&str>) -> Result<()> {
        let db = self.db();
        match value {
            Some(v) => db.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, v],
            )?,
            None => db.execute("DELETE FROM kv WHERE key = ?1", [key])?,
        };
        Ok(())
    }

    pub fn flag(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_add_up_per_provider_and_day() {
        let s = Store::in_memory().unwrap();
        let now = Utc::now();
        s.insert_job(&JobRow {
            id: "j1".into(),
            provider: "claude".into(),
            repo: "r".into(),
            task_key: "k".into(),
            title: "t".into(),
            status: JobStatus::Running,
            started: now,
            ended: None,
            spent: 0.0,
            pr_url: None,
            detail: None,
            state: None,
        })
        .unwrap();
        s.charge(
            ProviderId::Claude,
            now - chrono::Duration::days(1),
            5.0,
            "j1",
        )
        .unwrap();
        s.charge(ProviderId::Claude, now, 2.0, "j1").unwrap();
        s.charge(ProviderId::Codex, now, 9.0, "j1").unwrap();
        let today = now - chrono::Duration::hours(1);
        assert_eq!(s.spent_since(ProviderId::Claude, today).unwrap(), 2.0);
        assert_eq!(s.running_jobs().unwrap()[0].spent, 16.0);
    }

    #[test]
    fn attempts_count_up() {
        let s = Store::in_memory().unwrap();
        s.start_attempt("k").unwrap();
        s.end_attempt("k", JobStatus::Failed, None).unwrap();
        s.start_attempt("k").unwrap();
        assert_eq!(
            s.attempts("k").unwrap().unwrap().last_status,
            JobStatus::Running
        );
        s.end_attempt("k", JobStatus::Done, Some("https://pr"))
            .unwrap();
        let a = s.attempts("k").unwrap().unwrap();
        assert_eq!(a.count, 2);
        assert_eq!(a.last_status, JobStatus::Done);
        assert_eq!(a.pr_url.as_deref(), Some("https://pr"));

        // A job from before attempts were counted at the start still counts once.
        s.end_attempt("old", JobStatus::Failed, None).unwrap();
        assert_eq!(s.attempts("old").unwrap().unwrap().count, 1);

        assert!(s.clear_attempts("k").unwrap());
        assert!(s.attempts("k").unwrap().is_none());
        assert!(!s.clear_attempts("k").unwrap());
    }

    #[test]
    fn jobs_are_found_by_part_of_their_id() {
        let s = Store::in_memory().unwrap();
        for id in ["subr-1-fix-a", "subr-2-fix-b", "subr-3-docs"] {
            s.insert_job(&JobRow {
                id: id.into(),
                provider: "claude".into(),
                repo: "r".into(),
                task_key: "k".into(),
                title: "t".into(),
                status: JobStatus::Running,
                started: Utc::now(),
                ended: None,
                spent: 0.0,
                pr_url: None,
                detail: None,
                state: None,
            })
            .unwrap();
        }
        let ids = |p: &str| -> Vec<String> {
            s.find_jobs(p).unwrap().into_iter().map(|j| j.id).collect()
        };
        assert_eq!(ids("docs"), ["subr-3-docs"]);
        assert_eq!(ids("fix").len(), 2);
        assert!(ids("nope").is_empty());
    }

    #[test]
    fn queued_tasks_can_be_removed() {
        let s = Store::in_memory().unwrap();
        let id = s.add_native("r", "t", "").unwrap();
        assert!(s.remove_native(id).unwrap());
        assert!(!s.remove_native(id).unwrap());
        assert!(s.open_native().unwrap().is_empty());
    }
}
