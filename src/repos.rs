//! Finding the repositories subr may work in, most recently active first.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use chrono::{DateTime, Utc};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::config::ReposCfg;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub path: PathBuf,
    /// `origin` as configured (`git@github.com:o/r.git`).
    pub remote_url: Option<String>,
    /// `host/owner/name`, for matching tasks to repos.
    pub remote: Option<String>,
    pub last_commit: Option<DateTime<Utc>>,
    /// The branch work starts from (the remote's default, else `HEAD`'s).
    pub base: Option<String>,
}

impl Repo {
    /// What subr calls this repo in its state: the remote, else the path.
    pub fn key(&self) -> String {
        self.remote
            .clone()
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map_or_else(|| self.key(), |n| n.to_string_lossy().into_owned())
    }

    /// Whether `hint` (a remote in any form, a path or a directory name) names this repo.
    pub fn matches(&self, hint: &str) -> bool {
        let hint = hint.trim();
        if hint.is_empty() {
            return false;
        }
        if let (Some(r), Some(h)) = (&self.remote, normalize_remote(hint))
            && r.eq_ignore_ascii_case(&h)
        {
            return true;
        }
        let p = crate::paths::expand_home(Path::new(hint));
        p == self.path || (!hint.contains('/') && self.name() == hint)
    }
}

/// `host/owner/name` from any git remote URL form, or `None` for local paths.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let rest = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme == "file" {
            return None;
        }
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        // Drop a port.
        match rest.split_once('/') {
            Some((host, path)) => format!("{}/{path}", host.split(':').next().unwrap_or(host)),
            None => return None,
        }
    } else if let Some((user_host, path)) = url.split_once(':') {
        // scp-like `git@host:owner/name`; a Windows drive or local path isn't.
        if user_host.contains('/') || path.starts_with('/') || user_host.len() < 2 {
            return None;
        }
        let host = user_host.rsplit_once('@').map_or(user_host, |(_, h)| h);
        format!("{host}/{path}")
    } else if url.starts_with("github.com/") || url.starts_with("gitlab.com/") {
        url.to_owned()
    } else {
        return None;
    };
    let (host, path) = rest.split_once('/')?;
    (!path.is_empty()).then(|| format!("{}/{path}", host.to_ascii_lowercase()))
}

fn globs(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(
            &crate::paths::expand_home(Path::new(p))
                .display()
                .to_string(),
        )?);
    }
    Ok(b.build()?)
}

/// Every repo under the configured roots, newest commit first.
pub fn scan(cfg: &ReposCfg) -> Result<Vec<Repo>> {
    let include = globs(&cfg.include)?;
    let exclude = globs(&cfg.exclude)?;
    let mut paths = Vec::new();
    for root in &cfg.roots {
        walk(root, cfg.max_depth, &mut paths);
    }
    paths.sort();
    paths.dedup();
    let mut repos: Vec<Repo> = paths
        .into_iter()
        .filter(|p| (cfg.include.is_empty() || include.is_match(p)) && !exclude.is_match(p))
        .map(|p| inspect(&p))
        .collect();
    repos.sort_by_key(|r| std::cmp::Reverse(r.last_commit));
    Ok(repos)
}

/// Repos with a commit in the last `active_days`.
pub fn active(repos: &[Repo], active_days: u32, now: DateTime<Utc>) -> Vec<Repo> {
    let since = now - chrono::Duration::days(i64::from(active_days));
    repos
        .iter()
        .filter(|r| r.last_commit.is_some_and(|t| t >= since))
        .cloned()
        .collect()
}

/// Collects repos under `dir`. It keeps going inside a repo, since a root
/// or a monorepo can hold others.
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if dir.join(".git").exists() {
        out.push(dir.to_path_buf());
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || matches!(name.as_ref(), "node_modules" | "target" | "vendor") {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&e.path(), depth - 1, out);
        }
    }
}

pub fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

pub fn inspect(path: &Path) -> Repo {
    let remote_url = git(path, &["remote", "get-url", "origin"]);
    let base = git(
        path,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .or_else(|| git(path, &["symbolic-ref", "--short", "HEAD"]));
    Repo {
        path: path.to_path_buf(),
        remote: remote_url.as_deref().and_then(normalize_remote),
        remote_url,
        last_commit: git(path, &["log", "-1", "--format=%cI"])
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc)),
        base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_remotes() {
        for (url, want) in [
            ("git@github.com:lum1n/acc.git", Some("github.com/lum1n/acc")),
            ("https://github.com/lum1n/acc", Some("github.com/lum1n/acc")),
            (
                "https://user:tok@GitHub.com/lum1n/acc.git/",
                Some("github.com/lum1n/acc"),
            ),
            (
                "ssh://git@gitlab.example.com:2222/a/b/c.git",
                Some("gitlab.example.com/a/b/c"),
            ),
            ("github.com/lum1n/acc", Some("github.com/lum1n/acc")),
            ("/srv/git/repo.git", None),
            ("file:///srv/git/repo.git", None),
            ("../other", None),
        ] {
            assert_eq!(normalize_remote(url).as_deref(), want, "{url}");
        }
    }

    #[test]
    fn matches_hints() {
        let r = Repo {
            path: PathBuf::from("/home/u/repos/acc"),
            remote_url: Some("git@github.com:lum1n/acc.git".into()),
            remote: Some("github.com/lum1n/acc".into()),
            last_commit: None,
            base: None,
        };
        assert!(r.matches("https://github.com/lum1n/acc"));
        assert!(r.matches("github.com/Lum1n/acc"));
        assert!(r.matches("/home/u/repos/acc"));
        assert!(r.matches("acc"));
        assert!(!r.matches("github.com/lum1n/other"));
        assert!(!r.matches(""));
    }

    #[test]
    fn scans_nested_roots() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let inner = dir.path().join("a/inner");
        let b = dir.path().join("group/b");
        let deep = dir.path().join("x/y/z/w");
        for p in [&a, &inner, &b, &deep] {
            std::fs::create_dir_all(p.join(".git")).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("node_modules/n/.git")).unwrap();
        let cfg = ReposCfg {
            roots: vec![dir.path().to_path_buf()],
            max_depth: 2,
            exclude: vec![format!("{}/group/*", dir.path().display())],
            ..ReposCfg::default()
        };
        let found: Vec<_> = scan(&cfg).unwrap().into_iter().map(|r| r.path).collect();
        assert_eq!(found, vec![a, inner]);
    }
}
