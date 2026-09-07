# voice-kb

Local push-to-talk dictation for Linux/X11 (i3). Hold a key, speak, release —
the text appears in a floating neovim window that never takes focus. Fully
local, CPU-only.

Hold **shift** with the hotkey instead and recording *latches*: let go, keep
talking, and press the hotkey again when you are done. For a long passage,
holding a key for two minutes is its own kind of friction.

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
| dictation window, cold open | ~820 ms with your nvim config, off the latency path (92 ms to reattach) |
| append to the buffer | 19-62 ms |

## Setup

```sh
uv sync
uv run scripts/fetch_model.py     # ~630 MB ASR + ~2 MB VAD, not committed
uv run scripts/install.py         # window manager rules + systemd unit
systemctl --user daemon-reload && systemctl --user enable --now voice-kb
```

`install.py` symlinks the two files voice-kb needs outside the repository:

| from | to | why it cannot live in the repo |
|---|---|---|
| `packaging/i3/voice-kb.conf` | `~/.config/i3/i3.d/voice-kb.conf` | i3 reads its rules from its own config directory |
| `packaging/voice-kb.service` | `~/.config/systemd/user/voice-kb.service` | systemd reads units from its own unit directory |

Symlinks, so editing them here takes effect on the next reload with no second
step and no copy to drift; `--copy` installs independent copies instead. It is
idempotent, it refuses to overwrite a file it did not write, it warns if your
i3 config has no `include i3.d/*.conf` line, and `--uninstall` removes both.

The dictation window runs **your** nvim configuration by default — your
colourscheme, your keybindings, your yank flash — while voice-kb strips the
chrome and sets prose wrapping over RPC regardless. A self-contained fallback
config ships at `src/voice_kb/dictation_init.lua` for a machine without one;
point `nvim.init` at it. See [docs/nvim-window.md](docs/nvim-window.md).

The unit assumes the checkout is at `~/Documents/voice-kb`; edit
`WorkingDirectory`/`ExecStart` if it is not, and `WantedBy` if your session
target is not `i3-session.target`. Watch it with
`journalctl --user -u voice-kb -f`; the full DEBUG log is in
`$XDG_STATE_HOME/voice-kb/voice-kb.log` either way.

## Note on `bpe.vocab`

sherpa-onnx needs a `bpe_vocab` file to encode hotwords, and the published
Parakeet model does not ship one. Its `bpe_vocab` is not the SentencePiece
protobuf — it is the two-column `.vocab` text file (piece, log-probability), so
it can be reconstructed from `tokens.txt` using the SentencePiece BPE convention
that score = `-index`. Verified working: `mkir` → `mkdir` at
`hotwords_score=1.5`. Scores above ~3.0 over-bias.

## License

MIT
