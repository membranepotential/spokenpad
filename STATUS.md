# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-22 @ 59428f1 (handoff, session paused by the user)_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now (handoff: two agent branches are NOT merged yet)
- corpus — branch `worktree-agent-ada6370eda939c1a8` (.claude/worktrees/):
  told to wrap up: dataset in eval-samples/local/ (audio copies, relative
  paths, README; full reproduction run skipped), harness `--corpus` relative
  paths, clamp decisions entry. Verify it committed, then rebase + checks +
  ff-merge. Until merged, references.json may still hold absolute paths.
- beam-fix — branch `worktree-agent-a653789adfe1c0a59`: told to commit its
  write-up (frame-cost variant, public repro, docs/asr.md fix). Same: verify,
  rebase, checks, ff-merge. Builds live in ~/.cache/spokenpad-dev/beam-fix/.
- own-window — P0-P2 merged and deployed; config still `mode = "managed"`.

## Next
1. Merge the two branches above (or salvage: uncommitted work is in their
   worktrees). Remove merged worktrees; ~/.cache/spokenpad-dev is ~4 GB.
2. User: P4 live check of `nvim.mode = "pane"` on i3 (steps: set the mode,
   `spokenpad check`, restart; dictate while typing elsewhere; click, type
   `Grüße @ € { }`; colours/font with tokyonight; close window, dictate again).
3. User decides: P3 other WMs (pacman: sway xorg-xwayland openbox bspwm
   awesome xfwm4) or skip; push (56+ commits unpushed); make the repo public.
4. Open, not isolated: new main loses 1 chunk of 333 the Bare retry used to
   rescue (e2 0 -> 1); candidates: sherpa 1.13.8, end-of-slice close, the
   30 s tick bound. See docs/experiments/2026-09-21-lead-padding-clamp-corpus.md.
5. Small: pane font log line prints the path twice; trailing-pad overlap
   (4 seams / 6 words on the corpus) unmeasured cost; beam + real vocabulary
   never measured (the user has no `[asr]` vocabulary).

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
