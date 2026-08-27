"""``inject_text`` sequencing, exercised entirely against fakes for
``subprocess.run``/``subprocess.Popen``.

These tests never spawn a real ``xdotool``/``xclip`` and never touch the
real X server, so they cannot send a real key event into whatever window
happens to have focus -- see ``docs/constraints.md`` on why that must never
happen from an automated run.
"""

from __future__ import annotations

import subprocess
import time
from collections.abc import Sequence
from typing import Any

import pytest

from voice_kb import inject
from voice_kb.config import PasteConfig
from voice_kb.inject import Injected, InjectionFailed, inject_text


class _FakeCompleted:
    def __init__(self, returncode: int, stdout: bytes = b"") -> None:
        self.returncode = returncode
        self.stdout = stdout


class _FakeStdin:
    def __init__(self, *, raise_on_write: bool = False) -> None:
        self.written = b""
        self.closed = False
        self._raise_on_write = raise_on_write

    def write(self, data: bytes) -> int:
        if self._raise_on_write:
            raise OSError("broken pipe")
        self.written += data
        return len(data)

    def close(self) -> None:
        self.closed = True


class _FakeConfirmProc:
    """Stands in for the ``xclip -loops N -quiet`` handle.

    ``confirm_after_s=None`` means ``poll()`` never reports an exit --
    simulating a paste nothing ever read (unconfirmed).
    """

    def __init__(self, confirm_after_s: float | None, *, raise_on_write: bool = False) -> None:
        self._confirm_at = None if confirm_after_s is None else time.monotonic() + confirm_after_s
        self.killed = False
        self.stdin = _FakeStdin(raise_on_write=raise_on_write)

    def poll(self) -> int | None:
        if self._confirm_at is not None and time.monotonic() >= self._confirm_at:
            return 0
        return None

    def wait(self, timeout: float | None = None) -> int:
        while self.poll() is None:
            time.sleep(0.001)
        return 0

    def kill(self) -> None:
        self.killed = True


def _fake_run(
    *,
    xdotool_class_stdout: bytes = b"",
    xdotool_key_rc: int = 0,
    read_clip_stdout: bytes | None = b"previous-clip",
    calls: list[list[str]],
) -> Any:
    def run(args: Sequence[str], **kwargs: Any) -> _FakeCompleted:
        args = list(args)
        calls.append(args)
        if args[0] == "xdotool" and args[1] == "getactivewindow":
            return _FakeCompleted(0, xdotool_class_stdout)
        if args[0] == "xdotool" and args[1] == "key":
            return _FakeCompleted(xdotool_key_rc)
        if args[0] == "xclip" and "-o" in args:
            if read_clip_stdout is None:
                return _FakeCompleted(1)
            return _FakeCompleted(0, read_clip_stdout)
        if args[0] == "xclip" and "-i" in args:
            return _FakeCompleted(0)
        raise AssertionError(f"unexpected subprocess.run call: {args}")

    return run


def _fake_popen(
    *,
    confirm_after_s: float | None,
    raise_oserror: bool = False,
    raise_on_write: bool = False,
    calls: list[list[str]],
) -> Any:
    def popen(args: Sequence[str], **kwargs: Any) -> _FakeConfirmProc:
        calls.append(list(args))
        if raise_oserror:
            raise OSError("no such file or directory")
        return _FakeConfirmProc(confirm_after_s, raise_on_write=raise_on_write)

    return popen


@pytest.fixture
def config() -> PasteConfig:
    return PasteConfig(restore_delay_ms=20)


# -- confirmed paste -----------------------------------------------------------


def test_confirmed_paste_restores_and_reports_confirmed(
    monkeypatch: pytest.MonkeyPatch, config: PasteConfig
) -> None:
    run_calls: list[list[str]] = []
    popen_calls: list[list[str]] = []
    monkeypatch.setattr(
        subprocess, "run", _fake_run(read_clip_stdout=b"previous-clip", calls=run_calls)
    )
    monkeypatch.setattr(
        subprocess, "Popen", _fake_popen(confirm_after_s=0.0, calls=popen_calls)
    )

    result = inject_text("hello world", config)

    assert isinstance(result, Injected)
    assert result.confirmed is True

    # The clipboard-holding xclip was launched with the transcript, watchable
    # (not self-daemonizing) and bounded to one served request.
    assert popen_calls[0][:5] == ["xclip", "-i", "-selection", "clipboard", "-loops"]
    assert "-quiet" in popen_calls[0]

    # The paste keystroke was sent, and the previous clipboard was restored
    # via a plain (blocking) xclip write.
    restore_calls = [c for c in run_calls if c[0] == "xclip" and "-i" in c]
    assert len(restore_calls) == 1


def test_unconfirmed_paste_still_restores_but_reports_unconfirmed(
    monkeypatch: pytest.MonkeyPatch, config: PasteConfig
) -> None:
    run_calls: list[list[str]] = []
    popen_calls: list[list[str]] = []
    monkeypatch.setattr(subprocess, "run", _fake_run(calls=run_calls))
    monkeypatch.setattr(
        subprocess, "Popen", _fake_popen(confirm_after_s=None, calls=popen_calls)
    )

    result = inject_text("hello world", config)

    assert isinstance(result, Injected)
    assert result.confirmed is False
    # Best-effort restore still happened even without confirmation.
    restore_calls = [c for c in run_calls if c[0] == "xclip" and "-i" in c]
    assert len(restore_calls) == 1


def test_no_restore_when_original_clipboard_was_unreadable(
    monkeypatch: pytest.MonkeyPatch, config: PasteConfig
) -> None:
    run_calls: list[list[str]] = []
    popen_calls: list[list[str]] = []
    monkeypatch.setattr(subprocess, "run", _fake_run(read_clip_stdout=None, calls=run_calls))
    monkeypatch.setattr(
        subprocess, "Popen", _fake_popen(confirm_after_s=0.0, calls=popen_calls)
    )

    result = inject_text("hello world", config)

    assert isinstance(result, Injected)
    restore_calls = [c for c in run_calls if c[0] == "xclip" and "-i" in c]
    assert restore_calls == []


# -- failure paths ---------------------------------------------------------


def test_xdotool_key_failure_reports_injection_failed_without_restoring(
    monkeypatch: pytest.MonkeyPatch, config: PasteConfig
) -> None:
    run_calls: list[list[str]] = []
    popen_calls: list[list[str]] = []
    monkeypatch.setattr(subprocess, "run", _fake_run(xdotool_key_rc=1, calls=run_calls))
    monkeypatch.setattr(
        subprocess, "Popen", _fake_popen(confirm_after_s=None, calls=popen_calls)
    )

    result = inject_text("hello world", config)

    assert isinstance(result, InjectionFailed)
    assert "xdotool key" in result.reason
    # No restore write was attempted after the paste keystroke failed.
    restore_calls = [c for c in run_calls if c[0] == "xclip" and "-i" in c]
    assert restore_calls == []


def test_clipboard_write_failure_reports_injection_failed(
    monkeypatch: pytest.MonkeyPatch, config: PasteConfig
) -> None:
    run_calls: list[list[str]] = []
    popen_calls: list[list[str]] = []
    monkeypatch.setattr(subprocess, "run", _fake_run(calls=run_calls))
    monkeypatch.setattr(
        subprocess,
        "Popen",
        _fake_popen(confirm_after_s=None, raise_oserror=True, calls=popen_calls),
    )

    result = inject_text("hello world", config)

    assert isinstance(result, InjectionFailed)
    assert "clipboard" in result.reason
    # Never got as far as sending the paste keystroke.
    key_calls = [c for c in run_calls if c[0] == "xdotool" and c[1] == "key"]
    assert key_calls == []


def test_clipboard_write_stdin_failure_reports_injection_failed(
    monkeypatch: pytest.MonkeyPatch, config: PasteConfig
) -> None:
    run_calls: list[list[str]] = []
    popen_calls: list[list[str]] = []
    monkeypatch.setattr(subprocess, "run", _fake_run(calls=run_calls))
    monkeypatch.setattr(
        subprocess,
        "Popen",
        _fake_popen(confirm_after_s=None, raise_on_write=True, calls=popen_calls),
    )

    result = inject_text("hello world", config)

    assert isinstance(result, InjectionFailed)
    assert "clipboard" in result.reason


# -- pure helper -------------------------------------------------------------


def test_focused_window_class_returns_none_on_missing_xdotool(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def run(args: Sequence[str], **kwargs: Any) -> _FakeCompleted:
        raise FileNotFoundError("xdotool not found")

    monkeypatch.setattr(subprocess, "run", run)
    assert inject.focused_window_class() is None
