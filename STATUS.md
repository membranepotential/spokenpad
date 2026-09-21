# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-22 @ 1869902 (handoff, session paused by the user)_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now (handoff: every agent branch is merged; nothing is running)
- own-window — P0-P2 merged and deployed; the user's config is on
  `mode = "pane"` since 09-22 00:22 (backup: config.toml.bak-2026-09-22).
- Dataset frozen in eval-samples/local/ (git-ignored, 144 MB: audio/, gladia/,
  probes/, runs/, references.json, README.md); tracked eval-samples/README.md
  links it. Full reproduction run from the copy not done (sha256 verified).
- beam-fix merged (1443aac): public repro, two patch variants; builds in
  ~/.cache/spokenpad-dev/beam-fix/.

## Next
1. Clean up: remove the merged worktrees under .claude/worktrees/ and their
   branches; ~/.cache/spokenpad-dev is ~4 GB (keep beam-fix/ if wanted).
2. User: P4 live check of `nvim.mode = "pane"` on i3 (steps: set the mode,
   `spokenpad check`, restart; dictate while typing elsewhere; click, type
   `Grüße @ € { }`; colours/font with tokyonight; close window, dictate again).
3. User decides: P3 other WMs (pacman: sway xorg-xwayland openbox bspwm
   awesome xfwm4) or skip; push (56+ commits unpushed); make the repo public.
4. Lost chunk isolated (1 of 333): the end-of-slice close + silence advance
   split off a 1.6 s window with 0.6 s of speech that decodes to "" (that
   capture still scores better: 4.8% vs 7.0%). Open: count empty/non-empty
   flips over the corpus; see experiments/…-lead-padding-clamp-corpus.md.
5. Small: pane font log prints the path twice, `FontFamily("…")` Debug text
   in `spokenpad check`; trailing-pad overlap (4 seams / 6 words) cost
   unmeasured; beam + a real vocabulary never measured.

## Done (2026-09-21, all on main, deployed as d5d7418 at 23:51)
- Constant RAM while recording (1.7 vs 116 MiB per 30 min), lead padding
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
- Earlier on 09-21: greedy default, control socket CLI, model download,
  portability (Rust only, static binary, asr families), README rewrite.

## Known issues / open questions
- Dying input stream: seen once, root cause unknown; the watchdog recovers it.

## Decided
- Hard constraints: docs/constraints.md. No input device is read. No paste.
- 09-21 (user): post-roll stays; no "no speech" notice; no LLM cleanup; no
  upstream sherpa PR for now; every experiment goes to docs/experiments/;
  recordings may go to Gladia only, transcripts stay in eval-samples/local/.
