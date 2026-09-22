# spokenpad docs

Local push-to-talk dictation for Linux. This folder is the technical
reference behind the [project README](../README.md) and the live dashboard in
[STATUS.md](../STATUS.md).

## Reading order

Start with **[rust.md](rust.md)** for the runtime, thread ownership, build,
and verification. Historical measurements are labelled where retained.

1. **[architecture.md](architecture.md)** — the functional-core / imperative-shell
   split, the module map, and the keypress-to-text-in-the-buffer event flow.
2. **[constraints.md](constraints.md)** — the hard rules this project will not
   break, each traced to a failure of the dictation tool this project
   replaced.
3. **[progressive-commit.md](progressive-commit.md)** — the decode design:
   settled chunks land while the user is still speaking, and releasing the
   key only decodes the open tail.
4. **[nvim-window.md](nvim-window.md)** — the dictation editor: pane and
   attach mode, how the pane is opened, placed and refused focus, and what
   runs inside nvim.
5. **[asr.md](asr.md)** — the model, the measured decode speed, and the
   `bpe.vocab` reconstruction that makes hotword biasing possible.
6. **[configuration.md](configuration.md)** — every setting of
   `config.example.toml`: why its default is what it is, and what was
   measured on the way.
7. **[evaluation.md](evaluation.md)** — the regression harness
   (`examples/eval.rs`): WER on the local clips and what it is worth.
8. **[hardware.md](hardware.md)** — what spokenpad needs from the machine:
   the keys' codes, CPU, audio, displays and window manager.
9. **[decisions.md](decisions.md)** — an ADR-style log of what was chosen,
   what was rejected, and why.
10. **[experiments/](experiments/README.md)** — one file per experiment:
   benchmarks, corpus replays, model comparisons and spikes, with their
   numbers, including the ones that led nowhere.

## Scope

These docs describe the system as implemented. Every module described in
[architecture.md](architecture.md) exists. All of them carry unit tests; the
event loop in `shell/daemon.rs`, which has no unit tests of its own, is
covered end to end by `tests/e2e.rs` — the real `shell::daemon::serve` driven
headlessly
against a synthetic microphone, a counting recognizer and a real
`nvim --headless`. For current build status and the active task list, see
[STATUS.md](../STATUS.md), not this folder.
