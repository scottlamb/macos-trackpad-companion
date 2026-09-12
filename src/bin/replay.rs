//! Replay a recorded frame stream through the gesture engine.
//!
//! `companion --record FILE` captures what a trackpad reported; this
//! replays it offline and prints what the engine would have done. No
//! HID, no CGEvents, no permissions — so a capture from hardware nobody
//! here owns can still be diagnosed, and a misclassification can be
//! reproduced as many times as it takes.
//!
//! With `--scope` the same replay is drawn in the gesture scope, with
//! a transport under it: play, pause, step a frame at a time, scrub. A
//! misclassification is usually decided in one frame, and that is the
//! frame you want to stop on.
//!
//! Usage: `replay FILE [--pad WxH] [--scope] [--speed N]`

use std::cell::RefCell;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};
use core_foundation_sys::runloop::CFRunLoopTimerRef;
use macos_trackpad_companion::gesture::{CursorAccel, PadGeometry, State};
use macos_trackpad_companion::output::{Config, MouseButton, Output, Phase, SwipeAxis};
use macos_trackpad_companion::report::Frame;
use macos_trackpad_companion::time::Timestamp;
use macos_trackpad_companion::{app_kit, capture, scope};

/// Prints what the engine emits instead of posting it.
#[derive(Default)]
struct Printer {
    /// Cursor moves are by far the most common event; summarised rather
    /// than printed per frame so the interesting events stay visible.
    cursor_px: RefCell<(i64, i64)>,
    cursor_events: RefCell<u64>,
}

impl Printer {
    fn flush_cursor(&self) {
        let n = *self.cursor_events.borrow();
        if n == 0 {
            return;
        }
        let (x, y) = *self.cursor_px.borrow();
        println!("  cursor: {n} moves totalling ({x:+}, {y:+}) px");
        *self.cursor_events.borrow_mut() = 0;
        *self.cursor_px.borrow_mut() = (0, 0);
    }
}

impl Output for Printer {
    fn move_cursor_by(&self, dx_px: i32, dy_px: i32) {
        let mut acc = self.cursor_px.borrow_mut();
        acc.0 += dx_px as i64;
        acc.1 += dy_px as i64;
        *self.cursor_events.borrow_mut() += 1;
    }
    fn click(&self, button: MouseButton) {
        self.flush_cursor();
        println!("  click {button:?}");
    }
    fn set_left_button_held(&self, held: bool) {
        self.flush_cursor();
        println!("  left button {}", if held { "down" } else { "up" });
    }
    fn scroll(&self, dx_mm: f64, dy_mm: f64, phase: Phase) {
        self.flush_cursor();
        println!("  scroll {phase:?} d=({dx_mm:+.2},{dy_mm:+.2})mm");
    }
    fn scroll_inertia(&self, vx: f64, vy: f64) {
        self.flush_cursor();
        println!("  inertia v=({vx:+.0},{vy:+.0})mm/s");
    }
    fn cancel_inertia(&self) -> bool {
        false
    }
    fn pinch(&self, delta: f64, phase: Phase) {
        self.flush_cursor();
        println!("  pinch {phase:?} delta={delta:+.4}");
    }
    fn rotate(&self, delta_degrees: f64, phase: Phase) {
        self.flush_cursor();
        println!("  rotate {phase:?} delta={delta_degrees:+.2}deg");
    }
    fn swipe(&self, axis: SwipeAxis, progress: f64, velocity: f64, phase: Phase) {
        self.flush_cursor();
        println!("  swipe {axis:?} {phase:?} progress={progress:+.3} v={velocity:+.0}mm/s");
    }
    fn set_config(&self, _cfg: Config) {}
}

// ------------------------------------------------------------- player

/// Playback cadence. Finer than any capture's frame rate, so the
/// stream is paced by its own timestamps rather than by this timer.
const TICK_HZ: f64 = 240.0;

/// A capture, the engine it is being fed to, and where in the stream we
/// are.
///
/// Seeking backwards rebuilds the engine and replays from the start
/// rather than trying to undo frames. Replays are deterministic, so
/// frame N reached that way is the same frame N every time — which is
/// the property that makes a scrubber meaningful at all, and it costs
/// microseconds on a capture of any size worth studying.
struct Player {
    frames: Vec<(Timestamp, Frame)>,
    pad: Option<PadGeometry>,
    /// Number of frames fed so far; also the transport position.
    idx: usize,
    playing: bool,
    speed: f64,
    /// Wall clock and capture clock at the moment playback started.
    anchor_wall: f64,
    anchor_ns: u64,
    state: State<Printer>,
    /// Set once the scope window has been seen open, so closing it can
    /// end the process rather than leaving a headless event loop.
    saw_open: bool,
}

thread_local! {
    static PLAYER: RefCell<Option<Player>> = const { RefCell::new(None) };
}

fn with_player<R>(f: impl FnOnce(&mut Player) -> R) -> Option<R> {
    PLAYER.with(|p| p.borrow_mut().as_mut().map(f))
}

impl Player {
    fn new(frames: Vec<(Timestamp, Frame)>, pad: Option<PadGeometry>, speed: f64) -> Self {
        let mut me = Self {
            frames,
            pad,
            idx: 0,
            playing: false,
            speed,
            anchor_wall: 0.0,
            anchor_ns: 0,
            state: State::new(Printer::default(), CursorAccel::default()),
            saw_open: false,
        };
        me.rebuild();
        me
    }

    /// Start over with a fresh engine and a cleared scope.
    fn rebuild(&mut self) {
        let mut state = State::new(Printer::default(), CursorAccel::default());
        state.set_pad_geometry(self.pad);
        state.set_observer(Some(Box::new(scope::Feed)));
        self.state = state;
        self.idx = 0;
        scope::reset();
    }

    /// Feed frames until `target` frames have been processed.
    fn advance_to(&mut self, target: usize) {
        let target = target.min(self.frames.len());
        if target < self.idx {
            self.rebuild();
        }
        if target == self.idx {
            return;
        }
        // Everything between here and the target is being replayed to
        // reconstruct state, not because anyone asked to see it. Only
        // the frame actually landed on gets to log.
        let restore = log::max_level();
        if target - self.idx > 1 {
            log::set_max_level(log::LevelFilter::Off);
        }
        while self.idx < target {
            if self.idx + 1 == target {
                log::set_max_level(restore);
            }
            let (ts, frame) = self.frames[self.idx].clone();
            self.state.on_frame_at(frame, ts);
            self.idx += 1;
        }
        log::set_max_level(restore);
    }

    fn toggle_play(&mut self) {
        if self.playing {
            self.playing = false;
            self.state.output().flush_cursor();
            return;
        }
        if self.idx >= self.frames.len() {
            self.rebuild();
        }
        self.anchor_wall = unsafe { CFAbsoluteTimeGetCurrent() };
        self.anchor_ns = self
            .frames
            .get(self.idx)
            .map(|(t, _)| t.as_nanos())
            .unwrap_or(0);
        self.playing = true;
    }

    fn tick(&mut self) {
        if !self.playing {
            return;
        }
        if self.idx >= self.frames.len() {
            self.playing = false;
            self.state.output().flush_cursor();
            return;
        }
        let elapsed = (unsafe { CFAbsoluteTimeGetCurrent() } - self.anchor_wall) * self.speed;
        let target_ns = self.anchor_ns as f64 + elapsed * 1e9;
        let mut target = self.idx;
        while target < self.frames.len() && (self.frames[target].0.as_nanos() as f64) <= target_ns {
            target += 1;
        }
        self.advance_to(target);
        if self.idx >= self.frames.len() {
            self.playing = false;
            self.state.output().flush_cursor();
        }
    }
}

/// The scope's view of the player.
struct Controls;

impl scope::Transport for Controls {
    fn toggle_play(&self) {
        with_player(|p| p.toggle_play());
    }

    fn step(&self, delta: i64) {
        with_player(|p| {
            p.playing = false;
            let target = (p.idx as i64 + delta).max(0) as usize;
            p.advance_to(target);
            p.state.output().flush_cursor();
        });
    }

    fn seek(&self, frame: usize) {
        with_player(|p| {
            p.playing = false;
            p.advance_to(frame);
            p.state.output().flush_cursor();
        });
    }

    fn position(&self) -> (usize, usize, bool) {
        with_player(|p| (p.idx, p.frames.len(), p.playing)).unwrap_or((0, 0, false))
    }
}

extern "C" fn on_tick(_timer: CFRunLoopTimerRef, _info: *mut std::ffi::c_void) {
    let closed = with_player(|p| {
        p.tick();
        if scope::is_open() {
            p.saw_open = true;
            false
        } else {
            p.saw_open
        }
    })
    .unwrap_or(false);
    // Closing the window is how you quit a scope replay; without this
    // the event loop would keep running with nothing to show.
    if closed {
        app_kit::request_stop();
    }
}

fn main() -> Result<()> {
    // The engine's own reasoning — the 2F lock scores especially — is
    // logged, and it is most of the value of replaying something.
    // RUST_LOG=debug for the per-frame detail.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .format_target(false)
        .init();

    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .context("usage: replay FILE [--pad WxH] [--scope] [--speed N]")?;
    let mut pad_override = None;
    let mut show_scope = false;
    let mut speed = 1.0_f64;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pad" => {
                let spec = args.next().context("--pad needs WxH in millimetres")?;
                let (w, h) = spec.split_once('x').context("--pad wants WxH")?;
                pad_override = Some((w.parse()?, h.parse()?));
            }
            "--scope" => show_scope = true,
            "--speed" => {
                speed = args
                    .next()
                    .context("--speed needs a multiplier")?
                    .parse()
                    .context("--speed wants a number")?;
                if speed <= 0.0 {
                    bail!("--speed must be positive");
                }
            }
            other => bail!("unknown argument {other:?}"),
        }
    }

    let capture = capture::read(&path)?;
    println!("device: {}", capture.device.as_deref().unwrap_or("unknown"));
    let pad = pad_override.or(capture.pad);
    match pad {
        Some((w, h)) => println!("pad: {w:.1}x{h:.1} mm"),
        None => println!("pad: unknown — engine will use its unscaled defaults"),
    }
    println!("frames: {}", capture.frames.len());
    let span = capture
        .frames
        .last()
        .zip(capture.frames.first())
        .map(|((b, _), (a, _))| b.saturating_duration_since(*a))
        .unwrap_or_default();
    println!("duration: {:.2}s", span.as_secs_f64());
    println!("---");

    let geometry = pad.map(|(w, h)| PadGeometry {
        width_mm: w,
        height_mm: h,
    });

    if show_scope {
        return run_scope(capture.frames, geometry, speed);
    }

    let mut state = State::new(Printer::default(), CursorAccel::default());
    state.set_pad_geometry(geometry);
    for (ts, frame) in capture.frames {
        state.on_frame_at(frame, ts);
    }
    state.output().flush_cursor();
    println!("---");
    Ok(())
}

/// Drive the replay from the scope window instead of straight through.
fn run_scope(frames: Vec<(Timestamp, Frame)>, pad: Option<PadGeometry>, speed: f64) -> Result<()> {
    let mtm = objc2::MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("main() must run on the main thread"))?;

    PLAYER.with(|p| *p.borrow_mut() = Some(Player::new(frames, pad, speed)));
    scope::set_transport(Box::new(Controls));
    scope::show(mtm);

    let interval = 1.0 / TICK_HZ;
    let mut ctx = CFRunLoopTimerContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    let fire_at = unsafe { CFAbsoluteTimeGetCurrent() } + interval;
    let timer = CFRunLoopTimer::new(fire_at, interval, 0, 0, on_tick, &mut ctx);
    // Common modes: a menu or a slider drag must not stall playback.
    CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopCommonModes });

    // Start playing straight away — the usual reason to open a replay
    // in the scope is to watch it.
    with_player(|p| p.toggle_play());

    println!("scope open — space plays/pauses, ← → step a frame, shift+← → steps ten");
    app_kit::run_event_loop(mtm);

    unsafe { core_foundation_sys::runloop::CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef()) };
    with_player(|p| p.state.output().flush_cursor());
    println!("---");
    Ok(())
}
