# voice-kb — local push-to-talk dictation for Linux/X11

## Goal
Hold M4, speak, release, the text appears in a floating nvim that never takes
focus. Fully local, CPU-only (the 4 GB GTX 1650 stays free). For recording long
passages while reading something else, and for Claude prompts and shell commands.

## Now
- **Pivoted 2026-09-07: the sink is neovim, not the clipboard.** Transcript
  appended over msgpack-RPC to a floating nvim, on a dated file saved after
  every utterance. Nothing pasted anywhere; `inject.py` deleted. Verified live:
  focus unchanged across an open, i3 reports `focused: false`.
- ruff + mypy --strict (25 files) + **123 tests** green, incl. real-headless-nvim
  integration tests. Not yet exercised with a live voice end to end.

## Done
- Evaluated and rejected Handy 0.9.6 (README); reverted every change. v1 built,
  reviewed, all 10 findings fixed. All 5 references verified 2026-08-27.
- Live preview: whole-utterance re-decode, monotonic, adaptive, capped at 15s.
- nvim sink: `nvim.py` + `nvim_indicator.lua`, mouse-anchored quarter-screen
  placement, winbar indicator, preview as virtual text. Overlay off by default;
  preview settings moved to `[preview]`. See `docs/nvim-window.md` + ADR.

## Measured (i7-9850H, 6 threads, CPU, 0 VRAM)
- Warm, idle: 20 s -> 1.19 s (**16.8x**), 37 s -> 2.55 s. Linear; no cliff.
  Handy managed 1.37x and discarded the 37 s clip at its 30 s cap.
- WER on 5 verified refs, empty vocabulary: **13.4% vs Handy's 48.7%** — but
  that gap is entirely the clip Handy dropped; on the other four, 18.4% vs
  **15.8%**: we win on reliability, not yet accuracy.
- Live: 11.2s held -> 0.78s decode + ~40ms append = **~0.8s release-to-text**.
- Window: cold open 1.0-1.2s (13.5s the first ever, one-off plugin work),
  reattach 430ms, append 19-62ms — all off the latency path.
- Hotwords stay viable: `bpe_vocab` is the two-column SentencePiece `.vocab`
  (not the protobuf), rebuilt from `tokens.txt` as score = `-index`.

## Hard constraints — full rationale in `docs/constraints.md`
Read evdev **read-only** (no `EVIOCGRAB`, no uinput clones); **never synthesise
characters** (no `xdotool type`/enigo) — both destroy per-device `setxkbmap`.
One-shot **committed** decode. CPU only. Bias vocabulary at decode time, never
fuzzy replacement. **No window voice-kb opens may take focus**, and nothing is
written to a window it did not open. M4 = evdev `186` (`KEY_F16`) → X keycode
`194`, keysym `XF86Launch7`, which keysym-based hotkey libraries cannot bind.

## Next
- **Dictate into it with a real voice** — the one thing not verified after the
  pivot; everything upstream of the sink is unchanged.
- systemd --user unit for autostart — the last thing before daily use.
- Dying input stream: root cause unknown (3rd occurrence). Watchdogs recover it.
- `cd home` -> `C D home.` — short commands spell out (Handy got this right).

## Open questions
- Cloud ASR (Gladia) as a second backend — issue #1. Not the default.
- Is an LLM cleanup pass worth it? The technical vocabulary gap (mkdir, udev,
  `cd home`) is unaddressed while hotwords stay deferred.

## Decided
- Python + `uv`; TOML config. **PySide6** overlay — GTK4 has no `move()` on X11.
- **No hotwords in v1** — biasing is risky on pairs like set/sed, gap ~2.6 pts.
- **Deleted the paste path** rather than keep a second sink: one with no caller
  rots untested while looking maintained; one `git revert` away.
