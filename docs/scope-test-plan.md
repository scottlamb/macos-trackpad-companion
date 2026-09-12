# Gesture scope — manual test plan

Everything here needs hands and eyes, which is why it isn't in
`cargo test`. Work through it and report by number; "4.2 failed" is
enough to find the problem.

Setup: the companion running from
`~/Applications/Trackpad Companion.app` (launched with `open -n`, never
by running the binary directly — macOS attributes Input Monitoring to
the parent process otherwise), with the PTP pad attached. Open the
scope from the menu-bar icon ▸ **Gesture Scope…**.

---

## 1. The canvas

| | Step | Expect |
| --- | --- | --- |
| 1.1 | One finger on the pad, move it around | A filled dot follows your finger. A small cross marks where it landed, and a line trails behind it |
| 1.2 | Put the finger down near each corner in turn | The dot reaches each corner of the drawn rectangle. If it stops short, or the pad looks the wrong shape, the geometry mapping is wrong |
| 1.3 | Four fingers at once | Four dots, four colours, each labelled with its contact id. **On the test pad four is the maximum** — its firmware fills four of the five contact slots its descriptor advertises, so a fifth finger is not a scope bug (see `known-gaps.md`). On other hardware, expect as many dots as the pad reports |
| 1.4 | Lift everything | Tracks stay on screen, dimmed |
| 1.5 | Touch again | The dimmed tracks vanish as the new touch starts |

## 2. The 2F decision

| | Step | Expect |
| --- | --- | --- |
| 2.1 | Two fingers down, hold still | `2F deciding` in amber, `tap window open` |
| 2.2 | Keep holding past ~150 ms without moving | `tap window open` disappears; still `2F deciding` |
| 2.3 | Scroll | Kind becomes `2F scroll`; the **pan** bar crossed the middle tick; `margin`, `align`/`balance` in green |
| 2.4 | Pinch | Kind becomes `2F pinch+rotate`; the **pinch** bar crossed; `pan` likely shows `disq:margin` in red |
| 2.5 | Compare the frozen **LOCKED →** banner with the newest `2F lock=…` line in `~/Library/Logs/macos-trackpad-companion.log` | Identical numbers, same order. **Any disagreement here is a real bug** |
| 2.6 | Lift after a lock | Banner dims to `last gesture · … · fingers lifted` |
| 2.7 | Pinch slowly with one finger almost still | `pinch/rotate gated: one finger in the noise band` may appear — it is telling you why the lock is waiting |

## 3. The bits nobody has ever seen on screen

These two render paths have never had real input through them — they
are the highest-risk items in this list.

| | Step | Expect |
| --- | --- | --- |
| 3.1 | Three fingers, swipe left or right | Panel switches to `3F swipe`, shows `axis horizontal`, a signed **progress** bar growing from the centre, and a velocity line. An amber arrow tracks the centroid across the pad |
| 3.2 | Three fingers, swipe up or down | Same with `axis vertical` |
| 3.3 | Three fingers down, hold still | `axis not locked` in amber, travel below the 3 mm lock distance |
| 3.4 | Four fingers, swipe | `4F swipe`, otherwise as 3.1 |
| 3.5 | Two fingers down, then press the pad's integrated button | Status line shows `physical drag — gestures suppressed`; the 2F score bars disappear (the engine is not classifying) |
| 3.6 | While the button is held | Every contact dot swells and gains a black hole in the middle |
| 3.7 | Release the button | Dots return to normal size |

## 4. Live tuning

| | Step | Expect |
| --- | --- | --- |
| 4.1 | Drag **Cursor ▸ Speed** and let go, then move one finger | The cursor feels different **immediately** on release — no perceptible delay |
| 4.1a | While still holding the knob | The number tracks, but the pointer does **not** change speed under you — the knob keeps following your finger |
| 4.1b | Click a slider once, then press <kbd>←</kbd> / <kbd>→</kbd> | It nudges, and applies at once. This is the way to tune without dragging at all |
| 4.2 | Wait a second, then `cat ~/.config/macos-trackpad-companion/config.toml` | `cursor.sensitivity` is the new value, and every comment in the file is still there |
| 4.3 | Move one finger slowly, then quickly | The `cursor … mm/s → … px/s` line tracks. *Effective px/mm* rises with speed when Acceleration is above 1.00 |
| 4.4 | Set Acceleration to 1.00 | *Effective px/mm* stops changing with speed and sits on the Speed slider's value |
| 4.5 | Set Acceleration high, then move at roughly the **Accel reference** speed | *Effective px/mm* passes through the Speed slider's value there. This is the property that makes the reference meaningful |
| 4.6 | Scroll with two fingers | A `scroll … mm/s → … px/s` line appears and tracks the same way |
| 4.7 | Open **Settings…** as well, drag a slider there | The scope's matching slider follows within a second |
| 4.8 | Drag a scope slider while the settings window is open | The settings window follows within a second |
| 4.9 | One finger down | The **Cursor** header brightens, **Scroll** stays dim |
| 4.10 | Two-finger scroll | **Scroll** brightens, **Cursor** dims |
| 4.11 | **Reset to Defaults** in the settings window | Both windows return to 25 / 1.00 / 80 / 20 / 1.30 / 60 |

## 5. Closing

| | Step | Expect |
| --- | --- | --- |
| 5.1 | Click **Close** | Window closes; the trackpad keeps working |
| 5.2 | Reopen from the menu | Opens clean — no tracks, no lock banner, sliders on the file's current values |
| 5.3 | Drag a slider and hit **Close** within a quarter-second | The value still reaches the config file |
| 5.4 | Press <kbd>esc</kbd> with the scope focused | Closes, same as **Close** |

## 6. Replay and its transport

Also never driven by anything but the compiler.

```sh
./target/release/replay <a capture file> --scope
```

A capture comes from `companion --record FILE` — quit the companion
first, since only one instance can hold the device.

| | Step | Expect |
| --- | --- | --- |
| 6.1 | Launch as above | The scope opens and starts playing; contacts move; the terminal prints scrolls/pinches as they happen |
| 6.2 | Click **Pause** | Playback stops on a frame. The frame counter stops |
| 6.3 | Click **▶** a few times | Advances one frame per click; the canvas changes by one chip frame |
| 6.4 | Click **◀** a few times | Steps *backwards* correctly — the tracks shorten again rather than glitching |
| 6.5 | Drag the scrubber to the middle, then to the start | The canvas matches that point in the recording. Dragging to the start shows an empty pad |
| 6.6 | Scrub to the same frame twice by different routes | Identical canvas both times. Replays are deterministic; if they differ, that guarantee is broken |
| 6.7 | <kbd>space</kbd> | Plays / pauses |
| 6.8 | <kbd>←</kbd> <kbd>→</kbd> | Step one frame |
| 6.9 | <kbd>shift</kbd> + <kbd>←</kbd> <kbd>→</kbd> | Step ten |
| 6.10 | Let it play to the end, then click **Play** | Restarts from the beginning |
| 6.11 | Note the window | **No tuning column** — a replay has nothing you can feel |
| 6.12 | Close the window | The `replay` process exits |

## 7. Should not happen

| | |
| --- | --- |
| 7.1 | The scope must never write a capture file. Watching is not recording — `--record` is the only thing that captures |
| 7.2 | Opening the scope must not change how gestures behave. Recognition is identical whether it is open or closed |
| 7.3 | The scope must not stutter the pointer. If the cursor gets choppy while it is open, that is a bug |
| 7.4 | No slider in the scope may change a recognition threshold — only speed and acceleration |
