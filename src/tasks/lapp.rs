//! Lapp (or any MCP server with Lapp's agent tools): todos from
//! `Agents/<project>/todos.md`, ticked with `complete_todo` and logged with
//! `append_log`. Report-mode results become notes (`write_note`).
//!
//! abuser only writes through the server, never to the vault's files.

use anyhow::Result;
use serde_json::{Value, json};

use super::mcp::McpClient;
use super::{Outcome, Source, Task, open_checkboxes};
use crate::repos::Repo;

pub struct Lapp {
    command: Vec<String>,
}

impl Lapp {
    pub fn new(command: Vec<String>) -> Self {
        Self { command }
    }

    async fn todos(c: &mut McpClient, remote: &str) -> Result<Vec<String>> {
        let listing = c.call("list_project", json!({"remote": remote})).await?;
        let Some(id) = listing
            .get("notes")
            .and_then(Value::as_array)
            .and_then(|n| {
                n.iter()
                    .find(|n| n.get("kind").and_then(Value::as_str) == Some("todos"))
            })
            .and_then(|n| n.get("id").and_then(Value::as_str))
        else {
            return Ok(Vec::new());
        };
        let note = c.call("read_note", json!({"id": id})).await?;
        Ok(open_checkboxes(
            note.get("body").and_then(Value::as_str).unwrap_or(""),
        ))
    }
}

fn remote(repo: &Repo) -> Option<&str> {
    repo.remote_url.as_deref()
}

#[async_trait::async_trait]
impl Source for Lapp {
    fn name(&self) -> &str {
        "lapp"
    }

    async fn list(&self, repos: &[Repo]) -> Result<Vec<Task>> {
        let with_remote: Vec<_> = repos.iter().filter(|r| remote(r).is_some()).collect();
        if with_remote.is_empty() {
            return Ok(Vec::new());
        }
        let mut c = McpClient::connect(&self.command).await?;
        let mut out = Vec::new();
        for repo in with_remote {
            let Some(url) = remote(repo) else { continue };
            match Self::todos(&mut c, url).await {
                Ok(items) => out.extend(items.into_iter().map(|text| Task {
                    source: "lapp".to_owned(),
                    id: format!("{}:{text}", repo.key()),
                    title: text,
                    body: String::new(),
                    repo: Some(url.to_owned()),
                    meta: Default::default(),
                })),
                Err(e) => tracing::debug!(repo = %repo.key(), "lapp: {e:#}"),
            }
        }
        Ok(out)
    }

    async fn complete(&self, task: &Task, repo: &Repo, outcome: &Outcome) -> Result<()> {
        let Some(url) = remote(repo) else {
            return Ok(());
        };
        let mut c = McpClient::connect(&self.command).await?;
        c.call("complete_todo", json!({"remote": url, "text": task.title}))
            .await?;
        let line = match &outcome.pr_url {
            Some(pr) => format!("abuser: {} — draft PR {pr}", task.title),
            None => format!("abuser: {} — {}", task.title, first_line(&outcome.summary)),
        };
        c.call("append_log", json!({"remote": url, "text": line}))
            .await?;
        Ok(())
    }

    async fn fail(&self, task: &Task, repo: &Repo, outcome: &Outcome) -> Result<()> {
        let Some(url) = remote(repo) else {
            return Ok(());
        };
        let mut c = McpClient::connect(&self.command).await?;
        let line = format!(
            "abuser: gave up on \"{}\" ({}): {}",
            task.title,
            outcome.status.as_str(),
            first_line(&outcome.summary)
        );
        c.call("append_log", json!({"remote": url, "text": line}))
            .await?;
        Ok(())
    }

    async fn report(&self, repo: &Repo, title: &str, body: &str) -> Result<bool> {
        let Some(url) = remote(repo) else {
            return Ok(false);
        };
        let mut c = McpClient::connect(&self.command).await?;
        c.call(
            "write_note",
            json!({"remote": url, "title": title, "body": body}),
        )
        .await?;
        Ok(true)
    }
}

fn first_line(s: &str) -> &str {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake Lapp MCP server in sh: answers initialize, list_project and read_note.
    const FAKE: &str = r##"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"initialize"'*) r='{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fake"}}';;
    *list_project*) r='{"content":[{"type":"text","text":"{\"notes\":[{\"id\":\"N1\",\"kind\":\"todos\"}]}"}]}';;
    *read_note*) r='{"content":[{"type":"text","text":"{\"body\":\"# Todos\\n- [x] old\\n- [ ] add retries\\n\"}"}]}';;
    *) r='{"content":[{"type":"text","text":"ok"}]}';;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$r"
done
"##;

    #[tokio::test]
    async fn lists_open_todos() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("lapp.sh");
        std::fs::write(&server, FAKE).unwrap();
        std::fs::set_permissions(&server, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let repo = Repo {
            path: dir.path().to_path_buf(),
            remote_url: Some("git@github.com:o/r.git".into()),
            remote: Some("github.com/o/r".into()),
            last_commit: None,
            base: None,
        };
        // Through `sh`: exec of a just-written file can fail with ETXTBSY.
        let lapp = Lapp::new(vec!["sh".into(), server.display().to_string()]);
        let tasks = lapp.list(&[repo]).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "add retries");
    }
}
