//! Installing abuser as a user service: systemd on Linux, launchd on macOS.

use std::path::Path;

use anyhow::{Context, Result, bail};

fn exe() -> Result<String> {
    Ok(std::env::current_exe()
        .context("finding the abuser binary")?
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
         Description=abuser: spare AI subscription quota on background work\n\
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
    let log = crate::paths::state_dir().join("abuser.log");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key><string>dev.abuser</string>\n\
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

/// How to (re)start the service on this platform.
struct Commands {
    /// Makes the service manager read the new file and start it at login.
    setup: &'static str,
    /// Whether the service runs now.
    active: &'static str,
    /// Starts it, or restarts it on the new binary.
    restart: &'static str,
}

const SYSTEMD: Commands = Commands {
    setup: "systemctl --user daemon-reload && systemctl --user enable -q abuser",
    active: "systemctl --user is-active -q abuser",
    // `enable --now` would leave a running service on the old binary.
    restart: "systemctl --user restart abuser",
};

const LAUNCHD: Commands = Commands {
    setup: "true",
    active: "launchctl list dev.abuser >/dev/null 2>&1",
    restart: "launchctl unload ~/Library/LaunchAgents/dev.abuser.plist 2>/dev/null; \
              launchctl load -w ~/Library/LaunchAgents/dev.abuser.plist",
};

fn sh(script: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .status()
        .is_ok_and(|s| s.success())
}

/// Writes the service file, then starts the service, or restarts it so it
/// runs this binary. A running job isn't cut short: then it says how to
/// restart once the job is done.
pub fn install(print_only: bool, config: Option<&Path>, running_jobs: &[String]) -> Result<()> {
    let exe = exe()?;
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let (text, path, cmds) = if cfg!(target_os = "macos") {
        (
            launchd_plist(&exe, config),
            home.join("Library/LaunchAgents/dev.abuser.plist"),
            LAUNCHD,
        )
    } else {
        (
            systemd_unit(&exe, config),
            std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), std::path::PathBuf::from)
                .join("systemd/user/abuser.service"),
            SYSTEMD,
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
    println!("wrote {}", path.display());
    let manual = format!("{} && {}", cmds.setup, cmds.restart);
    if !sh(cmds.setup) {
        bail!("couldn't register the service; start it yourself with: {manual}");
    }
    let active = sh(cmds.active);
    if active && let Some(job) = running_jobs.first() {
        println!(
            "not restarting: job {job} is running, and a restart would stop it.\n\
             Restart when it's done with: {}",
            cmds.restart
        );
        return Ok(());
    }
    if !sh(cmds.restart) {
        bail!("couldn't start the service; try: {manual}");
    }
    println!(
        "{} abuser {}",
        if active { "restarted" } else { "started" },
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_abuser() {
        let u = systemd_unit("/usr/bin/abuser", Some(Path::new("/etc/abuser.toml")));
        assert!(u.contains("ExecStart=/usr/bin/abuser --config /etc/abuser.toml run"));
        let p = launchd_plist("/usr/local/bin/abuser", None);
        assert!(p.contains("<string>/usr/local/bin/abuser</string>\n    <string>run</string>"));
    }
}
