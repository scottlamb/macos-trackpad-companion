//! The text behind "Copy Diagnostics".
//!
//! Its own module because both the settings window and (historically)
//! the menu want it, and because the list of what matters when
//! something misbehaves is worth keeping in one readable place.

/// Everything worth pasting into a bug report.
pub fn text() -> String {
    let perms = crate::permissions::State::current();
    let os = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    format!(
        "macos-trackpad-companion {version}\n\
         macOS: {os}\n\
         executable: {exe}\n\
         \n\
         input monitoring: {im:?}\n\
         accessibility: {ax}\n\
         ignores built-in trackpad when external present: {ignore}\n\
         built-in trackpad seen: {builtin}\n\
         \n\
         device: {device}\n\
         paused: {paused}\n\
         start at login: {login}\n\
         \n\
         config: {config}\n\
         log: {log}\n\
         \n\
         {settings}",
        version = env!("CARGO_PKG_VERSION"),
        os = os,
        exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into()),
        im = perms.input_monitoring,
        ax = perms.accessibility,
        ignore = crate::system_prefs::builtin_trackpad_ignored(),
        builtin = crate::hid::builtin_trackpad_present(),
        device = crate::hid::device_summary().unwrap_or_else(|| "none attached".into()),
        paused = crate::pause::is_paused(),
        login = crate::launch_agent::is_enabled(),
        config = crate::settings::config_path_display(),
        log = crate::config_watch::log_file_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "stderr".into()),
        settings = settings(),
    )
}

/// The config values themselves, not just the path to them.
///
/// Load-bearing since the gesture scope started changing six of these
/// while you gesture: someone can tune by feel, hit something odd and
/// paste this, and without the values there is no way to tell whether
/// they were running anything like the defaults. Read from the file,
/// which is where they settle — the scope hands a slider to the engine
/// first, but writes it behind and reverts if the write fails.
fn settings() -> String {
    let Some(path) = crate::settings::config_path() else {
        return "settings: no config path registered".into();
    };
    let cfg = match crate::config::load(Some(&path)) {
        Ok((cfg, _)) => cfg,
        Err(e) => return format!("settings: config unreadable: {e:#}"),
    };
    format!(
        "cursor: sensitivity {} exponent {} ref {} mm/s\n\
         scroll: sensitivity {} exponent {} ref {} mm/s natural {}\n\
         gestures: pinch {:?} rotate {:?}\n\
         swipe: horizontal {:?} via {:?}, vertical {:?} via {:?}\n\
         overlay: {}\n\
         device filter: vid {:?} pid {:?}\n\
         log level: {}",
        cfg.cursor.sensitivity,
        cfg.cursor.accel_exponent,
        cfg.cursor.accel_ref,
        cfg.scroll.sensitivity,
        cfg.scroll.accel_exponent,
        cfg.scroll.accel_ref,
        cfg.scroll.natural,
        cfg.gestures.pinch.enable,
        cfg.gestures.rotate.enable,
        cfg.gestures.swipe.horizontal.enable,
        cfg.gestures.swipe.horizontal.backend,
        cfg.gestures.swipe.vertical.enable,
        cfg.gestures.swipe.vertical.backend,
        cfg.overlay.enable,
        cfg.device.vid,
        cfg.device.pid,
        cfg.log.level,
    )
}
