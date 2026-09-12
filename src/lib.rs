//! Library facade so binaries beyond the main `companion` daemon can
//! reuse the gesture/output stack. Shared between `src/main.rs` (the
//! daemon) and `src/bin/gesture_tap.rs` (the read-only event tap used
//! to characterize real-trackpad event streams).

pub mod app_context;
pub mod app_kit;
pub mod capture;
pub mod config;
pub mod config_edit;
pub mod config_watch;
pub mod descriptor;
pub mod diagnostics;
pub mod gesture;
pub mod hid;
pub mod instance_lock;
pub mod launch_agent;
pub mod onboarding;
pub mod output;
pub mod overlay;
pub mod pause;
pub mod permissions;
pub mod report;
mod run_loop_timer;
pub mod scan_clock;
pub mod scope;
pub mod settings;
pub mod status_item;
pub mod system_prefs;
pub mod time;
