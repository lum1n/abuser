//! Installing subr as a user service: systemd on Linux, launchd on macOS.

use std::path::Path;

use anyhow::{Context, Result};

fn exe() -> Result<String> {
    Ok(std::env::current_exe()
        .context("finding the subr binary")?
        .display()
        .to_string())
}

/// The login shell's PATH, so the service finds `claude`, `codex`, `gh`.
fn path_env() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned())
}

pub fn systemd_unit(exe: &str, config: Option<&Path>) -> String {
    let config = config
        .map(|c| format!(" --config {}", c.display()))
        .unwrap_or_default();
    format!(
        "[Unit]\n\
         Description=subr: spare AI subscription quota on background work\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe}{config} run\n\
         Environment=PATH={path}\n\
         Restart=on-failure\n\
         RestartSec=60\n\
         # Let a running agent wind down before the stop is forced.\n\
         TimeoutStopSec=60\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        path = path_env(),
    )
}

pub fn launchd_plist(exe: &str, config: Option<&Path>) -> String {
    let mut args = vec![exe.to_owned()];
    if let Some(c) = config {
        args.extend(["--config".to_owned(), c.display().to_string()]);
    }
    args.push("run".to_owned());
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{a}</string>\n"))
        .collect();
    let log = crate::paths::state_dir().join("subr.log");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key><string>dev.subr</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n\
         \x20 <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string></dict>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
         \x20 <key>StandardErrorPath</key><string>{log}</string>\n\
         </dict>\n</plist>\n",
        path = path_env(),
        log = log.display(),
    )
}

pub fn install(print_only: bool, config: Option<&Path>) -> Result<()> {
    let exe = exe()?;
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let (text, path, next) = if cfg!(target_os = "macos") {
        (
            launchd_plist(&exe, config),
            home.join("Library/LaunchAgents/dev.subr.plist"),
            "launchctl unload ~/Library/LaunchAgents/dev.subr.plist 2>/dev/null; launchctl load -w ~/Library/LaunchAgents/dev.subr.plist",
        )
    } else {
        (
            systemd_unit(&exe, config),
            std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), std::path::PathBuf::from)
                .join("systemd/user/subr.service"),
            // `enable --now` leaves a running service on the old binary.
            "systemctl --user daemon-reload && systemctl --user enable subr && systemctl --user restart subr",
        )
    };
    if print_only {
        print!("{text}");
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}\n(re)start it with: {next}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_subr() {
        let u = systemd_unit("/usr/bin/subr", Some(Path::new("/etc/subr.toml")));
        assert!(u.contains("ExecStart=/usr/bin/subr --config /etc/subr.toml run"));
        let p = launchd_plist("/usr/local/bin/subr", None);
        assert!(p.contains("<string>/usr/local/bin/subr</string>\n    <string>run</string>"));
    }
}
