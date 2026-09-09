# spokenpad

Local push-to-talk dictation for Linux/X11 (i3). Hold a key, speak, release —
the text appears in a floating neovim window that never takes focus. Fully
local, CPU-only.

The daemon and recovery command are implemented in Rust. Python is retained
only for model setup, offline evaluation, and Rust/Python ASR/VAD differential
checks; the Rust executable never starts Python. See [the Rust implementation
notes](docs/rust.md).

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
- Every committed sample is decoded exactly once, never from a growing
  buffer. Streaming models re-decode as audio arrives and silently drop long
  utterances. Here a chunk is decoded and appended the moment no later audio
  can change it, and releasing the key only decodes the open tail -- about a
  second, however long the passage. The preview of that tail is virtual text
  in nvim, so it *cannot* reach the file. See `docs/progressive-commit.md`.
- Bias vocabulary at decode time, never by fuzzy string replacement
  (`set`→`sed`, `reset`→`rust`).

## Measured

These original benchmarks describe the Python reference. Exact Rust parity
and release-tail measurements are in [docs/rust.md](docs/rust.md#verification).

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
| dictation window, cold open | 0.21 s bundled nvim config, 0.73 s with a full LazyVim; off the latency path |
| append to the buffer | 19-62 ms |

## Setup

```sh
cargo build --locked --release
# If the weights are not already present (Python is only needed by this setup tool):
uv sync
uv run scripts/fetch_model.py
python3 scripts/install.py        # window manager rules + systemd unit
systemctl --user daemon-reload && systemctl --user enable --now spokenpad
```

`install.py` symlinks the two files spokenpad needs outside the repository:

| from | to | why it cannot live in the repo |
|---|---|---|
| `packaging/i3/spokenpad.conf` | `~/.config/i3/i3.d/spokenpad.conf` | i3 reads its rules from its own config directory |
| `packaging/spokenpad.service` | `~/.config/systemd/user/spokenpad.service` | systemd reads units from its own unit directory |

Symlinks, so editing them here takes effect on the next reload with no second
step and no copy to drift; `--copy` installs independent copies instead. It is
idempotent, it refuses to overwrite a file it did not write, it warns if your
i3 config has no `include i3.d/*.conf` line, and `--uninstall` removes both.

The dictation window runs **your** nvim configuration by default. It opens
3x faster on the bundled one (`nvim.init = "bundled"`) and still looks like
your editor if you name your theme (`nvim.colorscheme`), since only that one
plugin is loaded — measured both ways in
[docs/nvim-window.md](docs/nvim-window.md). Either way spokenpad strips the
chrome and sets prose wrapping over RPC, and writes committed text with
`noautocmd` so a format-on-save cannot reflow a transcript.

The unit assumes the checkout is at `~/Documents/spokenpad`; edit
`WorkingDirectory`/`ExecStart` if it is not, and `WantedBy` if your session
target is not `i3-session.target`. Watch it with
`journalctl --user -u spokenpad -f`; the full DEBUG log is in
`$XDG_STATE_HOME/spokenpad/spokenpad.log` either way.

The editor needs Neovim 0.10 or newer for buffer-scoped diagnostic control.
The build needs Rust, a C toolchain, `pkg-config`, and PortAudio development
files (`portaudio` on Arch). It downloads the pinned sherpa-onnx native runtime
unless `SHERPA_ONNX_LIB_DIR` points to an existing **1.13.6** library directory.
Keep the shared libraries in `target/release/` beside the binary when moving
it. Model weights are still loaded from `models/` or the configured paths.

Run `target/release/spokenpad check` to validate the config and load both CPU
models without opening a microphone, hotkey watcher, or editor. Missing config
files use defaults, matching the original command.

For an existing Python installation, finish the current dictation, build Rust,
then run `systemctl --user daemon-reload && systemctl --user restart spokenpad`.
Keep only one daemon running. The service restart closes its child windows;
their saved transcripts remain on disk. A surviving dedicated editor can be
reattached without starting a new file.

### Migrating from voice-kb

Rename the checkout to `~/Documents/spokenpad`, build Rust, then install the
new unit and rules. Before enabling it, stop the old daemon so two processes do
not read the same hotkey:

```sh
systemctl --user disable --now voice-kb
python3 scripts/install.py
systemctl --user daemon-reload && systemctl --user enable --now spokenpad
i3-msg reload
```

## Recovering a dictation

Every capture is also written to a wav in
`$XDG_STATE_HOME/spokenpad/audio/`, as it is spoken and independently of
decoding, and the capture's log line names the file. So a decode that failed,
was cancelled, or stopped short is not a lost dictation:

```console
$ target/release/spokenpad transcribe ~/.local/state/spokenpad/audio/capture-2026-09-08-141530.wav
```

It decodes through the same VAD and model the daemon uses and prints the
transcript (`--out PATH` writes it to a file instead). The directory is pruned
oldest-first at 5 GiB, ~46 hours of speech; `[recording]` in
`config.example.toml` turns it off or moves it.

## Verification

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

The nvim integration tests use isolated headless editors and temporary files.
They require permission to bind local Unix sockets. No test reads keyboard
events or captures microphone audio. With the local evaluation WAVs and the
small Python reference environment installed, compare exact native results:

```sh
cargo build --locked --release --example verify_native
.venv/bin/python scripts/verify_rust.py
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
