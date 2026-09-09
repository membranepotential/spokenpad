# spokenpad — local push-to-talk dictation for Linux/X11

## Goal
Hold M4 (or latch with shift), speak, and the text appears in a floating nvim
that never takes focus. Fully local, CPU-only (the 4 GB GTX 1650 stays free).
For recording long passages while reading something else.

## Now
- **progressive-commit (2026-09-08)** — settled chunks are decoded once and
  appended *while speaking*; release decodes only the open tail (bound ~1-2.5s
  instead of the 31-55s measured on 379s/821s passages); preview = open tail,
  never cropped. PySide6 overlay deleted (Qt stays as the event loop). **Built,
  reviewed (2 findings fixed), 252 tests green, simulated live: WER 13.5% vs
  14.6% before, release 1.9s vs 9.3s on a 101s passage. Daemon restarted on
  it; not yet dictated into.** Spec + as-built: `docs/progressive-commit.md`.
  Uncommitted, together with:
- **audio-safety-net (2026-09-08)** — every capture written to a wav as spoken;
  `spokenpad transcribe <wav>` replays it. Cap 3600s and loud. **Done.**

## Done
- Project rename to **spokenpad** (2026-09-09): checkout, GitHub repository,
  runtime identifiers, i3 rule and user service migrated; live daemon healthy.
- nvim sink (2026-09-07): pointer-anchored window, winbar indicator, one file per window.
- Latched recording: shift+M4 records until M4 is pressed again.
- VAD chunking (`vad.py`): short utterances no longer decode to nothing.
- Window open: placed by the terminal, msgpack readiness probe, never moves.
- systemd `--user` unit, enabled; `scripts/install.py` symlinks `packaging/`.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- Warm, idle: 20 s -> 1.19 s (**16.8x**); Handy managed 1.37x and 48.7% WER
  against ours. Live: 11.2s held = **~0.8s to text**.
- VAD-chunked **WER 12.8%** vs 13.4% whole-buffer (`eval.py --vad`).
- sherpa reports a span's end ~0.9s after speech stops; a closed last chunk
  therefore settles ~2s into a pause (`SETTLE_SILENCE_SECONDS` = 1s past it).
- Window: cold open **244ms**, append 14-62ms — off the path.

## Hard constraints — full rationale in `docs/constraints.md`
Read evdev **read-only** (no `EVIOCGRAB`, no uinput clones); **never synthesise
characters** (no `xdotool type`/enigo) — both destroy per-device `setxkbmap`.
Every committed sample decoded exactly once, never from a growing buffer. CPU
only. Bias vocabulary at decode time, never fuzzy replacement. **No window
spokenpad opens may take focus**, and nothing is written to a window it did not
open. M4 = evdev `186` (`KEY_F16`) → X keycode `194`, keysym `XF86Launch7`.

## Next
- **Dictate a long passage** through the restarted daemon; check the log line
  `decoded the last N.Ns in M.MMs` against the ~1-2.5s bound, then commit.
- A latched recording costs ~230 MB RAM/hour, cut at 3600s; `transcribe` recovers the rest.
- First words lost on some long dictations (2026-09-07). Not reproduced; logged.
- Dying input stream: root cause unknown (3rd). Watchdogs recover it.
- `cd home` -> `C D home.` — short commands spell out.
- Recorder, accepted: a capped notice can go stale if the writer fails after it.

## Open questions
- LLM cleanup pass? Technical vocabulary (mkdir, udev) is open. Gladia: #1.
- Should a latched recording auto-commit at some length, or keep growing?
- Rust rewrite (floated 2026-09-08): later, if at all; the design is neutral.

## Decided
- Python + `uv`; TOML config. PySide6 only as the event loop (overlay gone).
  **No hotwords** — biasing is risky on set/sed, gap ~2.6 pts.
- Deleted the paste path and the overlay: a second sink/UI with no caller rots.
