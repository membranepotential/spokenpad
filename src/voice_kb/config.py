"""Configuration: TOML in, frozen dataclasses out.

Parsing and validation happen once, here, at the boundary. Everything
downstream receives already-valid values, so no other module needs to defend
against a missing key or a negative duration.
"""

from __future__ import annotations

import functools
import os
import re
import tomllib
from collections.abc import Mapping
from dataclasses import dataclass, field, replace
from datetime import date
from importlib import resources
from pathlib import Path
from typing import Any, Literal, Self

DEFAULT_CONFIG_PATH = Path.home() / ".config" / "voice-kb" / "config.toml"

type DecodingMethod = Literal["greedy_search", "modified_beam_search"]
type LatchModifier = Literal["shift", "ctrl", "alt"]

_LATCH_KEY_CODES: dict[str, frozenset[int]] = {
    # evdev codes from linux/input-event-codes.h, both sides of each modifier.
    # Hardcoded rather than imported from evdev so this module stays pure and
    # importable without the kernel headers -- the same reason `key_code`
    # defaults to a bare 186 rather than `ecodes.KEY_F16`.
    "shift": frozenset({42, 54}),
    "ctrl": frozenset({29, 97}),
    "alt": frozenset({56, 100}),
}


class ConfigError(ValueError):
    """Raised for a malformed config, with the offending key in the message."""


@dataclass(frozen=True, slots=True)
class HotkeyConfig:
    key_code: int = 186
    """evdev key code. 186 is ``KEY_F16`` -- the Keychron Q10 Pro's M4 key.

    Note this is the *evdev* code, not the X11 keycode (which is this + 8).
    """

    cancel_key_code: int | None = 1
    """evdev ``KEY_ESC``. ``None`` disables cancelling."""

    latch_modifier: LatchModifier | None = "shift"
    """Modifier that turns a press into a *latched* recording.

    Hold it with the hotkey and recording runs until the hotkey is pressed
    again, instead of ending at the release. For long passages, holding a key
    for two minutes is its own kind of friction.

    ``None`` disables latching entirely, leaving pure push-to-talk.
    """

    def __post_init__(self) -> None:
        if not 0 < self.key_code < 0x300:
            raise ConfigError(f"hotkey.key_code out of range: {self.key_code}")
        if self.latch_modifier is not None and self.latch_modifier not in _LATCH_KEY_CODES:
            raise ConfigError(
                f"hotkey.latch_modifier must be one of "
                f"{', '.join(sorted(_LATCH_KEY_CODES))}, or omitted: {self.latch_modifier!r}"
            )

    @property
    def latch_key_codes(self) -> frozenset[int]:
        """evdev codes any of which count as the latch modifier being held.

        Both sides of the modifier, so it does not matter which hand is on it.
        Empty when latching is disabled.
        """
        if self.latch_modifier is None:
            return frozenset()
        return _LATCH_KEY_CODES[self.latch_modifier]


@dataclass(frozen=True, slots=True)
class AudioConfig:
    sample_rate: int = 16000
    """Parakeet expects 16 kHz. Changing this requires a matching model."""

    preroll_ms: int = 250
    """Audio retained before the key goes down, so the first word is not clipped.

    Costs an always-open input stream. ``0`` disables it and the stream is
    opened lazily on keypress instead.
    """

    device: str | None = None
    """``None`` means the PipeWire/PulseAudio default source."""

    def __post_init__(self) -> None:
        if self.sample_rate <= 0:
            raise ConfigError(f"audio.sample_rate must be positive: {self.sample_rate}")
        if self.preroll_ms < 0:
            raise ConfigError(f"audio.preroll_ms must not be negative: {self.preroll_ms}")

    @property
    def preroll_frames(self) -> int:
        return self.sample_rate * self.preroll_ms // 1000


@dataclass(frozen=True, slots=True)
class AsrConfig:
    model_dir: Path = Path("models/parakeet-tdt-0.6b-v3-int8")
    num_threads: int = 6
    decoding: DecodingMethod = "modified_beam_search"

    hotwords_score: float = 1.5
    """Per-token bias for hotwords.

    Measured on this model: 1.5 fixes ``mkir`` -> ``mkdir``; 3.0 starts firing
    on unrelated audio; 6.0 rewrites ordinary words. Raise with care and
    re-run ``scripts/eval.py``.
    """

    vocabulary: tuple[str, ...] = ()
    """Words and phrases to bias toward. Written to a hotwords file at startup.

    Only used with ``modified_beam_search``; greedy decoding ignores it.
    """

    def __post_init__(self) -> None:
        if self.num_threads < 1:
            raise ConfigError(f"asr.num_threads must be >= 1: {self.num_threads}")
        if self.vocabulary and self.decoding != "modified_beam_search":
            raise ConfigError(
                "asr.vocabulary requires decoding = 'modified_beam_search'; "
                f"got {self.decoding!r}"
            )

    @property
    def encoder(self) -> Path:
        return self.model_dir / "encoder.int8.onnx"

    @property
    def decoder(self) -> Path:
        return self.model_dir / "decoder.int8.onnx"

    @property
    def joiner(self) -> Path:
        return self.model_dir / "joiner.int8.onnx"

    @property
    def tokens(self) -> Path:
        return self.model_dir / "tokens.txt"

    @property
    def bpe_vocab(self) -> Path:
        """Generated by ``scripts/build_hotwords.py``; not shipped with the model."""
        return self.model_dir / "bpe.vocab"


@dataclass(frozen=True, slots=True)
class VadConfig:
    """Voice activity detection, used to split a capture before decoding.

    On by default, and the default is load-bearing: without it a short
    utterance inside a long quiet capture decodes to an empty string. See
    ``voice_kb.vad`` for the measurements. Turning it off restores exactly the
    older whole-buffer behaviour, bug included.
    """

    enabled: bool = True

    model: Path = Path("models/silero_vad.onnx")
    """Silero VAD weights, ~2 MB, fetched by ``scripts/fetch_model.py``.

    Resolved like :attr:`AsrConfig.model_dir`: relative to the config file's
    directory when one is loaded, else to the working directory. Absent means
    the daemon runs without segmentation rather than refusing to start.
    """

    threshold: float = 0.5
    """Speech probability above which a frame counts as speech."""

    min_silence_seconds: float = 0.35
    """Silence needed to end a segment.

    This is the sentence-boundary knob. Too low and one sentence is chopped at
    every breath, which costs quality: the recogniser capitalises each segment
    as a fresh start, so mid-sentence splits produce stray capitals. Too high
    and segments grow until incremental delivery stops being incremental.
    """

    min_speech_seconds: float = 0.15
    """Shortest run that counts as speech, so a cough is not a segment."""

    max_speech_seconds: float = 20.0
    """Hard cap on one detected run of speech, cutting it even mid-sentence.

    Nothing is lost -- the audio continues in the next one -- and it is what
    bounds time to first text when someone talks without pausing.
    """

    chunk_seconds: float = 10.0
    """Speech to accumulate before closing a chunk and decoding it.

    Detected segments are *merged* up to this before being handed to the
    recogniser, because a boundary costs accuracy: the model sees no context
    across one. Measured over the eval samples, decoding every segment
    separately scored 37.7% WER against 33.7% for the whole buffer, while
    merging to 10 s scored 33.4% -- indistinguishable from not splitting at
    all, and still bounding time to first text by the chunk rather than by the
    length of the recording. That is the whole point on a long latched
    passage: text starts landing after one chunk instead of after everything.

    Lower it for text sooner at some cost in accuracy; raise it to approach
    whole-buffer decoding with a longer wait for the first words.
    """

    edge_pad_seconds: float = 2.0
    """Extra audio kept before the first chunk and after the last one.

    The edges of a capture are where losing audio actually costs something: a
    word clipped off the front of a dictation is the first word of the
    sentence, and nobody notices a word trimmed from the middle of a pause.
    So the outer edges get a wider margin than the interior boundaries do.

    Bounded rather than "extend to the ends of the buffer", because a chunk
    swamped by silence is exactly what makes the recogniser return nothing --
    the failure ``voice_kb.vad`` exists to prevent. It applies only to chunks
    with real speech in them (see ``_EDGE_MARGIN_MIN_SPEECH_S``), so a short
    utterance buried in a long quiet capture is still trimmed tightly.
    """

    pad_seconds: float = 0.5
    """Audio kept either side of a chunk, beyond what the detector marked.

    Silero's boundaries are tight and clip word onsets and endings. Measured:
    at every chunk size tried, 0.5 s of padding beat 0.2 s by 2-4 WER points.
    It is real audio from the capture, not silence, so it also gives the
    recogniser the run-up it wants into the first word.
    """

    def __post_init__(self) -> None:
        if not 0.0 < self.threshold < 1.0:
            raise ConfigError(f"vad.threshold must be between 0 and 1: {self.threshold}")
        for name in ("pad_seconds", "edge_pad_seconds"):
            padding: float = getattr(self, name)
            if padding < 0:
                raise ConfigError(f"vad.{name} must not be negative: {padding}")
        if self.chunk_seconds < 0:
            raise ConfigError(f"vad.chunk_seconds must not be negative: {self.chunk_seconds}")
        for name in ("min_silence_seconds", "min_speech_seconds", "max_speech_seconds"):
            value: float = getattr(self, name)
            if value <= 0:
                raise ConfigError(f"vad.{name} must be > 0: {value}")
        if self.max_speech_seconds <= self.min_speech_seconds:
            raise ConfigError(
                "vad.max_speech_seconds must exceed vad.min_speech_seconds: "
                f"{self.max_speech_seconds} <= {self.min_speech_seconds}"
            )


@dataclass(frozen=True, slots=True)
class TextConfig:
    strip_fillers: bool = True
    fillers: tuple[str, ...] = ("uh", "um", "erm", "hmm")
    """Removed only as standalone words, never as substrings."""

    replacements: Mapping[str, str] = field(default_factory=dict)
    """Exact, whole-word substitutions applied after decoding.

    Deliberately not fuzzy. Edit-distance matching on short tokens is what
    turned ``set`` into ``sed`` and ``reset`` into ``rust`` in the tool this
    project replaced. Prefer ``asr.vocabulary`` -- biasing the beam beats
    rewriting the output.
    """

    trailing_space: bool = False


#: ``nvim.init`` value selecting voice-kb's own config, resolved from the
#: installed package. A named value rather than a path because the file lives
#: inside the package and a user config should not have to name a checkout
#: directory that may move. nvim's own ``-u NONE`` is the same idea.
BUNDLED_INIT = "bundled"

_INSTANCE_PLACEHOLDER = "{instance}"
_X_PLACEHOLDER = "{x}"
_Y_PLACEHOLDER = "{y}"
"""Token in :attr:`NvimConfig.terminal` replaced by the window instance name.

Substituted with :meth:`str.replace`, not :meth:`str.format`, so a terminal
argument that happens to contain braces is passed through untouched rather
than raising or being reinterpreted.
"""

_INSTANCE_RE = re.compile(r"^[A-Za-z0-9_-]+$")
"""Allowed window instance names.

An allowlist, because this name is interpolated into an i3 criteria string
(``[instance="..."]``) in :meth:`voice_kb.nvim.NvimSession.place_window`. A
name containing a quote or a bracket would let a config value become i3
syntax; the same reasoning that keeps transcript text out of argv keeps
config values out of another program's grammar.
"""


def xdg_state_home() -> Path:
    return Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local" / "state")


def _xdg_runtime_dir() -> Path:
    """The user's runtime directory, falling back to the state directory.

    ``XDG_RUNTIME_DIR`` is where a socket belongs -- it is user-private and
    cleared at logout, so a stale socket cannot outlive the session that made
    it. It is not guaranteed to exist, hence the fallback.
    """
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    return Path(runtime) if runtime else xdg_state_home()


@dataclass(frozen=True, slots=True)
class NvimConfig:
    """Where the transcript goes: a floating neovim, never the clipboard."""

    terminal: tuple[str, ...] = (
        "alacritty",
        "--class",
        f"Floating,{_INSTANCE_PLACEHOLDER}",
        # Where the window should map, so it is in the right place in its very
        # first frame. i3 honours this for a floating window (verified: the
        # window appeared at exactly the requested point, floating, unfocused),
        # which removes the visible jump from wherever the window manager first
        # put it to where voice-kb wants it. The size is still corrected over
        # i3 afterwards -- alacritty measures its window in character cells,
        # and guessing the font's metrics to avoid one small resize would be
        # worse than the resize.
        "-o",
        f"window.position.x={_X_PLACEHOLDER}",
        "-o",
        f"window.position.y={_Y_PLACEHOLDER}",
        "-e",
    )
    """Terminal to run the editor in, with the editor's argv appended.

    Empty means run :attr:`editor` directly, for a GUI editor that needs no
    terminal. The default names the window ``Floating`` (class) /
    ``voice-kb`` (instance) so a window manager rule can float it and refuse
    it focus -- see ``docs/nvim-window.md``.

    ``{instance}`` is replaced with :attr:`window_instance`, and ``{x}``/
    ``{y}`` with the top-left corner the window should open at. A terminal
    that cannot be told where to open simply leaves those out; the window is
    still placed, just one frame later.
    """

    editor: tuple[str, ...] = ("nvim",)
    """The editor itself. ``-u <init>``, ``--listen <socket>`` and the
    dictation file are appended; anything here is passed before them."""

    init: Path | None = None
    """nvim config for the window. ``None`` uses nvim's own, i.e. the user's.

    Default because the window *is* the user's editor: their colourscheme,
    their keybindings, their yank flash. A bundled config was tried and the
    verdict from use was clear -- it never looked like their neovim, and each
    missing habit had to be reimplemented one at a time to no real end.

    voice-kb still applies what the window needs (chrome off, prose wrapping)
    over RPC once attached, and re-applies it on ``BufWinEnter``/``WinNew``/
    ``FileType`` so a plugin reacting to those cannot undo it. Committed text
    is written with ``noautocmd``, so a format-on-save cannot reflow dictated
    prose behind the user's back either.

    ``"bundled"`` selects voice-kb's own config instead: no plugins, chrome
    off before the first frame, and **0.30 s to open the window against 0.78 s
    for a full LazyVim setup** -- with :attr:`colorscheme` it still gets the
    user's theme, because only that one plugin is put on the runtimepath.
    ``"NONE"`` gives a bare nvim. Any other value is a path to a config file.
    """

    colorscheme: str | None = None
    """Colourscheme to load in the dictation window. ``None`` changes nothing.

    Mainly for use with ``init = "bundled"``, where it is what makes a
    plugin-free window still look like the user's editor: the one directory
    providing the named scheme is added to the runtimepath, and nothing else
    is, so the colours arrive without the startup cost of the configuration
    they normally live in.

    Harmless with a full config loaded, where it simply switches theme.
    """

    transparent: bool = True
    """Let the terminal's background show through the window.

    On by default, and it is what makes :attr:`colorscheme` look right. A
    theme loaded straight from its plugin directory is the theme's
    *defaults*, not the theme as the user configured it -- so a scheme someone
    runs with ``transparent = true`` in their own config arrives opaque here,
    painting a block of its own background inside the terminal's border.
    Inheriting reproduces that setting, and is the right answer for a window
    floating over a terminal in any case.

    Set ``false`` to take the colourscheme's own background instead.
    """

    window_instance: str = "voice-kb"
    """X11 instance name of the dictation window.

    Substituted into :attr:`terminal` and used to find the window again. It
    must match whatever the window manager rules key on.
    """

    socket_path: Path = field(default_factory=lambda: _xdg_runtime_dir() / "voice-kb-nvim.sock")
    """Where nvim listens for RPC. Reconnected to rather than recreated."""

    dictation_dir: Path = field(default_factory=lambda: xdg_state_home() / "voice-kb" / "dictation")
    """Directory holding the dated dictation files."""

    file_template: str = "dictation-%Y-%m-%d-%H%M%S.md"
    """:meth:`~datetime.datetime.strftime` template for the file name.

    A file per *window*, on disk, saved after every utterance. Closing the
    window ends the passage; the next dictation starts a new file rather than
    appending under everything said earlier. Include a time, not just a date,
    or two sessions on the same day will share a page.

    On disk rather than in a scratch buffer, because a transcript that
    disappears because a buffer was closed is the same failure as one dropped
    by a streaming decoder.
    """

    window_fraction: float = 0.33
    """Size of the dictation window, as a fraction of each screen axis.

    A third of the width and a third of the height. Big enough to read a
    paragraph of wrapped prose, small enough to sit beside the document being
    read rather than over it.

    The window is placed with its top-left corner at the mouse pointer, so it
    opens next to whatever the user is looking at, and is clamped on-screen --
    which tucks it flush into the corner when the pointer is already near an
    edge, or when the pointer cannot be read at all.
    """

    startup_timeout_s: float = 20.0
    """How long to wait for a freshly spawned nvim to start answering RPC.

    Generous on purpose. A full plugin configuration takes seconds before it
    serves API requests -- ~4s measured here for LazyVim, and far longer the
    first time a plugin manager installs. Waiting costs nothing the user
    feels: the window is opened on key-down, from a thread of its own, while
    the utterance is still being spoken, and the append that follows simply
    queues behind it.
    """

    def __post_init__(self) -> None:
        if not self.editor:
            raise ConfigError("nvim.editor must name an executable")
        if not _INSTANCE_RE.match(self.window_instance):
            raise ConfigError(
                f"nvim.window_instance must match {_INSTANCE_RE.pattern}: "
                f"{self.window_instance!r}"
            )
        if not 0.0 < self.window_fraction <= 1.0:
            raise ConfigError(
                f"nvim.window_fraction must be in (0, 1]: {self.window_fraction}"
            )
        if self.startup_timeout_s <= 0:
            raise ConfigError(
                f"nvim.startup_timeout_s must be positive: {self.startup_timeout_s}"
            )
        rendered = date(2000, 1, 2).strftime(self.file_template)
        if not rendered or "/" in rendered or rendered in {".", ".."}:
            raise ConfigError(
                "nvim.file_template must render to a single file name, "
                f"got {rendered!r} from {self.file_template!r}"
            )

    def spawn_argv(self, *, socket: Path, target: Path, at: tuple[int, int] | None) -> list[str]:
        """The full command line for a new dictation window.

        ``target`` is passed as its own argument rather than through a shell,
        so a path with spaces in it needs no quoting and cannot be re-parsed.

        ``at`` is where the window should map. ``None`` -- no usable screen
        geometry -- substitutes ``0``, which is no worse than the window
        manager's own choice and keeps the command line one shape.
        """
        x, y = at if at is not None else (0, 0)
        substitutions = {
            _INSTANCE_PLACEHOLDER: self.window_instance,
            _X_PLACEHOLDER: str(x),
            _Y_PLACEHOLDER: str(y),
        }
        terminal = [
            functools.reduce(lambda a, kv: a.replace(*kv), substitutions.items(), arg)
            for arg in self.terminal
        ]
        init = ["-u", str(self.resolved_init)] if self.init is not None else []
        theme = (
            ["-c", _colorscheme_command(self.colorscheme, transparent=self.transparent)]
            if self.colorscheme
            else []
        )
        return [
            *terminal,
            *self.editor,
            *init,
            *theme,
            "--listen",
            str(socket),
            str(target),
        ]

    @property
    def resolved_init(self) -> Path:
        """:attr:`init` with ``"bundled"`` resolved to the packaged file."""
        if self.init is None:
            raise ConfigError("nvim.init is unset; there is nothing to resolve")
        if str(self.init) == BUNDLED_INIT:
            return self.bundled_init
        return self.init

    @property
    def bundled_init(self) -> Path:
        """The self-contained nvim config shipped with the package.

        Not used unless :attr:`init` names it. It exists for a machine with no
        nvim configuration of its own, and as the written-down statement of
        what this window actually needs.
        """
        return Path(str(resources.files("voice_kb").joinpath("dictation_init.lua")))

    @property
    def announces_instance(self) -> bool:
        """Whether the spawned command will actually name the window.

        ``{instance}`` is substituted into :attr:`terminal`, so only a
        terminal template that contains it produces a window findable by
        :attr:`window_instance`. Without one there is nothing for the window
        manager rules to match and nothing for voice-kb to wait for -- the
        editor is on its own for placement, which is right for a bare
        ``nvim --headless`` and for a GUI editor that places itself.
        """
        return any(_INSTANCE_PLACEHOLDER in arg for arg in self.terminal)




@dataclass(frozen=True, slots=True)
class PreviewConfig:
    """The live transcript preview, shown while the key is held.

    Its own section rather than a corner of ``[overlay]``: the preview now
    feeds the nvim indicator, which is the primary UI, and the overlay is off
    by default. Tying "do I see what I am saying" to "is the Qt pill enabled"
    would be a lie about what these settings control.

    A preview is *cosmetic only*. The text that actually gets committed is
    still produced by exactly one decode of the complete captured buffer at
    key release (``docs/constraints.md``, "One-shot committed decode"):
    preview output is never appended, never merged into the final text, and
    never influences it.
    """

    enabled: bool = True

    interval_ms: int = 1100
    """Minimum time between preview decodes while recording.

    A floor, not a fixed period: the real gap is
    ``max(interval_ms - last_decode, last_decode)``, which keeps the worker
    idle at least half the time however long the utterance gets. See
    :attr:`max_seconds` for why that idle fraction is the thing being
    protected.
    """

    max_seconds: float = 600.0
    """Stop previewing once the utterance is longer than this. A backstop.

    Previews used to decode the whole utterance from its beginning every time,
    so each cost more than the last -- ~4s by the one-minute mark -- and this
    limit was load-bearing: past it they stopped being issued at all, and the
    winbar had to explain that a frozen preview was not lost audio. In use
    that was reported straight back as the wrong behaviour, which it was. The
    limit was 30s and ordinary passages ran longer.

    A preview now costs the same at ten minutes as at ten seconds. Chunks the
    detector has closed are settled -- no later speech changes them -- so
    their text is kept and only the open tail is re-decoded (see
    ``_Worker._preview_text``). Cost is bounded by ``vad.chunk_seconds``, not
    by the utterance, so there is nothing left for a time limit to protect
    against.

    It stays as a backstop for the case that reasoning does not cover: no VAD
    model, where previews fall back to decoding the whole buffer and the old
    growth returns. Ten minutes is far past any dictation and still short of
    a runaway.

    The number is a latency budget either way. A *queued* preview is dropped
    the instant the key is released, but one already inside ``decode_stream``
    cannot be interrupted, so the committed decode waits for it. With chunking
    that wait is one chunk (~0.8s worst case, ~0.4s expected from the duty
    cycle) regardless of what this is set to.
    """

    def __post_init__(self) -> None:
        if self.interval_ms < 200:
            raise ConfigError(f"preview.interval_ms must be >= 200: {self.interval_ms}")
        if self.max_seconds <= 0:
            raise ConfigError(f"preview.max_seconds must be positive: {self.max_seconds}")


@dataclass(frozen=True, slots=True)
class OverlayConfig:
    enabled: bool = False
    """Off by default since the transcript sink became nvim.

    The nvim window carries the indicator now (``nvim_indicator.lua``): the
    same phase, level meter and live preview, rendered where the text is
    about to land instead of in a second widget elsewhere on screen. This
    overlay still works and still never takes focus -- turn it back on if you
    want feedback before the dictation window has opened.
    """

    # Sizes below are in Qt's *logical* pixels, which is what Qt's resize()
    # takes. On a display with a device pixel ratio above 1 -- Xft.dpi 192
    # gives 2.0 -- the overlay is drawn at twice these numbers in device
    # pixels, so it keeps the same apparent size as on a 96dpi screen. That is
    # deliberate: it follows the DPI the user configured rather than shrinking
    # to a sliver on a 4K panel. See voice_kb.overlay.screen_rect.
    width: int = 720
    """Wide enough to read a line of transcript, not just the status label."""

    height: int = 96
    """Height of the status row (dot, label, level meter) on its own."""

    margin_px: int = 32
    """Gap between the overlay and the bottom edge of the output."""

    follow_focus: bool = True
    """Place the overlay on the output holding the focused window."""

    preview_height: int = 64
    """Extra pixels below the status row for the preview band.

    Only consulted when previews are on (:attr:`PreviewConfig.enabled`); the
    policy for previews themselves lives in ``[preview]``, because they now
    feed the nvim indicator as well as this overlay.
    """

    def __post_init__(self) -> None:
        if self.preview_height < 0:
            raise ConfigError(f"overlay.preview_height must not be negative: {self.preview_height}")

    def total_height(self, *, preview_band: bool) -> int:
        """Full widget height: the status row, plus the preview band if shown."""
        return self.height + self.preview_height if preview_band else self.height


@dataclass(frozen=True, slots=True)
class Config:
    hotkey: HotkeyConfig = field(default_factory=HotkeyConfig)
    audio: AudioConfig = field(default_factory=AudioConfig)
    asr: AsrConfig = field(default_factory=AsrConfig)
    vad: VadConfig = field(default_factory=VadConfig)
    text: TextConfig = field(default_factory=TextConfig)
    nvim: NvimConfig = field(default_factory=NvimConfig)
    preview: PreviewConfig = field(default_factory=PreviewConfig)
    overlay: OverlayConfig = field(default_factory=OverlayConfig)

    @classmethod
    def load(cls, path: Path | None = None) -> Self:
        """Read TOML from ``path``, falling back to defaults if it is absent."""
        target = path or DEFAULT_CONFIG_PATH
        if not target.exists():
            return cls()
        try:
            raw = tomllib.loads(target.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as e:
            raise ConfigError(f"{target}: {e}") from e
        return cls.from_mapping(raw, base_dir=target.parent)

    @classmethod
    def from_mapping(cls, raw: Mapping[str, Any], base_dir: Path | None = None) -> Self:
        known = {"hotkey", "audio", "asr", "vad", "text", "nvim", "preview", "overlay"}
        if unknown := set(raw) - known:
            raise ConfigError(f"unknown config section(s): {', '.join(sorted(unknown))}")

        return cls(
            hotkey=_build(HotkeyConfig, raw.get("hotkey", {}), "hotkey"),
            audio=_build(AudioConfig, raw.get("audio", {}), "audio"),
            asr=_build(
                AsrConfig,
                _resolve_model_dir(raw.get("asr", {}), base_dir),
                "asr",
                tuple_fields=("vocabulary",),
            ),
            vad=_build(VadConfig, _resolve_vad_model(raw.get("vad", {}), base_dir), "vad"),
            text=_build(TextConfig, raw.get("text", {}), "text", tuple_fields=("fillers",)),
            nvim=_build(
                NvimConfig,
                _resolve_nvim_paths(raw.get("nvim", {})),
                "nvim",
                tuple_fields=("terminal", "editor"),
            ),
            preview=_build(PreviewConfig, raw.get("preview", {}), "preview"),
            overlay=_build(OverlayConfig, raw.get("overlay", {}), "overlay"),
        )

    def with_model_dir(self, model_dir: Path) -> Self:
        """Override the model location, e.g. from a CLI flag."""
        return replace(self, asr=replace(self.asr, model_dir=model_dir))


def _resolve_model_dir(section: Mapping[str, Any], base_dir: Path | None) -> Mapping[str, Any]:
    """Make a relative ``model_dir`` relative to the config file, not the cwd."""
    if "model_dir" not in section:
        return section
    model_dir = Path(str(section["model_dir"])).expanduser()
    if not model_dir.is_absolute() and base_dir is not None:
        model_dir = base_dir / model_dir
    return {**section, "model_dir": model_dir}


def _colorscheme_command(name: str, *, transparent: bool) -> str:
    """A ``-c`` command that applies ``name`` under either kind of config.

    The bundled config defines ``VoiceKbColorscheme``, which finds the scheme
    among installed plugins, puts only its directory on the runtimepath, and
    keeps the terminal's background unless told otherwise. A user's own config
    already has its theme loaded and configured, so a plain ``colorscheme`` is
    right there -- and ``pcall`` because a window that cannot take focus must
    never be left showing a message waiting for a keypress.
    """
    opaque = "true" if not transparent else "false"
    return (
        f"lua if _G.VoiceKbColorscheme then VoiceKbColorscheme({name!r}, {opaque}) "
        f"else pcall(vim.cmd.colorscheme, {name!r}) end"
    )


def _resolve_vad_model(section: Mapping[str, Any], base_dir: Path | None) -> Mapping[str, Any]:
    """Make a relative VAD ``model`` relative to the config file, not the cwd.

    Same rule as :func:`_resolve_model_dir`, for the same reason: a config
    file is read from wherever the daemon happens to have been started, and a
    path in it should mean what it looks like it means.
    """
    if "model" not in section:
        return section
    model = Path(str(section["model"])).expanduser()
    if not model.is_absolute() and base_dir is not None:
        model = base_dir / model
    return {**section, "model": model}


_NVIM_PATH_KEYS = ("socket_path", "dictation_dir", "init")


def _resolve_nvim_paths(section: Mapping[str, Any]) -> Mapping[str, Any]:
    """TOML has no path type, so the path-valued keys arrive as strings.

    Both ``~`` and ``$VAR`` are expanded here, at the boundary, so nothing
    downstream has to remember to. ``$XDG_RUNTIME_DIR`` in particular is the
    natural way to write the socket path, and a literal ``$XDG_RUNTIME_DIR``
    directory appearing in the user's home is not a failure worth shipping.
    An unset variable is left as-is by :func:`os.path.expandvars`, which would
    do exactly that, so it is rejected instead.
    """
    resolved = dict(section)
    for key in _NVIM_PATH_KEYS:
        if key not in resolved:
            continue
        expanded = os.path.expandvars(str(resolved[key]))
        if "$" in expanded:
            raise ConfigError(
                f"nvim.{key} references an unset environment variable: {resolved[key]!r}"
            )
        resolved[key] = Path(expanded).expanduser()
    return resolved


def _build[T](
    kind: type[T],
    section: Mapping[str, Any],
    name: str,
    tuple_fields: tuple[str, ...] = (),
) -> T:
    """Construct a config dataclass, converting lists to tuples and reporting
    unknown keys against the section they appeared in."""
    fields = {f for f in kind.__dataclass_fields__}  # type: ignore[attr-defined]
    if unknown := set(section) - fields:
        raise ConfigError(f"unknown key(s) in [{name}]: {', '.join(sorted(unknown))}")
    values = {
        k: tuple(v) if k in tuple_fields and isinstance(v, list) else v
        for k, v in section.items()
    }
    try:
        return kind(**values)
    except TypeError as e:
        raise ConfigError(f"[{name}]: {e}") from e
