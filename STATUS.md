# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
A Wispr-Flow-class dictation tool for i3/X11: hold M4, speak, release, text lands
at the cursor. Fully local, CPU-only (the 4 GB GTX 1650 stays free), tuned for
dictating Claude prompts and shell commands.

## Now
- v1 feature-complete. All modules landed; daemon starts, loads the model and
  arms the hotkey. ruff + mypy --strict (18 files) + 54 tests green.
- Not yet done end-to-end with a live voice through the real hotkey.
- Next: code review, then a real dictation test.

## Done
- Surveyed Wispr Flow (cloud, no Linux) + Linux alternatives.
- Ran Handy 0.9.6 end-to-end, rejected it, reverted every change, uninstalled.
- Captured 5 real dictation samples in `eval-samples/` (audio gitignored).
- uv project + `sherpa-onnx` 1.13.6 running on CPU.
- Private repo `membranepotential/voice-kb`; Gladia captured as issue #1.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- 20.4 s utterance: **2.12 s** greedy (9.7x RT), 2.38 s `modified_beam_search`
  (8.6x RT). Handy managed 1.37x RT on the same clip and failed at 30 s.
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
- `scripts/eval.py` — the regression harness; `eval-samples/references.json`
  now holds ground truth (all entries `verified: false` until listened to).
- Re-measure decode RTF on an **idle** machine. The 9.7x figure was measured
  idle; every later timing was taken under load average ~19 (a large rustc
  build) and is not trustworthy.
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
