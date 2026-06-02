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

```sh
cd companion
cargo build --release
./target/release/companion -v
```

CLI flags (intentionally tiny — everything else lives in the config file):

| Flag | Default | Meaning |
| --- | --- | --- |
| `--config PATH` | XDG default | TOML config path. See **Configuration** below. |
| `-v`, `-vv` | info | Increase log level. Overrides `[log].level` from the file. |

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
sensitivity = 20.0          # px per mm
natural     = true          # finger-down → content-down (macOS default since 10.7)

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
```

## Permissions

The first run on a fresh macOS install will prompt for two privacy
permissions; without them the companion exits with an actionable error.

- **Input Monitoring** — required to read raw HID input reports from
  the trackpad. macOS surfaces error `0xE00002C5` from `IOHIDManagerOpen`
  if this isn't granted.
- **Accessibility** — required to post synthetic CGEvents (cursor moves,
  clicks, scroll, gestures). Granted via System Settings → Privacy &
  Security → Accessibility.

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

This produces a **6-byte-per-contact** layout. The companion's
`Layout::validate` rejects anything else; if you change the per-contact
field set, update both ends.

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
| `main.rs` | CLI parsing, logging, wiring. |

## Caveats

- **Private CGEvent gesture types are reverse-engineered.** Pinch,
  rotate, and swipe injection use undocumented CGEvent types (18, 19,
  20, 30, 31) and field IDs (110, 113, 115, 132). These are stable on
  recent macOS versions and used by BetterTouchTool, Karabiner-Elements,
  and similar tools — but they're not in any public Apple header and
  could break on a future macOS update. Pass `--no-private-gestures` to
  disable them; cursor / click / phased scroll all use public CGEvent
  APIs and won't be affected.
- **Two-finger ambiguity is resolved by first-significant-motion lock.**
  Once the centroid moves, the distance changes by 4%, or the angle
  changes by 6°, that mode wins for the duration of the touch. The
  thresholds in `gesture.rs` may need tuning once we have hardware.
