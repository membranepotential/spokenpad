# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-23 @ b10f24f (review-round worktrees merged)_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now
- Nothing running. main 22f98d0 deployed; not pushed.

## Next
1. User live check passed 09-23 (preview, Ctrl+V). Still open: "tiled",
   hover focus.
2. Follow-ups from the simplification pass: CLI connect has no deadline
   (a hung daemon blocks `spokenpad start`); run/run_bounded out of wm.rs.
3. User decides: make the repo public, tag v0.2.0 (then updpkgsums,
   .SRCINFO, AUR publish). Pushing is allowed (user 09-22).
4. Open: main is 0.5 WER points better at a forced 10 s preview window
   (trailing pad?); corpus replay with the silence timeout; the lost-chunk
   guard (experiments/2026-09-22-empty-chunk-flips.md).

## Done
- 09-23 review round: preview drawn where its text lands (inline; two
  scroll bugs fixed); `spokenpad daemon`, bare prints help; Ctrl+V pastes
  in Insert; no desktop notifications; recording.max_total_size "5 GB";
  font 12, tick 1.0 s; startup timeout a constant; one model manifest;
  README shortened, details in docs/usage.md; vad.pad_seconds stays 0.5;
  stale-preview race fixed; Astra's 5 simplifications (-140 code lines).
- 09-22/23 pre-release: managed mode removed; autosave, :q writes; closing
  the pane cancels its capture; pane opens >= 20 px beside the pointer,
  hover may focus (user); audit (47 findings) all fixed incl. two P1 speech
  losses and the daemon split; other ASR families, vad/preview.enabled
  removed; durations in _seconds; no transcript text in logs; private dirs;
  bounded download; CI pinned + green; package deps/licences (eSpeak NG
  GPL declared, user choice A); README setup rewritten; screenshot; live
  path reads the whole tail (no window cuts; corpus unchanged at default).
- 09-22: pane never focuses by itself on i3, sway (runtime no_focus rule
  over IPC, sway found from the display), Openbox, KWin Wayland + X11; on
  top; size in cells; tiled where proven; default mode = pane; font in pt x
  Xft.dpi = Alacritty cells. Packaging: socket activation, presses taken
  before model ready, config reload per window, PKGBUILD, CI;
  daemon never exits under the socket; waiting recordings survive restarts.
  Corpus: dev subset (28 captures, ~2 min), 15 mixed-language flagged.
- 09-21: constant RAM while recording, auto-stop of forgotten latches,
  sherpa 1.13.8, corpus harness (Gladia refs): greedy stays, 1 s padding,
  no model switch; beam bug cause found, patch documented, not shipped.

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
