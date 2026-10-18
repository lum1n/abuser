//! `- [ ]` items in a Markdown file in each repo (`TODO.md` by default).
//! The agent ticks the item on its branch, so the tick lands with the PR.

use anyhow::Result;

use super::{Outcome, Source, Task, open_checkboxes};
use crate::repos::Repo;

pub struct Markdown {
    file: String,
}

impl Markdown {
    pub fn new(file: String) -> Self {
        Self { file }
    }
}

#[async_trait::async_trait]
impl Source for Markdown {
    fn name(&self) -> &str {
        "markdown"
    }

    async fn list(&self, repos: &[Repo]) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        for repo in repos {
            let Ok(text) = std::fs::read_to_string(repo.path.join(&self.file)) else {
                continue;
            };
            for item in open_checkboxes(&text) {
                out.push(Task {
                    source: "markdown".to_owned(),
                    id: format!("{}:{item}", repo.key()),
                    title: item,
                    body: String::new(),
                    repo: Some(repo.path.display().to_string()),
                    meta: Default::default(),
                });
            }
        }
        Ok(out)
    }

    async fn complete(&self, _task: &Task, _repo: &Repo, _outcome: &Outcome) -> Result<()> {
        Ok(())
    }

    async fn fail(&self, _task: &Task, _repo: &Repo, _outcome: &Outcome) -> Result<()> {
        Ok(())
    }

    fn agent_hint(&self, task: &Task) -> Option<String> {
        Some(format!(
            "This task is the item `- [ ] {}` in `{}`. When you have done it, change that line to `- [x]`.",
            task.title, self.file
        ))
    }
}
