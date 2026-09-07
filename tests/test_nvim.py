"""Integration tests for :mod:`voice_kb.nvim`, against a real headless nvim.

Unlike ``test_e2e.py`` (which fakes ``NvimSession`` entirely so the daemon's
wiring can be tested without a real editor), these tests exercise the real
thing: a real ``nvim --headless`` process, a real msgpack-RPC socket, and a
real file on disk. That is deliberate -- the guarantees this module exists
for ("the text is really on disk", "a preview is really never buffer
content") are not provable against a fake of the very object being tested.

Skipped entirely when ``nvim`` is not on ``PATH``.
"""

from __future__ import annotations

import shutil
import subprocess
from collections.abc import Callable, Iterator
from datetime import datetime
from pathlib import Path

import pytest

from voice_kb.config import NvimConfig
from voice_kb.nvim import Appended, AppendFailed, NvimSession
from voice_kb.state import Phase

pytestmark = pytest.mark.skipif(shutil.which("nvim") is None, reason="nvim is not installed")

_START = datetime(2026, 1, 15, 9, 30, 0)


def _clock_from(*moments: datetime) -> Callable[[], datetime]:
    """A clock returning each moment once, then repeating the last.

    Every *spawn* stamps a new file name, so a test that opens two windows
    needs two distinct moments to tell the two files apart.
    """
    remaining = list(moments)

    def _now() -> datetime:
        return remaining.pop(0) if len(remaining) > 1 else remaining[0]

    return _now


@pytest.fixture
def nvim_config(tmp_path: Path) -> NvimConfig:
    """Spawns nvim directly (no terminal emulator) headless, on a socket and
    dictation directory scoped to ``tmp_path`` so no test can collide with a
    real dictation session or another test's socket.

    ``window_instance`` is deliberately not the default ``"voice-kb"``: this
    suite runs against the real X server (``NvimSession.place_window`` and
    ``raise_window`` shell out to real ``i3-msg``), and a headless nvim opens
    no window for either to find -- but a distinct instance name means that
    even a no-op ``i3-msg`` call from these tests can never be mistaken for
    one aimed at an actual, currently-running dictation window.
    """
    return NvimConfig(
        terminal=(),
        editor=("nvim", "--headless", "-u", "NONE"),
        window_instance="voice-kb-pytest",
        socket_path=tmp_path / "voice-kb-test.sock",
        dictation_dir=tmp_path / "dictation",
        startup_timeout_s=10.0,
    )


@pytest.fixture
def make_session(nvim_config: NvimConfig) -> Iterator[Callable[..., NvimSession]]:
    """Builds :class:`NvimSession` instances and kills every nvim process any
    of them spawned, at teardown.

    ``NvimSession.close`` deliberately leaves the real nvim running -- see its
    docstring -- which is right for a user's actual editor and wrong for a
    test process that must not leak nvims across the suite.
    """
    created: list[NvimSession] = []

    def _make(
        config: NvimConfig | None = None,
        *,
        clock: Callable[[], datetime] | None = None,
    ) -> NvimSession:
        session = NvimSession(config or nvim_config, clock=clock or _clock_from(_START))
        created.append(session)
        return session

    yield _make

    for session in created:
        session.close()
        process = session._process
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()


def _read(session: NvimSession) -> str:
    """The dictation file's contents. Asserts there is one, so a test that
    silently lost its path fails on that rather than on a confusing None."""
    assert session.path is not None
    return session.path.read_text(encoding="utf-8")


# -- lifecycle -----------------------------------------------------------------


def test_ensure_spawns_connects_and_opens_a_timestamped_file(
    make_session: Callable[..., NvimSession],
) -> None:
    session = make_session()
    # Bound to a local: asserting on the property itself narrows it to None
    # for the rest of the function, and every later assertion goes unchecked.
    before = session.path
    assert before is None, "no file until a window has been opened"

    assert session.ensure() is True
    assert session.connected is True
    assert session.path is not None
    assert session.path.name == "dictation-2026-01-15-093000.md"
    assert session.path.parent == session._config.dictation_dir


def test_ensure_is_idempotent_and_reuses_the_same_nvim(
    make_session: Callable[..., NvimSession],
) -> None:
    session = make_session()
    assert session.ensure() is True
    process, buffer = session._process, session._buffer

    assert session.ensure() is True

    assert session._process is process, "a second ensure() must not spawn a new nvim"
    assert session._buffer == buffer


def test_ensure_reattaches_after_the_connection_is_dropped(
    make_session: Callable[..., NvimSession],
) -> None:
    """A broken RPC connection must be rebuilt onto the *same* nvim, not
    replaced with a freshly spawned one.

    ``ensure()`` itself only checks that the socket still accepts
    connections (a plain ``connect``, not a round trip -- see
    ``NvimSession._socket_alive``'s docstring for why), so a client that has
    merely gone stale is only discovered by the next call that actually talks
    to it. That is what a broken pipe on ``append``/``set_state`` looks like
    in production, and what is reproduced here.
    """
    session = make_session()
    assert session.ensure() is True
    process = session._process

    assert session._nvim is not None
    session._nvim.close()  # the client is now unusable; the process is not

    result = session.append("this discovers the drop")
    assert isinstance(result, AppendFailed)

    assert session.ensure() is True
    assert session.connected is True
    assert session._process is process, "should reattach, not respawn"


def test_a_new_session_reattaches_to_the_still_running_nvim(
    nvim_config: NvimConfig, make_session: Callable[..., NvimSession]
) -> None:
    """Simulates a daemon restart: a second ``NvimSession`` built against the
    same config must reuse the nvim the first one spawned rather than
    littering the desktop with a second one. ``_process`` is only ever set by
    the spawn path (never by reattaching), so a session that stays ``None``
    there proves it went through ``_attach_existing`` instead.
    """
    first = make_session(nvim_config)
    assert first.ensure() is True
    process = first._process
    assert process is not None

    second = make_session(nvim_config)
    assert second.ensure() is True

    assert second._process is None, "a reattached session must not have spawned one"
    assert process.poll() is None, "the original nvim must still be the one running"


def test_stale_socket_file_does_not_wedge_ensure(
    nvim_config: NvimConfig, make_session: Callable[..., NvimSession]
) -> None:
    """A socket file left behind by a dead nvim must be removed and replaced,
    not mistaken for a live one."""
    nvim_config.socket_path.parent.mkdir(parents=True, exist_ok=True)
    nvim_config.socket_path.write_bytes(b"")  # a file, not a socket -- nothing is listening

    session = make_session(nvim_config)

    assert session.ensure() is True
    assert session.connected is True


# -- writing ---------------------------------------------------------------


def test_append_lands_on_disk(make_session: Callable[..., NvimSession]) -> None:
    """The "nothing vanishes" guarantee: the text is not just in the return
    value, it is really in the dated file on disk afterwards."""
    session = make_session()
    session.ensure()

    result = session.append("hello world")

    assert isinstance(result, Appended)
    assert session.path is not None
    assert session.path.read_text(encoding="utf-8") == "hello world\n"


def test_two_appends_are_one_blank_line_apart_with_no_leading_blank(
    make_session: Callable[..., NvimSession],
) -> None:
    session = make_session()
    session.ensure()

    session.append("first utterance")
    session.append("second utterance")

    assert (
        _read(session)
        == "first utterance\n\nsecond utterance\n"
    )


def test_append_without_ensure_returns_append_failed_not_a_raise(
    make_session: Callable[..., NvimSession],
) -> None:
    session = make_session()

    result = session.append("nobody is listening")

    assert isinstance(result, AppendFailed)


def test_set_state_without_ensure_is_a_silent_noop(
    make_session: Callable[..., NvimSession],
) -> None:
    session = make_session()

    session.set_state(phase=Phase.RECORDING, level=0.5, preview="hi")  # must not raise


# -- the preview is never committed --------------------------------------------


def test_preview_is_virtual_text_never_buffer_content_or_file_content(
    make_session: Callable[..., NvimSession],
) -> None:
    """The physical enforcement of "a preview is never committed"
    (``docs/constraints.md``): the preview lives only as an extmark's virtual
    text, which cannot be written to the file, yanked, or undone into the
    buffer. This is checked by querying nvim directly, not by trusting
    ``NvimSession``'s own bookkeeping.
    """
    session = make_session()
    session.ensure()
    session.append("first line")  # so the file exists and has real content to compare against

    session.set_state(phase=Phase.RECORDING, preview="not committed")

    nvim = session._nvim
    buf = session._buffer
    assert nvim is not None
    assert buf is not None

    lines = nvim.api.buf_get_lines(buf, 0, -1, False)
    assert "not committed" not in "\n".join(lines)
    assert "not committed" not in _read(session)

    ns = nvim.api.create_namespace("voice_kb")
    extmarks = nvim.api.buf_get_extmarks(buf, ns, 0, -1, {"details": True})
    assert len(extmarks) == 1, "the preview should be exactly one extmark"
    details = extmarks[0][-1]
    virt_lines = details["virt_lines"]
    rendered = "".join(chunk[0] for line in virt_lines for chunk in line)
    assert "not committed" in rendered


# -- one file per window -------------------------------------------------------


def test_a_second_window_starts_a_new_file(
    make_session: Callable[..., NvimSession], nvim_config: NvimConfig
) -> None:
    """A closed window ends the passage. The next dictation must open a clean
    page rather than appending under everything said earlier -- which is the
    behaviour that made the day-long file wrong.
    """
    first = make_session(clock=_clock_from(_START))
    assert first.ensure() is True
    first.append("from the first window")
    first_path = first.path
    assert first_path is not None

    # The user closes the window; the daemon keeps running.
    process = first._process
    assert process is not None
    process.terminate()
    process.wait(timeout=5)
    first.close()
    nvim_config.socket_path.unlink(missing_ok=True)

    later = _START.replace(hour=14, minute=5, second=1)
    second = make_session(clock=_clock_from(later))
    assert second.ensure() is True
    second.append("from the second window")

    assert second.path is not None
    assert second.path != first_path
    assert second.path.name == "dictation-2026-01-15-140501.md"
    assert "from the first window" not in _read(second)
    assert first_path.read_text(encoding="utf-8") == "from the first window\n"


def test_reattaching_adopts_the_open_buffer_instead_of_starting_a_new_file(
    make_session: Callable[..., NvimSession],
) -> None:
    """While the window is open the passage continues -- across a daemon
    restart included. Opening a second file underneath a window the user is
    still reading would split one passage across two places.
    """
    first = make_session(clock=_clock_from(_START))
    assert first.ensure() is True
    first.append("before the restart")
    original = first.path
    assert original is not None
    first.close()  # detaches; deliberately leaves the nvim running

    # A fresh daemon, with a clock that would name a different file.
    restarted = make_session(clock=_clock_from(_START.replace(hour=16)))
    assert restarted.ensure() is True
    restarted.append("after the restart")

    assert restarted.path == original
    assert _read(restarted) == "before the restart\n\nafter the restart\n"


# ---------------------------------------------- one utterance, several segments


def test_a_continued_append_extends_the_paragraph_instead_of_starting_one(
    make_session: Callable[..., NvimSession],
) -> None:
    """An utterance decoded in segments must still read as one paragraph.

    This is the on-disk proof for what ``test_e2e.py`` asserts at the signal
    boundary: three appends, one paragraph, words separated by single spaces.
    """
    session = make_session()
    assert session.ensure()

    session.append("The first piece", continued=False)
    session.append("and the second", continued=True)
    session.append("and the third.", continued=True)

    assert _read(session).strip() == "The first piece and the second and the third."


def test_a_fresh_utterance_still_starts_its_own_paragraph(
    make_session: Callable[..., NvimSession],
) -> None:
    session = make_session()
    assert session.ensure()

    session.append("First utterance.", continued=False)
    session.append("still first", continued=True)
    session.append("Second utterance.", continued=False)

    assert _read(session).strip() == "First utterance. still first\n\nSecond utterance."


def test_continuing_into_an_empty_buffer_does_not_produce_a_leading_space(
    make_session: Callable[..., NvimSession],
) -> None:
    """``continued=True`` with nothing above it can only happen if the first
    segment decoded to nothing after post-processing. It must not leave the
    file starting with a stray space."""
    session = make_session()
    assert session.ensure()

    session.append("Text with nowhere to continue from", continued=True)

    assert _read(session) == "Text with nowhere to continue from\n"


# ------------------------------------------ the preview stays up while decoding


def _preview_lines(session: NvimSession) -> list[str]:
    """The preview extmark's virtual lines, flattened to plain strings."""
    nvim = session._nvim
    buf = session._buffer
    assert nvim is not None and buf is not None
    ns = nvim.api.create_namespace("voice_kb")
    marks = nvim.api.buf_get_extmarks(buf, ns, 0, -1, {"details": True})
    return [
        chunk[0]
        for mark in marks
        for line in mark[-1].get("virt_lines", [])
        for chunk in line
    ]


def test_the_preview_survives_the_decode_so_there_is_something_to_read(
    make_session: Callable[..., NvimSession],
) -> None:
    """Clearing it at the key release blanked the window for exactly as long
    as the user had to wait -- the one moment they most want to start
    reading."""
    session = make_session()
    assert session.ensure()

    session.set_state(phase=Phase.RECORDING, preview="what was heard so far")
    assert _preview_lines(session), "sanity: the preview shows while recording"

    session.set_state(phase=Phase.TRANSCRIBING)

    assert "".join(_preview_lines(session)) == "what was heard so far"


def test_committed_text_replaces_the_preview(
    make_session: Callable[..., NvimSession],
) -> None:
    session = make_session()
    assert session.ensure()
    session.set_state(phase=Phase.RECORDING, preview="rough version")
    session.set_state(phase=Phase.TRANSCRIBING)

    session.append("The polished version.")

    assert _preview_lines(session) == []
    assert "rough version" not in _read(session)


def test_a_stale_preview_never_greets_the_next_utterance(
    make_session: Callable[..., NvimSession],
) -> None:
    """A preview that was never replaced by committed text -- a decode that
    produced nothing, a cancellation -- must not still be sitting there when
    the user speaks again, where it would read as what they are saying now."""
    session = make_session()
    assert session.ensure()
    session.set_state(phase=Phase.RECORDING, preview="last time's words")
    session.set_state(phase=Phase.IDLE)  # decode produced nothing

    session.set_state(phase=Phase.RECORDING)

    assert _preview_lines(session) == []
