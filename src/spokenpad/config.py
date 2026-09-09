"""Configuration used by the offline Python ASR and evaluation helpers.

The Rust executable owns production configuration and validates every section.
Python reads the same TOML but materializes only the audio, ASR, VAD, and text
values needed by the retained local tools.
"""

from __future__ import annotations

import math
import os
import tomllib
from collections.abc import Mapping
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Literal, Self

type DecodingMethod = Literal["greedy_search", "modified_beam_search"]

_HELPER_SECTIONS = frozenset({"audio", "asr", "vad", "text"})
_RUST_ONLY_SECTIONS = frozenset({"hotkey", "recording", "nvim", "preview"})


class ConfigError(ValueError):
    """A helper-relevant configuration value is malformed."""


def _default_config_path() -> Path:
    home = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    return home / "spokenpad" / "config.toml"


@dataclass(frozen=True, slots=True)
class AudioConfig:
    sample_rate: int = 16000
    preroll_ms: int = 250
    device: str | None = None

    def __post_init__(self) -> None:
        if self.sample_rate <= 0:
            raise ConfigError(f"audio.sample_rate must be positive: {self.sample_rate}")
        if self.preroll_ms < 0:
            raise ConfigError(f"audio.preroll_ms must not be negative: {self.preroll_ms}")


@dataclass(frozen=True, slots=True)
class AsrConfig:
    model_dir: Path = Path("models/parakeet-tdt-0.6b-v3-int8")
    num_threads: int = 6
    decoding: DecodingMethod = "modified_beam_search"
    hotwords_score: float = 1.5
    vocabulary: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        if self.num_threads < 1:
            raise ConfigError(f"asr.num_threads must be >= 1: {self.num_threads}")
        if not math.isfinite(self.hotwords_score):
            raise ConfigError("asr.hotwords_score must be finite")
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
        return self.model_dir / "bpe.vocab"


@dataclass(frozen=True, slots=True)
class VadConfig:
    enabled: bool = True
    model: Path = Path("models/silero_vad.onnx")
    threshold: float = 0.5
    min_silence_seconds: float = 0.35
    min_speech_seconds: float = 0.15
    max_speech_seconds: float = 20.0
    chunk_seconds: float = 10.0
    edge_pad_seconds: float = 2.0
    pad_seconds: float = 0.5

    def __post_init__(self) -> None:
        if not math.isfinite(self.threshold) or not 0.0 < self.threshold < 1.0:
            raise ConfigError(f"vad.threshold must be between 0 and 1: {self.threshold}")
        for name in ("pad_seconds", "edge_pad_seconds", "chunk_seconds"):
            value = getattr(self, name)
            if not math.isfinite(value) or value < 0:
                raise ConfigError(f"vad.{name} must not be negative: {value}")
        for name in ("min_silence_seconds", "min_speech_seconds", "max_speech_seconds"):
            value = getattr(self, name)
            if not math.isfinite(value) or value <= 0:
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
    replacements: Mapping[str, str] = field(default_factory=dict)
    trailing_space: bool = False

    def __post_init__(self) -> None:
        if any(not value for value in (*self.fillers, *self.replacements)):
            raise ConfigError("text fillers and replacement keys must not be empty")


@dataclass(frozen=True, slots=True)
class Config:
    audio: AudioConfig = field(default_factory=AudioConfig)
    asr: AsrConfig = field(default_factory=AsrConfig)
    vad: VadConfig = field(default_factory=VadConfig)
    text: TextConfig = field(default_factory=TextConfig)

    @classmethod
    def load(cls, path: Path | None = None) -> Self:
        """Read helper settings, using defaults when the config is absent."""
        target = path or _default_config_path()
        if not target.exists():
            return cls()
        try:
            raw = tomllib.loads(target.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as error:
            raise ConfigError(f"{target}: {error}") from error
        return cls.from_mapping(raw, base_dir=target.parent)

    @classmethod
    def from_mapping(cls, raw: Mapping[str, Any], base_dir: Path | None = None) -> Self:
        known = _HELPER_SECTIONS | _RUST_ONLY_SECTIONS
        if unknown := set(raw) - known:
            raise ConfigError(f"unknown config section(s): {', '.join(sorted(unknown))}")

        audio = _section(raw, "audio")
        asr = _section(raw, "asr")
        vad = _section(raw, "vad")
        text = _section(raw, "text")
        return cls(
            audio=_build(AudioConfig, audio, "audio"),
            asr=_build(
                AsrConfig,
                _with_path(asr, "model_dir", base_dir, "asr.model_dir"),
                "asr",
                tuple_fields=("vocabulary",),
            ),
            vad=_build(
                VadConfig,
                _with_path(vad, "model", base_dir, "vad.model"),
                "vad",
            ),
            text=_build(TextConfig, text, "text", tuple_fields=("fillers",)),
        )

    def with_model_dir(self, model_dir: Path) -> Self:
        return replace(self, asr=replace(self.asr, model_dir=model_dir))


def _section(raw: Mapping[str, Any], name: str) -> Mapping[str, Any]:
    section = raw.get(name, {})
    if not isinstance(section, Mapping):
        raise ConfigError(f"[{name}] must be a table")
    return section


def _with_path(
    section: Mapping[str, Any], key: str, base_dir: Path | None, label: str
) -> Mapping[str, Any]:
    if key not in section:
        return section
    raw = str(section[key])
    expanded = os.path.expandvars(raw)
    if "$" in expanded:
        raise ConfigError(f"{label} references an unset environment variable: {raw!r}")
    path = Path(expanded).expanduser()
    if not path.is_absolute() and base_dir is not None:
        path = base_dir / path
    return {**section, key: path}


def _build[T](
    kind: type[T],
    section: Mapping[str, Any],
    name: str,
    tuple_fields: tuple[str, ...] = (),
) -> T:
    fields = set(kind.__dataclass_fields__)  # type: ignore[attr-defined]
    if unknown := set(section) - fields:
        raise ConfigError(f"unknown key(s) in [{name}]: {', '.join(sorted(unknown))}")
    values = {
        key: tuple(value) if key in tuple_fields and isinstance(value, list) else value
        for key, value in section.items()
    }
    try:
        return kind(**values)
    except TypeError as error:
        raise ConfigError(f"[{name}]: {error}") from error
