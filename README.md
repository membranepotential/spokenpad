# voice-kb

Local push-to-talk dictation for Linux/X11 (i3). Hold a key, speak, release —
text lands at the cursor. Fully local, CPU-only.

Built for dictating Claude prompts and shell commands, which is a narrower and
more technical vocabulary than prose dictation tools assume.

## Status

Early. See [STATUS.md](STATUS.md) for the current state and
the constraints that shape the design.

## Why not an existing tool

We evaluated [Handy](https://github.com/cjpais/Handy) 0.9.6 first. It is the
closest thing available and it did not work out — seven defects in an afternoon,
four of which damaged the running system (see STATUS.md). Those failures became
this project's hard constraints:

- Read `/dev/input/event*` **read-only**. Never `EVIOCGRAB`, never clone
  keyboards through uinput — the clones inherit the default XKB layout and
  destroy per-device `setxkbmap` configuration.
- Never synthesise characters (`xdotool type` / enigo). It rewrites the *core*
  X keymap. Clipboard + `ctrl+v` instead: measured 183 ms vs 3.3 s.
- One-shot *committed* decode at key release. Streaming models re-decode a
  growing buffer and silently drop long utterances. The live overlay preview is
  a bounded, throwaway decode that is never injected and never delays the real
  one -- see `docs/constraints.md`.
- Bias vocabulary at decode time, never by fuzzy string replacement
  (`set`→`sed`, `reset`→`rust`).

## Measured

Parakeet TDT 0.6B v3 int8 on an i7-9850H, 6 threads, CPU only:

Idle machine, warm model, `modified_beam_search`, best of 3:

| audio | decode | real-time factor |
|---|---|---|
| 5 s | 0.38 s | 13.0× |
| 20 s | 1.19 s | **16.8×** |
| 37 s | 2.55 s | 14.5× |

Scaling is linear; there is no long-utterance cliff. For comparison, the tool
this replaces managed 1.37× on the same hardware and silently discarded the
37 s clip entirely (its streaming decoder hit a hard 30 s cap).

| | |
|---|---|
| VRAM used | none |

## Setup

```sh
uv sync
uv run scripts/fetch_model.py     # ~630 MB, not committed
```

## Note on `bpe.vocab`

sherpa-onnx needs a `bpe_vocab` file to encode hotwords, and the published
Parakeet model does not ship one. Its `bpe_vocab` is not the SentencePiece
protobuf — it is the two-column `.vocab` text file (piece, log-probability), so
it can be reconstructed from `tokens.txt` using the SentencePiece BPE convention
that score = `-index`. Verified working: `mkir` → `mkdir` at
`hotwords_score=1.5`. Scores above ~3.0 over-bias.

## License

MIT
