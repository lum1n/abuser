//! Picking the next piece of work: queued tasks first, in repo-recency
//! order, then presets that are due, round-robin over active repos.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::presets::{Mode, Preset};
use crate::repos::Repo;
use crate::store::{JobStatus, Store};
use crate::tasks::{Source, Task};

/// The key of the work to run next, whatever its attempts or cooldown say
/// (`subr run-now`). Cleared when the job starts.
pub const NEXT_FLAG: &str = "next";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum What {
    Task(Task),
    Preset(Preset),
}

/// One piece of work in one repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub what: What,
    pub repo: Repo,
}

impl Work {
    pub fn title(&self) -> String {
        match &self.what {
            What::Task(t) => t.title.clone(),
            What::Preset(p) => format!("{} in {}", p.name, self.repo.name()),
        }
    }

    /// Key for attempts; presets count per repo.
    pub fn key(&self) -> String {
        match &self.what {
            What::Task(t) => t.key(),
            What::Preset(p) => format!("preset:{}:{}", self.repo.key(), p.name),
        }
    }

    pub fn mode(&self) -> Mode {
        match &self.what {
            What::Task(_) => Mode::Pr,
            What::Preset(p) => p.mode,
        }
    }

    /// Short `[a-z0-9-]` name for branches.
    pub fn slug(&self) -> String {
        let base = match &self.what {
            What::Task(t) => t.title.clone(),
            What::Preset(p) => p.name.clone(),
        };
        let mut s = String::new();
        for c in base.to_lowercase().chars() {
            if c.is_ascii_alphanumeric() {
                s.push(c);
            } else if !s.ends_with('-') && !s.is_empty() {
                s.push('-');
            }
            if s.len() >= 32 {
                break;
            }
        }
        let s = s.trim_matches('-').to_owned();
        if s.is_empty() { "work".to_owned() } else { s }
    }
}

/// Why something was left out, for `subr tasks`.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub title: String,
    pub why: String,
    /// The work's key, when it was skipped for its attempts.
    pub key: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub work: Vec<Work>,
    pub skipped: Vec<Skipped>,
    pub errors: Vec<String>,
}

/// Whether past attempts rule a task out now.
pub fn attempt_block(
    store: &Store,
    key: &str,
    cfg: &Config,
    now: DateTime<Utc>,
) -> Result<Option<String>> {
    let Some(a) = store.attempts(key)? else {
        return Ok(None);
    };
    Ok(match a.last_status {
        JobStatus::Done => Some(match a.pr_url {
            Some(pr) => format!("done in {pr}"),
            None => "already done".to_owned(),
        }),
        JobStatus::Blocked | JobStatus::NoChange => {
            Some(format!("agent reported {}", a.last_status.as_str()))
        }
        JobStatus::Skipped => Some("skipped with `subr skip`".to_owned()),
        _ if a.count >= cfg.jobs.max_attempts => {
            Some(format!("gave up after {} attempts", a.count))
        }
        _ if now - a.last_at < chrono::Duration::hours(cfg.jobs.retry_after_hours as i64) => {
            Some(format!("retrying after {}h", cfg.jobs.retry_after_hours))
        }
        _ => None,
    })
}

pub async fn plan(
    cfg: &Config,
    store: &Store,
    sources: &[Box<dyn Source>],
    repos: &[Repo],
    presets: &[Preset],
    now: DateTime<Utc>,
) -> Result<Plan> {
    let mut plan = Plan::default();
    let next = store.flag(NEXT_FLAG)?;
    let is_next = |w: &Work| next.as_deref() == Some(w.key().as_str());
    let mut first: Option<Work> = None;
    let usable: Vec<Repo> = repos
        .iter()
        .filter(|r| !cfg.repo_override(r.remote.as_deref(), &r.path).skip)
        .cloned()
        .collect();
    let rank: BTreeMap<String, usize> = usable
        .iter()
        .enumerate()
        .map(|(i, r)| (r.key(), i))
        .collect();

    let excluded: Vec<String> = cfg.tasks.exclude.iter().map(|e| e.to_lowercase()).collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut tasks: Vec<(usize, Work)> = Vec::new();
    for source in sources {
        let listed = match source.list(&usable).await {
            Ok(t) => t,
            Err(e) => {
                plan.errors.push(format!("{}: {e:#}", source.name()));
                continue;
            }
        };
        for task in listed {
            let lower = task.title.to_lowercase();
            if let Some(e) = excluded.iter().find(|e| lower.contains(e.as_str())) {
                plan.skipped.push(Skipped {
                    title: task.title.clone(),
                    why: format!("matches tasks.exclude {e:?}"),
                    key: None,
                });
                continue;
            }
            let Some(repo) = task
                .repo
                .as_deref()
                .and_then(|h| usable.iter().find(|r| r.matches(h)))
            else {
                plan.skipped.push(Skipped {
                    title: task.title.clone(),
                    why: format!(
                        "no known repo matches {:?}",
                        task.repo.as_deref().unwrap_or("")
                    ),
                    key: None,
                });
                continue;
            };
            let work = Work {
                what: What::Task(task),
                repo: repo.clone(),
            };
            // Two checkouts of one remote list the same todos.
            if !seen.insert(work.key()) {
                continue;
            }
            if is_next(&work) {
                first = Some(work);
                continue;
            }
            if let Some(why) = attempt_block(store, &work.key(), cfg, now)? {
                plan.skipped.push(Skipped {
                    title: work.title(),
                    why,
                    key: Some(work.key()),
                });
                continue;
            }
            tasks.push((rank.get(&repo.key()).copied().unwrap_or(usize::MAX), work));
        }
    }
    tasks.sort_by_key(|(r, _)| *r);
    plan.work.extend(tasks.into_iter().map(|(_, w)| w));

    // Presets due in active repos: the longest-waiting first, then by repo recency.
    let active = crate::repos::active(&usable, cfg.repos.active_days, now);
    let mut due: Vec<(Option<DateTime<Utc>>, usize, Work)> = Vec::new();
    for (i, repo) in active.iter().enumerate() {
        let allowed = cfg
            .repo_override(repo.remote.as_deref(), &repo.path)
            .presets;
        for p in presets {
            if allowed.as_ref().is_some_and(|a| !a.contains(&p.name)) {
                continue;
            }
            let last = store.preset_last_run(&repo.key(), &p.name)?;
            let cooldown = chrono::Duration::seconds((p.cooldown_days * 86_400.0) as i64);
            if last.is_some_and(|t| now - t < cooldown) {
                continue;
            }
            due.push((
                last,
                i,
                Work {
                    what: What::Preset(p.clone()),
                    repo: repo.clone(),
                },
            ));
        }
    }
    due.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    plan.work.extend(due.into_iter().map(|(_, _, w)| w));

    // The work asked for next, even a preset that isn't due or allowed here.
    if first.is_none()
        && let Some(next) = next.as_deref().filter(|n| n.starts_with("preset:"))
    {
        first = usable
            .iter()
            .flat_map(|r| {
                presets.iter().map(|p| Work {
                    what: What::Preset(p.clone()),
                    repo: r.clone(),
                })
            })
            .find(|w| w.key() == next);
    }
    if let Some(w) = first {
        plan.work.retain(|x| x.key() != w.key());
        plan.work.insert(0, w);
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo(name: &str, days_ago: i64) -> Repo {
        Repo {
            path: PathBuf::from(format!("/r/{name}")),
            remote_url: Some(format!("git@github.com:o/{name}.git")),
            remote: Some(format!("github.com/o/{name}")),
            last_commit: Some(Utc::now() - chrono::Duration::days(days_ago)),
            base: None,
        }
    }

    #[tokio::test]
    async fn tasks_first_then_due_presets() {
        let store = Store::in_memory().unwrap();
        store
            .add_native("github.com/o/old", "Fix old thing", "")
            .unwrap();
        store
            .add_native("github.com/o/new", "Fix new thing", "")
            .unwrap();
        store
            .add_native("github.com/o/missing", "Nowhere", "")
            .unwrap();
        let cfg = Config::default();
        let sources = crate::tasks::build(&cfg.sources, &store);
        let repos = vec![repo("new", 1), repo("old", 30)];
        let presets = vec![crate::presets::builtin("tests").unwrap()];
        store.preset_ran("github.com/o/new", "tests").unwrap();
        let p = plan(&cfg, &store, &sources, &repos, &presets, Utc::now())
            .await
            .unwrap();
        let titles: Vec<_> = p.work.iter().map(Work::title).collect();
        // "old" isn't active, so no presets there; "new" ran tests just now.
        assert_eq!(titles, ["Fix new thing", "Fix old thing"]);
        assert_eq!(p.skipped.len(), 1);

        // A task that failed recently waits; one that's done is skipped.
        let key = p.work[0].key();
        store.start_attempt(&key).unwrap();
        store.end_attempt(&key, JobStatus::Failed, None).unwrap();
        let p = plan(&cfg, &store, &sources, &repos, &presets, Utc::now())
            .await
            .unwrap();
        assert_eq!(p.work.len(), 1);

        // So does one that started and never finished (subr was killed).
        store.start_attempt(&p.work[0].key()).unwrap();
        let p = plan(&cfg, &store, &sources, &repos, &presets, Utc::now())
            .await
            .unwrap();
        assert!(p.work.is_empty());
    }

    #[tokio::test]
    async fn next_goes_first_past_attempts_and_cooldowns() {
        let store = Store::in_memory().unwrap();
        store.add_native("github.com/o/new", "First", "").unwrap();
        store.add_native("github.com/o/new", "Second", "").unwrap();
        let cfg = Config::default();
        let sources = crate::tasks::build(&cfg.sources, &store);
        let repos = vec![repo("new", 1), repo("old", 30)];
        let presets = vec![crate::presets::builtin("tests").unwrap()];
        let now = Utc::now();
        let p = plan(&cfg, &store, &sources, &repos, &presets, now)
            .await
            .unwrap();
        let second = p.work[1].key();

        // Skipped, then asked for next anyway.
        store
            .end_attempt(&second, JobStatus::Skipped, None)
            .unwrap();
        let p = plan(&cfg, &store, &sources, &repos, &presets, now)
            .await
            .unwrap();
        assert_eq!(p.skipped[0].why, "skipped with `subr skip`");
        assert_eq!(p.skipped[0].key.as_deref(), Some(second.as_str()));
        store.set_flag(NEXT_FLAG, Some(&second)).unwrap();
        let p = plan(&cfg, &store, &sources, &repos, &presets, now)
            .await
            .unwrap();
        let titles: Vec<_> = p.work.iter().map(Work::title).collect();
        assert_eq!(titles, ["Second", "First", "tests in new"]);

        // A preset in a repo that isn't active, so never due there.
        store
            .set_flag(NEXT_FLAG, Some("preset:github.com/o/old:tests"))
            .unwrap();
        let p = plan(&cfg, &store, &sources, &repos, &presets, now)
            .await
            .unwrap();
        assert_eq!(p.work[0].title(), "tests in old");
        assert_eq!(p.work.len(), 3);
    }

    #[test]
    fn slugs() {
        let w = Work {
            what: What::Task(Task {
                source: "x".into(),
                id: "1".into(),
                title: "Fix: the *flaky* test in `foo::bar` (again!) and a much longer tail".into(),
                body: String::new(),
                repo: None,
                meta: Default::default(),
            }),
            repo: repo("a", 0),
        };
        assert_eq!(w.slug(), "fix-the-flaky-test-in-foo-bar-ag");
    }
}
