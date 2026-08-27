# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
A Wispr-Flow-class dictation tool for i3/X11: hold M4, speak, release, text lands
at the cursor. Fully local, CPU-only (the 4 GB GTX 1650 stays free), tuned for
dictating Claude prompts and shell commands.

## Now
- Evaluated Handy 0.9.6 (AUR `handy-bin`) as a baseline. Rejected — see Findings.
- All Handy changes reverted; package uninstall pending.
- Next: write the implementation plan.

## Done
- Surveyed Wispr Flow (cloud, no Linux) + Linux alternatives (Handy, whisrs,
  OpenWhispr, Speech Note, nerd-dictation).
- Ran Handy end-to-end; captured 5 real dictation samples in `eval-samples/`.

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
- Plan: architecture, language/stack, overlay toolkit, cleanup-pass design.

## Open questions
- Cleanup layer: local LLM (Ollama) vs Claude API vs none?
- Overlay stack: GTK4, Tauri, or a plain X11 shaped window?
