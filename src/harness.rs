//! Running agents through acc. The only module that touches `acc_core`.
//!
//! acc is sans-IO: it hands out shell scripts ([`Exec`]) and takes the
//! bytes the harness writes. abuser runs the scripts with `sh` on this host
//! and reads the session log (`~/.acc/s/<id>/out.ndjson`) straight from
//! disk. Sessions are detached, so a job survives a restart of abuser: the
//! runner saves [`Agent::snapshot`] and picks it up with [`Agent::restore`].

use std::path::{Path, PathBuf};

use acc_core::command::Command;
use acc_core::event::{Event, ItemKind, PermissionOptionKind, Quota};
use acc_core::policy::{Decision, Policy};
use acc_core::session::{LaunchRequest, Session, SessionSpec};
use acc_core::{Exec, Harness, RunState};
use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const CLIENT_TAG: &str = "abuser";

/// The answer to every question an agent asks: it runs unattended.
const NO_ONE_TO_ASK: &str = "Nobody is available to answer. Decide yourself, and note the assumption in your final summary.";

/// What to start.
#[derive(Debug, Clone)]
pub struct Spec {
    pub harness: Harness,
    pub repo: PathBuf,
    pub base: Option<String>,
    /// Worktree and session name (`[A-Za-z0-9_-]{1,64}`).
    pub name: String,
    pub prompt: String,
    pub model: Option<String>,
    pub permission: Option<String>,
    pub launcher: Option<acc_core::harness::Launcher>,
    pub policy: Vec<acc_core::policy::Rule>,
    pub labels: Vec<(String, String)>,
}

/// What happened since the last [`Agent::pump`].
#[derive(Debug, Default)]
pub struct Progress {
    /// The turn ended (and no background work will start another).
    pub turn_done: bool,
    /// The turn ended in an error, with the harness's message.
    pub error: Option<String>,
    pub exited: bool,
    /// The harness wants a sign-in; nothing will happen until a human does it.
    pub needs_auth: bool,
    pub quota: Option<Quota>,
    /// Approvals rejected because no policy rule allowed them.
    pub rejected: Vec<String>,
}

pub struct Agent {
    session: Session,
    log: PathBuf,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("id", &self.session.id())
            .finish()
    }
}

/// Runs an acc script with `sh`; returns stdout, or fails with stderr.
pub async fn sh(exec: &Exec) -> Result<String> {
    let (code, out, err) = sh_status(exec).await?;
    if code != 0 {
        bail!("exit {code}: {}", err.trim());
    }
    Ok(out)
}

async fn sh_status(exec: &Exec) -> Result<(i32, String, String)> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&exec.script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("starting sh")?;
    if let Some(mut stdin) = child.stdin.take() {
        if !exec.stdin.is_empty() {
            stdin.write_all(&exec.stdin).await?;
        }
        drop(stdin);
    }
    let out = child.wait_with_output().await?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

pub fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

#[cfg(test)]
pub fn valid_name(name: &str) -> bool {
    acc_core::worktree::valid_name(name)
}

/// The branch acc puts a worktree's changes on.
pub fn branch(name: &str) -> String {
    acc_core::worktree::branch(name)
}

impl Agent {
    /// Makes the worktree and starts the harness in it, detached.
    pub async fn launch(spec: Spec) -> Result<Self> {
        let repo = spec.repo.display().to_string();
        let add = Exec::new(acc_core::worktree::add_script(
            &repo,
            &spec.name,
            spec.base.as_deref(),
        ));
        let cwd = sh(&add)
            .await
            .context("making the worktree")?
            .trim()
            .to_owned();
        let req = LaunchRequest {
            harness: Some(spec.harness),
            cwd,
            model: spec.model,
            permission: spec.permission,
            title: Some(spec.name.clone()),
            prompt: Some(spec.prompt),
            labels: spec.labels.into_iter().collect(),
            launcher: spec.launcher,
            worktree: Some(spec.name.clone()),
            ..LaunchRequest::default()
        };
        let (mut session, exec) = Session::launch(req, &spec.name, CLIENT_TAG, now_ms())?;
        sh(&exec).await.context("starting the harness")?;
        session.set_policy(Some(Policy { rules: spec.policy }));
        let mut agent = Self {
            log: crate::paths::acc_session(&spec.name).join("out.ndjson"),
            session,
        };
        agent.flush().await?;
        Ok(agent)
    }

    /// Picks a session up again from [`Agent::snapshot`].
    pub async fn restore(
        snapshot: &str,
        name: &str,
        policy: Vec<acc_core::policy::Rule>,
    ) -> Result<Self> {
        let dir = crate::paths::acc_session(name);
        let spec_json = tokio::fs::read_to_string(dir.join("spec.json"))
            .await
            .context("the acc session is gone")?;
        let spec = SessionSpec::from_json(&spec_json).context("unreadable spec.json")?;
        let mut session = Session::restore(snapshot, spec.clone())
            .or_else(|_| Session::attach(spec, CLIENT_TAG))
            .context("reattaching")?;
        let status = sh(&session.status_exec()).await.unwrap_or_default();
        session.on_status(&status, now_ms());
        session.set_policy(Some(Policy { rules: policy }));
        Ok(Self {
            log: dir.join("out.ndjson"),
            session,
        })
    }

    pub fn snapshot(&self) -> Option<String> {
        self.session.snapshot()
    }

    /// Writes whatever the session wants to send to the harness.
    async fn flush(&mut self) -> Result<()> {
        let bytes = self.session.take_transmit();
        if !bytes.is_empty() {
            sh(&self.session.input_exec(bytes))
                .await
                .context("writing to the harness")?;
        }
        if let Some(exec) = self.session.take_spec_write() {
            let _ = sh(&exec).await;
        }
        Ok(())
    }

    pub async fn send(&mut self, cmd: Command) -> Result<()> {
        if let acc_core::session::Outbound::Send(bytes) = self.session.command(cmd)?
            && !bytes.is_empty()
        {
            sh(&self.session.input_exec(bytes)).await?;
        }
        Ok(())
    }

    /// Reads what the harness wrote since last time and answers what is
    /// waiting: the policy answers what it can, everything else is
    /// rejected, since nobody is there to ask.
    pub async fn pump(&mut self) -> Result<Progress> {
        let mut p = Progress::default();
        let bytes = read_from(&self.log, self.session.offset()).await?;
        let mut updates = if bytes.is_empty() {
            self.session.take_updates(now_ms())
        } else {
            self.session.feed(&bytes, now_ms())
        };
        let status = sh(&self.session.status_exec()).await.unwrap_or_default();
        updates.extend(self.session.on_status(&status, now_ms()));
        for u in &updates {
            match &u.event {
                Event::TurnCompleted => p.turn_done = true,
                Event::Quota(q) => p.quota = Some(q.clone()),
                Event::Exited { .. } => p.exited = true,
                _ => {}
            }
        }
        if let Some(state) = self.session.state() {
            match state.state {
                RunState::Error if p.turn_done => {
                    p.error = Some(
                        state
                            .message
                            .clone()
                            .unwrap_or_else(|| "harness error".to_owned()),
                    );
                }
                RunState::Waiting => p.turn_done = false,
                RunState::Blocked {
                    kind: acc_core::BlockKind::Auth,
                } => p.needs_auth = true,
                RunState::Exited => p.exited = true,
                _ => {}
            }
        }
        if !self.session.is_alive() && !self.session.is_replaying() {
            p.exited = true;
        }
        let waiting: Vec<_> = self.session.permissions().to_vec();
        for perm in waiting {
            if perm.kind == acc_core::BlockKind::Question {
                let questions = acc_core::question::parse(&perm.input);
                let answers: Vec<Vec<String>> = (0..questions.len().max(1))
                    .map(|_| vec![NO_ONE_TO_ASK.to_owned()])
                    .collect();
                self.send(Command::AnswerPermission {
                    request_id: perm.request_id.clone(),
                    option_id: acc_core::question::encode(&answers),
                })
                .await?;
                continue;
            }
            let reject = acc_core::policy::option_for(Decision::Deny, &perm.options)
                .or_else(|| {
                    perm.options
                        .iter()
                        .find(|o| matches!(o.kind, Some(PermissionOptionKind::RejectOnce)))
                })
                .map(|o| o.id.clone());
            let Some(option_id) = reject else { continue };
            p.rejected.push(perm.title.clone());
            self.send(Command::AnswerPermission {
                request_id: perm.request_id.clone(),
                option_id,
            })
            .await?;
        }
        self.flush().await?;
        Ok(p)
    }

    /// The agent's last message of the session.
    pub fn last_message(&self) -> Option<String> {
        self.session
            .transcript()
            .top_level()
            .filter(|i| i.kind == ItemKind::AgentMessage && !i.text.trim().is_empty())
            .last()
            .map(|i| i.text.clone())
    }

    pub fn usage(&self) -> &acc_core::event::Usage {
        self.session.usage()
    }

    /// Asks the harness to report usage and quota again.
    pub async fn refresh(&mut self) {
        let _ = self.send(Command::Refresh).await;
    }

    pub async fn interrupt(&mut self) {
        let _ = self.send(Command::Interrupt).await;
    }

    /// Stops the harness (TERM, then KILL). The worktree stays.
    pub async fn kill(&self) -> Result<()> {
        sh(&self.session.kill_exec()).await.map(drop)
    }

    /// Stops the harness and deletes the worktree and its branch, then the
    /// session directory.
    pub async fn discard(&self) -> Result<()> {
        sh(&self.session.worktree_discard_exec()?).await?;
        sh(&self.session.remove_exec()).await.map(drop)
    }

    /// Stops the harness and deletes the session directory; the worktree stays.
    pub async fn remove_session(&self) -> Result<()> {
        sh(&self.session.remove_exec()).await.map(drop)
    }

    pub fn worktree_dir(&self) -> PathBuf {
        crate::paths::acc_worktree(self.session.id())
    }
}

async fn read_from(path: &Path, offset: u64) -> Result<Vec<u8>> {
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return Ok(Vec::new());
    };
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await?;
    Ok(buf)
}

/// One thing that happened in a session, for people to read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LogLine {
    /// `agent`, `run`, `edit`, `tool`, `subagent`, `ask`, `notice`, `error` or `turn`.
    pub kind: &'static str,
    pub text: String,
}

/// Reads a session's log (`out.ndjson`) as [`LogLine`]s, picking up where
/// the last [`LogReader::read`] stopped.
pub struct LogReader {
    session: Session,
}

impl LogReader {
    pub fn new(id: &str, harness: Harness) -> Result<Self> {
        let spec = SessionSpec::new(id, harness, "", harness.launch(None), 0);
        let session = Session::attach(spec, CLIENT_TAG).context("reading the session log")?;
        Ok(Self { session })
    }

    /// What was added to the log at `path` since the last read. The path
    /// may change between reads to a copy of the same log.
    pub async fn read(&mut self, path: &Path) -> Result<Vec<LogLine>> {
        let bytes = read_from(path, self.session.offset()).await?;
        // Only whole lines: the harness may be halfway through one.
        let Some(end) = bytes.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };
        let updates = self.session.feed(&bytes[..=end], now_ms());
        Ok(updates.iter().filter_map(|u| self.line(&u.event)).collect())
    }

    fn line(&self, event: &Event) -> Option<LogLine> {
        use acc_core::item::ItemDetail;
        let (kind, text) = match event {
            Event::ItemCompleted { id, detail, .. } => {
                let item = self.session.transcript().get(id)?;
                match detail {
                    ItemDetail::Text if item.kind == ItemKind::AgentMessage => {
                        ("agent", item.text.trim().to_owned())
                    }
                    ItemDetail::Text => return None,
                    ItemDetail::Command {
                        command, exit_code, ..
                    } => (
                        "run",
                        match exit_code {
                            Some(c) if *c != 0 => format!("{command}  (exit {c})"),
                            _ => command.clone(),
                        },
                    ),
                    ItemDetail::FileChange { files, .. } => (
                        "edit",
                        files
                            .iter()
                            .map(|f| f.path.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    ItemDetail::Tool { tool, title, .. } => {
                        ("tool", format!("{tool} {title}").trim().to_owned())
                    }
                    ItemDetail::Subagent { description, .. } => ("subagent", description.clone()),
                }
            }
            Event::PermissionRequested { title, .. } => ("ask", title.clone()),
            Event::Notice { message } => ("notice", message.clone()),
            Event::StateChanged(s) if s.state == RunState::Error => (
                "error",
                s.message
                    .clone()
                    .unwrap_or_else(|| "harness error".to_owned()),
            ),
            Event::TurnCompleted => ("turn", "done".to_owned()),
            _ => return None,
        };
        (!text.is_empty()).then_some(LogLine { kind, text })
    }
}

/// Stops a session's harness by id, without its state.
pub async fn kill_session(id: &str) -> Result<()> {
    let dir = acc_core::detached::SessionDir::new(id).context("bad session id")?;
    sh(&Exec::new(dir.kill_script(3_000))).await.map(drop)
}
