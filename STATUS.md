# spokenpad — local push-to-talk dictation for Linux

_reconciled: 2026-09-21 @ 7bc7ed8_

## Goal
Hold a key (or latch with shift), speak, and text appears in an nvim that
never takes focus. Fully local, CPU-only. Next milestone: public release.

## Now
- corpus — Gladia references for the ~176 captures (private, eval-samples/
  local/) + corpus WER harness; greedy vs beam vs zero padding — agent, wt
- beam-fix — build sherpa-onnx with upstream PR #3657, test the lost tail and
  the empty-chunk replay — agent, scratch build
- models — sherpa-onnx 1.13.8; try parakeet-unified-en (RNNT: beam + hotwords
  may work) and a Qwen3-ASR family (prompt hotwords) — agent, wt. Research
  (.claude/plans/gpu-models-research.md): GPU not worth it on a GTX 1650.
- own-window P2 — `nvim.mode = "pane"` in the daemon (plan: .claude/plans/
  own-window.md) — agent, wt. P0+P1 passed headless on i3 (63ed094: grid ==
  nvim screen, umlauts, no focus); P1 code review running. Then P3.

## Done
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
- 09-21: Portability for the public release merged (13 commits): Python removed,
  eval is examples/eval.rs; static sherpa binary; `[asr] family` parakeet /
  whisper / sense_voice; models in $XDG_DATA_HOME; install.sh/fetch-models.sh;
  attach mode (default) + managed i3/sway over native IPC, terminal table.
  169 lib + 5 CLI + 18 e2e (+1 real-model) green.
- 09-21: Preview auto-scroll fixed; arrow Up/Down move by screen line.

## Next
1. Decide the decoder from the corpus numbers: patched beam, another model,
   or GPU (would lift the "CPU only" constraint on purpose).
2. own-window P4: live checks by the user.

## Known issues / open questions
- Dying input stream: seen once, root cause unknown; the watchdog recovers it
  and shows the gap.

## Decided
- Hard constraints: docs/constraints.md. No input device is read: keys are
  bound in the WM to the control socket CLI. Rust-only since 09-21; TOML,
  nvim Lua UI and the tested progressive-commit policy stay. No paste path.
- Cancel only while recording; a tap under 120ms is discarded with a notice;
  silence is not decoded; notices are ranked, shown in the winbar.
- 09-21 (user): post-roll 250 ms stays; no "no speech" notice; no LLM
  transcript cleanup; every experiment is written up in docs/experiments/.
