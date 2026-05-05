//! Library facade so binaries beyond the main `companion` daemon can
//! reuse the gesture/output stack. Shared between `src/main.rs` (the
//! daemon) and `src/bin/scroll_replay.rs` (the captured-stream
//! playback tool).

pub mod config;
pub mod descriptor;
pub mod gesture;
pub mod hid;
pub mod instance_lock;
pub mod output;
pub mod report;
pub mod scan_clock;
pub mod time;
