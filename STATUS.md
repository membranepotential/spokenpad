# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-21 @ 7bc7ed8_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now
- corpus — isolating the one chunk new main loses (e2 0 -> 1) — agent, wt
- beam-fix — wrapping up (no upstream PR, user 09-21) — agent
- own-window — P0-P2 merged with both review rounds fixed (02b68f1):
  `nvim.mode = "pane"`, verified headless on i3 only. Waiting for the user:
  P4 live check on i3; P3 (other WMs) needs packages installed.

## Done
- 09-21 late: DEPLOYED main d5d7418 (const RAM, pane mode, sherpa 1.13.8,
  auto-stop). Corpus harness merged: old -> new main 11.20% -> 10.85% WER;
  the lead-padding clamp changes nothing on 181 captures, kept.
- 09-21: Auto-stop (6d36c0c, reviewed): a latch with no speech for
  `capture.silence_timeout_s` (300) and no key down ends as a normal stop;
  every capture ends at 4 h; the memory ceiling now really ends it.
- 09-21: sherpa-onnx 1.13.8 (same WER; TDT beam bug NOT fixed by it). Tried
  parakeet-unified-en (8.2% on 5 clips, beam+hotwords work, but English only:
  16 empty chunks on the corpus) and Qwen3-ASR (loses nothing, reads German,
  0.4-0.7x real time, unmerged). GPU research: not worth it on a GTX 1650.
- 09-21: Constant RAM while recording (1e2631f): only the uncommitted tail is
  held (1.7 MiB vs 116 MiB per 30 min); lead padding stops at committed
  speech; a tick sees at most preview.max_seconds. Reviewed. NOT deployed:
  first replay the real captures before/after (corpus harness) for WER.
- 09-21: Lost tail fixed: Parakeet now decodes greedy by default (beam search
  = sherpa-onnx #3267: "" / "Yeah."; 19 vs 4 empty chunks on 170 captures).
  shell-commands reference corrected. Deployed (b6939db): control socket,
  i3 M4 bindings in keybindings.conf, clipboard on for the user.
  User confirmed live: M4 hold, Shift+M4 latch, Ctrl+M4 cancel work.
- 09-21: Control socket + `spokenpad start|stop|toggle|cancel` (no /dev/input);
  built-in model download; clipboard copy opt-in. 177 lib + 22 e2e green.
- 09-21: Review fixes: no spawn on an empty workspace, only loaded WM config
  proves no_focus (sway: main file), unsent appends go to the pending
  passage, no writes behind an editor, greedy+hotwords rejected. README
  rewritten, docs current. Own setup migrated: models in ~/.local/share,
  `mode = "managed"`, unit on graphical-session.target. 172 lib + 19 e2e.
- 09-21: Portability merged: Python removed, static binary, `[asr] family`.

## Next
1. User: P4 live check of `mode = "pane"` on i3; install P3 packages or skip.
   Decoder stays greedy; no upstream sherpa PR for now (user, 09-21).

## Known issues / open questions
- Dying input stream: seen once, root cause unknown; the watchdog recovers it
  and shows the gap.

## Decided
- Hard constraints: docs/constraints.md. No input device is read: keys are
  bound in the WM to the control socket CLI. Rust-only. No paste path.
- Cancel only while recording; a tap under 120ms is discarded with a notice;
  silence is not decoded; notices are ranked, shown in the winbar.
- 09-21 (user): post-roll 250 ms stays; no "no speech" notice; no LLM
  transcript cleanup; every experiment is written up in docs/experiments/.
