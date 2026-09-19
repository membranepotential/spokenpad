# spokenpad — local push-to-talk dictation for Linux/X11

_reconciled: 2026-09-19 @ b3ee6b3_

## Goal
Hold M4 (or latch with shift), speak, and text appears in a floating nvim
without taking focus. Fully local, CPU-only; the GTX 1650 stays free.

## Now
- Nothing in flight. main pushed at b3ee6b3 (+ this reconcile) and the service
  restarted on it 09-19 — next: the user's live check (Next 1).

## Done
- 09-19: Lost words fixed at two causes found by replaying 128 recovery WAVs:
  Parakeet returned "" for 4/18 short speech chunks (now retried without
  trailing silence; also ends the `cd home` Rust/Python mismatch), and speech
  still sounding at key-up in 22/128 (250 ms post-roll). Whole buffer copied
  to `+` after every release. Astra audited, critiqued and reviewed; its
  review found 4 bugs, fixed. 143 lib + 16 e2e (+1 real-model) green, WER
  17.6% unchanged.
- 09-11: Astra review applied and pushed as six logical commits: core/shell
  layout (core imports nothing from shell), notices shown in the winbar in every
  phase, ranked and width-fitted; silence not decoded when the VAD finds no
  speech (Python reference in step); ownership-probe retry that ends a 1-in-10
  test flake (traced with strace to a fork-window race); CLAUDE.md added.
- 09-11: Review pass c039aaa fixed nine live bugs, added the headless e2e suite.
- 09-08/09: progressive commit, recovery WAV, Rust rewrite deployed.
- Checks: 143 lib + 1 bin + 4 CLI + 16 e2e (+1 real-model), 72 Python; WER
  17.6% VAD / 13.9% whole vs Handy 48.7% on five clips.

## Next
1. Live check after restart: short sentences land first time, word endings
   at key-up survive, the clipboard holds the whole buffer, a quick re-press
   and a latched stop behave; plus the 09-11 checks (tap notice, Escape).
2. Decide whether a "no speech detected" notice is wanted for a press the VAD
   judged silent (needs the release result to carry a reason).
3. Investigate the Rust/Python ASR divergence (Known issues) by comparing the
   onnxruntime linked by the Python wheel with the sherpa prebuilt libs.
4. `ruff format` on the three pre-existing unformatted Python files.

## Known issues / open questions
- Rust/Python ASR divergence: `um z E T` vs `um Z S E T` on one clip, and
  punctuation/one token in some progressive commits. Segments and offsets match.
- Post-roll length (250 ms) is unproven live; a mic stalling at key-up only logs.
- First words lost on long dictations: historical (Python runtime), never
  reproduced; the idle-stream repair is the addressed candidate cause.
- Dying input stream: root cause unknown; the watchdog recovers it and a gap
  is now shown to the user. Latched capture ~230 MB RAM/hour, capped at 3600s.
- LLM cleanup / technical vocabulary still open; no fuzzy replacements.

## Decided
- Hard constraints: docs/constraints.md. M4 = evdev 186 (KEY_F16). Rust runtime
  authorized 2026-09-09; Parakeet/Silero, TOML, nvim Lua UI and the tested
  progressive-commit policy stay. No overlay, no paste path.
- Cancel only while recording; a tap under 120ms is discarded with a notice;
  losing the hotkey keyboard ends the recording by decoding; silence is not
  decoded; notices are ranked and shown in the winbar. See docs/decisions.md.
