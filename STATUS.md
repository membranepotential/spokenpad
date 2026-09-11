# spokenpad — local push-to-talk dictation for Linux/X11

## Goal
Hold M4 (or latch with shift), speak, and text appears in a floating nvim
without taking focus. Fully local, CPU-only; the GTX 1650 stays free.

## Now
- Review pass 2026-09-11 complete: two fix rounds applied, docs true against
  the code, all checks green — next: user reviews the diff, commits, restarts.

## Done
- Review pass 2026-09-11 fixed nine live bugs: committed text lost on cancel or
  shutdown, permanently stale notices, Escape destroying a finished dictation
  during decode, stale pre-roll, a dead idle stream never repaired, the last
  callback lost at release, an utterance lost to a deleted buffer, append retry
  desyncing the RPC stream, and xrandr called on the headless path.
- Same pass: headless e2e suite (tests/e2e.rs) drives the real daemon loop
  against a synthetic mic and a real nvim; typed CaptureEvents, the Frames
  newtype and a monotone Utterance lifecycle replaced ad-hoc flags; the two Lua
  files merged into src/lua/spokenpad.lua; docs made true against the code.
- Rust rewrite deployed: daemon and recovery run from target/release/spokenpad;
  CPU models ready, callbacks and read-only evdev verified. Python daemon/UI/
  input/recorder removed, offline eval helpers retained. Signed local commits
  dd3495d and a855dbb; not pushed.
- Progressive commit + audio safety net: every capture has a recovery WAV, the
  3600s memory ceiling is visible, missing tails recovered from intact WAVs.
- Rust/Python parity 2026-09-09: all 5 eval WAVs matched; a 102.5s passage
  matched all six commits, offsets and final text (see Known issues).
- First live Rust dictation: 89.3s captured, 2.6s tail decoded in 0.27s, append
  33ms; X11 smoke: exact Unicode save, focus unchanged.
- Editor hardening: retry dedup, RPC deadlines, safe sockets, i3-rule proof.
- 123 lib + 1 bin + 4 CLI + 10 e2e Rust tests and 68 Python tests pass; strict
  clippy/fmt, Ruff and mypy clean. Accuracy: 17.6% WER with VAD, 13.9% without,
  against Handy 0.9.6's 48.7% on the same five verified clips.

## Next
- User live check of the new build. The service still runs the previous release
  binary — started 2026-09-09 13:22, binary rebuilt 2026-09-11 10:56 — so
  `systemctl --user restart spokenpad` comes first. Then: first preview, long
  pauses, scroll-follow, a too-short tap, Escape mid-recording, a latched pass.

## Known issues / open questions
- First words lost on some long dictations: **historical**, seen on the Python
  runtime, never reproduced. The 2026-09-11 idle-stream repair (a dead stream is
  reopened while idle, so the pre-roll is full at the next press) is the
  addressed candidate cause; watch for recurrence.
- Dying input stream: root cause unknown; the watchdog recovers it, and a gap
  during a capture is now reported to the user rather than only logged.
- Short commands error-prone: the `cd home` clip now decodes empty in Rust
  (Python: `C D home.`, same segments; reproduced at b1159ec, so pre-existing).
  VAD splitting costs ~3.7 WER points on five clips.
- LLM cleanup / technical vocabulary open; no fuzzy replacements. Latched
  capture ~230 MB RAM/hour, capped at 3600s; the WAV continues past it.

## Decided
- Hard constraints: docs/constraints.md. M4 = evdev 186 (KEY_F16). Rust runtime
  authorized 2026-09-09; Parakeet/Silero, TOML, nvim Lua UI and the tested
  progressive-commit policy stay. No overlay, no paste path.
- Cancel only while recording; a tap under 120ms is discarded with a notice;
  losing the hotkey keyboard ends the recording by decoding. docs/decisions.md.
