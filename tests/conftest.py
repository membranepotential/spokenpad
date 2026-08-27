"""Shared fixtures for the end-to-end tests.

``QT_QPA_PLATFORM`` must be set before anything imports PySide6 (transitively,
that means before ``voice_kb.app``/``voice_kb.overlay`` are imported anywhere
in the process), so it happens at module import time, first thing.
"""

from __future__ import annotations

import os

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

from collections.abc import Callable, Iterator

import pytest
from fakes import FakeAudioCapture, FakeInjector, FakeTranscriber, FakeX11
from PySide6.QtWidgets import QApplication

from voice_kb import app as app_module
from voice_kb import x11
from voice_kb.app import Daemon
from voice_kb.config import Config


@pytest.fixture(scope="session")
def qapp() -> QApplication:
    """One ``QApplication`` for the whole run -- PySide6 allows only one per
    process, and the overlay/daemon machinery needs it to exist even under
    the offscreen platform plugin."""
    existing = QApplication.instance()
    if isinstance(existing, QApplication):
        return existing
    return QApplication([])


@pytest.fixture
def fake_x11(monkeypatch: pytest.MonkeyPatch) -> FakeX11:
    """Replaces ``voice_kb.x11.outputs``/``focused_window_rect`` so overlay
    placement never shells out to ``xrandr``/``xdotool``."""
    fake = FakeX11()
    monkeypatch.setattr(x11, "outputs", fake.outputs)
    monkeypatch.setattr(x11, "focused_window_rect", fake.focused_window_rect)
    return fake


@pytest.fixture
def fake_injector(monkeypatch: pytest.MonkeyPatch) -> FakeInjector:
    """Replaces ``inject_text`` as seen from ``voice_kb.app`` (where
    ``_Worker.run_inject`` looks it up), so injection never touches a real
    clipboard or sends a real key event into whatever window has focus."""
    fake = FakeInjector()
    monkeypatch.setattr(app_module, "inject_text", fake)
    return fake


@pytest.fixture
def make_daemon(
    qapp: QApplication,
    monkeypatch: pytest.MonkeyPatch,
    fake_x11: FakeX11,
    fake_injector: FakeInjector,
) -> Iterator[Callable[[Config | None], Daemon]]:
    """Builds a :class:`Daemon` wired entirely to fakes, and stops every
    daemon it built at teardown so no ``QThread`` leaks between tests.

    The daemon's worker ``QThread`` is deliberately never started: nothing
    in these tests calls ``Daemon.start()`` (that would also start the real
    ``HotkeyWatcher``, which is off-limits here -- see the module docstring
    of ``test_e2e.py``). Instead each test sets ``daemon._worker._transcriber``
    directly and calls ``daemon._worker.run_decode``/``run_inject`` itself
    wherever production code would have crossed threads via a queued signal.
    That queued connection is otherwise inert without a running event loop
    on the worker thread, so leaving it unstarted and driving the worker
    methods directly is what makes these tests both fast and deterministic
    rather than racing real thread scheduling.
    """
    monkeypatch.setattr(app_module, "ensure_model_files", lambda config: None)
    monkeypatch.setattr(app_module, "AudioCapture", FakeAudioCapture)

    created: list[Daemon] = []

    def _make(config: Config | None = None) -> Daemon:
        daemon = Daemon(config if config is not None else Config())
        # `FakeTranscriber` duck-types `Transcriber` (never loads a real
        # model) rather than subclassing it, so this is a real type
        # mismatch from mypy's point of view -- intentional, see fakes.py.
        daemon._worker._transcriber = FakeTranscriber()  # type: ignore[assignment]
        created.append(daemon)
        return daemon

    yield _make

    for daemon in created:
        daemon.stop()
