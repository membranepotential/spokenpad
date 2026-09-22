# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-22 @ fd36708 (merged worktrees removed; beam-fix/ kept)_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now
- simplify — managed mode removed, autosave, close cancels, layout log:
  done on branch worktree-agent-a6071f6a0e01869a4 — in review — merge,
  deploy, migrate the user's config (i3.d/spokenpad.conf symlink held the
  float rule that floated the tiled pane).
- hover-focus (implementer, on that branch) — pane opens >= 20 px from the
  pointer, hover may focus it (user 09-22) — running.
- first CI run on push of 1b21933 — queued.
- audit + security review, fresh-user walkthrough, README screenshot —
  starting (user 09-22: push allowed, release waits for the user's go).

## Next
1. User live check: typing elsewhere ok, `Grüße @ € {}` ok; still to try:
   pane_dimensions edit at the next window, pane_layout = "tiled".
2. Before public: fresh-user walkthrough (clean account: README only),
   codebase audit + security review, README screenshot of the pane.
3. User decides: push (then the first real CI run), make the repo public,
   tag v0.2.0 (PKGBUILD source sha256 is SKIP until then).
4. Lost chunk: the end-of-slice close is both the one lost chunk and the
   whole 0.35-point gain. Open: a guard that keeps the gain (see
   experiments/2026-09-22-empty-chunk-flips.md; dev subset: ~2 min a run).

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
