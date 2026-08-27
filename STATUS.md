# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
A Wispr-Flow-class dictation tool for i3/X11: hold M4, speak, release, text lands
at the cursor. Fully local, CPU-only (the 4 GB GTX 1650 stays free), tuned for
dictating Claude prompts and shell commands.

## Now
- **Works end to end with a live voice**, overlay and live preview included.
  Confirmed 2026-08-27: 3 consecutive clean dictations, no dropped audio.
- ruff + mypy --strict (25 files) + 111 tests green, incl. e2e tests pinning
  every regression found so far (each verified by reverting the fix).

## Done
- Evaluated and rejected Handy 0.9.6 (README); reverted every change. v1
  built, reviewed, all 10 findings fixed. Gladia is issue #1.
- All 5 references verified (2026-08-27); both `handy WER` biases discharged.
- Live overlay preview: whole-utterance re-decode, monotonic, adaptive
  cadence, capped at 15s, dropped on key-up, never injected. 111 tests green.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- Warm, idle: 20 s -> 1.19 s (**16.8x**), 37 s -> 2.55 s. Linear; no cliff.
  Handy managed 1.37x and discarded the 37 s clip at its 30 s cap.
- WER on 5 verified refs, empty vocabulary: **13.4% vs Handy's 48.7%** -- but
  that gap is entirely the clip Handy dropped. On the other four: 18.4% vs
  **15.8%**. We win on reliability, not yet on accuracy.
- Live: 11.2s held -> 0.78s decode + 167ms inject = **~0.95s release-to-text**.
  Captured audio runs ~0.3s over the hold, so the 250ms pre-roll works.
- `modified_beam_search` works on the TDT checkpoint, so hotwords are viable
  if ever wanted: `bpe_vocab` is the two-column SentencePiece `.vocab` (piece,
  log-prob), not the protobuf — rebuild from `tokens.txt` as score = `-index`.
  Verified `mkir` → `mkdir` at score 1.5; >3.0 over-biases badly.

## Hard constraints — full rationale in `docs/constraints.md`
Read evdev **read-only** (no `EVIOCGRAB`, no uinput clones); **never synthesise
characters** (no `xdotool type`/enigo) — both destroy the per-device
`setxkbmap` layout. One-shot **committed** decode. CPU only. Bias vocabulary
at decode time, never fuzzy replacement. Overlay must never take focus.
M4 = evdev `186` (`KEY_F16`) → X keycode `194`, keysym `XF86Launch7`, which is
why keysym-based hotkey libraries cannot bind it.

## Next
- systemd --user unit for autostart -- the last thing between this and daily
  use. Everything else below is polish.
- Dying input stream: root cause unknown (3rd occurrence). Watchdogs recover
  it; nobody has explained it.
- `cd home` -> `C D home.` -- short commands spell out (Handy got this right).

## Open questions
- Cloud ASR (Gladia) as a second backend — issue #1. Not the default.
- Is an LLM cleanup pass worth it? Hotwords are deferred, so the technical
  vocabulary gap (mkdir, udev, `cd home`) is currently unaddressed.

## Decided
- Python + `uv`; overlay-only UI with TOML config.
- **No hotwords in v1** -- plain transcription. Machinery kept and swept-able,
  vocabulary left empty: biasing is too risky on pairs like set/sed, and the
  gap it would close is ~2.6 points. See docs/decisions.md.
- **PySide6** for the overlay — GTK4 has no `move()`/`set_type_hint()` on X11
  (verified), which disqualifies it for a positioned, non-focusable window.
