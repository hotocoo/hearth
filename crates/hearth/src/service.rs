//! `hearth service`: run hearthd at login, under the platform's own service
//! manager (launchd on macOS, systemd user units on Linux), as this user.
//!
//! The unit names the hearthd next to this `hearth`, and carries this
//! shell's `HEARTH_CONFIG` and `HEARTH_STATE` when they are set, so the
//! service sees the same configuration and state as the commands run here.
//! Nothing runs as root.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(clap::Subcommand)]
enum Action {
    /// Write the unit and start hearthd now and at every login.
    Install,
    /// Stop hearthd and remove the unit.
    Uninstall,
    /// Print the unit that `install` would write, and where.
    Show,
}

/// Reverse-DNS label for launchd; systemd uses the file name.
const LABEL: &str = "io.github.hotocoo.hearthd";

fn hearthd() -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = me.with_file_name(format!("hearthd{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{} not found next to hearth; build or install both binaries",
            path.display()
        ))
    }
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

/// `HEARTH_*` variables of this shell that the service must share.
fn carried_env() -> Vec<(&'static str, String)> {
    ["HEARTH_CONFIG", "HEARTH_STATE"]
        .into_iter()
        .filter_map(|k| Some((k, std::env::var(k).ok().filter(|v| !v.is_empty())?)))
        .collect()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The launchd property list: start at login, keep alive, log to the state
/// directory.
fn launchd_plist(hearthd: &Path, log_dir: &Path, env: &[(&str, String)]) -> String {
    let mut vars = String::new();
    for (k, v) in env {
        let _ = writeln!(
            vars,
            "\t\t<key>{k}</key>\n\t\t<string>{}</string>",
            xml_escape(v)
        );
    }
    let log = |name: &str| xml_escape(&log_dir.join(name).display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
{vars}	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{}</string>
	<key>StandardErrorPath</key>
	<string>{}</string>
</dict>
</plist>
"#,
        xml_escape(&hearthd.display().to_string()),
        log("hearthd.log"),
        log("hearthd.log"),
    )
}

/// Quotes a value for a systemd unit line.
fn systemd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The systemd user unit. `KillMode=process` because hearthd's servers stop
/// through their tethers; systemd killing the whole cgroup at once would cut
/// that short.
fn systemd_unit(hearthd: &Path, env: &[(&str, String)]) -> String {
    let mut vars = String::new();
    for (k, v) in env {
        let _ = writeln!(vars, "Environment={}", systemd_quote(&format!("{k}={v}")));
    }
    format!(
        "[Unit]\nDescription=hearthd: keeps local model servers alive\n\n[Service]\nExecStart={}\n{vars}Restart=on-failure\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
        systemd_quote(&hearthd.display().to_string())
    )
}

struct Unit {
    path: PathBuf,
    text: String,
}

fn unit() -> Result<Unit, String> {
    let hearthd = hearthd()?;
    let env = carried_env();
    if cfg!(target_os = "macos") {
        let log_dir = hearth_core::state::dir().map_err(|e| e.to_string())?;
        Ok(Unit {
            path: home()?
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            text: launchd_plist(&hearthd, &log_dir, &env),
        })
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map_or_else(|| home().map(|h| h.join(".config")), Ok)?;
        Ok(Unit {
            path: base.join("systemd/user/hearthd.service"),
            text: systemd_unit(&hearthd, &env),
        })
    }
}

fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn install(u: &Unit) -> Result<String, String> {
    // The service manager restarts hearthd whenever it exits; one with
    // nothing to keep would exit and be restarted forever.
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    if config.servers.is_empty() {
        return Err(
            "no servers to keep: add [servers.NAME] to your config before installing the service"
                .into(),
        );
    }
    if let Some(dir) = u.path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if cfg!(target_os = "macos") {
        // A previous version must be unloaded before the new one loads.
        let path = u.path.display().to_string();
        let _ = run_tool("launchctl", &["unload", &path]);
        std::fs::write(&u.path, &u.text).map_err(|e| format!("{path}: {e}"))?;
        if let Ok(dir) = hearth_core::state::dir() {
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        run_tool("launchctl", &["load", "-w", &path])?;
    } else {
        std::fs::write(&u.path, &u.text).map_err(|e| format!("{}: {e}", u.path.display()))?;
        run_tool("systemctl", &["--user", "daemon-reload"])?;
        run_tool(
            "systemctl",
            &["--user", "enable", "--now", "hearthd.service"],
        )?;
    }
    Ok(format!(
        "installed {}; hearthd starts now and at every login (hearth status shows it)\n",
        u.path.display()
    ))
}

fn uninstall(u: &Unit) -> Result<String, String> {
    if !u.path.exists() {
        return Ok(format!("nothing installed at {}\n", u.path.display()));
    }
    let path = u.path.display().to_string();
    if cfg!(target_os = "macos") {
        run_tool("launchctl", &["unload", "-w", &path])?;
    } else {
        run_tool(
            "systemctl",
            &["--user", "disable", "--now", "hearthd.service"],
        )?;
    }
    std::fs::remove_file(&u.path).map_err(|e| format!("{path}: {e}"))?;
    Ok(format!("removed {path}; hearthd is stopped\n"))
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let u = unit()?;
    let text = match args.action {
        Action::Show => {
            if json {
                return crate::to_json(&serde_json::json!({ "path": u.path, "unit": u.text }));
            }
            return Ok(format!("# {}\n{}", u.path.display(), u.text));
        }
        Action::Install => install(&u)?,
        Action::Uninstall => uninstall(&u)?,
    };
    if json {
        crate::to_json(&serde_json::json!({ "path": u.path, "result": text.trim() }))
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_quote_what_they_carry() {
        let env = [("HEARTH_CONFIG", "/a b/<c>&\"d\".toml".to_string())];
        let plist = launchd_plist(Path::new("/opt/h/hearthd"), Path::new("/s"), &env);
        assert!(plist.contains("<string>/a b/&lt;c&gt;&amp;&quot;d&quot;.toml</string>"));
        assert!(plist.contains("<string>/s/hearthd.log</string>"));
        let unit = systemd_unit(Path::new("/opt/h b/hearthd"), &env);
        assert!(unit.contains("ExecStart=\"/opt/h b/hearthd\""));
        assert!(unit.contains("Environment=\"HEARTH_CONFIG=/a b/<c>&\\\"d\\\".toml\""));
        assert!(unit.contains("KillMode=process"));
    }
}
