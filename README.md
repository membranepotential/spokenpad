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

## While you dictate

The winbar of the dictation window is the whole interface: a phase dot, a level
meter while recording, a lock while latched, and the live preview of the tail
below the committed text. When something happened to a capture, a **notice**
is shown in the winbar — in whatever phase you are in, beside the phase label,
until the next key press — one at a time, per capture, never a repeating
warning. It never displaces the preview: dictated text and a warning about it
are different things and are never in the same place.

A notice has two parts: a **headline** (`⚠ microphone gap`), always drawn, and
the sentence explaining it, appended only when the window is wide enough to
hold all of it. A narrow window gives up the level meter and then the
explanation — never the phase label or the headline, and never half a sentence
ending mid-word. When two things happen to the same capture, the more serious
one is shown and nothing lesser displaces it afterwards: memory limit reached >
capture incomplete > microphone unavailable > microphone gap > nearly silent >
held too briefly > preview paused. The daemon log always has the whole
sentence, and the full paths in it.

- **Recording continues for a quarter second after you let go**
  (`audio.postroll_ms`), so a word still sounding at key-up is not cut off.
  The winbar switches to transcribing at once; pressing the key again ends the
  wait early and starts the next capture straight away.
- **Escape cancels, but only while recording.** After the key is released the
  audio is captured and the final decode is already running, so the cancel key
  is ignored from there on: it is read from every keyboard regardless of focus,
  and honouring it then would destroy a finished dictation. Cancelling stops
  *decoding*; text that already reached the file stays, and the WAV is kept.
- **A tap shorter than 120 ms is discarded**, with "held too briefly — hold the
  key while speaking" in the winbar. The WAV is still written.
- **Losing the hotkey keyboard ends the recording by decoding it**: unplugging
  mid-sentence, held or latched, decodes what was said instead of throwing it
  away.
- **A microphone that stops delivering audio is reopened**, and the capture it
  interrupted is marked as having a gap. While idle the same repair runs
  quietly, so the pre-roll is full at the next press.
- **A capture that came back much shorter than the hold, or nearly silent,**
  says so rather than only appearing in the log.
- **A capture with no speech in it is not transcribed at all.** A recogniser
  asked to transcribe silence invents words — an empty half-second press once
  produced "Thank you." — so when the VAD hears nothing, nothing is appended.
  Without a VAD model the whole capture is still decoded.
- **Speech the VAD heard is never silently dropped by the recogniser.**
  Parakeet sometimes returns nothing for a short sentence; such a chunk is
  decoded once more without its trailing silence, which recovered every such
  loss found in your recordings.
- **Previews pause on a long uncommitted tail** (`preview.max_seconds`, 30 s)
  and resume by themselves once it settles. Without a VAD model no preview is
  issued at all — decoding the whole growing capture is the one thing this
  project refuses — and the capture is decoded at release instead.
- **Past the 60-minute in-memory ceiling** the capture is released for decoding
  and the winbar names the recovery WAV, which keeps recording — by file name,
  since it has a window's width; the log names the directory it is in.
- **After every release the whole buffer is on the clipboard**: every press
  in the window plus anything you edited by hand, ready to paste wherever you
  want it. The dictation nvim sets its own `+` register through its clipboard
  provider (xclip/xsel); nothing is pasted for you, and an empty buffer leaves
  the clipboard alone.

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
  keystrokes, no paste, and the text crosses no shell or argv boundary.
- Every committed sample is decoded exactly once, never from a growing
  buffer. Streaming models re-decode as audio arrives and silently drop long
  utterances. Here a chunk is decoded and appended the moment no later audio
  can change it, and releasing the key only decodes the open tail -- about a
  second, however long the passage. The preview of that tail is virtual text
  in nvim, so it *cannot* reach the file. See `docs/progressive-commit.md`.
- Bias vocabulary at decode time, never by fuzzy string replacement
  (`set`→`sed`, `reset`→`rust`).

## Measured

**The decode table below is the Python reference, pre-port.** The model, thread
count and provider are unchanged in Rust, and exact Rust/Python parity plus the
live release-tail and append measurements are in
[docs/rust.md](docs/rust.md#verification).

Parakeet TDT 0.6B v3 int8 on an i7-9850H, 6 threads, CPU only (Python,
pre-port). Idle machine, warm model, `modified_beam_search`, best of 3:

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
| append to the buffer | 19-62 ms (Python); 33 ms measured on the first live Rust dictation |

Accuracy on the five local reference clips, `uv run scripts/eval.py`: 13.9%
aggregate WER whole-buffer, 17.6% through the VAD path spokenpad actually uses,
against Handy 0.9.6's 48.7% on the same five. Five verified references are a
regression proxy, not an accuracy guarantee — see
[docs/evaluation.md](docs/evaluation.md).

## Setup

```sh
cargo build --locked --release
# If the weights are not already present (Python is only needed by this setup tool):
uv sync
uv run scripts/fetch_model.py
uv run scripts/install.py         # window manager rules + systemd unit
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
models without opening a microphone, hotkey watcher, or editor.

### Command line

```
spokenpad [OPTIONS]                    # the daemon
spokenpad transcribe <WAV> [--out PATH]  # recover a recording
spokenpad check                        # validate config, load and warm the models
```

| flag | |
|---|---|
| `-c`, `--config PATH` | Config file. Without it, `$XDG_CONFIG_HOME/spokenpad/config.toml`, and defaults if that does not exist. A `--config` naming a file that does **not** exist is an error — a typo must not silently run on defaults. |
| `--model-dir PATH` | Override `asr.model_dir` for this run. |
| `-v`, `--verbose` | Copy DEBUG to stderr as well. Without it stderr gets INFO and above; the log *file* is DEBUG either way. |
| `--log-file PATH` | Log file location; the literal `none` disables file logging. Defaults to `$XDG_STATE_HOME/spokenpad/spokenpad.log` — mode 0600, rotated at 1 MB with three kept. |
| `--dump-audio DIR` | Daemon only: also write each capture, exactly as decoded, to `DIR/capture-<timestamp>.wav`. For debugging what the recognizer was given. |

`audio.sample_rate` must be 16000: Silero's VAD window is 512 samples at 16 kHz
and Parakeet's feature extractor assumes the same rate, and nothing resamples in
between. Any other value is rejected at startup.

Exit codes: `2` model files missing, `3` the hotkey watcher could not start
(no readable `/dev/input/event*` advertising the key code), `4` the recording
handed to `transcribe` is unreadable or has the wrong sample rate, `1` anything
else — including "another daemon already holds
`$XDG_STATE_HOME/spokenpad/daemon.lock`", which is why systemd retries `1` and
refuses to retry `2` and `3`.

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
uv run scripts/install.py
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

`tests/e2e.rs` drives the real `daemon::serve` loop end to end with three
substitutions and nothing else: a synthetic microphone in place of PortAudio, a
channel in place of evdev, and a counting recognizer in place of the models.
Session policy, the capture arithmetic, the recovery WAV, the decode worker, the
editor thread and a real `nvim --headless` over msgpack-RPC are production code.
One test loads the actual models and is `#[ignore]`d; it needs `models/`:

```sh
cargo test --locked --test e2e -- --ignored
```

The nvim tests use isolated headless editors and temporary files, and require
permission to bind local Unix sockets. They **fail** if nvim is not installed
rather than reporting a green suite; set `SPOKENPAD_ALLOW_MISSING_NVIM=1` to skip
them deliberately. No test reads keyboard events, captures microphone audio,
takes the daemon lock, or touches the real state directory, so the suite runs
while your own service does.

With the local evaluation WAVs and the small Python reference environment
installed, compare exact native results:

```sh
cargo build --locked --release --example verify_native
uv run scripts/verify_rust.py
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
