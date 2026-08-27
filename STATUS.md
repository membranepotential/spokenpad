# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
A Wispr-Flow-class dictation tool for i3/X11: hold M4, speak, release, text lands
at the cursor. Fully local, CPU-only (the 4 GB GTX 1650 stays free), tuned for
dictating Claude prompts and shell commands.

## Now
- Scaffold + ASR spikes landed. Both plan risks retired (see Measured).
- Next: implement the modules in plan order — `config.py`/`state.py` first.

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

## Findings that constrain the build
- **M4 key** = evdev `186` (`KEY_F16`) → X11 keycode `194`, keysym `XF86Launch7`.
  Keysym-based hotkey libs cannot resolve it. Read evdev directly.
- **Never `EVIOCGRAB` + uinput-clone the keyboards.** Handy did; the clones appear
  as new XInput slaves that inherit the default layout, destroying the per-device
  `setxkbmap -device N -layout us -variant de_se_fi` from `keychron-add.sh`.
  Read `/dev/input/event*` read-only (user is in `input` group).
- **Never inject text via `xdotool type`/enigo.** It rewrites the *core* X keymap
  to synthesise characters, which also wipes the per-device layout.
  Use clipboard + `ctrl+v`: measured **183 ms vs 3.3 s** for 159 chars.
- **Use a one-shot (TDT) model, not a streaming one.** Streaming re-decodes a
  growing buffer (~1.37x real-time, 16 revisions) and silently drops anything
  past a ~30 s cap. `parakeet-tdt-0.6b-v3-Q8_0.gguf` is already in
  `~/.cache/huggingface/hub/`; its text quality was rated good.
- **Overlay** must be borderless, floating, `no_focus` (focus steal aborts an
  in-flight transcription), and positioned for dual 4K:
  HDMI-1-0 `0,0 3840x2160`, eDP-1 (primary) `3840,0 3840x2160`.
- **Fuzzy word-replacement is dangerous** at short lengths (`set`→`sed`,
  `reset`→`rust`). Bias vocabulary at decode time or via an LLM pass instead.
- Remaining accuracy gap is technical vocabulary and context
  (`commands`→`comments`, `dir`→`there`, `rm -rf`→`RMRF`), not raw ASR quality.

## Next
- `config.py` + `state.py` (types first), then `hotkey.py`, `audio.py`, `asr.py`.
- `scripts/fetch_model.py` + `build_hotwords.py` to replace the spikes.
- **Hand-correct `eval-samples/transcripts.json`** — it currently holds Handy's
  *output* (errors included), not ground truth, so it cannot score anything yet.
- Re-apply the i3 change: comment `bindcode $m4 [con_mark="m4"] focus`
  (`~/.config/i3/i3.d/keybindings.conf:79`). Reverted during cleanup.

## Open questions
- Cloud ASR (Gladia) as a second backend — issue #1. Not the default; conflicts
  with the local-processing requirement.
- Does hotword biasing alone close the technical-vocabulary gap, or is an LLM
  cleanup pass still needed? Answer with the eval harness once references exist.

## Decided
- Python + `uv`; overlay-only UI with TOML config; hotwords-only cleanup for v1.
- **PySide6** for the overlay — GTK4 has no `move()`/`set_type_hint()` on X11
  (verified), which disqualifies it for a positioned, non-focusable window.
