//! Presets: standing chores for active repos when no task is queued.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, PresetCfg};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Changes go to a branch and a draft PR.
    Pr,
    /// Read only: the summary is the result, filed as a note or report file.
    Report,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub prompt: String,
    pub mode: Mode,
    pub cooldown_days: f64,
}

struct Builtin {
    name: &'static str,
    prompt: &'static str,
    mode: Mode,
    cooldown_days: f64,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "review-recent",
        prompt: include_str!("review-recent.md"),
        mode: Mode::Pr,
        cooldown_days: 7.0,
    },
    Builtin {
        name: "tests",
        prompt: include_str!("tests.md"),
        mode: Mode::Pr,
        cooldown_days: 7.0,
    },
    Builtin {
        name: "lint-fix",
        prompt: include_str!("lint-fix.md"),
        mode: Mode::Pr,
        cooldown_days: 7.0,
    },
    Builtin {
        name: "docs-sync",
        prompt: include_str!("docs-sync.md"),
        mode: Mode::Pr,
        cooldown_days: 14.0,
    },
    Builtin {
        name: "deps-audit",
        prompt: include_str!("deps-audit.md"),
        mode: Mode::Report,
        cooldown_days: 14.0,
    },
];

pub fn builtin(name: &str) -> Option<Preset> {
    BUILTINS.iter().find(|b| b.name == name).map(|b| Preset {
        name: b.name.to_owned(),
        prompt: b.prompt.to_owned(),
        mode: b.mode,
        cooldown_days: b.cooldown_days,
    })
}

/// The enabled presets: built-ins with their overrides, then custom ones.
pub fn all(cfg: &Config, config_dir: &Path) -> Result<Vec<Preset>> {
    let mut out = Vec::new();
    for b in BUILTINS {
        let over = cfg.presets.get(b.name);
        if let Some(p) = resolve(b.name, builtin(b.name), over, config_dir)? {
            out.push(p);
        }
    }
    for (name, over) in &cfg.presets {
        if builtin(name).is_none()
            && let Some(p) = resolve(name, None, Some(over), config_dir)?
        {
            out.push(p);
        }
    }
    Ok(out)
}

fn resolve(
    name: &str,
    base: Option<Preset>,
    over: Option<&PresetCfg>,
    dir: &Path,
) -> Result<Option<Preset>> {
    let Some(over) = over else { return Ok(base) };
    if over.enabled == Some(false) {
        return Ok(None);
    }
    let prompt = match (&over.prompt, &over.prompt_file) {
        (Some(p), _) => p.clone(),
        (None, Some(f)) => {
            let f = dir.join(crate::paths::expand_home(f));
            std::fs::read_to_string(&f)
                .with_context(|| format!("preset {name}: reading {}", f.display()))?
        }
        (None, None) => match &base {
            Some(b) => b.prompt.clone(),
            None => return Ok(None),
        },
    };
    Ok(Some(Preset {
        name: name.to_owned(),
        prompt,
        mode: over
            .mode
            .or(base.as_ref().map(|b| b.mode))
            .unwrap_or(Mode::Pr),
        cooldown_days: over
            .cooldown_days
            .or(base.as_ref().map(|b| b.cooldown_days))
            .unwrap_or(7.0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_and_custom_presets() {
        let mut cfg = Config::default();
        cfg.presets.insert(
            "lint-fix".into(),
            PresetCfg {
                enabled: Some(false),
                ..Default::default()
            },
        );
        cfg.presets.insert(
            "tests".into(),
            PresetCfg {
                cooldown_days: Some(1.0),
                ..Default::default()
            },
        );
        cfg.presets.insert(
            "changelog".into(),
            PresetCfg {
                prompt: Some("Update CHANGELOG.md".into()),
                ..Default::default()
            },
        );
        let all = all(&cfg, Path::new("/")).unwrap();
        let names: Vec<_> = all.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "review-recent",
                "tests",
                "docs-sync",
                "deps-audit",
                "changelog"
            ]
        );
        assert_eq!(all[1].cooldown_days, 1.0);
        assert_eq!(all[3].mode, Mode::Report);
    }
}
