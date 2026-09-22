//! The control socket: how key bindings reach the daemon.
//!
//! The daemon listens on a Unix socket ([`config::control_socket`]) and
//! `spokenpad start|stop|toggle|cancel` each send one request over it. The
//! daemon reads no input device: which key does what is the window manager's
//! business, configured by the user. The protocol is
//! [`core::control`](crate::core::control).
//!
//! The socket is either bound by the daemon itself or, under systemd socket
//! activation, bound by systemd and handed over as file descriptor 3
//! ([`Socket::from_systemd`]). Only the daemon's own socket is removed when
//! the daemon stops: systemd's belongs to `spokenpad.socket`, and keeps
//! queueing presses across daemon restarts.
//!
//! [`config::control_socket`]: crate::config::control_socket

use crate::core::control::{MAX_LINE, Received, Reply, Request};
use anyhow::{Context, Result, bail, ensure};
use std::{
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How long the daemon waits for a client's request line. A client is a
/// process that writes its request right after connecting, so anything
/// slower is stuck, and the daemon must not wait on it: requests are served
/// one at a time, in arrival order.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a client waits for the daemon's reply. Under socket activation
/// the first press of a session connects before the daemon runs, and is
/// answered once systemd has started the daemon and it has taken the socket
/// over: well under this.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the listener looks at its stop flag while no client connects.
const STOP_POLL: Duration = Duration::from_millis(100);
/// The first file descriptor systemd passes (`SD_LISTEN_FDS_START`).
const LISTEN_FDS_START: libc::c_int = 3;

/// The listening control socket, and who owns its path.
pub enum Socket {
    /// The daemon bound it, and removes the path when it stops.
    Bound {
        listener: UnixListener,
        path: PathBuf,
    },
    /// systemd bound it (`spokenpad.socket`) and passed it in. The path is
    /// systemd's: it stays, and keeps accepting presses, after the daemon
    /// exits.
    Inherited(UnixListener),
}

impl Socket {
    /// Binds `path`, replacing a socket file no process is listening on any
    /// more, and refusing when one is. The socket is made private to the
    /// user: whoever can connect can start the microphone.
    pub fn bind(path: &Path) -> Result<Self> {
        remove_stale(path)?;
        let listener = UnixListener::bind(path)
            .with_context(|| format!("listen on control socket {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("make {} private", path.display()))?;
        Ok(Self::Bound {
            listener,
            path: path.to_owned(),
        })
    }

    /// The listening socket systemd passed this process, if it passed one:
    /// file descriptor 3, when `LISTEN_PID` names this process and
    /// `LISTEN_FDS` is 1. It must be a listening Unix stream socket bound to
    /// `expected`, or this fails rather than serve a socket the key bindings
    /// do not connect to.
    ///
    /// Removes `LISTEN_PID`, `LISTEN_FDS` and `LISTEN_FDNAMES` from the
    /// environment either way, and marks the descriptor close-on-exec, so
    /// that no process the daemon starts (an editor, a terminal) mistakes
    /// itself for the activated service.
    ///
    /// # Safety
    ///
    /// No other thread may be running: this modifies the environment, which
    /// is undefined behaviour while another thread may read it.
    pub unsafe fn from_systemd(expected: &Path) -> Result<Option<Self>> {
        let passed = passed_to_us(
            env::var_os("LISTEN_PID").as_deref(),
            env::var_os("LISTEN_FDS").as_deref(),
            std::process::id(),
        );
        for name in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
            // SAFETY: the caller guarantees that this process runs no other
            // thread, so nothing reads the environment concurrently.
            unsafe { env::remove_var(name) };
        }
        if !passed? {
            return Ok(None);
        }
        // SAFETY: F_GETFD reads the flags of an integer descriptor and
        // touches no memory; a descriptor that is not open yields EBADF.
        let flags = unsafe { libc::fcntl(LISTEN_FDS_START, libc::F_GETFD) };
        ensure!(
            flags != -1,
            "systemd passed file descriptor {LISTEN_FDS_START}, but it is not open: {}",
            io::Error::last_os_error()
        );
        // SAFETY: descriptor 3 is open (checked above), systemd passed it to
        // this process to own, and nothing in this process has adopted it:
        // the caller runs this before any other code takes a descriptor over.
        let fd = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START) };
        // SAFETY: F_SETFD sets the flags of the descriptor `fd` owns and
        // touches no memory.
        let set = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if set == -1 {
            return Err(io::Error::last_os_error()).context("mark the passed socket close-on-exec");
        }
        let listening_unix_stream = socket_option(fd.as_fd(), libc::SO_DOMAIN)
            .context("inspect the socket systemd passed")?
            == libc::AF_UNIX
            && socket_option(fd.as_fd(), libc::SO_TYPE)? == libc::SOCK_STREAM
            && socket_option(fd.as_fd(), libc::SO_ACCEPTCONN)? == 1;
        ensure!(
            listening_unix_stream,
            "the socket systemd passed is not a listening Unix stream socket; spokenpad.socket needs ListenStream= with a path"
        );
        let listener = UnixListener::from(fd);
        let address = listener
            .local_addr()
            .context("read the address of the socket systemd passed")?;
        ensure!(
            address.as_pathname() == Some(expected),
            "systemd passed a socket bound to {address:?}, but key bindings connect to {}; spokenpad.socket must listen on %t/spokenpad.sock",
            expected.display()
        );
        Ok(Some(Self::Inherited(listener)))
    }
}

/// Whether `LISTEN_PID` and `LISTEN_FDS` pass exactly one descriptor to the
/// process `pid`. Variables meant for another process (a `LISTEN_PID` that is
/// not ours) are no activation; any other count is a unit that does not
/// match this program, and an error.
fn passed_to_us(listen_pid: Option<&OsStr>, listen_fds: Option<&OsStr>, pid: u32) -> Result<bool> {
    let Some(listen_pid) = listen_pid else {
        return Ok(false);
    };
    if listen_pid.to_str().and_then(|p| p.parse::<u32>().ok()) != Some(pid) {
        return Ok(false);
    }
    let count = listen_fds
        .and_then(OsStr::to_str)
        .and_then(|n| n.parse::<u32>().ok())
        .context("LISTEN_PID names this process, but LISTEN_FDS is not a number")?;
    ensure!(
        count == 1,
        "systemd passed {count} sockets; spokenpad.socket must define exactly one ListenStream="
    );
    Ok(true)
}

/// One integer `SOL_SOCKET` option of `fd`.
fn socket_option(fd: BorrowedFd<'_>, name: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `length` are initialised locals that outlive the
    // call, and `length` holds `value`'s exact size, so the kernel writes at
    // most that many bytes to it; `fd` is borrowed for the whole call.
    let result = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            name,
            (&raw mut value).cast(),
            &raw mut length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

/// Listens on the control socket and forwards each request, stamped, to the
/// session. Stops when dropped, and then removes the socket file if the
/// daemon bound it.
pub struct ControlServer {
    /// The path to remove on drop: only that of a socket this process bound.
    bound: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ControlServer {
    pub fn start(socket: Socket, sender: Sender<Received>) -> Result<Self> {
        let (listener, bound) = match socket {
            Socket::Bound { listener, path } => (listener, Some(path)),
            Socket::Inherited(listener) => (listener, None),
        };
        listener
            .set_nonblocking(true)
            .context("make the control socket nonblocking")?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("spokenpad-control".into())
            .spawn(move || serve(&listener, &sender, &stopping))
            .context("start the control socket thread")?;
        Ok(Self {
            bound,
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::error!("control socket thread panicked");
        }
        // Only this process can be listening here: nothing binds a path
        // another listener answers on (see `remove_stale`).
        if let Some(path) = &self.bound
            && let Err(e) = fs::remove_file(path)
        {
            log::debug!("could not remove {}: {e}", path.display());
        }
    }
}

/// A socket file nobody answers on is what a crashed daemon leaves behind.
/// Anything else at the path is not ours to delete.
fn remove_stale(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        );
    }
    match UnixStream::connect(path) {
        Ok(_) => bail!(
            "another spokenpad daemon is listening on {}",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            log::info!("removing stale control socket {}", path.display());
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("probe {}", path.display())),
    }
}

fn serve(listener: &UnixListener, sender: &Sender<Received>, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = answer(stream, sender) {
                    log::warn!("control request failed: {e:#}");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_readable(listener),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                log::error!("control socket failed: {e}");
                return;
            }
        }
    }
}

/// Blocks until a client connects or [`STOP_POLL`] passes.
fn wait_readable(listener: &UnixListener) {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `descriptor` is one initialized pollfd that outlives the call,
    // matching the count of 1; its fd is owned by `listener`, borrowed for
    // the whole call. poll retains no pointer. Any error, EINTR included,
    // only sends the caller round its loop again.
    unsafe {
        libc::poll(&mut descriptor, 1, STOP_POLL.as_millis() as libc::c_int);
    }
}

/// Serves one connection: read the request, stamp it, queue it, reply.
fn answer(mut stream: UnixStream, sender: &Sender<Received>) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let line = read_line(&mut stream)?;
    let at = Instant::now();
    let reply = match Request::decode(&line) {
        Ok(request) => match sender.send(Received { request, at }) {
            Ok(()) => {
                log::debug!("control: {request}");
                Reply::Accepted
            }
            Err(_) => Reply::ShuttingDown,
        },
        Err(reply) => {
            log::warn!(
                "control: rejected {:?} ({reply})",
                String::from_utf8_lossy(&line)
            );
            reply
        }
    };
    stream.write_all(reply.encode().as_bytes())?;
    Ok(())
}

/// Reads up to and including the first newline, at most [`MAX_LINE`] bytes.
/// Returns what arrived when the peer stops early; the decoder rejects it.
fn read_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut line = Vec::with_capacity(MAX_LINE);
    let mut byte = [0_u8];
    while line.len() < MAX_LINE && !line.ends_with(b"\n") {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(line)
}

/// Why a request did not reach the daemon.
#[derive(Debug)]
pub enum SendError {
    /// Nothing is listening: neither `spokenpad.socket` nor a daemon started
    /// by hand holds the socket.
    NoDaemon { path: PathBuf, cause: io::Error },
    /// The daemon answered, but not with `ok`.
    Refused(Reply),
    /// The connection failed after it was made, or the reply was not one this
    /// version knows.
    Failed(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDaemon { path, cause } => write!(
                f,
                "no spokenpad daemon is listening on {} ({cause}); `spokenpad.socket` starts it on the first press: check `systemctl --user status spokenpad.socket`",
                path.display()
            ),
            Self::Refused(reply) => write!(f, "the daemon refused the request: {reply}"),
            Self::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SendError {}

/// Sends one request and waits for the daemon's acknowledgement: what
/// `spokenpad start|stop|toggle|cancel` do.
pub fn send(path: &Path, request: Request) -> Result<(), SendError> {
    let mut stream = UnixStream::connect(path).map_err(|cause| SendError::NoDaemon {
        path: path.to_owned(),
        cause,
    })?;
    match exchange(&mut stream, request).map_err(SendError::Failed)? {
        Reply::Accepted => Ok(()),
        reply => Err(SendError::Refused(reply)),
    }
}

fn exchange(stream: &mut UnixStream, request: Request) -> Result<Reply> {
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    stream.write_all(request.encode().as_bytes())?;
    let line = read_line(stream).context("read the daemon's reply")?;
    Reply::decode(&line).with_context(|| {
        format!(
            "unexpected reply {:?}; is the daemon the same version as this command?",
            String::from_utf8_lossy(&line)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_request_reaches_the_session_stamped_and_is_acknowledged() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (sender, receiver) = mpsc::channel();
        let server = ControlServer::start(Socket::bind(&path).unwrap(), sender).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the socket is private");
        for request in Request::ALL {
            let before = Instant::now();
            send(&path, request).unwrap();
            let received = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(received.request, request);
            assert!(received.at >= before && received.at <= Instant::now());
        }
        drop(server);
        assert!(!path.exists(), "the socket is removed on shutdown");
        assert!(matches!(
            send(&path, Request::Start),
            Err(SendError::NoDaemon { .. })
        ));
    }

    #[test]
    fn garbage_is_rejected_and_not_forwarded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (sender, receiver) = mpsc::channel();
        let _server = ControlServer::start(Socket::bind(&path).unwrap(), sender).unwrap();
        for (line, reply) in [
            (&b"record\n"[..], Reply::UnknownRequest),
            (b"start", Reply::Malformed),
            (&[b'x'; MAX_LINE][..], Reply::Malformed),
        ] {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream.write_all(line).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut answer = Vec::new();
            stream.read_to_end(&mut answer).unwrap();
            assert_eq!(Reply::decode(&answer), Some(reply), "{line:?}");
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "a crashed daemon leaves its socket behind");
        let (sender, _receiver) = mpsc::channel();
        let live = ControlServer::start(Socket::bind(&path).unwrap(), sender.clone()).unwrap();
        let error = Socket::bind(&path)
            .err()
            .expect("a second listener on a live socket");
        assert!(
            error.to_string().contains("another spokenpad daemon"),
            "{error}"
        );
        drop(live);

        fs::write(&path, "not a socket").unwrap();
        let error = Socket::bind(&path)
            .err()
            .expect("a plain file is not replaced");
        assert!(error.to_string().contains("not a socket"), "{error}");
    }

    /// A socket systemd bound belongs to `spokenpad.socket`: the daemon stops
    /// serving it, and leaves it where it is for the next activation.
    #[test]
    fn an_inherited_socket_is_served_and_left_in_place() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (sender, receiver) = mpsc::channel();
        let server = ControlServer::start(
            Socket::Inherited(UnixListener::bind(&path).unwrap()),
            sender,
        )
        .unwrap();
        send(&path, Request::Toggle).unwrap();
        let received = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(received.request, Request::Toggle);
        drop(server);
        assert!(path.exists(), "systemd's socket file stays");
    }

    #[test]
    fn only_one_descriptor_passed_to_this_process_is_an_activation() {
        let os = |s: &'static str| Some(OsStr::new(s));
        assert!(!passed_to_us(None, None, 7).unwrap(), "not activated");
        assert!(
            !passed_to_us(os("8"), os("1"), 7).unwrap(),
            "the variables were meant for another process"
        );
        assert!(passed_to_us(os("7"), os("1"), 7).unwrap());
        assert!(passed_to_us(os("7"), os("2"), 7).is_err(), "two sockets");
        assert!(passed_to_us(os("7"), os("0"), 7).is_err(), "no socket");
        assert!(passed_to_us(os("7"), None, 7).is_err(), "no count");
    }

    #[test]
    fn a_daemon_that_is_shutting_down_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let (sender, receiver) = mpsc::channel();
        let _server = ControlServer::start(Socket::bind(&path).unwrap(), sender).unwrap();
        drop(receiver);
        assert!(matches!(
            send(&path, Request::Stop),
            Err(SendError::Refused(Reply::ShuttingDown))
        ));
    }
}
