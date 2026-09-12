# macos-trackpad-companion

Userspace bridge from a PTP (Microsoft Precision Touchpad / Windows
Precision Touchpad) HID device to native macOS gesture events. Reads
touch frames from any matched PTP digitizer, runs them through a gesture
state machine, and posts CGEvents — cursor, click, phased smooth scroll,
and (via private CGEvent gesture types) pinch, rotate, 3-finger swipe.

Linux and Windows handle PTP devices natively; this companion exists
because macOS has no built-in PTP consumer. macOS does have similar
support for Apple's own Magic Trackpads, but their driver will only
talk to USB devices using Apple's USB VID.

**Prototype-quality**, vibe-coded. The code is gross. But it works decently
well for me and has served as a base for refining gesture recognition. It has
tests inspired by real usage logs. Eventually I hope to distill what I've
learned into a nice spec and develop a high-quality codebase from it.

## Build & run

The primary artifact is a menu-bar app bundle:

```sh
./scripts/bundle.sh
open target/companion.app
```

The companion runs as an `LSUIElement` agent — no Dock tile, no window,
no app-switcher entry. Its only UI is a menu-bar icon whose menu reports
device state and offers Quit. Quit raises `SIGTERM` at the process, the
same path Ctrl+C takes, so `DeviceState::drop` still reverts the
firmware to mouse mode on the way out.

The plain binary still works for debugging, with logs on stderr:

```sh
cargo build --release
./target/release/companion -v
```

**Permissions are keyed to code identity.** macOS matches TCC grants
against the signature's *designated requirement*, so how the bundle is
signed decides whether Input Monitoring and Accessibility survive a
rebuild. `bundle.sh` picks the best identity it finds — Developer ID
Application, else Apple Development, else ad-hoc — and prints the
resulting requirement:

```
identifier "net.guemez.trackpad-companion" and anchor apple generic
  and certificate leaf[subject.OU] = <TEAMID>
```

That names the bundle id and the signing team, with no cdhash, so the
grants persist across rebuilds. Ad-hoc signing (the fallback when no
certificate is installed) pins the cdhash instead, which changes every
build and re-prompts for both permissions each time.

```sh
SIGN_IDENTITY="Developer ID Application: …" ./scripts/bundle.sh  # override
HARDENED=1 ./scripts/bundle.sh                                   # notarization prep
```

Hardened runtime is opt-in because it strips `get-task-allow`, which
stops `lldb` attaching; it is required for notarization but irrelevant
to TCC. Changing `BUNDLE_ID` resets the grants — it is part of the
requirement.

CLI flags (intentionally tiny — everything else lives in the config file):

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config PATH` | XDG default | TOML config path. See **Configuration** below. |
| `-v`, `-vv` | info | Increase log level. Overrides `[log].level` from the file. |
| `--dump-descriptors` | off | Print every matching device's HID report descriptor and exit. Read-only: no mode switching, no instance lock, output on stderr — safe to run while the companion is running. |

## The menu bar

The companion's only UI is a status-bar icon.

| Item | What it does |
| --- | --- |
| *(status line)* | Device state — waiting, connected, paused, or what's missing. |
| **Pause / Resume** | Suspends synthesis without stopping the daemon. The device stays acquired, so it doesn't go dormant the way it does when nothing is driving it, and resuming is instant. |
| **Settings…** (⌘,) | Sliders and toggles that write the config file. |
| **Gesture Scope…** (⌘G) | Live view of the contacts and the 2F lock decision. See below. |
| **Quit** (⌘Q) | Warns first if it would leave no working pointer. |

Everything else lives in the settings window: **Start at Login**, and a
**Troubleshooting** row with **Permissions…**, **Copy Diagnostics**
(versions, both permission states, the pointer policy, the attached
device's geometry, and the config and log paths, onto the clipboard),
**Reveal Log…**, **Reveal Config File…** and **Reset to Defaults**.

Settings written from that window go through the same file the daemon
watches, so the file stays the single source of truth and the window
can't drift from it. Comments and formatting are preserved. A gesture
whose `enable` holds an app list shows a disabled checkbox — a checkbox
can't express `{ only = [...] }`, and a toggle shouldn't silently
discard one.

## Start at login

**Start at Login** writes `~/Library/LaunchAgents/net.guemez.trackpad-companion.plist`
and loads it. `KeepAlive` is `{ SuccessfulExit = false }` on purpose:
launchd restarts the companion after a *crash*, but leaves a clean quit
alone — quitting from the menu should stay quit until the next login.

That matters more than convenience. A pad on the spec Input Mode path
stops responding the moment nothing is driving it, so an unattended
crash would otherwise leave a dead trackpad until someone noticed.

## Configuration

All tuning lives in a TOML file at
`$XDG_CONFIG_HOME/macos-trackpad-companion/config.toml`, falling back to
`~/.config/macos-trackpad-companion/config.toml` when `XDG_CONFIG_HOME`
is unset. A missing file is fine — defaults take over. Unknown keys are
rejected so typos surface at startup.

```toml
[device]                    # optional — match a specific USB device
# vid = 0x1234              #   (omit either field for any PTP digitizer)
# pid = 0x5678

[log]
level = "info"              # error | warn | info | debug | trace
# file  = "~/Library/Logs/macos-trackpad-companion.log"
                            # if set, logs are appended here instead of stderr;
                            # `~/` is expanded and parent dirs are created.

[cursor]
sensitivity   = 25.0        # px per mm of finger motion at accel_ref
accel_exponent = 1.0        # 1.0 = linear; >1 boosts fast flicks
accel_ref     = 80.0        # mm/s — velocity at which sensitivity is the linear feel

[scroll]
sensitivity    = 20.0       # px per mm at accel_ref
accel_exponent = 1.3        # 1.0 = linear; above it, fast scrolls amplify more
accel_ref      = 60.0       # mm/s at which sensitivity is the plain linear feel
natural        = true       # finger-down → content-down (macOS default since 10.7)

# Each gesture has an `enable` key with three forms:
#   enable = "on"                                  # always
#   enable = "off"                                 # never
#   enable = { only   = ["com.apple.Safari"] }     # frontmost-app allowlist
#   enable = { except = ["com.apple.Terminal"] }   # frontmost-app denylist
# `only` and `except` are mutually exclusive. Bundle IDs are matched
# against the app owning the topmost normal window under the cursor,
# sampled at gesture *start* and held for the rest of the touch (so a
# mid-gesture window switch can't kill its own gesture). Under-cursor
# rather than frontmost because that's how macOS itself routes
# pinch/rotate/scroll/click — Mission Control / Spaces 3F/4F swipes
# are system-wide and ignore window targeting, but the same filter
# still expresses "don't fire this gesture when my cursor is parked
# over Terminal."
#
# To learn the bundle ID for an app, any of these work:
#   osascript -e 'id of app "Safari"'                       # by user-facing name
#   mdls -name kMDItemCFBundleIdentifier -r /Applications/Safari.app
#   lsappinfo info -only bundleid -app Safari               # currently running
#   lsappinfo info -only bundleid `lsappinfo front`         # whatever is frontmost now

[gestures.pinch]
enable = "on"

[gestures.rotate]
enable = "on"

[gestures.swipe.horizontal]   # left/right 3F/4F → Spaces / Full-Screen Apps
enable  = "on"
backend = "synthetic"         # synthetic | notification | off
                              #   (notification is silently `off` on this axis —
                              #    no Dock notification exists for switching spaces)

[gestures.swipe.vertical]     # up/down 3F/4F → Mission Control / App Exposé
enable  = "on"
backend = "synthetic"

[overlay]                     # on-screen HUD naming each gesture as it locks
enable      = false
duration_ms = 600             # how long the flash stays up
```

### Live reload

The config file is watched while the companion runs; edits are picked up
within a second, no restart needed. Cursor and scroll tuning, the
per-gesture `enable` policies, swipe backends and the overlay all apply
immediately.

Two sections are only read at startup — `[device]` and `[log]`. Changing
either is reported in the log as needing a restart rather than being
silently ignored.

A file that fails to parse never disturbs a running daemon: the error is
logged and the previous settings stay in force. (At startup a bad file
is still fatal — unknown keys are rejected so typos surface immediately.)

## Permissions

The first run on a fresh macOS install will prompt for two privacy
permissions. Without them the companion no longer exits — it keeps
running, reports the problem in its menu-bar menu, and retries in the
background (see **Caveats**).

A setup window opens automatically when either grant is missing, shows
live state, and links straight to the right System Settings pane.
Reopen it later from **Setup…** in the menu.

- **Input Monitoring** — required to read raw HID input reports from
  the trackpad. macOS surfaces error `0xE00002C5` from `IOHIDManagerOpen`
  if this isn't granted.

  In practice macOS often declines to *show* the prompt for a background
  agent and records a refusal instead, leaving the app absent from the
  Input Monitoring list — so "Open Settings" lands on a pane with
  nothing to toggle. Add the app by hand with the `+` button; the grant
  then sticks. `TODO(permissions)` in `permissions.rs` records what has
  been tried.
- **Accessibility** — required to post synthetic CGEvents (cursor moves,
  clicks, scroll, gestures). Granted via System Settings → Privacy &
  Security → Accessibility.

  This is the dangerous one to be missing: without it `CGEventPost`
  still *succeeds* and macOS discards the events, so the gesture engine
  runs normally and nothing moves on screen. The companion checks the
  grant explicitly and says so at startup rather than failing silently.

## Reading the logs

When a two-finger gesture locks (the moment the companion commits to
either scroll or pinch+rotate), an `INFO` line records all three
candidate scores and the geometric inputs that drove the choice:

```
2F lock=pinch+rotate scores[pinch=1.56 rot=1.35 pan=0.42 disq:margin] common=0.42mm diff=1.45mm align=0.62 balance=0.45
```

A score `≥ 1.00` means that signal crossed its lock threshold. Pan is
mutually exclusive with pinch+rotate and only wins if it both crosses
*and* dominates; otherwise the pair locks.

Tags after a score say *why* it didn't compete:

| Tag | Meaning |
| --- | --- |
| `disq:margin` | Pan: centroid translation didn't beat differential motion by 20% — most of the motion is asymmetric, not translational. |
| `disq:participation` | Pan: margin OK, but neither finger balance (slower ≥ 30% of faster) nor alignment (motion vectors near-parallel) qualified. |
| `gated:noise` | Pinch/rot: one finger sat in the 0.3–1.0 mm noise band where differential signal is dominated by jitter; lock deferred. |
| `gated:policy` | Pinch/rot: the under-cursor app's `enable` policy blocked this gesture, so the score was zeroed for selection. |

Trailing fields:

- `common` — magnitude of the shared (centroid) translation in mm.
- `diff` — magnitude of the per-finger differential motion in mm.
- `align` — cosine of the angle between the two fingers' motion vectors. ~1.0 = parallel, 0 = perpendicular, <0 = anti-parallel.
- `balance` — slower finger's motion / faster finger's motion. 1.0 = symmetric, 0 = one finger anchored.

For the contrasting case, `2F lock=scroll` uses the same format with
`pan` first.

## The gesture scope

The log line above is the decision after the fact. **Gesture Scope…**
(⌘G in the menu) is the same decision while your fingers are still on
the pad:

- the pad drawn to its real aspect ratio, with each contact, where it
  landed, and the path it has taken since;
- the lever arm between two fingers — its length is the pinch signal,
  its angle is the rotate signal — and the centroid, which is what pan
  and the swipe axis lock on;
- the three normalized scores as bars against their shared 1.0 lock
  threshold, with the raw score as a ghost behind the score the lock
  actually selects on, so a gate that zeroed a signal is visible as the
  gap between them;
- every gate that is deciding the outcome: margin, alignment, balance,
  the noise band, the under-cursor app's policy, and the tap window
  that is the usual reason nothing has happened yet;
- the lock itself, frozen at the frame it fired, in the same field
  order as the log line.

Tracks and the lock banner outlive the gesture on purpose — you look up
after your fingers have left the pad, and a view that cleared on lift
would be blank exactly when you went to read it. Once the fingers are
off, the tracks ghost and the banner dims to *last gesture · … ·
fingers lifted*, so a record of something finished never reads as
something still happening. The next touch clears both.

### Tuning while you gesture

A column down the right-hand side carries the settings you can only
judge by feel, live:

| | |
| --- | --- |
| **Cursor — Speed** | `cursor.sensitivity` |
| **Cursor — Acceleration** | `cursor.accel_exponent` |
| **Cursor — Accel reference** | `cursor.accel_ref` |
| **Scroll — Speed** | `scroll.sensitivity` |
| **Scroll — Acceleration** | `scroll.accel_exponent` |
| **Scroll — Accel reference** | `scroll.accel_ref` |

Letting go of a slider reaches the engine on the spot, and the config
file is written behind it, debounced. That inverts what the settings
window does — it writes the file and lets the watcher apply it —
because going through the file costs a debounce plus a poll, and over a
second between moving a slider and feeling the change makes the slider
useless. The file still ends up authoritative: when the watcher notices
it, it re-applies the identical values.

**On release, not during the drag.** You drag these sliders with the
trackpad they configure, so applying mid-drag moves the pointer doing
the dragging — the knob stops tracking your finger, and at the low end
of Speed it takes several times the travel to drag it back. Nothing is
lost by waiting: you cannot perform the gesture you are trying to judge
while your finger is on the knob. The number under the slider still
tracks the drag, so you can see where you are heading. Clicking a
slider focuses it, and the arrow keys then nudge it without touching
the pad at all — which applies immediately, since no button is held. Editing the file by hand, or from the
settings window, updates the scope's sliders the same way — except
while a drag is in flight, which is the one moment a refresh would yank
the slider out from under you.

The canvas shows what the curve is doing to the speed your hand is
actually producing:

```
cursor    212 mm/s →   1043 px/s    effective  4.9 px/mm   (set 25.0 px/mm at 80 mm/s)
```

`accel_exponent` and `accel_ref` shape a curve you can otherwise only
feel. *Effective px/mm* is in the same unit as the Speed slider, so the
gap between the two numbers is exactly how far acceleration has carried
you from where you set it — and you can watch it pass through the
slider's value as you cross `accel_ref`. Scroll gets the same line
while panning. Both are computed by calling the engine's own
`accelerate_cursor` / `accelerate_scroll`, not a second copy of the
formula.

The section headers brighten for whichever group your current gesture
exercises, because you cannot feel cursor acceleration with two fingers
down. **Close** (or <kbd>esc</kbd>) dismisses the window; a pending
write lands first, so closing mid-drag never loses the value the engine
is already running on.

**Feel only, deliberately.** Nothing here moves a recognition
threshold. A slider on `PAN_BALANCE_MIN` or
`ANCHORED_FINGER_FLOOR_MM` would let you fix the gesture in front of
you while silently breaking one you made yesterday, and with a single
test device there is no way to notice; `docs/known-gaps.md` has the
argument in full. The scope makes those thresholds *visible*, which is
the useful half.

The scope reads a snapshot the engine hands to an optional observer
alongside its event sink. `gesture.rs` has no idea the window exists,
which is what keeps it unit-testable — and the observer declines
frames while the window is closed, so a companion whose scope has
never been opened does no extra work per frame.

### Capture and replay

The same window will render a recording, which is how a trackpad
nobody here owns gets diagnosed:

```sh
companion --record ~/scroll-misfires.txt   # use the pad, then quit
replay ~/scroll-misfires.txt               # what the engine would have done
replay ~/scroll-misfires.txt --scope       # ...and watch it
```

A capture is plain text — one line per frame, contacts in millimetres —
so it survives being pasted into an issue and a human can read what
the pad reported. Replaying it uses no HID, no CGEvents and no
permissions, and is deterministic: the same file produces byte-identical
output every time, which makes a capture a usable regression fixture.

Under `--scope` the transport below the canvas plays, pauses, steps and
scrubs. A misclassification is usually decided in a single frame, and
stepping is how you stop on it:

| Key | |
| --- | --- |
| <kbd>space</kbd> | play / pause |
| <kbd>←</kbd> <kbd>→</kbd> | step one frame |
| <kbd>shift</kbd>+<kbd>←</kbd> <kbd>→</kbd> | step ten |

Seeking backwards rebuilds the engine and replays from the start rather
than trying to undo frames — which is only sound because replays are
deterministic, and is why frame N is the same frame N however you got
there. `--speed N` scales playback; `--pad WxH` overrides the recorded
pad size.

## Wire-format contract

The companion parses the device's HID report descriptor at runtime, so
firmware is free to choose VID/PID, contact count, and physical/logical
coordinate scale. To remain compatible:

- Expose a Digitizer Application Collection at usage page `0x0D`,
  usage `0x05` (Touch Pad).
- Inside it, declare N nested Logical collections of usage page `0x0D`,
  usage `0x22` (Finger). Each finger collection must input these fields,
  in this order, with these sizes:
  - Confidence — Digitizer 0x47 — 1 bit
  - Tip Switch — Digitizer 0x42 — 1 bit
  - 6 bits padding (so the contact-id falls on a byte boundary)
  - Contact Identifier — Digitizer 0x51 — 8 bits
  - X — Generic Desktop 0x30 — 16 bits. Set Logical Max to your
    coordinate space *and* Physical Max + Unit + Unit Exponent so the
    companion can derive mm/pixel. SI Linear cm (Unit `0x11`) and
    English Linear inches (Unit `0x13`) are both supported. Without
    physical units, descriptor parse fails (gesture thresholds and
    cursor sensitivity are expressed in mm).
  - Y — Generic Desktop 0x31 — 16 bits, same
- After the finger collections, declare:
  - Scan Time — Digitizer 0x56 — 16 bits (100 µs ticks per spec)
  - Contact Count — Digitizer 0x54 — 8 bits
  - Button 1 — Button 0x01 — 1 bit (then 7 bits padding)

The companion reads each field at the bit offset the descriptor gives
it, so the per-contact stride is whatever you declare. Optional extras
(Width, Height, Pressure, Azimuth) are skipped rather than rejected —
the reference firmware above packs 6 bytes per contact, while the
third-party pad in `descriptor.rs`'s tests uses 5, and both work
unchanged. `Layout::validate` only checks that the required fields fit
inside the stride.

If a device still doesn't work, run `companion --dump-descriptors`: it
prints every matching device's report descriptor as hex and changes
nothing (no mode switching, and it doesn't need the instance lock, so it
runs alongside the daemon). That hex is enough to reproduce the parse
offline and turn the device into a test case.

The Microsoft "PTPHQA" feature report is needed for Windows certification
but ignored by macOS, so it's optional from the companion's perspective.

## Reference firmware

A working PTP firmware lives at commit `7f3ee1c:firmware/src/main.rs` in
this repo. It produces a composite USB device:

- Interface 0 — boot Mouse (gives macOS a working cursor before the
  companion is running, and is a sane fallback everywhere)
- Interface 1 — PTP digitizer (5 contacts, 65×40 mm, logical 3936×2424,
  PTPHQA blob, all four feature reports)

The companion's unit test
`descriptor::tests::parses_wpt_descriptor` reproduces the bytes that
7f3ee1c emits. That descriptor is the canonical "this works" reference.

Don't put the Mouse collection in the *same* HID interface as the
digitizer — macOS can route by primary usage and bind a different driver
that intercepts cursor before the digitizer becomes visible. Keep them
on separate interfaces.

## Module map

| File | Responsibility |
| --- | --- |
| `descriptor.rs` | Walks a HID report descriptor and extracts the touch-report `Layout` (contact count, X/Y max, field offsets). |
| `report.rs` | Decodes one input-report buffer into a `Frame` of normalized contacts. |
| `gesture.rs` | Pure state machine — classifies 1F/2F/3F/4F gestures, locks 2F mode on first significant motion. Tested without I/O. |
| `output.rs` | macOS event synthesis. Public CGEvent for cursor/click/scroll, private CGEvent type/field IDs for pinch/rotate/swipe. |
| `hid.rs` | IOHIDManager FFI: device matching, descriptor + input-report subscription, run-loop pumping. |
| `app_kit.rs` | Shared `NSApp` bring-up (accessory policy) and the `[NSApp run]` event loop, plus the cross-thread stop. |
| `status_item.rs` | Menu-bar status item and menu, including the guard that refuses to quit silently when it would leave no pointer. |
| `permissions.rs` | Input Monitoring and Accessibility state via the real APIs, plus System Settings deep links. |
| `onboarding.rs` | First-run setup window; opens automatically when either grant is missing. |
| `settings.rs` | Settings window. Writes the config file; never touches the engine directly. |
| `config.rs` | TOML config loading and defaults. Unknown keys are rejected. |
| `config_watch.rs` | Watches the config file and re-applies it live, keeping the previous settings if a file fails to parse. |
| `config_edit.rs` | Format-preserving writes (`toml_edit`), saved atomically, so the UI can't delete your comments. |
| `launch_agent.rs` | Start-at-login LaunchAgent: install, remove, and the crash-only `KeepAlive` policy. |
| `pause.rs` | Suspends synthesis, settling the engine first so nothing is left mid-gesture. |
| `system_prefs.rs` | Reads whether macOS is set to ignore the built-in trackpad while an external pointer is attached. |
| `app_context.rs` | Resolves the bundle ID of the app under the cursor, for the per-gesture `only` / `except` filters. |
| `overlay.rs` | Optional click-through `NSPanel` HUD that flashes the gesture name at lock time. |
| `scope.rs` | The gesture scope window: draws a `gesture::Snapshot`. Reads engine state, never asks it anything. |
| `capture.rs` | Records the device's frame stream as plain text, and reads it back. |
| `scan_clock.rs` | Maps the device's wrapping 100 µs scan-time counter onto host `CLOCK_UPTIME_RAW` timestamps. |
| `time.rs` | Monotonic `Timestamp` shared by the gesture engine and the event synthesizer. |
| `instance_lock.rs` | `flock(2)` single-instance guard — two companions racing one trackpad is destructive. |
| `main.rs` | CLI parsing, logging, wiring. |
| `bin/gesture_tap.rs` | Separate `gesture-tap` binary: read-only event tap that dumps the gesture events macOS routes, for comparing against a real trackpad. |
| `bin/replay.rs` | Separate `replay` binary: feeds a capture back through the same engine offline, printing what would have been emitted or drawing it in the scope. |

## Testing

`cargo test` covers the parser, the gesture engine, the capture format
and the scope's own state — everything that can be checked without a
screen or a hand.

The rest can't be. [docs/scope-test-plan.md](docs/scope-test-plan.md)
is the manual pass for the gesture scope: what to do, what should
happen, numbered so a failure can be reported as "4.2".

## Known gaps

Open problems, unfinished work, and platform behaviour worth knowing
about are collected in [docs/known-gaps.md](docs/known-gaps.md).

## Caveats

- **Private CGEvent gesture types are reverse-engineered.** Pinch,
  rotate, and swipe injection use undocumented CGEvent types (18, 19,
  20, 30, 31) and field IDs (110, 113, 115, 132). These are stable on
  recent macOS versions and used by BetterTouchTool, Karabiner-Elements,
  and similar tools — but they're not in any public Apple header and
  could break on a future macOS update. To turn them off, set
  `enable = "off"` under `[gestures.pinch]` / `[gestures.rotate]` and
  `backend = "off"` under each `[gestures.swipe.*]` axis; cursor /
  click / phased scroll all use public CGEvent APIs and won't be
  affected.
- **Two-finger ambiguity is resolved by first-significant-motion lock.**
  Once the centroid travels 0.4 mm, the inter-finger distance changes
  by 4%, or the angle changes by 4°, that mode wins for the duration of
  the touch. The thresholds live at the top of `gesture.rs`.
- **A failed HID open is not fatal.** The menu-bar icon is the only UI,
  so exiting on `IOHIDManagerOpen` failure would flash it up and tear it
  down before the error could be read. Instead the failure is logged,
  shown in the menu, and retried on a timer. Note that the internal
  Apple trackpad *does* match the digitizer filter — `IOHIDManager`
  matches on `DeviceUsagePairs`, not just `PrimaryUsagePage` — and is
  held seized by the system, which is a common source of
  `kIOReturnExclusiveAccess` (`0xE00002E2`) at startup. It clears on its
  own; the descriptor parse then rejects the device properly.
- **A spec-path device goes dormant when the companion stops.** On the
  standard Input Mode path (no RMK vendor report), reverting the device
  to mouse mode on shutdown is acknowledged but leaves it sending
  nothing at all — not touch reports, not mouse reports. The pointer
  comes back when a companion process acquires it again (just relaunch;
  no replug needed) or when the device is re-enumerated. A 300 ms delay
  between the revert and closing the manager was tried and made no
  difference: the device needs re-acquisition, not time.

  This matters because macOS may be configured to ignore the built-in
  trackpad while an external pointing device is attached (System
  Settings → Accessibility → Pointer Control), in which case quitting
  the companion can leave no working pointer at all. Turning that
  setting off keeps the built-in trackpad as a fallback. Running the
  companion under a `KeepAlive` LaunchAgent also covers the crash case,
  since launchd restarts it and the pad revives.
- **Retries are subject to App Nap.** The retry timer asks for 3 s, but
  a windowless `LSUIElement` agent gets its timers coalesced — observed
  ~9 s in practice. Recovery works, just slower than the interval
  suggests. `NSProcessInfo beginActivityWithOptions` would opt out.
