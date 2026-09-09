"""Shared fixtures for the end-to-end tests.

``QT_QPA_PLATFORM`` must be set before anything imports PySide6 (transitively,
that means before ``spokenpad.app`` is imported anywhere in the process), so it
happens at module import time, first thing.
"""

from __future__ import annotations

import os

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

from collections.abc import Callable, Iterator
from pathlib import Path

import pytest
from fakes import FakeAudioCapture, FakeNvimSession, FakeTranscriber, FakeX11
from PySide6.QtWidgets import QApplication

from spokenpad import app as app_module
from spokenpad import x11
from spokenpad.app import Daemon
from spokenpad.config import Config


@pytest.fixture(autouse=True)
def _never_the_real_state_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Point ``XDG_STATE_HOME`` at a scratch dir for *every* test.

    Not hygiene -- a guard against the suite destroying the user's data.
    ``Daemon`` builds a real ``CaptureRecorder``, whose constructor prunes
    the recordings directory to ``max_total_bytes``. With the real
    ``XDG_STATE_HOME`` that is the directory holding the user's dictation
    audio, so once it passed 5 GiB any ``pytest`` run would delete their
    oldest recordings. Verified before this fixture existed: a single
    ``pytest -k dead_microphone`` deleted a seeded 3 GiB capture.

    Autouse and unconditional, because the next test to reach the real
    directory will not be one anybody thought to audit -- the same reasoning
    that put the recording below the ceiling rather than above it.
    """
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path))


@pytest.fixture(scope="session")
def qapp() -> QApplication:
    """One ``QApplication`` for the whole run -- PySide6 allows only one per
    process, and the daemon's threads and timers need it to exist even under
    the offscreen platform plugin."""
    existing = QApplication.instance()
    if isinstance(existing, QApplication):
        return existing
    return QApplication([])


@pytest.fixture
def fake_x11(monkeypatch: pytest.MonkeyPatch) -> FakeX11:
    """Replaces ``spokenpad.x11.outputs`` so nothing here shells out to
    ``xrandr``."""
    fake = FakeX11()
    monkeypatch.setattr(x11, "outputs", fake.outputs)
    return fake


@pytest.fixture
def fake_nvim(monkeypatch: pytest.MonkeyPatch) -> FakeNvimSession:
    """Replaces ``spokenpad.nvim.NvimSession`` as seen from ``spokenpad.app``
    (where ``_NvimBridge.__init__`` constructs it: ``self._session =
    NvimSession(config.nvim)``), so opening the dictation window, appending
    to it, and pushing indicator state never touch a real socket or spawn a
    real terminal.

    A single fake is returned regardless of the arguments the bridge passes
    to the constructor, since a test only ever has one ``Daemon`` (and so one
    ``_NvimBridge``) to inspect. As with the worker's ``QThread``, the
    bridge's ``QThread`` is deliberately never started: tests drive
    ``daemon._nvim.open()`` / ``.append(text)`` / ``.set_phase(phase)``
    directly on the main thread, exactly where production code would have
    crossed to the bridge thread via a queued signal -- see ``make_daemon``
    below for why that is both faster and deterministic.
    """
    fake = FakeNvimSession()
    monkeypatch.setattr(app_module, "NvimSession", lambda config, **kwargs: fake)
    return fake


@pytest.fixture
def make_daemon(
    qapp: QApplication,
    monkeypatch: pytest.MonkeyPatch,
    fake_x11: FakeX11,
    fake_nvim: FakeNvimSession,
) -> Iterator[Callable[[Config | None], Daemon]]:
    """Builds a :class:`Daemon` wired entirely to fakes, and stops every
    daemon it built at teardown so no ``QThread`` leaks between tests.

    The daemon's worker ``QThread`` is deliberately never started: nothing
    in these tests calls ``Daemon.start()`` (that would also start the real
    ``HotkeyWatcher``, which is off-limits here -- see the module docstring
    of ``test_e2e.py``). Instead each test sets ``daemon._worker._transcriber``
    directly and calls ``daemon._worker.run_decode``/``run_preview`` itself
    wherever production code would have crossed threads via a queued signal.
    That queued connection is otherwise inert without a running event loop
    on the worker thread, so leaving it unstarted and driving the worker
    methods directly is what makes these tests both fast and deterministic
    rather than racing real thread scheduling. The bridge's ``QThread`` is
    left unstarted for the same reason -- see ``fake_nvim`` above.
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
