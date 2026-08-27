# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
A Wispr-Flow-class dictation tool for i3/X11: hold M4, speak, release, text lands
at the cursor. Fully local, CPU-only (the 4 GB GTX 1650 stays free), tuned for
dictating Claude prompts and shell commands.

## Now
- **Works end to end with a live voice.** First real dictation transcribed
  correctly; Ctrl-C, file logging and the M4 hotkey all confirmed working.
- ruff + mypy --strict (24 files) + 81 tests green, incl. 9 e2e tests that
  pin every regression found so far (each verified by reverting the fix).
- `scripts/eval.py` gives repeatable WER + per-error checks + a score sweep.

## Done
- Surveyed Wispr Flow + Linux alternatives; evaluated and rejected Handy 0.9.6
  (rationale in README), reverted every change, uninstalled.
- 5 real dictation samples in `eval-samples/` (audio gitignored).
- v1 built, reviewed, all 10 review findings fixed. Gladia is issue #1.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- Idle + warm, `modified_beam_search`: 5 s -> 0.38 s (13x), 20 s -> 1.19 s
  (**16.8x**), 37 s -> 2.55 s (14.5x). Linear; no long-utterance cliff.
  Handy managed 1.37x and discarded the 37 s clip at its 30 s cap.
- `modified_beam_search` **works** on the TDT checkpoint → hotwords viable.
- `bpe_vocab` is the two-column SentencePiece `.vocab` (piece, log-prob), not
  the protobuf — reconstructable from `tokens.txt` as score = `-index`.
  Verified: `mkir` → `mkdir` at `hotwords_score=1.5`; >3.0 over-biases badly.

## Hard constraints (rationale in README)
- **M4** = evdev `186` (`KEY_F16`) → X keycode `194`, keysym `XF86Launch7`.
  Keysym-based hotkey libs cannot resolve it — read evdev directly.
- **Read `/dev/input/event*` read-only.** No `EVIOCGRAB`, no uinput clones:
  clones inherit the default layout and destroy the per-device `setxkbmap`
  from `keychron-add.sh`.
- **Never synthesise characters** (`xdotool type`/enigo) — rewrites the core X
  keymap. Clipboard + `ctrl+v`: **183 ms vs 3.3 s** for 159 chars.
- **One-shot decode**, never streaming. **CPU only**, `num_threads=6`.
- **Bias vocabulary at decode time**, never fuzzy replacement.
- **Overlay**: borderless, `no_focus` (focus steal aborts transcription).
  Dual 4K: HDMI-1-0 `0,0 3840x2160`, eDP-1 (primary) `3840,0 3840x2160`.
- Remaining gap is technical vocabulary/context (`dir`→`there`), not raw ASR.

## Next
- **Populate `asr.vocabulary`** — it is empty, so `uv`/`pnpm`/`mkdir`/`rm -rf`
  still mis-transcribe. `eval.py --sweep` is the tool for tuning it.
- **Verify `eval-samples/references.json` by listening** and set
  `verified: true`. Until then the aggregate WER is not meaningful -- and the
  Handy baseline column is biased in Handy's favour (partly circular
  references; its worst failure excluded). See docs/evaluation.md.
- Short commands spell out: `cd home` -> `C D home.` (100% WER on 2 words).
- Re-apply the i3 change: comment `bindcode $m4 [con_mark="m4"] focus`
  (`~/.config/i3/i3.d/keybindings.conf:79`). Reverted during cleanup.
- systemd --user unit for autostart.

## Open questions
- Cloud ASR (Gladia) as a second backend — issue #1. Not the default; conflicts
  with the local-processing requirement.
- Does hotword biasing alone close the technical-vocabulary gap, or is an LLM
  cleanup pass still needed? Answer with the eval harness once references exist.

## Decided
- Python + `uv`; overlay-only UI with TOML config; hotwords-only cleanup for v1.
- **PySide6** for the overlay — GTK4 has no `move()`/`set_type_hint()` on X11
  (verified), which disqualifies it for a positioned, non-focusable window.
