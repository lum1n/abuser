//! `config.toml`: what subr may spend, where it looks for work and how it runs it.
//!
//! Every field has a default, so an empty file is a valid config. The only
//! thing subr can't guess is where your repositories are (`repos.roots`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Per subscription: `[providers.claude]`, `[providers.codex]`.
    pub providers: BTreeMap<String, ProviderCfg>,
    pub schedule: ScheduleCfg,
    pub repos: ReposCfg,
    pub jobs: JobsCfg,
    pub publish: PublishCfg,
    /// Where todos come from, tried in order.
    pub sources: Vec<SourceCfg>,
    pub tasks: TasksCfg,
    /// Overrides of the built-in presets, or new ones.
    pub presets: BTreeMap<String, PresetCfg>,
    /// Per-repo overrides, keyed by normalised remote (`github.com/o/r`) or path.
    pub repo: BTreeMap<String, RepoOverride>,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert("claude".to_owned(), ProviderCfg::default());
        providers.insert("codex".to_owned(), ProviderCfg::default());
        Self {
            providers,
            schedule: ScheduleCfg::default(),
            repos: ReposCfg::default(),
            jobs: JobsCfg::default(),
            publish: PublishCfg::default(),
            sources: vec![SourceCfg::Native {}],
            tasks: TasksCfg::default(),
            presets: BTreeMap::new(),
            repo: BTreeMap::new(),
        }
    }
}

/// Budget and harness settings for one subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderCfg {
    pub enabled: bool,
    /// subr's own use per day, in percent of the weekly window.
    pub daily_limit_pct: f64,
    /// Don't start a job when the weekly window is above this.
    pub max_weekly_pct: f64,
    /// Don't start a job when the 5-hour window is above this.
    pub max_session_pct: f64,
    /// Daily headroom a job needs to start.
    pub min_job_pct: f64,
    /// Stop a running job this close to the daily limit.
    pub stop_margin_pct: f64,
    /// Model passed to the harness; its own default when unset.
    pub model: Option<String>,
    /// Claude permission mode or Codex permission profile.
    pub permission: Option<String>,
    /// Start the harness through a wrapper (`bwrap ... --`), another
    /// executable or with extra flags.
    pub launcher: Option<acc_core::harness::Launcher>,
    /// Approval rules, answered without asking (acc policy rules), added
    /// to the built-in ones. Requests no rule matches are rejected: nobody
    /// is there to ask.
    pub policy: Vec<acc_core::policy::Rule>,
    /// Start from the built-in rules (reading, builds, tests).
    pub default_policy: bool,
    /// Usage endpoint, for tests and proxies.
    pub usage_url: Option<String>,
    /// Least seconds between usage requests; the endpoints rate-limit.
    pub min_fetch_secs: u64,
}

impl Default for ProviderCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            daily_limit_pct: 100.0 / 7.0,
            max_weekly_pct: 85.0,
            max_session_pct: 60.0,
            min_job_pct: 2.0,
            stop_margin_pct: 1.0,
            model: None,
            permission: None,
            launcher: None,
            policy: Vec::new(),
            default_policy: true,
            usage_url: None,
            min_fetch_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleCfg {
    /// Local time the budget day starts, `HH:MM`.
    pub day_starts: String,
    /// Local time ranges jobs may start in (`22:00-07:00`); any time when empty.
    pub windows: Vec<String>,
    /// Seconds between checks while idle.
    pub tick_secs: u64,
}

impl Default for ScheduleCfg {
    fn default() -> Self {
        Self {
            day_starts: "00:00".to_owned(),
            windows: Vec::new(),
            tick_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReposCfg {
    /// Directories searched for git repositories.
    pub roots: Vec<PathBuf>,
    pub max_depth: usize,
    /// A repo with a commit in this many days counts as active.
    pub active_days: u32,
    /// Globs over repo paths; only matching repos are used when given.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Default for ReposCfg {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_depth: 3,
            active_days: 14,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobsCfg {
    pub timeout_mins: u64,
    /// Tries per task before subr gives up on it.
    pub max_attempts: u32,
    /// Hours before a failed task is tried again.
    pub retry_after_hours: u64,
    /// Stop a job after this long without a fresh quota reading.
    pub stale_kill_after_secs: u64,
    /// Seconds between budget checks while a job runs.
    pub watch_secs: u64,
}

impl Default for JobsCfg {
    fn default() -> Self {
        Self {
            timeout_mins: 45,
            max_attempts: 2,
            retry_after_hours: 24,
            stale_kill_after_secs: 300,
            watch_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PublishCfg {
    /// Push the branch and open a draft PR. Off: keep the branch local.
    pub push: bool,
    pub remote: String,
    pub branch_prefix: String,
    pub label: String,
    /// Skip a repo that already has this many open subr PRs.
    pub max_open_prs: usize,
}

impl Default for PublishCfg {
    fn default() -> Self {
        Self {
            push: true,
            remote: "origin".to_owned(),
            branch_prefix: "subr/".to_owned(),
            label: "subr".to_owned(),
            max_open_prs: 2,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TasksCfg {
    /// Leave tasks whose title contains any of these (case-insensitive),
    /// such as ones only a human can do.
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceCfg {
    /// subr's own queue (`subr add`).
    Native {},
    /// `- [ ]` lines in a file in each repo.
    Markdown {
        #[serde(default = "default_todo_file")]
        file: String,
    },
    /// GitHub issues with a label, through `gh`.
    Github {
        #[serde(default = "default_label")]
        label: String,
    },
    /// An MCP server with Lapp's agent tools (`list_project`, `read_note`,
    /// `complete_todo`, `append_log`, `write_note`) over stdio.
    Lapp {
        #[serde(default = "default_lapp_command")]
        command: Vec<String>,
    },
    /// Any program: `<command> list` prints JSON tasks, `<command> complete
    /// <id>` / `<command> fail <id>` get the result on stdin.
    Command { name: String, command: Vec<String> },
}

fn default_todo_file() -> String {
    "TODO.md".to_owned()
}
fn default_label() -> String {
    "subr".to_owned()
}
fn default_lapp_command() -> Vec<String> {
    vec!["lapp-headless".to_owned(), "mcp".to_owned()]
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PresetCfg {
    pub enabled: Option<bool>,
    /// Prompt text; replaces the built-in one.
    pub prompt: Option<String>,
    pub prompt_file: Option<PathBuf>,
    pub mode: Option<crate::presets::Mode>,
    pub cooldown_days: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoOverride {
    /// Leave this repo alone.
    pub skip: bool,
    /// Presets allowed here; all enabled ones when unset.
    pub presets: Option<Vec<String>>,
    pub push: Option<bool>,
    /// Run in the worktree before pushing; a failure stops the PR.
    pub verify: Option<String>,
    /// Provider to use here (`claude`, `codex`).
    pub provider: Option<String>,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = path.map_or_else(paths::config_file, Path::to_path_buf);
        if !path.exists() {
            return Ok((Self::default(), path));
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        for root in &mut cfg.repos.roots {
            *root = paths::expand_home(root);
        }
        cfg.validate()?;
        Ok((cfg, path))
    }

    pub fn validate(&self) -> Result<()> {
        for (id, p) in &self.providers {
            if crate::quota::ProviderId::parse(id).is_none() {
                bail!("unknown provider {id:?} (known: claude, codex)");
            }
            for (name, v) in [
                ("daily_limit_pct", p.daily_limit_pct),
                ("max_weekly_pct", p.max_weekly_pct),
                ("max_session_pct", p.max_session_pct),
                ("min_job_pct", p.min_job_pct),
                ("stop_margin_pct", p.stop_margin_pct),
            ] {
                if !(0.0..=100.0).contains(&v) {
                    bail!("providers.{id}.{name} must be between 0 and 100, got {v}");
                }
            }
            if p.min_job_pct < p.stop_margin_pct {
                bail!("providers.{id}.min_job_pct must be at least stop_margin_pct");
            }
        }
        crate::budget::parse_hhmm(&self.schedule.day_starts)
            .with_context(|| format!("schedule.day_starts {:?}", self.schedule.day_starts))?;
        for w in &self.schedule.windows {
            crate::budget::parse_window(w).with_context(|| format!("schedule.windows {w:?}"))?;
        }
        for (name, p) in &self.presets {
            if p.prompt.is_none()
                && p.prompt_file.is_none()
                && crate::presets::builtin(name).is_none()
            {
                bail!("preset {name:?} is not built in, so it needs `prompt` or `prompt_file`");
            }
        }
        Ok(())
    }

    /// The override for a repo, by remote first, then by path.
    pub fn repo_override(&self, remote: Option<&str>, path: &Path) -> RepoOverride {
        remote
            .and_then(|r| self.repo.get(r))
            .or_else(|| self.repo.get(&path.display().to_string()))
            .cloned()
            .unwrap_or_default()
    }
}

/// Sets (or with `None`, removes) the setting at the dotted `key` in the
/// config file, keeping its comments and layout. `value` is TOML, or else
/// taken as a string. The file is only written if the result is valid.
pub fn set(path: &Path, key: &str, value: Option<&str>) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let keys = toml_edit::Key::parse(key).with_context(|| format!("bad key {key:?}"))?;
    let Some((last, parents)) = keys.split_last() else {
        bail!("empty key");
    };
    let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
    for k in parents {
        if value.is_none() && !table.contains_key(k.get()) {
            bail!("{key} isn't set");
        }
        let entry = table.entry(k.get()).or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        });
        table = entry
            .as_table_like_mut()
            .with_context(|| format!("{} in {key} isn't a table", k.get()))?;
    }
    match value {
        Some(v) => {
            let v = v
                .parse::<toml_edit::Value>()
                .unwrap_or_else(|_| toml_edit::Value::from(v));
            table.insert(last.get(), toml_edit::value(v));
        }
        None => {
            if table.remove(last.get()).is_none() {
                bail!("{key} isn't set");
            }
        }
    }
    let text = doc.to_string();
    let cfg: Config = toml::from_str(&text).with_context(|| format!("setting {key}"))?;
    cfg.validate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Commented starting point (`subr config example`).
pub const EXAMPLE: &str = include_str!("../config.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_and_empty_configs_are_valid() {
        let cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.sources.len(), 1);
        let empty: Config = toml::from_str("").unwrap();
        empty.validate().unwrap();
        assert!((empty.providers["claude"].daily_limit_pct - 14.2857).abs() < 0.001);
    }

    #[test]
    fn set_keeps_comments_and_checks_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[repos]\nroots = [\"~/src\"] # here\n").unwrap();
        set(&path, "providers.claude.daily_limit_pct", Some("10")).unwrap();
        set(&path, "providers.claude.model", Some("claude-sonnet-5")).unwrap();
        set(&path, r#"repo."github.com/o/r".verify"#, Some("cargo test")).unwrap();
        set(&path, "repos.active_days", Some("7")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine") && text.contains("# here"), "{text}");
        let (cfg, _) = Config::load(Some(&path)).unwrap();
        assert!((cfg.providers["claude"].daily_limit_pct - 10.0).abs() < f64::EPSILON);
        assert_eq!(
            cfg.providers["claude"].model.as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            cfg.repo["github.com/o/r"].verify.as_deref(),
            Some("cargo test")
        );
        assert_eq!(cfg.repos.active_days, 7);

        assert!(set(&path, "providers.claude.daily_limit_pct", Some("140")).is_err());
        assert!(set(&path, "no_such_key", Some("1")).is_err());
        assert!(set(&path, "repos.nope", None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        set(&path, "providers.claude.model", None).unwrap();
        let (cfg, _) = Config::load(Some(&path)).unwrap();
        assert!(cfg.providers["claude"].model.is_none());
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            "[providers.gemini]",
            "[providers.claude]\ndaily_limit_pct = 140",
            "[providers.claude]\nmin_job_pct = 0.5",
            "[schedule]\nwindows = [\"9-5\"]",
            "[presets.mine]\nmode = \"pr\"",
            "unknown_key = 1",
        ] {
            let parsed: Result<Config, _> = toml::from_str(bad);
            assert!(parsed.map_or(true, |c| c.validate().is_err()), "{bad}");
        }
    }
}
