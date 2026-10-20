//! Open GitHub issues with a label, through the `gh` CLI.

use anyhow::Result;
use serde::Deserialize;

use super::{Outcome, Source, Task};
use crate::repos::Repo;

pub struct Github {
    label: String,
}

impl Github {
    pub fn new(label: String) -> Self {
        Self { label }
    }
}

#[derive(Deserialize)]
struct Issue {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    url: String,
}

fn slug(repo: &Repo) -> Option<String> {
    repo.remote
        .as_deref()?
        .strip_prefix("github.com/")
        .map(str::to_owned)
}

#[async_trait::async_trait]
impl Source for Github {
    fn name(&self) -> &str {
        "github"
    }

    async fn list(&self, repos: &[Repo]) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        for repo in repos {
            let Some(slug) = slug(repo) else { continue };
            let json = match crate::gh::run(
                &[
                    "issue",
                    "list",
                    "-R",
                    &slug,
                    "--label",
                    &self.label,
                    "--state",
                    "open",
                    "--json",
                    "number,title,body,url",
                    "--limit",
                    "20",
                ],
                None,
            )
            .await
            {
                Ok(j) => j,
                Err(e) => {
                    tracing::debug!(repo = %slug, "github issues: {e:#}");
                    continue;
                }
            };
            for i in serde_json::from_str::<Vec<Issue>>(&json).unwrap_or_default() {
                out.push(Task {
                    source: "github".to_owned(),
                    id: format!("{slug}#{}", i.number),
                    title: i.title,
                    body: i.body,
                    repo: repo.remote.clone(),
                    meta: [
                        ("number".to_owned(), i.number.to_string()),
                        ("url".to_owned(), i.url),
                        ("slug".to_owned(), slug.clone()),
                    ]
                    .into(),
                });
            }
        }
        Ok(out)
    }

    async fn complete(&self, _task: &Task, _repo: &Repo, _outcome: &Outcome) -> Result<()> {
        // The PR body says `Closes #N`, so merging it closes the issue.
        Ok(())
    }

    async fn fail(&self, task: &Task, _repo: &Repo, outcome: &Outcome) -> Result<()> {
        let (Some(n), Some(slug)) = (task.meta.get("number"), task.meta.get("slug")) else {
            return Ok(());
        };
        let body = format!(
            "abuser tried this issue and stopped ({}):\n\n{}",
            outcome.status.as_str(),
            outcome.summary
        );
        crate::gh::run(
            &["issue", "comment", n, "-R", slug, "--body-file", "-"],
            Some(&body),
        )
        .await?;
        Ok(())
    }

    fn agent_hint(&self, task: &Task) -> Option<String> {
        task.meta
            .get("url")
            .map(|u| format!("This task is the GitHub issue {u}."))
    }
}
