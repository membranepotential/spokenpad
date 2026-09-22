//! The directories that hold what the user dictated, and who may look in.
//!
//! Every file spokenpad writes there is 0600, but a directory's listing still
//! gives away when and how much someone dictated: file names are timestamps,
//! sizes are lengths. So every directory spokenpad creates is 0700 from the
//! start, and its own state directory is narrowed to that if an earlier
//! version created it open. A directory the user named is theirs: it is
//! created private when it does not exist, and otherwise left as they set it.
use std::{
    fs::{self, DirBuilder},
    io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
};

/// Only the owner may list or enter it.
const PRIVATE: u32 = 0o700;

/// Creates `directory` and every missing parent, each new one 0700. One
/// that exists already is left as it is.
pub fn create_private(directory: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE)
        .create(directory)
}

/// spokenpad's own state directory: created 0700, or narrowed to 0700 when
/// it exists with more. True when it had to be narrowed.
pub fn secure_own(directory: &Path) -> io::Result<bool> {
    create_private(directory)?;
    if !shared(directory)? {
        return Ok(false);
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(PRIVATE))?;
    Ok(true)
}

/// Whether anyone but its owner may list or enter `directory`.
pub fn shared(directory: &Path) -> io::Result<bool> {
    Ok(fs::metadata(directory)?.permissions().mode() & 0o077 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_directories_are_private_down_to_the_last() {
        let temporary = tempfile::tempdir().unwrap();
        let deep = temporary.path().join("state/spokenpad/dictation");
        create_private(&deep).unwrap();
        assert_eq!(mode(&temporary.path().join("state")), PRIVATE);
        assert_eq!(mode(&deep), PRIVATE);
        assert!(!shared(&deep).unwrap());
    }

    /// An existing directory keeps its mode: only spokenpad's own is
    /// narrowed.
    #[test]
    fn only_the_own_directory_is_narrowed() {
        let temporary = tempfile::tempdir().unwrap();
        let theirs = temporary.path().join("shared");
        fs::create_dir(&theirs).unwrap();
        fs::set_permissions(&theirs, fs::Permissions::from_mode(0o755)).unwrap();
        create_private(&theirs).unwrap();
        assert_eq!(mode(&theirs), 0o755);
        assert!(shared(&theirs).unwrap());
        assert!(secure_own(&theirs).unwrap(), "narrowed");
        assert_eq!(mode(&theirs), PRIVATE);
        assert!(!secure_own(&theirs).unwrap(), "already private");
    }
}
