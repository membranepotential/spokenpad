# spokenpad docs

Local push-to-talk dictation for Linux/X11 (i3). This folder is the technical
reference behind the one-page pitch in the [project README](../README.md) and
the live dashboard in [STATUS.md](../STATUS.md).

## Reading order

Start with **[rust.md](rust.md)** for the current runtime, thread ownership,
build, and verification. Historical measurements are labeled where retained.

1. **[architecture.md](architecture.md)** — the functional-core / imperative-shell
   split, the module map, and the keypress-to-text-in-the-buffer event flow.
2. **[constraints.md](constraints.md)** — the hard rules this project will not
   break, each traced to a specific Handy 0.9.6 failure.
3. **[progressive-commit.md](progressive-commit.md)** — the decode design:
   settled chunks land while the user is still speaking, and releasing the
   key only decodes the open tail.
4. **[nvim-window.md](nvim-window.md)** — the dictation window: how it is
   opened, placed, refused focus, and what runs inside it.
5. **[asr.md](asr.md)** — the ASR model, the measured decode speed, and the
   `bpe.vocab` reconstruction that makes hotword biasing possible.
6. **[evaluation.md](evaluation.md)** — the regression harness: WER, per-error
   checks, and how to tune `hotwords_score` without guessing.
7. **[hardware.md](hardware.md)** — the exact keyboard, keycode, and display
   setup this is built and tuned against.
8. **[decisions.md](decisions.md)** — an ADR-style log of what was chosen,
   what was rejected, and why.

## Scope

These docs describe the system as implemented. Every module described in
[architecture.md](architecture.md) exists and is covered by tests. For current
build status and the active task list, see [STATUS.md](../STATUS.md), not this
folder.
