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

macOS

## Build & run

```sh
cd companion
cargo build --release
./target/release/companion -v
```

CLI flags:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--vid HEX` | any | Match only this USB vendor ID. |
| `--pid HEX` | any | Match only this USB product ID. |
| `--accel N` | 25 | Screen pixels per millimeter of finger motion (cursor). |
| `--scroll-accel N` | 20 | Screen pixels per millimeter of finger motion (scroll). |
| `--invert-scroll` | off | Use the legacy "wheel" scroll direction (off → macOS-style natural scrolling). |
| `--no-private-gestures` | off | Disable pinch/rotate/swipe injection. |
| `-v`, `-vv` | info | Increase log level. |

## Permissions

The first run on a fresh macOS install will prompt for two privacy
permissions; without them the companion exits with an actionable error.

- **Input Monitoring** — required to read raw HID input reports from
  the trackpad. macOS surfaces error `0xE00002C5` from `IOHIDManagerOpen`
  if this isn't granted.
- **Accessibility** — required to post synthetic CGEvents (cursor moves,
  clicks, scroll, gestures). Granted via System Settings → Privacy &
  Security → Accessibility.

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
- **Hardware verification is pending.** The companion's gesture state
  machine is unit-tested but not yet validated against a flashed PTP
  device. Smoke-test plan: Photos.app pinch + rotate on a real photo;
  Safari 3-finger swipe between back/forward; Mission Control 4-finger
  swipe up.
- **Two-finger ambiguity is resolved by first-significant-motion lock.**
  Once the centroid moves, the distance changes by 4%, or the angle
  changes by 6°, that mode wins for the duration of the touch. The
  thresholds in `gesture.rs` may need tuning once we have hardware.
