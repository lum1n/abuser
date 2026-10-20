//! A minimal MCP client over stdio: start the server, initialise, call tools.
//! One connection per batch of calls; it ends when the client is dropped.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const TIMEOUT: Duration = Duration::from_secs(30);

pub struct McpClient {
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl McpClient {
    pub async fn connect(command: &[String]) -> Result<Self> {
        let (prog, args) = command.split_first().context("empty MCP command")?;
        let mut child = tokio::process::Command::new(crate::paths::expand_home(prog.as_ref()))
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting MCP server {prog}"))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let mut c = Self {
            _child: child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
        };
        c.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "abuser", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await
        .context("MCP initialize")?;
        c.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(c)
    }

    async fn send(&mut self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let read = async {
            loop {
                let line = self.lines.next_line().await?.context("MCP server closed")?;
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                // Skip notifications and requests from the server.
                if msg.get("id").and_then(Value::as_u64) != Some(id) || msg.get("method").is_some()
                {
                    continue;
                }
                if let Some(err) = msg.get("error") {
                    bail!(
                        "{method}: {}",
                        err.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                    );
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        tokio::time::timeout(TIMEOUT, read)
            .await
            .context("MCP request timed out")?
    }

    /// Calls a tool and returns its result: the first text content parsed as
    /// JSON when it is JSON, else as a string.
    pub async fn call(&mut self, tool: &str, args: Value) -> Result<Value> {
        let r = self
            .request("tools/call", json!({"name": tool, "arguments": args}))
            .await?;
        let text = r
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.iter().find_map(|p| p.get("text").and_then(Value::as_str)))
            .unwrap_or("");
        if r.get("isError").and_then(Value::as_bool) == Some(true) {
            bail!("{tool}: {text}");
        }
        if let Some(s) = r.get("structuredContent") {
            return Ok(s.clone());
        }
        Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned())))
    }
}
