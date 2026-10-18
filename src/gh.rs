//! The GitHub CLI, for issues and pull requests.

use anyhow::{Context, Result, bail};

/// Runs `gh` with `args` and optional stdin; returns stdout.
pub async fn run(args: &[&str], stdin: Option<&str>) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(args)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("running gh (is the GitHub CLI installed?)")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "gh {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
