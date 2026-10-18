//! Where subr keeps its config and state.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// `$SUBR_CONFIG`, else `$XDG_CONFIG_HOME/subr/config.toml` (or `~/.config/...`).
pub fn config_file() -> PathBuf {
    if let Some(p) = std::env::var_os("SUBR_CONFIG") {
        return PathBuf::from(p);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(|| home().join(".config"), PathBuf::from)
        .join("subr/config.toml")
}

/// `$SUBR_STATE_DIR`, else `$XDG_STATE_HOME/subr` (or `~/.local/state/subr`).
pub fn state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SUBR_STATE_DIR") {
        return PathBuf::from(p);
    }
    std::env::var_os("XDG_STATE_HOME")
        .map_or_else(|| home().join(".local/state"), PathBuf::from)
        .join("subr")
}

pub fn expand_home(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// acc's worktree directory for `name` (see `acc_core::worktree`).
pub fn acc_worktree(name: &str) -> PathBuf {
    home().join(acc_core::worktree::ROOT).join(name)
}

/// acc's directory for detached session `id` (see `acc_core::detached`).
pub fn acc_session(id: &str) -> PathBuf {
    home().join(acc_core::detached::ROOT).join(id)
}
