//! Any program as a task source, for trackers subr doesn't know.
//!
//! - `<command> list` prints a JSON array of tasks:
//!   `[{"id": "...", "title": "...", "body": "...", "repo": "github.com/o/r"}]`.
//! - `<command> complete <id>` and `<command> fail <id>` get the outcome as
//!   JSON on stdin: `{"status": "done", "summary": "...", "pr_url": "..."}`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use super::{Outcome, Source, Task};
use crate::repos::Repo;

pub struct CommandSource {
    name: String,
    command: Vec<String>,
}

#[derive(Deserialize)]
struct Listed {
    id: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    repo: Option<String>,
}

impl CommandSource {
    pub fn new(name: &str, command: Vec<String>) -> Self {
        Self {
            name: name.to_owned(),
            command,
        }
    }

    async fn run(&self, args: &[&str], stdin: Option<String>) -> Result<String> {
        let (prog, rest) = self.command.split_first().context("empty command")?;
        let mut child = tokio::process::Command::new(crate::paths::expand_home(prog.as_ref()))
            .args(rest)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {prog}"))?;
        if let Some(mut pipe) = child.stdin.take()
            && let Some(text) = stdin
        {
            pipe.write_all(text.as_bytes()).await?;
        }
        let out =
            tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
                .await
                .context("timed out")??;
        if !out.status.success() {
            bail!(
                "{prog} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[async_trait::async_trait]
impl Source for CommandSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list(&self, _repos: &[Repo]) -> Result<Vec<Task>> {
        let text = self.run(&["list"], None).await?;
        let listed: Vec<Listed> =
            serde_json::from_str(&text).with_context(|| format!("{}: list output", self.name))?;
        Ok(listed
            .into_iter()
            .map(|l| Task {
                source: self.name.clone(),
                id: l.id,
                title: l.title,
                body: l.body,
                repo: l.repo,
                meta: Default::default(),
            })
            .collect())
    }

    async fn complete(&self, task: &Task, _repo: &Repo, outcome: &Outcome) -> Result<()> {
        self.run(
            &["complete", &task.id],
            Some(serde_json::to_string(outcome)?),
        )
        .await?;
        Ok(())
    }

    async fn fail(&self, task: &Task, _repo: &Repo, outcome: &Outcome) -> Result<()> {
        self.run(&["fail", &task.id], Some(serde_json::to_string(outcome)?))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn speaks_the_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("src.sh");
        let log = dir.path().join("log");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n list) echo '[{{\"id\":\"T-1\",\"title\":\"Fix it\",\"repo\":\"github.com/o/r\"}}]';;\n complete) cat > {log};;\nesac\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let s = CommandSource::new("tracker", vec!["sh".into(), script.display().to_string()]);
        let tasks = s.list(&[]).await.unwrap();
        assert_eq!(tasks[0].id, "T-1");
        assert_eq!(tasks[0].repo.as_deref(), Some("github.com/o/r"));
        let repo = crate::repos::inspect(dir.path());
        let outcome = Outcome {
            status: crate::store::JobStatus::Done,
            summary: "did it".into(),
            pr_url: Some("https://x/pr/1".into()),
        };
        s.complete(&tasks[0], &repo, &outcome).await.unwrap();
        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .contains("\"pr_url\":\"https://x/pr/1\"")
        );
    }
}
