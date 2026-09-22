//! The dictation file when no editor is there to hold it.
//!
//! In attach mode the user opens the editor, so a dictation can arrive with
//! none open. The text must not be lost and must not wait in memory for an
//! editor that may never come, so it is written straight into a dictation
//! file — the *pending passage* — with the same paragraph rule the editor
//! applies (`transactional_append` in `spokenpad.lua`), and the path is kept
//! beside the socket in `<socket>.pending`. The next editor, however it is
//! opened, starts on that file, so it shows everything said meanwhile.
//!
//! The file is never written behind an editor's back, since that editor's
//! buffer would go stale and its next save would stop at nvim's "file
//! changed since reading" prompt. The daemon opens the passage in an editor
//! only on its own editor thread, the thread that writes it, and removes the
//! pointer once the editor holds it. `spokenpad editor`, a separate process,
//! [`take`]s the passage instead: the pointer is removed before the editor
//! reads the file, under a lock every write holds too, so a write either
//! finishes before the editor reads the file or starts a new passage.
use super::{inside_dictation_dir, new_file, utf8_path};
use crate::config::Nvim;
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

/// What [`append`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedWrite {
    /// The pending passage the text is now in.
    pub path: PathBuf,
    /// Whether this write started it, which is when the user is told.
    pub started: bool,
}

/// Appends committed text to the pending passage, starting one if there is
/// none, and does not return until the file is on disk.
pub fn append(config: &Nvim, text: &str, continued: bool) -> Result<DetachedWrite> {
    let _lock = lock(&config.socket_path)?;
    let (path, started) = match pending(config) {
        Some(path) => (path, false),
        None => {
            let path = new_file(config)?;
            // The text is written either way. A pointer that could not be
            // saved only means the next write starts another file.
            if let Err(error) = write_pointer(&config.socket_path, &path) {
                log::warn!("could not record the pending dictation file: {error:#}");
            }
            (path, true)
        }
    };
    let existing = fs::read_to_string(&path)
        .with_context(|| format!("read pending dictation file {}", path.display()))?;
    let updated = append_paragraph(&existing, text, continued);
    let directory = path.parent().context("dictation file has no directory")?;
    // A whole-file replace, so a crash mid-write leaves the old text rather
    // than half of the new; `tempfile` creates it 0600 like `new_file`.
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(updated.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("write pending dictation file {}", path.display()))?;
    Ok(DetachedWrite { path, started })
}

/// The pending passage, if the pointer names a regular file that is still
/// inside the dictation directory. Anything else is ignored: a pointer is a
/// convenience, and a stale or foreign one must not redirect a transcript.
pub fn pending(config: &Nvim) -> Option<PathBuf> {
    let recorded = fs::read_to_string(pointer_path(&config.socket_path)).ok()?;
    let path = inside_dictation_dir(config, Path::new(recorded.trim_end_matches('\n'))).ok()??;
    fs::metadata(&path).ok()?.is_file().then_some(path)
}

/// Forgets the pending passage once an editor the daemon opened holds
/// `pinned`. A pointer to a different file is kept: that passage has not
/// been shown yet.
pub fn settle(config: &Nvim, pinned: &Path) {
    let _lock = match lock(&config.socket_path) {
        Ok(lock) => lock,
        Err(error) => {
            log::warn!("could not clear the pending dictation pointer: {error:#}");
            return;
        }
    };
    let Some(pending) = pending(config) else {
        return;
    };
    if fs::canonicalize(pinned).is_ok_and(|pinned| pinned == pending)
        && let Err(error) = fs::remove_file(pointer_path(&config.socket_path))
    {
        log::warn!("could not clear the pending dictation pointer: {error:#}");
    }
}

/// Takes the pending passage for an editor that is about to open it, from
/// another process than the daemon's. The pointer is gone when this
/// returns, so no later write can land in the file that editor shows.
pub fn take(config: &Nvim) -> Result<Option<PathBuf>> {
    let _lock = lock(&config.socket_path)?;
    let Some(path) = pending(config) else {
        return Ok(None);
    };
    let pointer = pointer_path(&config.socket_path);
    fs::remove_file(&pointer).with_context(|| format!("remove {}", pointer.display()))?;
    Ok(Some(path))
}

/// Makes `path` the pending passage again after the editor that took it did
/// not start, unless another passage was started meanwhile.
pub fn restore(socket: &Path, path: &Path) -> Result<()> {
    let _lock = lock(socket)?;
    if fs::symlink_metadata(pointer_path(socket)).is_ok() {
        return Ok(());
    }
    write_pointer(socket, path)
}

/// An exclusive lock on `<socket>.pending.lock`, held until it is dropped.
/// It serializes the pending passage and its pointer between the daemon and
/// `spokenpad editor`, which are separate processes.
fn lock(socket: &Path) -> Result<File> {
    let mut name = socket.as_os_str().to_owned();
    name.push(".pending.lock");
    let path = PathBuf::from(name);
    let directory = path.parent().context("socket path has no directory")?;
    crate::shell::dirs::create_private(directory)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    loop {
        // SAFETY: `file` owns a live descriptor for the duration of the call,
        // and flock retains no pointer. Closing it when `file` is dropped
        // releases the lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).with_context(|| format!("lock {}", path.display()));
        }
    }
}

fn write_pointer(socket: &Path, path: &Path) -> Result<()> {
    let pointer = pointer_path(socket);
    let directory = pointer.parent().context("socket path has no directory")?;
    crate::shell::dirs::create_private(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    writeln!(temporary, "{}", utf8_path(path)?)?;
    temporary
        .persist(&pointer)
        .map_err(|error| error.error)
        .with_context(|| format!("persist {}", pointer.display()))?;
    Ok(())
}

pub(super) fn pointer_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".pending");
    PathBuf::from(name)
}

/// The file contents after appending `text` the way the editor would.
///
/// The same rule as `transactional_append`, over the lines nvim would read
/// from `existing`: trailing blank lines are dropped, a new utterance opens
/// a paragraph one blank line below the last text (none at the top of an
/// empty file), and `continued` extends the last line with a space instead —
/// with none when that line already ends in one.
pub fn append_paragraph(existing: &str, text: &str, continued: bool) -> String {
    let body = existing.strip_suffix('\n').unwrap_or(existing);
    let mut lines: Vec<&str> = if existing.is_empty() {
        Vec::new()
    } else {
        body.split('\n').collect()
    };
    // Lua's `%s`: space, \t, \n, \v, \f, \r.
    let space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r');
    let blank = |line: &str| line.chars().all(space);
    let last_text = lines
        .iter()
        .rposition(|line| !blank(line))
        .map_or(0, |index| index + 1);
    lines.truncate(last_text);
    let mut addition: Vec<String> = text.split('\n').map(str::to_owned).collect();
    match lines.pop() {
        Some(previous) if continued => {
            // No second space after one the last commit already ends in
            // (`text.trailing_space`), as in Lua's `%s$`.
            let separator = if addition[0].is_empty() || previous.ends_with(space) {
                ""
            } else {
                " "
            };
            addition[0] = format!("{previous}{separator}{}", addition[0]);
        }
        Some(previous) => {
            lines.push(previous);
            addition.insert(0, String::new());
        }
        None => {}
    }
    let mut all: Vec<&str> = lines;
    all.extend(addition.iter().map(String::as_str));
    let mut joined = all.join("\n");
    joined.push('\n');
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_follow_the_editors_rule() {
        let cases = [
            ("", "first", false, "first\n"),
            ("", "first", true, "first\n"),
            ("first\n", "second", false, "first\n\nsecond\n"),
            ("first\n", "more", true, "first more\n"),
            ("first\n", "", true, "first\n"),
            ("first\n\n\n  \n", "second", false, "first\n\nsecond\n"),
            ("first", "second", false, "first\n\nsecond\n"),
            ("a\nb\n", "c\nd\n", false, "a\nb\n\nc\nd\n\n"),
            ("\n\n", "x", false, "x\n"),
            ("Grüße\n", "東京", true, "Grüße 東京\n"),
            // `text.trailing_space` ends every commit in a space already.
            ("first \n", "more ", true, "first more \n"),
            ("first\t\n", "more", true, "first\tmore\n"),
        ];
        for (existing, text, continued, expected) in cases {
            assert_eq!(
                append_paragraph(existing, text, continued),
                expected,
                "{existing:?} + {text:?} (continued: {continued})"
            );
        }
    }

    fn config(directory: &Path) -> Nvim {
        Nvim {
            socket_path: directory.join("run/nvim.sock"),
            dictation_dir: directory.join("dictation"),
            ..Nvim::default()
        }
    }

    #[test]
    fn a_pending_passage_is_started_once_then_continued_then_settled() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path());
        let first = append(&config, "one", false).unwrap();
        assert!(first.started);
        let second = append(&config, "two", false).unwrap();
        assert!(!second.started);
        assert_eq!(second.path, first.path);
        append(&config, "more", true).unwrap();
        assert_eq!(
            fs::read_to_string(&first.path).unwrap(),
            "one\n\ntwo more\n"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // An editor holding another file leaves the pointer alone.
        let other = config.dictation_dir.join("other.md");
        fs::write(&other, "").unwrap();
        settle(&config, &other);
        assert_eq!(pending(&config), Some(first.path.clone()));
        settle(&config, &first.path);
        assert_eq!(pending(&config), None);
        let next = append(&config, "three", false).unwrap();
        assert!(next.started);
        assert_ne!(next.path, first.path);
    }

    #[test]
    fn a_pointer_outside_the_dictation_directory_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(directory.path());
        fs::create_dir_all(config.socket_path.parent().unwrap()).unwrap();
        fs::create_dir_all(&config.dictation_dir).unwrap();
        let foreign = directory.path().join("secrets.txt");
        fs::write(&foreign, "keep\n").unwrap();
        fs::write(
            pointer_path(&config.socket_path),
            format!("{}\n", foreign.display()),
        )
        .unwrap();
        assert_eq!(pending(&config), None);
        let write = append(&config, "text", false).unwrap();
        assert!(write.started);
        assert_eq!(fs::read_to_string(&foreign).unwrap(), "keep\n");
        // A pointer to a file that was deleted starts a new one too.
        fs::remove_file(&write.path).unwrap();
        assert!(append(&config, "again", false).unwrap().started);
    }
}
