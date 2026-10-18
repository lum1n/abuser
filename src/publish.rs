//! Getting finished work out of the worktree: commit what's left, verify,
//! push the branch and open a draft PR (GitHub, through `gh`).

use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::config::PublishCfg;
use crate::repos::Repo;

#[derive(Debug, Clone, PartialEq)]
pub enum Published {
    /// A draft PR.
    Pr(String),
    /// Pushed, but the host isn't GitHub: just the branch.
    Pushed(String),
    /// Kept locally (`push = false`).
    Local(String),
    /// Nothing to publish: no commits ahead of the base.
    Nothing,
}

impl Published {
    pub fn url(&self) -> Option<&str> {
        match self {
            Published::Pr(u) => Some(u),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Published::Pr(u) => format!("draft PR {u}"),
            Published::Pushed(b) => format!("pushed branch {b}"),
            Published::Local(b) => format!("local branch {b}"),
            Published::Nothing => "nothing to publish".to_owned(),
        }
    }
}

pub struct Request<'a> {
    pub repo: &'a Repo,
    pub worktree: &'a Path,
    /// acc's branch for the worktree (`acc/<name>`).
    pub local_branch: &'a str,
    /// The branch name to publish as (`subr/<name>`).
    pub branch: &'a str,
    /// The commit the work started from.
    pub base_commit: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub push: bool,
    pub verify: Option<&'a str>,
}

async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Build output that a repo without a `.gitignore` would otherwise get
/// committed with the agent's work.
const ARTIFACTS: &[&str] = &[
    ":(exclude,glob)**/__pycache__/**",
    ":(exclude,glob)**/*.pyc",
    ":(exclude,glob)**/node_modules/**",
    ":(exclude,glob)**/.pytest_cache/**",
    ":(exclude,glob)**/.mypy_cache/**",
    ":(exclude,glob)**/.DS_Store",
];

/// `-c user.name=... -c user.email=...` when git has no identity here.
async fn identity(dir: &Path) -> Vec<String> {
    if git(dir, &["config", "user.email"]).await.is_ok() {
        Vec::new()
    } else {
        ["-c", "user.name=subr", "-c", "user.email=subr@localhost"]
            .map(str::to_owned)
            .to_vec()
    }
}

/// Commits anything the agent left uncommitted. Returns whether there are
/// commits ahead of `base`.
pub async fn commit_leftovers(worktree: &Path, base: &str, message: &str) -> Result<bool> {
    let mut add = vec!["add", "-A", "--", "."];
    add.extend(ARTIFACTS);
    git(worktree, &add).await?;
    if git(worktree, &["diff", "--cached", "--quiet"])
        .await
        .is_err()
    {
        let mut args = identity(worktree).await;
        args.extend(["commit", "-q", "--no-verify", "-m", message].map(str::to_owned));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git(worktree, &args).await?;
    }
    let ahead = git(worktree, &["rev-list", "--count", &format!("{base}..HEAD")]).await?;
    Ok(ahead.parse::<u64>().unwrap_or(0) > 0)
}

pub async fn run_verify(worktree: &Path, cmd: &str) -> Result<()> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(worktree)
        .output()
        .await
        .context("running verify")?;
    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        bail!("verify `{cmd}` failed:\n{tail}");
    }
    Ok(())
}

pub async fn publish(req: &Request<'_>, cfg: &PublishCfg) -> Result<Published> {
    if !commit_leftovers(req.worktree, req.base_commit, req.title).await? {
        return Ok(Published::Nothing);
    }
    if let Some(v) = req.verify {
        run_verify(req.worktree, v).await?;
    }
    if !req.push {
        return Ok(Published::Local(req.local_branch.to_owned()));
    }
    git(
        req.worktree,
        &[
            "push",
            "-q",
            &cfg.remote,
            &format!("{}:refs/heads/{}", req.local_branch, req.branch),
        ],
    )
    .await
    .context("pushing")?;
    let Some(slug) = github_slug(req.repo) else {
        return Ok(Published::Pushed(req.branch.to_owned()));
    };
    let base = req
        .repo
        .base
        .as_deref()
        .map(|b| {
            b.strip_prefix(&format!("{}/", cfg.remote))
                .unwrap_or(b)
                .to_owned()
        })
        .unwrap_or_else(|| "main".to_owned());
    // Make sure the label exists; it fails harmlessly when it does.
    let _ = crate::gh::run(
        &[
            "label",
            "create",
            &cfg.label,
            "-R",
            &slug,
            "--description",
            "Opened by subr",
            "--color",
            "8B5CF6",
        ],
        None,
    )
    .await;
    let url = crate::gh::run(
        &[
            "pr",
            "create",
            "-R",
            &slug,
            "--draft",
            "--head",
            req.branch,
            "--base",
            &base,
            "--title",
            req.title,
            "--label",
            &cfg.label,
            "--body-file",
            "-",
        ],
        Some(req.body),
    )
    .await
    .context("opening the PR")?;
    Ok(Published::Pr(
        url.trim().lines().last().unwrap_or("").to_owned(),
    ))
}

/// Keeps the work as a local branch named `branch` and drops the worktree.
pub async fn keep_local(
    repo: &Path,
    worktree: &Path,
    local_branch: &str,
    branch: &str,
) -> Result<()> {
    git(
        repo,
        &[
            "worktree",
            "remove",
            "--force",
            &worktree.display().to_string(),
        ],
    )
    .await?;
    git(repo, &["branch", "-M", local_branch, branch]).await?;
    // acc's note about the worktree, now gone.
    let _ = tokio::fs::remove_file(worktree.with_extension("meta")).await;
    Ok(())
}

fn github_slug(repo: &Repo) -> Option<String> {
    repo.remote
        .as_deref()?
        .strip_prefix("github.com/")
        .map(str::to_owned)
}

/// Open subr PRs in a repo. `None` when it isn't on GitHub or `gh` can't tell.
pub async fn open_prs(repo: &Repo, label: &str) -> Option<usize> {
    let slug = github_slug(repo)?;
    let json = crate::gh::run(
        &[
            "pr", "list", "-R", &slug, "--label", label, "--state", "open", "--json", "number",
            "--limit", "100",
        ],
        None,
    )
    .await
    .ok()?;
    serde_json::from_str::<Vec<serde_json::Value>>(&json)
        .ok()
        .map(|v| v.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "{script}");
    }

    #[tokio::test]
    async fn commits_pushes_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        sh(
            d,
            "git init -q --bare remote.git && git init -q -b main repo && cd repo \
             && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m init \
             && git remote add origin ../remote.git && git push -q origin main \
             && git worktree add -q -b acc/w ../wt",
        );
        let repo_dir = d.join("repo");
        let wt = d.join("wt");
        let base = git(&wt, &["rev-parse", "HEAD"]).await.unwrap();
        // Nothing yet.
        assert!(!commit_leftovers(&wt, &base, "x").await.unwrap());
        std::fs::write(wt.join("a.txt"), "hi").unwrap();
        std::fs::create_dir_all(wt.join("__pycache__")).unwrap();
        std::fs::write(wt.join("__pycache__/a.cpython-314.pyc"), "x").unwrap();
        let repo = Repo {
            path: repo_dir.clone(),
            remote_url: Some("../remote.git".into()),
            remote: None,
            last_commit: None,
            base: Some("origin/main".into()),
        };
        let req = Request {
            repo: &repo,
            worktree: &wt,
            local_branch: "acc/w",
            branch: "subr/w",
            base_commit: &base,
            title: "Add a.txt",
            body: "body",
            push: true,
            verify: Some("test -f a.txt"),
        };
        let p = publish(&req, &PublishCfg::default()).await.unwrap();
        assert_eq!(p, Published::Pushed("subr/w".into()));
        let remote_branches = git(&d.join("remote.git"), &["branch", "--list"])
            .await
            .unwrap();
        assert!(remote_branches.contains("subr/w"));
        let files = git(&wt, &["ls-tree", "-r", "--name-only", "HEAD"])
            .await
            .unwrap();
        assert_eq!(files, "a.txt", "build output stays out");

        let failing = Request {
            verify: Some("false"),
            ..req
        };
        std::fs::write(wt.join("b.txt"), "more").unwrap();
        assert!(publish(&failing, &PublishCfg::default()).await.is_err());
    }
}
