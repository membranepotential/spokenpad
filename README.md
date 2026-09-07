# voice-kb

Local push-to-talk dictation for Linux/X11 (i3). Hold a key, speak, release —
the text appears in a floating neovim window that never takes focus. Fully
local, CPU-only.

Built for recording long passages quickly — dictating notes while reading
through a document, or drafting a prompt — over a narrower, more technical
vocabulary than prose dictation tools assume.

Nothing is pasted anywhere. The transcript is appended to a dated markdown
file in a neovim the daemon opens itself, so dictating neither depends on nor
disturbs whatever window you are working in. See
[docs/nvim-window.md](docs/nvim-window.md).

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
  X keymap. The transcript goes over neovim's msgpack-RPC socket instead — no
  keystrokes, no clipboard, and the text crosses no shell or argv boundary.
- One-shot *committed* decode at key release. Streaming models re-decode a
  growing buffer and silently drop long utterances. The live preview is a
  bounded, throwaway decode that is never committed and never delays the real
  one -- in nvim it is virtual text, so it *cannot* reach the file. See
  `docs/constraints.md`.
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
| dictation window, cold open | 1.0-1.2 s, off the latency path |
| append to the buffer | 19-62 ms |

## Setup

```sh
uv sync
uv run scripts/fetch_model.py     # ~630 MB, not committed
```

Then tell your window manager to float the dictation window and never focus
it. For i3, `~/.config/i3/i3.d/voice-kb.conf`:

```
for_window [instance="voice-kb"] floating enable
no_focus   [instance="voice-kb"]
```

Everything else -- size, position, the file it writes -- is voice-kb's own;
see [docs/nvim-window.md](docs/nvim-window.md).

## Note on `bpe.vocab`

sherpa-onnx needs a `bpe_vocab` file to encode hotwords, and the published
Parakeet model does not ship one. Its `bpe_vocab` is not the SentencePiece
protobuf — it is the two-column `.vocab` text file (piece, log-probability), so
it can be reconstructed from `tokens.txt` using the SentencePiece BPE convention
that score = `-index`. Verified working: `mkir` → `mkdir` at
`hotwords_score=1.5`. Scores above ~3.0 over-bias.

## License

MIT
