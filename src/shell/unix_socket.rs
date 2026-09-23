//! Connecting to a Unix socket under a deadline, and asking who listens on it.
//!
//! Two clients use it: the window manager's IPC socket
//! ([`shell::wm`](crate::shell::wm)) and the dictation editor's RPC socket
//! ([`shell::nvim`](crate::shell::nvim)). Each words its own errors; this
//! module only says which way a connection failed.
use anyhow::ensure;
use std::{
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        unix::{ffi::OsStrExt as _, net::UnixStream},
    },
    path::Path,
    time::{Duration, Instant},
};

/// Why [`connect`] made no connection.
#[derive(Debug)]
pub(crate) enum ConnectError {
    /// The kernel refused it: no socket at the path, or nothing listening on
    /// it, or no permission to reach it.
    Refused(std::io::Error),
    /// The listener's accept queue stayed full until the deadline.
    TimedOut,
    /// No connection was tried: the path cannot be a Unix socket address, or
    /// no socket could be created.
    Other(anyhow::Error),
}

/// Connects to the Unix socket at `path` before `deadline`.
///
/// A blocking connect to a Unix socket waits for as long as the listener's
/// accept queue is full, which a listener that has stopped accepting leaves
/// it forever. So the socket is non-blocking while it connects. Linux then
/// answers at once: connected, refused, or `EAGAIN` while the queue is full
/// (`connect(2)`: Unix sockets answer `EAGAIN` where others answer
/// `EINPROGRESS`). `EAGAIN` is retried until the deadline. The stream is
/// blocking again afterwards; its callers set per-call timeouts.
pub(crate) fn connect(path: &Path, deadline: Instant) -> Result<UnixStream, ConnectError> {
    let (address, length) = address(path).map_err(ConnectError::Other)?;
    // SAFETY: `socket` takes no pointers.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(ConnectError::Other(
            anyhow::Error::from(std::io::Error::last_os_error()).context("create a Unix socket"),
        ));
    }
    // SAFETY: `fd` was just returned by `socket` and nothing else owns it.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    loop {
        // SAFETY: `address` is an initialised `sockaddr_un` at least `length`
        // bytes long, and `socket` is open for the duration of the call.
        let connected = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                length,
            )
        };
        if connected == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            // The listener's queue is full: it has stopped accepting, or is
            // slow to. Try again until the deadline says otherwise.
            Some(libc::EAGAIN) => {
                let left = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                    .ok_or(ConnectError::TimedOut)?;
                std::thread::sleep(left.min(Duration::from_millis(5)));
            }
            _ => return Err(ConnectError::Refused(error)),
        }
    }
    let stream = UnixStream::from(socket);
    stream
        .set_nonblocking(false)
        .map_err(|error| ConnectError::Other(error.into()))?;
    Ok(stream)
}

/// `path` as a Unix socket address, and the length to pass with it.
fn address(path: &Path) -> anyhow::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero `sockaddr_un` is a valid value of the type; the
    // family and path are filled in below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    ensure!(!bytes.contains(&0), "socket path contains NUL");
    // Room for the terminating NUL, which the zeroed address supplies.
    ensure!(
        bytes.len() < address.sun_path.len(),
        "socket path is too long for a Unix socket address"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    Ok((address, length))
}

/// The process, user and group on the other end of a connected Unix socket
/// (`SO_PEERCRED`: for the side that listens, as it was at `listen`).
pub(crate) fn peer_credentials(stream: &UnixStream) -> std::io::Result<libc::ucred> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` is a live `ucred` of `length` bytes, which is
    // what SO_PEERCRED writes, and `stream` is open for the whole call.
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast::<libc::c_void>(),
            &raw mut length,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(credentials)
}

/// A listener whose accept queue is full, as a listener that has stopped
/// accepting leaves it: the next connect would block for good.
#[cfg(test)]
pub(crate) struct FullQueue {
    _directory: tempfile::TempDir,
    pub(crate) socket: std::path::PathBuf,
    _listener: std::os::unix::net::UnixListener,
    _queued: Vec<UnixStream>,
}

#[cfg(test)]
impl FullQueue {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("full.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut queued = Vec::new();
        while let Ok(stream) = connect(&socket, Instant::now() + Duration::from_millis(200)) {
            queued.push(stream);
            assert!(queued.len() < 10_000, "the accept queue never filled");
        }
        Self {
            _directory: directory,
            socket,
            _listener: listener,
            _queued: queued,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn socket_path_must_be_nul_free_and_short_enough() {
        assert!(address(Path::new("/run/user/1000/spokenpad-nvim.sock")).is_ok());
        let nul = address(Path::new("/run/\0/sock")).unwrap_err().to_string();
        assert!(nul.contains("NUL"), "{nul}");
        let long = "a".repeat(108);
        let long = address(Path::new(&long)).unwrap_err().to_string();
        assert!(long.contains("too long"), "{long}");
        assert!(address(Path::new(&"a".repeat(107))).is_ok());
    }

    #[test]
    fn a_full_accept_queue_times_out_instead_of_hanging() {
        let full = FullQueue::new();
        let started = Instant::now();
        let failure = connect(&full.socket, Instant::now() + Duration::from_millis(200));
        assert!(
            matches!(failure, Err(ConnectError::TimedOut)),
            "{failure:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_socket_with_no_listener_is_refused_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("abandoned.sock");
        drop(UnixListener::bind(&socket).unwrap());
        let started = Instant::now();
        let failure = connect(&socket, Instant::now() + Duration::from_secs(5));
        assert!(
            matches!(&failure, Err(ConnectError::Refused(error))
                if error.kind() == std::io::ErrorKind::ConnectionRefused),
            "{failure:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
