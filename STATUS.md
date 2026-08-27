# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
A Wispr-Flow-class dictation tool for i3/X11: hold M4, speak, release, text lands
at the cursor. Fully local, CPU-only (the 4 GB GTX 1650 stays free), tuned for
dictating Claude prompts and shell commands.

## Now
- **Works end to end with a live voice.** First real dictation transcribed
  correctly; Ctrl-C, file logging and the M4 hotkey all confirmed working.
- ruff + mypy --strict (25 files) + 100 tests green, incl. e2e tests pinning
  every regression found so far (each verified by reverting the fix).

## Done
- Evaluated and rejected Handy 0.9.6 (rationale in README); reverted every
  change. 5 real dictation samples in `eval-samples/` (audio gitignored).
- v1 built, reviewed, all 10 review findings fixed. Gladia is issue #1.
- All 5 references verified (2026-08-27); both `handy WER` biases discharged.
- Live overlay preview: bounded 6s tail re-decode every 1100ms, dropped on
  key-up, never injected. 100 tests green.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- Idle + warm, `modified_beam_search`: 20 s -> 1.19 s (**16.8x**), 37 s ->
  2.55 s (14.5x). Linear; no cliff. Handy managed 1.37x and discarded the 37 s.
- WER on 5 verified refs, empty vocabulary: **13.4% vs Handy's 48.7%** -- but
  that gap is entirely the clip Handy dropped. On the other four: 18.4% vs
  **15.8%**. We win on reliability, not yet on accuracy.
- `modified_beam_search` **works** on the TDT checkpoint → hotwords viable.
- `bpe_vocab` is the two-column SentencePiece `.vocab` (piece, log-prob), not
  the protobuf — reconstructable from `tokens.txt` as score = `-index`.
  Verified: `mkir` → `mkdir` at `hotwords_score=1.5`; >3.0 over-biases badly.

## Hard constraints — full rationale in `docs/constraints.md`
Read evdev **read-only** (no `EVIOCGRAB`, no uinput clones); **never synthesise
characters** (no `xdotool type`/enigo) — both destroy the per-device
`setxkbmap` layout. One-shot **committed** decode. CPU only. Bias vocabulary
at decode time, never fuzzy replacement. Overlay must never take focus.
M4 = evdev `186` (`KEY_F16`) → X keycode `194`, keysym `XF86Launch7`, which is
why keysym-based hotkey libraries cannot bind it.

## Next
- **Try the live preview** and report back on feel: is 1100ms too laggy, is
  6s of trailing text the right amount, does release-to-text still feel fast?
- Short commands spell out: `cd home` -> `C D home.` (100% WER; Handy got 0%).
- systemd --user unit for autostart.

## Open questions
- Cloud ASR (Gladia) as a second backend — issue #1. Not the default; conflicts
  with the local-processing requirement.
- Is an LLM cleanup pass worth it? Hotwords are deferred, so the technical
  vocabulary gap (mkdir, udev, `cd home`) is currently unaddressed.

## Decided
- Python + `uv`; overlay-only UI with TOML config.
- **No hotwords in v1** -- plain transcription. Machinery kept and swept-able,
  vocabulary left empty: biasing is too risky on pairs like set/sed, and the
  gap it would close is ~2.6 points. See docs/decisions.md.
- **PySide6** for the overlay — GTK4 has no `move()`/`set_type_hint()` on X11
  (verified), which disqualifies it for a positioned, non-focusable window.
