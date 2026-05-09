//! TOML configuration. Loaded once at startup; the only CLI knobs are
//! `--config PATH` (override file location) and `-v` (override
//! `[log].level`). Missing file → all defaults.
//!
//! Default location: `$XDG_CONFIG_HOME/macos-trackpad-companion/config.toml`,
//! falling back to `$HOME/.config/macos-trackpad-companion/config.toml`.
//!
//! See `README.md` for full syntax. Quick reference:
//!
//! ```toml
//! [device]                # optional; omit for any PTP digitizer
//! # vid = 0x1234
//! # pid = 0x5678
//!
//! [log]
//! level = "info"
//! # file  = "~/Library/Logs/macos-trackpad-companion.log"
//!
//! [cursor]
//! sensitivity   = 25.0
//! accel_exponent = 1.0
//! accel_ref     = 80.0
//!
//! [scroll]
//! sensitivity = 20.0
//! natural     = true
//!
//! [gestures.pinch]                 # enable = "on" | "off" |
//! enable = "on"                    #   { only = ["bundle.id", ..] } |
//! [gestures.rotate]                #   { except = ["bundle.id", ..] }
//! enable = "on"
//! [gestures.swipe.horizontal]
//! enable  = "on"
//! backend = "synthetic"            # synthetic | notification | off
//! [gestures.swipe.vertical]
//! enable  = "on"
//! backend = "synthetic"
//!
//! [overlay]                        # debug HUD; off by default
//! enable      = false
//! duration_ms = 600
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub device: Device,
    pub log: Log,
    pub cursor: Cursor,
    pub scroll: Scroll,
    pub gestures: Gestures,
    pub overlay: Overlay,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Overlay {
    pub enable: bool,
    pub duration_ms: u32,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            enable: false,
            duration_ms: 600,
        }
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Device {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Log {
    pub level: String,
    /// If set, logs are appended to this path instead of stderr. A
    /// leading `~/` is expanded against `$HOME`. Parent directories
    /// are created on demand.
    pub file: Option<PathBuf>,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".into(),
            file: None,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Cursor {
    pub sensitivity: f64,
    pub accel_exponent: f64,
    pub accel_ref: f64,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            sensitivity: 25.0,
            accel_exponent: 1.0,
            accel_ref: 80.0,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Scroll {
    pub sensitivity: f64,
    pub natural: bool,
}

impl Default for Scroll {
    fn default() -> Self {
        Self {
            sensitivity: 20.0,
            natural: true,
        }
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Gestures {
    pub pinch: Pinch,
    pub rotate: Rotate,
    pub swipe: Swipe,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Pinch {
    pub enable: GestureEnable,
}

impl Default for Pinch {
    fn default() -> Self {
        Self {
            enable: GestureEnable::On,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Rotate {
    pub enable: GestureEnable,
}

impl Default for Rotate {
    fn default() -> Self {
        Self {
            enable: GestureEnable::On,
        }
    }
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct Swipe {
    pub horizontal: SwipeAxisCfg,
    pub vertical: SwipeAxisCfg,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct SwipeAxisCfg {
    pub enable: GestureEnable,
    pub backend: SwipeBackend,
}

impl Default for SwipeAxisCfg {
    fn default() -> Self {
        Self {
            enable: GestureEnable::On,
            backend: SwipeBackend::Synthetic,
        }
    }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SwipeBackend {
    Synthetic,
    Notification,
    Off,
}

/// Per-gesture enable policy. Polymorphic in TOML so the common case
/// stays terse and the under-cursor filter lives in one key:
///
/// ```toml
/// enable = "on"                                # always
/// enable = "off"                               # never
/// enable = { only   = ["com.apple.Safari"] }   # allowlist by under-cursor app
/// enable = { except = ["com.apple.Terminal"] } # denylist by under-cursor app
/// ```
///
/// Matched against the bundle ID of the application owning the topmost
/// normal window under the cursor at gesture start; that decision is
/// held for the duration of the touch so a mid-gesture window switch
/// can't kill its own gesture. Mirrors how macOS itself dispatches
/// pinch/rotate/scroll/click — to the window under the cursor, not
/// strictly the frontmost app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GestureEnable {
    On,
    Off,
    Only(Vec<String>),
    Except(Vec<String>),
}

impl Default for GestureEnable {
    fn default() -> Self {
        Self::On
    }
}

impl<'de> Deserialize<'de> for GestureEnable {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EnableTable {
            #[serde(default)]
            only: Option<Vec<String>>,
            #[serde(default)]
            except: Option<Vec<String>>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            Table(EnableTable),
        }
        match Repr::deserialize(de)? {
            Repr::Str(s) => match s.as_str() {
                "on" => Ok(Self::On),
                "off" => Ok(Self::Off),
                other => Err(serde::de::Error::custom(format!(
                    "expected \"on\" or \"off\", got \"{other}\""
                ))),
            },
            Repr::Table(EnableTable { only, except }) => match (only, except) {
                (Some(only), None) => Ok(Self::Only(only)),
                (None, Some(except)) => Ok(Self::Except(except)),
                (Some(_), Some(_)) => Err(serde::de::Error::custom(
                    "`only` and `except` are mutually exclusive",
                )),
                (None, None) => Err(serde::de::Error::custom(
                    "expected `only` or `except` in enable table",
                )),
            },
        }
    }
}

/// Expand a leading `~/` (or bare `~`) against `$HOME`. Other path
/// forms pass through untouched.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = match p.to_str() {
        Some(s) => s,
        None => return p.to_path_buf(),
    };
    let home = match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => return p.to_path_buf(),
    };
    if s == "~" {
        home
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        p.to_path_buf()
    }
}

/// Resolve `$XDG_CONFIG_HOME/macos-trackpad-companion/config.toml`,
/// falling back to `$HOME/.config/...` when XDG_CONFIG_HOME is unset
/// (the common case on macOS).
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        });
    base.join("macos-trackpad-companion").join("config.toml")
}

/// Load `path` as TOML, or `default_path()` if `path` is `None`. A
/// missing file resolves to defaults — running with no config at all
/// is a supported mode. Parse errors are returned with file context.
pub fn load(path: Option<&Path>) -> Result<(Config, PathBuf)> {
    let resolved = path.map(PathBuf::from).unwrap_or_else(default_path);
    if !resolved.exists() {
        return Ok((Config::default(), resolved));
    }
    let s = std::fs::read_to_string(&resolved)
        .with_context(|| format!("read config {}", resolved.display()))?;
    let cfg: Config =
        toml::from_str(&s).with_context(|| format!("parse config {}", resolved.display()))?;
    Ok((cfg, resolved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.cursor.sensitivity, 25.0);
        assert_eq!(cfg.scroll.sensitivity, 20.0);
        assert!(cfg.scroll.natural);
        assert_eq!(cfg.gestures.pinch.enable, GestureEnable::On);
        assert_eq!(
            cfg.gestures.swipe.horizontal.backend,
            SwipeBackend::Synthetic
        );
    }

    #[test]
    fn enable_string_forms() {
        let cfg: Config = toml::from_str(
            r#"
            [gestures.pinch]
            enable = "off"
            [gestures.rotate]
            enable = "on"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.gestures.pinch.enable, GestureEnable::Off);
        assert_eq!(cfg.gestures.rotate.enable, GestureEnable::On);
    }

    #[test]
    fn enable_only_table() {
        let cfg: Config = toml::from_str(
            r#"
            [gestures.pinch]
            enable = { only = ["com.apple.Safari", "com.apple.Photos"] }
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.gestures.pinch.enable,
            GestureEnable::Only(vec!["com.apple.Safari".into(), "com.apple.Photos".into()])
        );
    }

    #[test]
    fn enable_except_table() {
        let cfg: Config = toml::from_str(
            r#"
            [gestures.rotate]
            enable = { except = ["com.apple.Terminal"] }
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.gestures.rotate.enable,
            GestureEnable::Except(vec!["com.apple.Terminal".into()])
        );
    }

    #[test]
    fn enable_only_and_except_is_error() {
        let err = toml::from_str::<Config>(
            r#"
            [gestures.pinch]
            enable = { only = ["a"], except = ["b"] }
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn enable_unknown_string_is_error() {
        let err = toml::from_str::<Config>(
            r#"
            [gestures.pinch]
            enable = "maybe"
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("expected \"on\" or \"off\""), "got: {err}");
    }

    #[test]
    fn unknown_top_level_key_rejected() {
        let err = toml::from_str::<Config>(
            r#"
            [misnamed]
            sensitivity = 25.0
        "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "got: {err}");
    }

    #[test]
    fn swipe_backend_parses() {
        let cfg: Config = toml::from_str(
            r#"
            [gestures.swipe.vertical]
            backend = "notification"
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.gestures.swipe.vertical.backend,
            SwipeBackend::Notification
        );
    }

    #[test]
    fn device_hex_literals() {
        let cfg: Config = toml::from_str(
            r#"
            [device]
            vid = 0x1234
            pid = 0x5678
        "#,
        )
        .unwrap();
        assert_eq!(cfg.device.vid, Some(0x1234));
        assert_eq!(cfg.device.pid, Some(0x5678));
    }
}
