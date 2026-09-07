# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
Hold M4 (or latch with shift), speak, and the text appears in a floating nvim
that never takes focus. Fully local, CPU-only (the 4 GB GTX 1650 stays free).
For recording long passages while reading something else.

## Now
- **Pivoted 2026-09-07: the sink is neovim, not the clipboard.** Appended over
  msgpack-RPC to a floating nvim that never takes focus, one file per window.
- **Used live, German and English, hold and latch.** ruff + mypy --strict +
  **175 tests** green, incl. 18 driving a real nvim.
- **Self-contained:** `scripts/install.py` symlinks the i3 rules and systemd
  unit out of `packaging/`; the window's nvim config is bundled too.

## Done
- Live preview: whole-utterance re-decode, monotonic, adaptive, capped at 30s.
- nvim sink: pointer-anchored one-third window, winbar indicator, preview as
  virtual text, one file per window. `docs/nvim-window.md` + ADR.
- Latched recording: shift+M4 records until M4 is pressed again.
- **VAD segmentation** (`vad.py`): fixes short utterances decoding to nothing;
  transcript lands progressively. Preview now survives the decode.
- **One-frame window open:** placed by the terminal, chrome from a bundled nvim
  config, msgpack readiness probe, X warm-up at start.
- **systemd `--user` unit**, enabled, `WantedBy=i3-session.target`.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- Warm, idle: 20 s -> 1.19 s (**16.8x**); Handy managed 1.37x and dropped a
  37 s clip at its 30 s cap. WER 13.4% vs its 48.7% (15.8/18.4 excluding that).
- Live: 11.2s held -> 0.78s decode + ~40ms append = **~0.8s to text**.
- VAD-chunked decode: first text after ~1s at any recording length, rest
  streams in. **WER unchanged at 13.4%** (`eval.py --vad`); 6 threads optimal.
- Window: cold open **244ms**, reattach 92ms, append 14-62ms — off the path.

## Hard constraints — full rationale in `docs/constraints.md`
Read evdev **read-only** (no `EVIOCGRAB`, no uinput clones); **never synthesise
characters** (no `xdotool type`/enigo) — both destroy per-device `setxkbmap`.
One decode per sample, split at silence, never on a growing buffer. CPU only.
Bias vocabulary at decode time, never fuzzy replacement. **No window voice-kb
opens may take focus**, and nothing is written to a window it did not open.
M4 = evdev `186` (`KEY_F16`) → X keycode `194`, keysym `XF86Launch7`,
unbindable by keysym-based hotkey libraries.

## Next
- **A latched recording has no upper bound in memory** (~64 KB/s: an hour is
  ~230 MB). Decode is no longer the worry — segments land progressively.
- Dying input stream: root cause unknown (3rd). Watchdogs recover it.
- `cd home` -> `C D home.` — short commands spell out.

## Open questions
- Cloud ASR (Gladia) as a second backend — issue #1. Not the default.
- Is an LLM cleanup pass worth it? The technical vocabulary gap (mkdir, udev,
  `cd home`) is open while hotwords stay deferred (`bpe.vocab` note in README).
- Should a latched recording auto-commit at some length, or keep growing?

## Decided
- Python + `uv`; TOML config. **PySide6** overlay — GTK4 has no `move()` on X11.
- **No hotwords** — biasing is risky on pairs like set/sed, gap ~2.6 pts.
- **Deleted the paste path** rather than keep a second sink: one with no caller
  rots untested while looking maintained; one `git revert` away.
