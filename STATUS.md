# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-19 @ b3ee6b3_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now
- own-window — research done (own X11 window, _NET_WM_USER_TIME=0, nvim
  --embed; Xwayland on Wayland); awaiting the user's go.
- live replay greedy vs beam over 170 captures (scratch, running) — confirms
  the decoder fix; then drop examples/live_replay.rs (untracked).

## Done
- 09-21: Lost tail fixed: Parakeet now decodes greedy by default (beam search
  = sherpa-onnx #3267: "" / "Yeah."; 19 vs 4 empty chunks on 170 captures).
  shell-commands reference corrected. Deployed (b6939db): control socket,
  i3 M4 bindings in keybindings.conf, clipboard on for the user.
- 09-21: Control socket + `spokenpad start|stop|toggle|cancel` (no /dev/input);
  built-in model download; clipboard copy opt-in. 177 lib + 22 e2e green.
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
3. Measure removing the 1 s zero padding (hurt greedy 2/9 on the lost tail;
   0.5 s once made `cd home` empty) with a corpus replay.

## Known issues / open questions
- Post-roll length (250 ms) is unproven live; a mic stalling at key-up only logs.
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
