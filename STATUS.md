# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-22 @ fd36708 (merged worktrees removed; beam-fix/ kept)_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now (2026-09-22: polish for the public release)
- p3-focus — pane never takes focus on sway/Xwayland, openbox, kwin
  (headless, own sessions); gates pane as default — agent running.
- packaging — socket activation, accept before model load, no auto
  download, PKGBUILD + CI, install.sh removed — agent running.

## Next
1. User: P4 live check of `nvim.mode = "pane"` on i3 (steps: set the mode,
   `spokenpad check`, restart; dictate while typing elsewhere; click, type
   `Grüße @ € { }`; colours/font with tokyonight; close window, dictate again).
2. After p3-focus passes: default mode = pane. User decides: push (62 commits unpushed); make the repo public.
3. Lost chunk: the end-of-slice close is both the one lost chunk and the
   whole 0.35-point gain (flip run: revert = old text on all 181). Open: a
   guard that keeps the gain; see experiments/2026-09-22-empty-chunk-flips.md.
4. Small: trailing-pad overlap (4 seams / 6 words) cost
   unmeasured; beam + a real vocabulary never measured.
5. Before public: fresh-user walkthrough, audit + security review, README
   screenshot; history scan clean except a Handy transcripts.json (user checks).

## Done
- 09-22: pane font in points x Xft.dpi = Alacritty cells (19x41 here),
  deployed 11:54, user font set to SauceCodePro 12; dev subset `--subset dev` (28 captures, ~2 min run, holds every
  failure main shows; beam loss caught); `spoken` flag: 15 mixed captures,
  their references cost ~1 WER point; flip count done.
- 09-21 (deployed as d5d7418): constant RAM while recording (1.7 vs 116 MiB per 30 min), lead padding
  stops at committed speech, a tick sees at most preview.max_seconds.
- Own window `nvim.mode = "pane"`: own X11 window + embedded nvim, no focus,
  X libs loaded at run time; two review rounds fixed; headless on i3 only.
- Auto-stop: a latch with no speech for `capture.silence_timeout_s` (300) and
  no key down ends as a normal stop; any capture ends at 4 h.
- sherpa-onnx 1.13.8. Corpus: 181 captures / 75 min with Gladia references
  (git-ignored), harness examples/corpus.rs: greedy 0 lost vs beam 2 lost,
  WER equal; 1 s padding stays; live path beats whole-file; old -> new main
  11.20% -> 10.85%. 21% of the words are German: parakeet-unified-en (7.9%
  English, beam + hotwords work) is no default; Qwen3-ASR too slow; GPU not
  worth it on the GTX 1650. Beam bug cause found (blank skips frames for
  free); one-line patch documented, not shipped.

## Known issues / open questions
- Dying input stream: seen once, root cause unknown; the watchdog recovers it.

## Decided
- Hard constraints: docs/constraints.md. No input device is read. No paste.
- 09-22 (user): Arch PKGBUILD, no install script; daemon starts by socket
  activation only (spokenpad.socket shipped enabled); models stay out of the
  package: `spokenpad fetch-models` is the one explicit setup step, no
  automatic download; default mode becomes pane after P3 focus checks on
  sway/Xwayland + another X11 WM; the dev subset prefers the least private
  captures; dictation content never enters the public repo.
- 09-21 (user): post-roll stays; no "no speech" notice; no LLM cleanup; no
  upstream sherpa PR for now; every experiment goes to docs/experiments/;
  recordings may go to Gladia only, transcripts stay in eval-samples/local/.
