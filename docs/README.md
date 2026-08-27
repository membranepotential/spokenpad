# voice-kb docs

Local push-to-talk dictation for Linux/X11 (i3). This folder is the technical
reference behind the one-page pitch in the [project README](../README.md) and
the live dashboard in [STATUS.md](../STATUS.md).

## Reading order

1. **[architecture.md](architecture.md)** — the functional-core / imperative-shell
   split, the module map, and the keypress-to-injected-text event flow.
2. **[constraints.md](constraints.md)** — the hard rules this project will not
   break, each traced to a specific Handy 0.9.6 failure.
3. **[asr.md](asr.md)** — the ASR model, the measured decode speed, and the
- [evaluation.md](evaluation.md) — the regression harness: WER, per-error checks, and how to tune `hotwords_score` without guessing.
   `bpe.vocab` reconstruction that makes hotword biasing possible.
4. **[hardware.md](hardware.md)** — the exact keyboard, keycode, and display
   setup this is built and tuned against.
5. **[decisions.md](decisions.md)** — an ADR-style log of what was chosen,
   what was rejected, and why.

## Scope

These docs describe the system as implemented. Every module described in
[architecture.md](architecture.md) exists and is covered by tests. For current
build status and the active task list, see [STATUS.md](../STATUS.md), not this
folder.
