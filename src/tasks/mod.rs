//! Where work comes from. Each source lists open tasks for the repos subr
//! knows and hears back when a task is done or failed.

mod command;
mod github;
mod lapp;
mod markdown;
pub mod mcp;
mod native;

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::SourceCfg;
use crate::repos::Repo;
use crate::store::Store;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// The source's name (`native`, `lapp`, ...).
    pub source: String,
    /// Stable id within the source.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// The repo it's for: a remote, path or directory name.
    #[serde(default)]
    pub repo: Option<String>,
    /// Source-specific data carried to `complete`/`fail`.
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

impl Task {
    /// Key for attempts and jobs; changes when the task's text does.
    pub fn key(&self) -> String {
        let mut h = Sha256::new();
        for part in [&self.source, &self.id, &self.title] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        format!("task:{}", &hex::encode(h.finalize())[..16])
    }
}

/// How a job for a task ended, as sources hear it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: crate::store::JobStatus,
    pub summary: String,
    pub pr_url: Option<String>,
}

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &str;
    /// Open tasks for `repos`. Tasks for repos subr doesn't know are dropped
    /// by the caller.
    async fn list(&self, repos: &[Repo]) -> Result<Vec<Task>>;
    async fn complete(&self, task: &Task, repo: &Repo, outcome: &Outcome) -> Result<()>;
    async fn fail(&self, task: &Task, repo: &Repo, outcome: &Outcome) -> Result<()>;
    /// Files a report-mode result. `Ok(false)` when this source can't.
    async fn report(&self, _repo: &Repo, _title: &str, _body: &str) -> Result<bool> {
        Ok(false)
    }
    /// What the agent should know about this source, added to its prompt.
    fn agent_hint(&self, _task: &Task) -> Option<String> {
        None
    }
}

pub fn build(cfg: &[SourceCfg], store: &Store) -> Vec<Box<dyn Source>> {
    cfg.iter()
        .map(|c| -> Box<dyn Source> {
            match c {
                SourceCfg::Native {} => Box::new(native::Native::new(store.clone())),
                SourceCfg::Markdown { file } => Box::new(markdown::Markdown::new(file.clone())),
                SourceCfg::Github { label } => Box::new(github::Github::new(label.clone())),
                SourceCfg::Lapp { command } => Box::new(lapp::Lapp::new(command.clone())),
                SourceCfg::Command { name, command } => {
                    Box::new(command::CommandSource::new(name, command.clone()))
                }
            }
        })
        .collect()
}

/// Open `- [ ] text` items in a Markdown document.
pub fn open_checkboxes(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let rest = l
                .strip_prefix("- [ ]")
                .or_else(|| l.strip_prefix("* [ ]"))?;
            let item = rest.trim();
            (!item.is_empty()).then(|| item.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkboxes() {
        let md = "---\nkind: todos\n---\n# Todos\n\n- [x] done\n- [ ] fix the flaky test\n  - [ ] nested\n* [ ] star\n- [ ]\n- [] not a box\n";
        assert_eq!(
            open_checkboxes(md),
            ["fix the flaky test", "nested", "star"]
        );
    }

    #[test]
    fn key_changes_with_text() {
        let t = Task {
            source: "lapp".into(),
            id: "a".into(),
            title: "x".into(),
            body: String::new(),
            repo: None,
            meta: BTreeMap::new(),
        };
        let mut u = t.clone();
        assert_eq!(t.key(), u.key());
        u.title = "y".into();
        assert_ne!(t.key(), u.key());
    }
}
