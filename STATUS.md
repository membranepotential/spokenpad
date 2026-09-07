# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
Hold M4 (or latch with shift), speak, and the text appears in a floating nvim
that never takes focus. Fully local, CPU-only (the 4 GB GTX 1650 stays free).
For recording long passages while reading something else.

## Now
- **Pivoted 2026-09-07: the sink is neovim, not the clipboard.** Transcript
  appended over msgpack-RPC to a floating nvim that never takes focus, one
  file per window. Nothing pasted anywhere; `inject.py` deleted.
- **Used live, German and English, hold and latch.** Daily-use shape.
- ruff + mypy --strict + **139 tests** green, incl. 12 driving a real nvim.

## Done
- Rejected Handy 0.9.6 (README); every change reverted. v1 built and reviewed,
  all 10 findings fixed. All 5 references verified 2026-08-27.
- Live preview: whole-utterance re-decode, monotonic, adaptive, capped at 30s.
- nvim sink: `nvim.py` + `nvim_indicator.lua`, pointer-anchored one-third
  placement, winbar indicator, preview as wrapped virtual text, one file per
  window. Overlay off by default. See `docs/nvim-window.md` + ADR.
- Latched recording: shift+M4 records until M4 is pressed again.
- **systemd `--user` unit** (`packaging/voice-kb.service`), enabled, running,
  `WantedBy=i3-session.target`.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- Warm, idle: 20 s -> 1.19 s (**16.8x**), 37 s -> 2.55 s. Linear; no cliff.
  Handy managed 1.37x and dropped the 37 s clip at its 30 s cap.
- WER on 5 refs, no vocabulary: **13.4% vs Handy's 48.7%** — but that gap is all
  the clip Handy dropped; on the other four, 18.4% vs **15.8%**.
- Live: 11.2s held -> 0.78s decode + ~40ms append = **~0.8s to text**.
- Window: cold open 1.0-1.2s, reattach 430ms, append 19-62ms — off the path.
- Hotwords stay viable: `bpe_vocab` is the two-column SentencePiece `.vocab`,
  not the protobuf; rebuild from `tokens.txt` as score = `-index`.

## Hard constraints — full rationale in `docs/constraints.md`
Read evdev **read-only** (no `EVIOCGRAB`, no uinput clones); **never synthesise
characters** (no `xdotool type`/enigo) — both destroy per-device `setxkbmap`.
One-shot **committed** decode. CPU only. Bias vocabulary at decode time, never
fuzzy replacement. **No window voice-kb opens may take focus**, and nothing is
written to a window it did not open. M4 = evdev `186` (`KEY_F16`) → X keycode
`194`, keysym `XF86Launch7`, which keysym-based hotkey libraries cannot bind.

## Next
- **A latched recording has no upper bound.** ~64 KB/s of audio held in memory,
  decode ~14.6x real-time: an hour latched is ~230 MB and a ~4 min decode.
- Dying input stream: root cause unknown (3rd occurrence). Watchdogs recover it.
- `cd home` -> `C D home.` — short commands spell out (Handy got this right).

## Open questions
- Cloud ASR (Gladia) as a second backend — issue #1. Not the default.
- Is an LLM cleanup pass worth it? The technical vocabulary gap (mkdir, udev,
  `cd home`) is unaddressed while hotwords stay deferred.
- Should a latched recording auto-commit at some length, or keep growing?

## Decided
- Python + `uv`; TOML config. **PySide6** overlay — GTK4 has no `move()` on X11.
- **No hotwords** — biasing is risky on pairs like set/sed, gap ~2.6 pts.
- **Deleted the paste path** rather than keep a second sink: one with no caller
  rots untested while looking maintained; one `git revert` away.
