# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-19 @ b3ee6b3_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now
- control-cli — replace the evdev watcher with a control socket + `spokenpad
  start|stop|toggle|cancel` bound in the WM — agent (worktree) — merge.
- model-fetch — built-in model download (command + first launch), clipboard
  copy configurable, default off — agent (worktree) — merge.
- own-window research — spokenpad draws nvim (--embed) in its own non-focusing
  window, no terminal; X11/Wayland/GNOME options — agent — report to user.

## Done
- 09-21: Review fixes: no spawn on an empty workspace, only loaded WM config
  proves no_focus (sway: main file), unsent appends go to the pending
  passage, no writes behind an editor, greedy+hotwords rejected. README
  rewritten, docs current. Own setup migrated: models in ~/.local/share,
  `mode = "managed"`, unit on graphical-session.target. 172 lib + 19 e2e.
- 09-21: Portability for the public release merged (13 commits): Python removed,
  eval is examples/eval.rs; static sherpa binary; `[asr] family` parakeet /
  whisper / sense_voice; models in $XDG_DATA_HOME; install.sh/fetch-models.sh;
  attach mode (default) + managed i3/sway over native IPC, terminal table.
  169 lib + 5 CLI + 18 e2e (+1 real-model) green.
- 09-21: Preview auto-scroll fixed (it hung below the window from the second
  paragraph on); arrow Up/Down move by screen line like j/k. CLAUDE.md now
  commits verified work unasked. 144 lib + 16 e2e green.
- 09-19: Lost words fixed (empty-chunk retry without trailing silence; 250 ms
  post-roll), found by replaying 128 recovery WAVs. WER 17.6% unchanged.

## Next
1. Live check after restart: short sentences land first time, word endings
   at key-up survive, the clipboard holds the whole buffer, a quick re-press
   and a latched stop behave; plus the 09-11 checks (tap notice, Escape).
2. Decide whether a "no speech detected" notice is wanted for a press the VAD
   judged silent (needs the release result to carry a reason).
3. Whole-buffer path (VAD off) loses words: `cd-home` decodes to "" (the
   empty-chunk retry is VAD-only) and `shell-commands` loses its first
   sentence; WER 18.7% whole vs 17.6% VAD (was documented as 13.9%).

## Known issues / open questions
- Post-roll length (250 ms) is unproven live; a mic stalling at key-up only logs.
- First words lost on long dictations: reproduced 09-21 only on the
  whole-buffer path (Next 3); never seen with the VAD on.
- Dying input stream: root cause unknown; the watchdog recovers it and a gap
  is now shown to the user. Latched capture ~230 MB RAM/hour, capped at 3600s.
- LLM cleanup / technical vocabulary still open; no fuzzy replacements.

## Decided
- Hard constraints: docs/constraints.md. Default hotkey evdev 186 (KEY_F16).
  Rust-only since 09-21; Parakeet/Silero, TOML, nvim Lua UI and the tested
  progressive-commit policy stay. No overlay, no paste path.
- Cancel only while recording; a tap under 120ms is discarded with a notice;
  losing the hotkey keyboard ends the recording by decoding; silence is not
  decoded; notices are ranked and shown in the winbar. See docs/decisions.md.
