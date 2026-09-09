# spokenpad — local push-to-talk dictation for Linux/X11

## Goal
Hold M4 (or latch with shift), speak, and text appears in a floating nvim
without taking focus. Fully local, CPU-only; the GTX 1650 stays free.

## Now
- Implementation complete and deployed; awaiting user live recording check.

## Done
- Signed local commits: Rust runtime dd3495d; Python cleanup a855dbb; not pushed.
- Removed Python daemon/UI/input/recorder and obsolete tests/dependencies;
  offline helpers/CLIs retained and independently reviewed; originals remain in Git.
- Bundled Neovim + Tokyonight active; no LazyVim, startup and service ready.
- First preview, grey styling, and long wrapped-tail scrolling fixed and deployed.
- Missing tail recovered from intact 17s WAV, direct and progressive paths:
  split long pauses and append 1s synthetic silence only to decoder input.
  Local aggregate VAD WER unchanged; per-clip tradeoff documented in docs/rust.md.
- Startup waits for nonce-specific post-VimEnter readiness; delayed startup
  regression, independent review, and live focus/save/cleanup checks pass.
- First live Rust dictation: 89.3s captured; 2.6s tail decoded in 0.27s; append 33ms.
- Rust rewrite deployed: daemon/recovery run from target/release/spokenpad;
  CPU models ready, microphone callbacks and read-only evdev verified, no restarts.
- Bundled X11 smoke: exact multiline Unicode save, focus unchanged on open/append,
  temporary editor closed; unrelated user editor left untouched.
- Rust editor fixes: restart/retry deduplication, multiline saves and rollback,
  absolute RPC deadlines, safe socket handling, strict loaded i3 rule proof.
- Rust audio review fixes landed: device matching, frame-bounded recording
  queue, immediate incomplete status; 19 focused recovery/fault tests pass.
- Independent Sol reviews: audio/recovery, nvim/X11, daemon/hotkey, Python cleanup;
  findings fixed, including worker death, shutdown, config, and socket handling.
- Rust native parity: all 5 eval WAVs match Python segments and text exactly.
  102.5s simulated passage: all 6 commits/offsets and final text match;
  1.8s release tail decoded in 0.53s after the endpoint fix.
- 71 Rust + 67 Python tests pass; strict clippy/fmt, Ruff/mypy and helper CLIs pass.
- Progressive commits + audio safety net committed 2026-09-08 (05d34fd).
  Every capture has a recovery WAV; memory ceiling 3600s is visible.

## Hard constraints — docs/constraints.md
Read evdev read-only: no EVIOCGRAB/uinput clones. Never synthesize characters
or touch the clipboard. No window opened by spokenpad may take focus; no
unrelated window may receive text. CPU only. Committed speech decoded once,
with only the existing empty-result retry exception; preview stays virtual.
M4 = evdev 186 (KEY_F16), X keycode 194, XF86Launch7.

## Next
- User live check: fresh first preview, long pauses, grey text, and scroll-follow.

## Known issues / open questions
- First words lost on some long Python dictations: not reproduced; logged.
- Dying input stream: root cause unknown; watchdogs recover it.
- Short commands remain error-prone; endpoint padding fixed the local `cd home` case.
- LLM cleanup/technical vocabulary still open; no fuzzy replacements.
- Latched capture: ~230 MB RAM/hour at 16kHz, capped at 3600s; WAV continues.

## Decided
- Rust runtime authorized 2026-09-09; Sol subagents for implementation.
- Keep Parakeet/Silero, TOML, nvim Lua UI, and tested progressive-commit policy.
- No separate overlay or paste path. Current port notes: docs/rust.md.
