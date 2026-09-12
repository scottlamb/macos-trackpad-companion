//! Start-at-login, as a LaunchAgent.
//!
//! `SMAppService` would be the modern way to register a login item, but
//! it only *starts* the app at login. A LaunchAgent additionally gives
//! `KeepAlive`, and that is the point here: a pad on the spec Input
//! Mode path goes dormant the moment nothing is driving it, so a crash
//! leaves the user with a dead trackpad until they notice and restart.
//! With launchd watching, it comes back in seconds on its own.
//!
//! `KeepAlive` is deliberately `{ SuccessfulExit = false }` rather than
//! plain `true`: restart after a crash, but leave a clean quit alone.
//! Quit from the menu should stay quit until the next login, not be
//! undone a second later by launchd.
//!
//! This pairs with [`crate::instance_lock`] returning a typed
//! `AlreadyRunning` that `main` turns into an exit status of 0 — a
//! duplicate launch must not look like a crash, or the two features
//! together would produce a restart loop.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const LABEL: &str = "net.guemez.trackpad-companion";

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn plist_path() -> Result<PathBuf> {
    Ok(home()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Whether start-at-login is currently configured.
pub fn is_enabled() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

/// Install the agent and start watching immediately.
pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    if !exe.exists() {
        bail!("current executable path does not exist: {}", exe.display());
    }
    // A bundle under target/ is deleted and recreated on every build,
    // which would leave the agent pointing at nothing. Worth saying so
    // rather than silently configuring something fragile.
    if exe.components().any(|c| c.as_os_str() == "target") {
        log::warn!(
            "start-at-login points into a build directory ({}); \
             reinstall from ~/Applications to make it durable",
            exe.display()
        );
    }

    let log_dir = home()?.join("Library/Logs");
    std::fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
    let out = log_dir.join("macos-trackpad-companion.agent.log");

    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&path, plist_contents(&exe, &out))
        .with_context(|| format!("write {}", path.display()))?;

    // bootout first so re-enabling picks up a changed executable path
    // instead of failing with "service already loaded".
    let _ = bootout();
    bootstrap(&path)?;
    log::info!("start-at-login enabled ({})", path.display());
    Ok(())
}

/// Remove the agent. Does not stop the currently running instance.
pub fn disable() -> Result<()> {
    let path = plist_path()?;
    let _ = bootout();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    log::info!("start-at-login disabled");
    Ok(())
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn bootstrap(plist: &Path) -> Result<()> {
    let out = std::process::Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(domain())
        .arg(plist)
        .output()
        .context("run launchctl bootstrap")?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn bootout() -> Result<()> {
    let out = std::process::Command::new("/bin/launchctl")
        .arg("bootout")
        .arg(format!("{}/{}", domain(), LABEL))
        .output()
        .context("run launchctl bootout")?;
    if !out.status.success() {
        bail!(
            "launchctl bootout failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn plist_contents(exe: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<!-- Restart after a crash, but leave a clean quit alone. -->
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<!-- Interactive keeps launchd from applying background throttling,
	     which would delay the HID retry and config-watch timers. -->
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = exe.display(),
        log = log.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_valid_and_says_what_we_mean() {
        let text = plist_contents(
            Path::new("/Applications/Trackpad Companion.app/Contents/MacOS/companion"),
            Path::new("/tmp/agent.log"),
        );
        assert!(text.contains("<string>net.guemez.trackpad-companion</string>"));
        assert!(text.contains("Trackpad Companion.app/Contents/MacOS/companion"));
        // The distinguishing detail: crash-only restart.
        assert!(text.contains("SuccessfulExit"));
        assert!(text.contains("<false/>"));
        assert!(text.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn plist_parses_as_a_property_list() {
        let dir = std::env::temp_dir().join(format!("tpc-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("test.plist");
        std::fs::write(
            &p,
            plist_contents(Path::new("/bin/true"), Path::new("/tmp/a.log")),
        )
        .unwrap();

        let out = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&p)
            .output()
            .expect("run plutil");
        assert!(
            out.status.success(),
            "plutil rejected the generated plist: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
