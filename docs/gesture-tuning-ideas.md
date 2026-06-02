# Gesture tuning ideas

Running list of tunings to explore. Add notes as we collect more log evidence.

## Background

The user's primary trackpad is the SoflePLUS2's integrated TPS65 — small, with the
wrist naturally off-center, leading to systematic finger-velocity asymmetry. The chip
also tends to briefly drop one of two contacts ("partial lift", visible in logs as
`2f → 1f partial lift; suppressing residual click`) several times during a single
physical scroll. Each partial lift currently resets the 2F baseline, so the next
classification frame catches one finger fresh and one finger mid-glide → frequent
pinch+rotate misclassifications when the user meant to scroll.

Tunables and structural ideas below.

## 1. Treat brief partial-lifts as one continuous 2F gesture (top priority) — IMPLEMENTED

Root cause for most pinch-when-meant-scroll cases. Detailed scope below in
[Partial-lift continuation: detailed scope](#partial-lift-continuation-detailed-scope).
First-pass implementation landed: see `TwoFingerRecent`, `capture_partial_lift`,
and `try_restore_partial_lift` in `src/gesture.rs`, plus the four
`partial_lift_rejoin_*` tests. Tuning notes from the implementation:

- Per-finger initial positions are reset to the rejoin frame for both fingers,
  not just the rearriving one. The locked kinds (Pan, PinchAndRotate) don't read
  `initial_a`/`initial_b` in their dispatch paths, and for Unclassified, keeping
  the surviving finger's pre-lift initial made it dominate the lock decision math
  on the rejoin frame.
- For locked-kind resumes, `cancel_inertia` is called on Pan rejoin and Began is
  re-emitted for both pinch+rotate streams. Downstream sees a brief Ended→Began
  pair, but inertia coast during the gap is killed cleanly.
- Cursor emission during the partial-lift gap (the visible jump in #678) is
  suppressed in `dispatch_one` while `two_finger_recent` is live.

Follow-ups worth considering (not yet done):

- **Defer the Ended emit at save time** rather than re-emitting Began on rejoin,
  so downstream sees no gap at all. Cleaner but more stateful — requires emitting
  the deferred Ended on window expiry or on full lift. Worth the complexity if
  apps visibly glitch on the current Ended→Began pair.
- **Tune `PARTIAL_LIFT_REJOIN_WINDOW`** (currently 80 ms) and
  `PARTIAL_LIFT_REJOIN_DRIFT_MM` (currently 10 mm) after collecting more user
  logs. The window must cover real chip drop-outs (~35–55 ms observed) without
  bridging intentional re-grips.
- **Generalize to 3F → 2F → 3F**: the same drop-out pattern probably affects
  three-finger swipes. Not seen in logs yet, but the structure is analogous.

## 2. Use motion-direction sign in pinch scoring — IMPLEMENTED

Real pinches have the two finger-motion vectors pointing roughly opposite
(`cos ≲ 0`). Today the gates only check magnitudes (margin, balance) and a
same-direction alignment threshold (`cos > 0.97`). A misclassified scroll often
sits in the middle: `cos` around +0.5, both fingers heading the same general
direction but not parallel enough to satisfy the pan-alignment gate.

Landed `pinch *= max(0, 1 - alignment)` (clipped linear falloff) on both
`pinch_raw` and `rot_raw`, applied just before the lock-decision crossing
check. Truly-anchored pinches (one finger zero) keep penalty 1.0 because the
code defaults `alignment` to `-1` when either motion vector is zero. Real
anti-parallel pinches (`cos ≤ 0`) are unaffected. For pinch #138, alignment
≈ 0.96 cuts pinch_raw from 1.59 → 0.06 and rot_raw similarly, so the gesture
defers indefinitely until the trailer's motion grows enough to flip
`pan_qualified` true and pan locks instead.

Tuning notes:

- The implementation is intentionally aggressive on the same-direction side
  because high-positive-alignment 2F gestures on the SoflePLUS2 are
  overwhelmingly lazy-trailer scrolls. Two existing tests changed semantics:
  - `anchored_finger_rotate_with_drift_locks_rotate_not_pan`: the original
    test data had the anchor drift in the *same* direction as the sweep
    (cos ≈ 0.74), which now triggers a meaningful penalty. The drift was
    rotated to a roughly orthogonal direction — representative of real chip
    noise, which is directionally random, not biased toward the sweep.
  - `asymmetric_directionally_correlated_motion_does_not_lock_pan`: an
    inherently ambiguous geometry (high alignment, very low balance,
    trailer in the anchored noise band) — structurally indistinguishable
    from a lazy-trailer scroll. The test now asserts that the lock defers
    rather than committing. The defensive "must not lock pan" property is
    preserved.
- New regression test: `slow_scroll_with_lazy_trailer_locks_pan_not_pinch`
  using coordinates lifted directly from log #138.

Follow-ups worth considering:

- **Sharper falloff curve.** Linear `1 - cos` penalizes already at `cos > 0`.
  A piecewise / smoothstep curve with the inflection at e.g. `cos = 0.85`
  would leave low-positive-alignment cases untouched while still squashing
  the near-parallel cases. Worth trying if real anchored-pinch traces with
  small same-direction drift show up in logs as no-longer-locking.

## 3. Lower the balance threshold for off-center grip

`PAN_LOCK` balance gate is 0.3 (slower contact must move ≥30% of faster). With a
wrist offset, one finger consistently moves less than the other on a real scroll.
A 0.2 threshold combined with a tightened margin (e.g. 1.3 instead of 1.2) might
restore the same anchored-pinch protection while admitting more legitimate
asymmetric scrolls.

Test before changing: pull the user's recent scroll-vs-pinch traces, compute
balance distributions for each, look for separation.

## 4. Tighten `ANCHORED_FINGER_FLOOR_MM` from 0.30 → 0.15

The current 0.30 floor lets per-finger displacements right at the boundary
(e.g. #678's 0.3007 mm) pass the noise-band gate as if the finger were committed.
0.15 firmly classifies the 0.15–1.0 mm band as "trailer hasn't decided yet" →
pinch lock deferred.

Smallest, safest change. Doesn't fix the root cause but stops one specific edge
case from misclassifying. Worth doing even if we also do #1.

## 5. Grace period at fresh 2F baselines

Require at least 1–2 frames (or ~30 ms) of 2F observation before any lock decision
can fire. Lets the trailing finger reveal its intent before the algorithm commits.

Cost: every real pinch/rotate also waits 30 ms longer to lock. Probably tolerable
(perceptually subliminal), but quantify before adopting.

## 6. Bias toward prior lock during quick re-grips

If 2F returns within N ms of a 2F→1F transition AND the prior lock was
TwoFingerPan, bias the new lock-decision toward pan (e.g. multiply pan_score by
1.5 or lower pan-qualification gates). Symmetric for pinch+rotate.

This is a softer version of #1. If #1 fully works, this is unneeded; if #1 is too
risky, this is a fallback that doesn't require sharing baseline state across the
1F gap.

## 7. Reconsider cursor emission during partial-lift 1F windows

Currently when a 2F gesture briefly drops to 1F (with `suppress_one_finger_click`
set), the residual finger still drives `dispatch_one` and emits cursor motion
(see log #678: `cursor: emit deferred d=(+2.919,-0.254)mm → (+54,-1)px` mid-scroll).
This is a user-visible cursor jump during what they perceive as a continuous
scroll. Either:

- (a) Suppress cursor emission for some short grace window after a 2f→1f
  partial-lift transition; or
- (b) Fold this into the partial-lift continuation work (#1): when we know we'll
  rejoin the 2F gesture, the 1F gap shouldn't drive anything.

## 8. Logging / instrumentation ideas

- Tag each lock-decision log with the gestures-per-second rate over the prior N
  seconds. High churn (many short 2F windows in a row) is a fingerprint of the
  partial-lift problem.
- Optionally count "this gesture is the Nth in a chain since last full lift" and
  surface it in logs; would make it easy to identify the worst offenders in
  retrospect.

---

## Partial-lift continuation: detailed scope

### Problem recap

Walking through `~/Library/Logs/macos-trackpad-companion.log` lines 1633847–1633965
(pinch #678, 2026-05-27 15:40:40.046–41.409): a single physical scroll gesture
fragments into one scroll lock (#674), three pinch+rotate locks (#675, #676, #677,
#678), and three intervening cursor jumps, because the TPS65 chip reports brief
single-frame contact drop-outs and each one resets the 2F baseline.

At each rejoin, the new baseline catches one finger fresh and the other already
mid-glide → asymmetric per-finger displacements that fail the pan-qualification
gates → pinch wins on raw score.

The fix: thread the prior 2F state through the brief 1F window so the second 2F
window is a continuation, not a new gesture.

### Affected files

Single file: `src/gesture.rs`. No public-API changes; no changes outside the
engine.

### Data model changes

1. **Per-finger initial positions in `TwoFingerBaseline`.** Currently
   `initial_a: (ContactId, (f64, f64))` and `initial_b: (ContactId, (f64, f64))`
   are paired (same moment in time). After a rejoin, the surviving finger keeps
   its original initial position, while the re-arriving finger gets a new initial
   position (its landing spot in the rejoin frame). The data layout already
   supports this — just need to allow them to be seeded at different times.
   Either:
   - Document that "initial" can be per-finger and rewrite `initial_X` for the
     re-arriving contact at rejoin, or
   - Rename to `anchor_a` / `anchor_b` and add a comment.

2. **New engine field: `two_finger_recent: Option<TwoFingerRecent>`.** Captured
   at the moment of a `2f → 1f partial lift` transition. Holds:
   - `baseline: TwoFingerBaseline` (preserved as-is from the previous session),
   - `kind: GestureKind` (one of `TwoFingerUnclassified`, `TwoFingerPan`,
     `TwoFingerPinchAndRotate`),
   - `started_at: Timestamp` (the original 2F start, preserved across the gap),
   - `max_move_sq: f64` (preserved so tap-eligibility math is consistent),
   - `surviving_contact_id: ContactId`,
   - `surviving_pos_at_lift: (f64, f64)`,
   - `lift_time: Timestamp`.

### Transition changes (function `transition` around line 660–688, 730–778)

In the existing `TwoFingerUnclassified | TwoFingerPan | TwoFingerPinchAndRotate`
arms when `new_kind` is `OneFinger`:

- Replace `self.suppress_one_finger_click = true;` with: capture
  `two_finger_recent`, AND set a separate flag for cursor-suppression during the
  gap (or fold cursor-suppression into the existence of `two_finger_recent`).
- The existing `pending_two_finger_tap` path (tap-eligible 2F) is independent;
  leave it as-is — a tap-eligible 2F window is too brief to need continuation
  logic and would interfere with the right-click semantics.

In the existing `OneFinger → TwoFingerUnclassified` path inside `transition`
(line 745–778):

- Before allocating a fresh `TwoFingerBaseline`, check `two_finger_recent`:
  - If present AND `now - lift_time <= REJOIN_WINDOW` (try 50 ms initially) AND
    one of the two new contacts matches `surviving_contact_id` AND that
    contact's current position is within `REJOIN_DRIFT_MM` (try 2 mm) of
    `surviving_pos_at_lift`:
    - Restore `self.kind` to the stashed kind.
    - Restore `self.started_at` and `self.max_move_sq` from the stash.
    - Rebuild `TwoFingerBaseline` from the stash: keep the surviving finger's
      `initial_X` position; overwrite the re-arriving finger's `initial_X` to
      its current landing position. Preserve `pinch_admitted` / `rotate_admitted`.
      Reset `pinch_rot_lock_pending = false`.
    - If the stashed kind was `TwoFingerPan` or `TwoFingerPinchAndRotate`, also
      seed `last_centroid`, `prev_scale`, `prev_angle` from the rejoin frame so
      the first post-rejoin Changed event emits a one-frame delta (matches the
      existing pre-lock invariant at lines 921–926).
    - Clear `two_finger_recent`.
  - Otherwise, take the normal fresh-baseline path and clear `two_finger_recent`
    anyway (don't carry it indefinitely).

- Any transition to `Idle`, `ThreeFingerLive`, `FourFingerLive`, `SwipeLatched`
  also clears `two_finger_recent`.

### Dispatch changes (function `dispatch_one` around line 817–862)

If `two_finger_recent.is_some()` AND we're inside the rejoin window, skip
cursor emission entirely (the residual finger's motion during the partial-lift
gap is part of the 2F gesture, not a 1F cursor intent). Concretely: early-return
before the deferred-motion emit.

This subsumes the existing "cursor jump during partial lift" annoyance noted in
tuning idea #7 above.

### Edge cases to verify with tests

- **ID swap on rejoin.** If the chip assigns a different ID to the re-arriving
  finger, the surviving-finger match should still let us continue (we only
  require the surviving ID to match; the new finger's ID is irrelevant).
- **Both fingers dropped briefly (2F → 0F → 2F).** Don't continue. Today this
  would go 2F → 1F (if asymmetric drop) → Idle → 1F → 2F, or
  2F → Idle → 1F → 2F. Clear `two_finger_recent` on any Idle transition.
- **Stale rejoin (> REJOIN_WINDOW).** Treat as new gesture.
- **Surviving finger moved far during gap.** Drift > REJOIN_DRIFT_MM → treat as
  new gesture (user probably regripped intentionally).
- **Multiple cascading partial-lifts.** Each rejoin re-captures
  `two_finger_recent` for the next gap — no cumulative state needed.
- **Locked-kind continuation: TwoFingerPan rejoining.** No re-classification:
  resume emitting scroll events from the next frame.
- **Locked-kind continuation: TwoFingerPinchAndRotate rejoining.** Same — resume
  pinch/rotate emits with the re-arriving finger anchored at its new position
  (so the first delta isn't a teleport).
- **Unclassified continuation.** Re-enter classification with the carried
  baseline; per-finger displacements accumulate from each finger's individual
  start, so the post-rejoin frame doesn't artificially favor pinch.
- **Pinch+rotate continuation across rejoin where user genuinely *wanted* to
  switch gesture types.** Unlikely within a 50 ms window; if it happens, the
  re-classification path on next gesture (after a full lift) will catch up.

### Test strategy

Add 3–4 tests in `src/gesture.rs` test module:

1. **`partial_lift_rejoin_preserves_scroll_lock`** — simulate 2F → scroll-lock →
   1F (one frame) → 2F. Assert that no new lock-decision log fires and that
   `Phase::Began` for scroll is emitted only once.
2. **`partial_lift_rejoin_during_unclassified_carries_baseline`** — simulate the
   #678 scenario: 2F (1 frame, no lock yet) → 1F (1 frame) → 2F continuing the
   physical motion. Assert that scroll locks (not pinch).
3. **`rejoin_beyond_window_starts_fresh`** — same as above but with the 1F gap
   > REJOIN_WINDOW. Assert fresh baseline behavior.
4. **`rejoin_with_surviving_finger_drift_starts_fresh`** — surviving finger
   moved > REJOIN_DRIFT_MM during the gap. Assert fresh baseline.

### Open questions

- Should `REJOIN_WINDOW` be measured in wall-clock time or in chip frames?
  Frames are more invariant to host-side scheduling jitter, but the dispatch
  loop already uses wall-clock everywhere — go with wall-clock unless tests
  suggest otherwise.
- Does `out.scroll(0.0, 0.0, Phase::Began)` need a corresponding `Ended` for the
  brief gap, or is the continuation truly seamless from the downstream CG event
  stream's perspective? Probably truly seamless — no Began/Ended pair during the
  gap — but verify by reading what `Output::scroll` does on Began.

### Estimated size

~150 LOC of engine changes plus ~150 LOC of tests. Self-contained.

