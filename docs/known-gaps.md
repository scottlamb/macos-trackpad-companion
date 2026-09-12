# Known gaps

Things that are understood but not done, and things that are not
understood. Kept here so they survive outside anyone's memory.

## Device compatibility

### Tuning constants are not derived from the device

`gesture.rs` holds ~17 tuning constants. Three families of them are
device-dependent, and only the fourth is genuinely universal:

| Family | Constants | Status |
| --- | --- | --- |
| Size-scaled | `SWIPE_PROGRESS_REF_MM` 50 | **Done** — now derived from the pad, see below. `SWIPE_AXIS_LOCK_MM` 3 and `PARTIAL_LIFT_REJOIN_DRIFT_MM` 10 are finger-scale, not pad-scale, and left alone |
| Rate-dependent | `DEFAULT_FRAME_DT` 8 ms, `PARTIAL_LIFT_REJOIN_WINDOW` 80 ms | **Done** — the one that mattered was the scroll-velocity EMA weight, since a fixed weight means a different smoothing time constant at 60 Hz than at 125 Hz. It is now `SCROLL_VELOCITY_TAU_SECS`, a duration, with the per-frame weight derived from the measured `dt`. `DEFAULT_FRAME_DT` only covers the first frame of a gesture; every later frame uses the measured delta |
| Noise-scaled | `MOTION_DEAD_ZONE_MM` 0.04, `PAN_LOCK_MM` 0.4, `ANCHORED_FINGER_FLOOR_MM` 0.3, `PHYSICAL_DRAG_SELECT_MM` 0.3, `TAP_MAX_MOVE_MM` 1.0 | Needs new measurement (resting-contact variance) |
| Judgement | `PAN_BALANCE_MIN` 0.3, `PAN_ALIGNMENT_COS_MIN` 0.97, the 1.2 margin | Not device-dependent so much as a trade between two failure modes. Loosening one to admit more scrolls admits more anchored pinches, and with one device the damage is invisible — see the gesture scope's note below on why none of these is a setting |
| Human | `TAP_MAX_DURATION` 150 ms, `PINCH_ROTATE_HYSTERESIS`, `PINCH_LOCK_RATIO`, `ROTATE_LOCK_RAD` | Correctly device-independent |

The table is not exhaustive and never was; it groups the ones whose
provenance is in question.

`SWIPE_PROGRESS_REF_MM` is now derived: a full swipe is
`SWIPE_TRAVEL_FRACTION` (0.6) of the pad's span along that axis, clamped
to 25–120 mm, falling back to the old fixed 50 mm when no device has
reported its size. The fraction is a judgement call and the one number
to change if swipes feel wrong.

Unresolved: which device the original constants were tuned against. 50 mm
suits a pad about 50 mm tall, which points at the reference firmware
(65 x 40 mm), but that is an inference from a code comment rather than
something recorded.

Measured afterwards on the 209 x 119 mm pad, and worth knowing before
tuning this further: **macOS commits a swipe on velocity, not on
progress reaching 1.0.** Across a dozen swipes at 150-300 mm/s, every
one was acted on by macOS and not one reported progress above 0.81 —
including the deliberately long ones, and including under the old fixed
50 mm reference.

So this constant does not decide whether a swipe fires. It decides how
far the Spaces / Mission Control animation tracks the fingers before
committing. Under the old 50 mm value on a 209 mm pad, a 46 mm flick
drove the animation to 92% — the animation outran the hand. Scaling to
the pad makes the tracking proportional, which is the actual
improvement; firing behaviour was unchanged in testing.

A consequence: velocity-driven completion means short fast flicks fire
regardless of distance. Whether that produces accidental triggers in
normal use is untested.

Suggested order: derive the size- and rate-scaled families first
(deterministic, unit-testable against the two descriptors already in the
tests), and treat noise adaptation as a later, separate step.

If noise adaptation is attempted, measure once per device and freeze it;
persist it per vid/pid where it is visible and overridable; log what was
measured and what changed; clamp hard so a bad measurement degrades
instead of breaking; never re-tune mid-gesture. Continuously drifting
thresholds make behaviour irreproducible and every bug report
unfalsifiable.

### Scroll's curve is configurable ✅

`SCROLL_CURVE_EXPONENT` (1.3) and `SCROLL_CURVE_REF_MM_PER_SEC` (60)
were constants while `cursor.accel_exponent` and `cursor.accel_ref`
were settings — the same curve, the same rationale in both comments,
exposed on one side only. A value whose own comment described it as
what "feels typical to the user" is a preference, and preferences
belong in the config.

They are now `scroll.accel_exponent` and `scroll.accel_ref`, with the
curve parameters gathered into an `output::ScrollAccel` shaped exactly
like `gesture::CursorAccel`. Both windows carry all six sliders.

### The descriptor's contact count is not a promise

The test pad's descriptor declares five finger collections, so the
startup line says `5 contacts` — and the pad tracks four. Measured, not
inferred: 766 trace-logged reports with four and five fingers on the
surface, and

- the Contact Count field (byte 28) never read above `4`;
- the fifth 5-byte contact slot was all zeros in *every* report, never
  once populated;
- only contact ids 0-3 ever appeared;
- no report was rejected and no decode failed.

So the firmware allocates five slots in the report and fills at most
four. The decoder surfaced exactly what arrived; what is wrong is the
startup line, which reports the *report layout's capacity* as if it
were the device's capability.

`contact_slots` comes from `finger_blocks.len()` — how many contacts
the report has room for. The number that actually answers "how many
fingers does this pad track" is Contact Count Maximum (usage `0x55`),
a feature report this project still doesn't read (see above: the walker
doesn't record its report id yet). Reading it would let the startup
line and the diagnostics say four, and would say whether this firmware
declares four and over-allocates, or declares five and under-delivers.

Worth knowing before trusting any descriptor-derived capability on
another device.

### Only one real device has ever been tested

There is one third-party pad (vid `0x258a` pid `0x0010`) plus the
reference firmware's descriptor. Building adaptation against a single
device produces something tuned to make *that* device work, untested
elsewhere — arguably more fragile than honest constants, because the
failure hides inside a derivation.

### Capture and replay ✅

`companion --record FILE` writes the frame stream as plain text; the
`replay` binary feeds it back through the same engine with no HID, no
CGEvents and no permissions, printing what would have been emitted along
with the engine's own lock reasoning. Replays are deterministic —
byte-identical across runs — so a capture is a valid regression fixture.

A trackpad's behaviour is now portable: a device nobody here owns can be
diagnosed from a file, and a misclassification can be reproduced as many
times as it takes.

The first real capture is also a worked example of the value. Three
two-finger gestures all locked to pinch+rotate with `align` between
-0.94 and -1.00, where two were believed to be scrolls — which looked
like a misclassification, with swapped contact ids as the obvious
suspect.

Analysing the raw capture directly, independent of the engine, settled
it: contact ids stayed `(0, 1)` throughout with zero discontinuous
jumps over 5 mm, and each finger travelled 38-49 mm in genuinely
opposite directions. All three were pinches. The engine classified
exactly what the hardware reported, and the scrolls simply weren't in
the recording.

The point is not the answer but the cost of getting it: about thirty
seconds against a file, rather than an inconclusive argument about what
someone's fingers did.

### Feature reports are read back ✅

`get_feature_byte` verifies mode switches rather than trusting that
SET_FEATURE succeeding means the device acted. Entering PTP mode is
verified, the shutdown revert is verified, and a device found already in
PTP mode at startup is reported as the sign of a previous run that
exited without reverting.

macOS returns numbered feature reports with the Report ID at the head of
the payload — confirmed against hardware: reading report `0x25` yields
`2500`, id then value.

Still not read: Contact Count Maximum (usage `0x55`), which would
cross-check the descriptor's contact count. The walker doesn't record
its report id yet.

## Permissions

### Input Monitoring never prompts and never self-registers

Observed on macOS 26.6.2 with a Developer ID-signed `LSUIElement`
bundle launched via `open`:

- no dialog ever appeared for the app;
- `IOHIDCheckAccess` went `Unknown` → `Denied` within ~70 ms of
  `IOHIDRequestAccess`, i.e. macOS recorded a refusal without asking;
- the app never appeared in the Input Monitoring list, so "Open
  Settings" landed on a pane with nothing to toggle;
- adding the bundle by hand with `+` worked, and the grant persisted.

`AXIsProcessTrustedWithOptions` (Accessibility) prompted correctly first
try, so this is specific to the HID path. Full notes and the
experiments worth running are in the `TODO(permissions)` comment on
`request_input_monitoring()` in `permissions.rs`.

Current workaround: the setup window tells the user to add the app with
`+`.

## Platform behaviour

### A spec-path device goes dormant when nothing drives it

**Measured, not inferred:** the revert is not merely acknowledged, it is
verified. Reading Input Mode straight back after the write returns
`0x00` — the device really is in mouse mode — and it still sends
nothing. So this is firmware behaviour, not a failed write and not a
race with closing the manager.

Reverting to mouse mode on shutdown is acknowledged, but the device then
sends nothing at all — not touch reports, not mouse reports. It returns
when a companion process acquires it again (relaunching is enough; no
replug needed). A settle delay between the revert and closing the
manager was tried and made no difference: the device needs
re-acquisition, not time.

Mitigations in place: the quit/pause guard, and start-at-login with
crash-only `KeepAlive`.

### App Nap throttles timers ✅

`app_kit::disable_app_nap()` begins an
`NSActivityUserInitiatedAllowingIdleSystemSleep` activity at startup and
holds it for the life of the process. Timers are how this daemon
recovers — retrying a failed HID open, noticing a config change — and a
windowless agent is exactly what App Nap targets (a 3 s interval was
measured firing at ~9 s).

`AllowingIdleSystemSleep` rather than plain `UserInitiated`: a trackpad
daemon has no business keeping the machine awake.
`ProcessType = Interactive` in the LaunchAgent plist covers the same
ground when running under launchd.

### macOS can disable the built-in trackpad

`USBMouseStopsTrackpad` in `com.apple.AppleMultitouchTrackpad` (mirrored
in `com.apple.driver.AppleBluetoothMultitouch.trackpad`), stored as a
number. **Not** `mouseDriverIgnoreTrackpad` under
`com.apple.universalaccess`, which is what the pane's location in System
Settings suggests and what older references describe; that key does not
exist on macOS 26. Read by `system_prefs.rs`.

Finding it took four attempts. What worked was toggling the switch and
looking for which plist changed:
`find ~/Library/Preferences -name "*.plist" -mmin -5`.

## UI

### The settings window is partial ✅ (mostly)

`cursor.accel_ref` is now a control, and the window re-reads the file
when it changes underneath — but never while a write of its own is
pending, which is exactly the mid-drag case where a refresh would yank
a slider out from under the user.

Still not exposed, deliberately:

- `[log]` and `[device]`, which only take effect at startup;
- the `only` / `except` app lists. A checkbox cannot represent
  `{ only = [...] }`, so those gestures show a disabled checkbox rather
  than being flattened to on/off and silently losing the list. A proper
  list editor is the real answer.

### The gesture scope ✅

Lock scores and contact geometry used to be observable only by reading
log lines after the fact. Menu > **Gesture Scope…** draws them live:
contacts and their tracks on a pad drawn to its real aspect ratio, the
three normalized scores as bars against their shared 1.0 threshold,
every gate that is deciding the outcome, and the lock frozen at the
frame it fired.

`gesture.rs` stayed pure. The engine hands a `Snapshot` to an optional
`Observer` alongside `Output`; `scope.rs` renders it, `main.rs` does the
wiring, and the 59 engine tests leave the observer unset. The observer
declines frames while the window is closed, so a companion whose scope
has never been opened pays one `Cell` read per frame and builds no
snapshot at all.

What made it worth doing properly rather than dumping more fields into
the log line: `TwoFingerBaseline::metrics` is now the single
implementation of the decision vector. The lock decision, the log line
and the scope all read the same value. A scope that derived the numbers
a second way would be worthless at exactly the moment it disagreed with
the engine.

`replay FILE --scope` renders a capture through the same window, with a
transport that plays, pauses, steps a frame at a time and scrubs — so a
device nobody here owns can be watched, not just summarised. Seeking
backwards rebuilds the engine and replays from the start rather than
undoing frames, which is sound only because replays are deterministic.

The scope also carries a live-tuning column — the cursor curve
(`sensitivity`, `accel_exponent`, `accel_ref`) and now the scroll curve
to match — applied to the engine on the drag with the config file
written behind it, debounced. A write that fails reverts both the
slider and the engine to what is on disk rather than leaving the engine
running on a value the file never accepted.

Applied on release, not continuously — found by using it. These
sliders are dragged with the trackpad they configure, so a live apply
changes the pointer performing the drag: the knob stops tracking the
finger, and at the low end of Speed it takes several times the travel
to drag back. The general shape of that is worth remembering for
anything else that tunes an input device from a window shown on that
device: the only moments a change can safely take effect are the ones
where the device is not being used to make it. Here that is the instant
you let go, which costs nothing, because you cannot perform the gesture
you are judging while your finger is on the knob. The inverted order (engine first, file second) is the
one place the "file is the single source of truth" rule bends, and it
bends on purpose: routing a slider through the file costs a 0.25 s
debounce plus a 1 s watcher poll, and a slider you feel 1.25 s later is
not a slider you can tune with. The file still converges — the watcher
re-applies the identical values — and both windows round through the
same helpers so neither can produce a value the other wouldn't.

Deliberately *not* exposed: any recognition threshold. Those trade two
failure modes against each other, and the scope only ever shows the
gesture in front of you, never the one a looser gate would have broken.
With one test device that failure is invisible. Making the thresholds
visible is the useful half; making them adjustable would reintroduce
exactly the unfalsifiability this file argues against above.

One narrow race, known and not fixed. Both the settings window and the
scope write *every* slider value on any one slider moving, because the
debounce coalesces a drag without tracking which key changed. With both
windows open, a drag in one within a second of a drag in the other —
before the mtime poll has re-read the file — writes that window's
second-old value over the other's change. It is visible, correctable,
and needs both windows open at once, which the scope exists to make
unnecessary. The real fix is per-key dirty tracking in both flushes;
worth doing only if anyone ever hits it.

Also not done, deliberately: nothing in the scope is recorded. Watching
is not capturing — `--record` already exists for that, and a scope that
quietly wrote files would be a surprise.

The obvious next thing, if recognition tuning is ever picked up again:
a capture records what the pad reported, never what you *meant*. Label
a capture with its intent and the pieces already here — deterministic
replay, one `metrics` implementation, the scope to check each label by
eye — become a corpus you can sweep a threshold across and count
misclassifications on. That is what
`docs/gesture-tuning-ideas.md` #3 asks for when it says "compute
balance distributions for each, look for separation", and it is the
only honest way to settle #3 and #4 with a single device.
