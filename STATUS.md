# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-22 @ fd36708 (merged worktrees removed; beam-fix/ kept)_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now
- main dee23dc deployed 22:30 (managed mode removed, autosave, close
  cancels, hover focus + 20 px gap, screenshot, examples cleanup, Gladia
  script untracked); user config migrated (backup .bak-2026-09-22-managed).
  WirePlumber restarted 22:30: it had dropped the sound card at 21:52 during
  the walkthrough's PipeWire rig. User must reload i3 (float rule gone).
- Audit fixes (REPORT.md in scratchpad reviews/2026-09-22-2100; user 09-22:
  fix all incl. big splits; remove other ASR families, vad.enabled,
  preview.enabled; durations in _seconds; never log transcript text; plus
  walkthrough fixes, xclip dependency, CLI starts the socket) — two lanes
  running: core/daemon/config (A-C done, D running); edges DONE (19
  commits, 427cda6; CI on ci/lane2). User 09-23: licence option A (declare
  eSpeak NG GPL-3.0+ from sherpa's prebuilt TTS; no rebuild).
  After merge (me): wire shell/models terminal_progress into main.rs;
  nvim/** + pane/** create_dir_all -> shell::dirs::create_private;
  daemon run(): Inherited socket -> nvim::with_manager_session on reload;
  stale: architecture.md silence_timeout_s, shell/models.rs:66 asr.family.

## Next
1. User live check after i3 reload: pane_layout = "tiled"; hover focuses the
   pane; :q mid-latch cancels; typing is saved.
2. Audit + walkthrough fixes merged, reviewed, CI green.
3. User decides: make the repo public, tag v0.2.0 (fill PKGBUILD sha256,
   .SRCINFO), AUR publish. Pushing is allowed (user 09-22).
4. Lost chunk: the end-of-slice close is both the one lost chunk and the
   whole 0.35-point gain (experiments/2026-09-22-empty-chunk-flips.md).

## Done
- 09-22: pane never focuses by itself on i3, sway (runtime no_focus rule
  over IPC, sway found from the display), Openbox, KWin Wayland + X11; on
  top; size in cells; tiled where proven; default mode = pane; font in pt x
  Xft.dpi = Alacritty cells. Packaging: socket activation, presses taken
  before model ready, config reload per window, PKGBUILD, CI (never run);
  daemon never exits under the socket; waiting recordings survive restarts.
  Machine migrated 13:09 (old unit in ~/.cache/spokenpad-dev/). Corpus: dev
  subset (28 captures, ~2 min), 15 mixed-language captures flagged.
- 09-21: constant RAM while recording, auto-stop of forgotten latches,
  sherpa 1.13.8, corpus harness with Gladia references: greedy stays (beam
  loses speech), 1 s padding, no model switch (21% German words); beam bug
  cause found, patch documented, not shipped.

## Decided (known issue: dying input stream seen once; watchdog recovers)
- Hard constraints: docs/constraints.md. No input device is read. No paste.
- 09-22 (user): Arch PKGBUILD, no install script; daemon starts by socket
  activation only (spokenpad.socket shipped enabled); models stay out of the
  package, automatic download stays: missing models download in the
  background while dictations record to disk and show a message; config
  reloads when a window opens; default mode becomes pane after P3 focus checks on
  sway/Xwayland + another X11 WM; the dev subset prefers the least private
  captures; dictation content never enters the public repo.
- 09-21 (user): post-roll stays; no "no speech" notice; no LLM cleanup; no
  upstream sherpa PR for now; every experiment goes to docs/experiments/;
  recordings may go to Gladia only, transcripts stay in eval-samples/local/.
