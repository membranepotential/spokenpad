//! The msgpack-RPC transport to a running Neovim, and nothing else.
//!
//! Every call carries an absolute deadline rather than a per-operation
//! timeout: one request can involve several reads, and a peer that dribbles
//! bytes must not be able to extend the call indefinitely by staying just
//! inside a per-read timeout. The editor thread is on the dictation path, so
//! no call here may block without a way out.
use anyhow::{Result, anyhow, ensure};
use rmpv::Value;
use std::{
    io::{BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
    time::Instant,
};

/// Depth limit for a decoded reply, so a hostile or broken peer cannot make
/// the decoder recurse without bound.
const RPC_MAX_DEPTH: usize = 32;
/// Byte budget for one message on the wire: a request is refused if its
/// encoding exceeds this, and a reply is abandoned once it has read this
/// much. It is per message, not per session — a long dictation sends many
/// appends, each of which is bounded on its own.
const RPC_MAX_BYTES: usize = 8 * 1024 * 1024;

/// A failure of one RPC call, classified by what the caller can do about it.
#[derive(Debug)]
pub(super) enum RpcFailure {
    /// The deadline expired. For a request, the outcome is *unknown*: the
    /// call may still complete inside the editor after the reply was lost.
    Timeout(String),
    /// The socket file exists but nothing is listening on it.
    StaleSocket(std::io::Error),
    Other(anyhow::Error),
}

impl RpcFailure {
    pub(super) fn is_stale_socket(&self) -> bool {
        matches!(
            self,
            Self::StaleSocket(error)
                if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
        )
    }

    fn from_io(error: std::io::Error, timed_out: &str) -> Self {
        if matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) {
            Self::Timeout(timed_out.to_owned())
        } else {
            Self::Other(error.into())
        }
    }
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(message) => formatter.write_str(message),
            Self::StaleSocket(error) => error.fmt(formatter),
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl From<RpcFailure> for anyhow::Error {
    fn from(value: RpcFailure) -> Self {
        match value {
            RpcFailure::Timeout(message) => anyhow!(message),
            RpcFailure::StaleSocket(error) => error.into(),
            RpcFailure::Other(error) => error,
        }
    }
}

pub(super) struct RpcClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl RpcClient {
    pub(super) fn connect(path: &Path, deadline: Instant) -> Result<Self, RpcFailure> {
        let writer = connect_unix(path, deadline)?;
        let reader = BufReader::new(
            writer
                .try_clone()
                .map_err(|error| RpcFailure::Other(error.into()))?,
        );
        Ok(Self {
            reader,
            writer,
            next_id: 1,
        })
    }

    /// Sends a request and waits for its reply, discarding anything else the
    /// editor says on the channel meanwhile.
    pub(super) fn request(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        deadline: Instant,
    ) -> Result<Value, RpcFailure> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.send(
            Value::Array(vec![
                Value::from(0),
                Value::from(id),
                Value::from(method),
                Value::Array(arguments),
            ]),
            deadline,
        )?;
        loop {
            let mut reader = DeadlineRead::new(&mut self.reader, deadline, RPC_MAX_BYTES);
            let message = rmpv::decode::read_value_with_max_depth(&mut reader, RPC_MAX_DEPTH)
                .map_err(|error| {
                    RpcFailure::from_io(error.into(), &format!("nvim RPC {method} timed out"))
                })?;
            let Some(parts) = message.as_array() else {
                continue;
            };
            if parts.len() != 4 || parts[0].as_i64() != Some(1) || parts[1].as_u64() != Some(id) {
                continue;
            }
            if !parts[2].is_nil() {
                return Err(RpcFailure::Other(anyhow!(
                    "nvim RPC {method} failed: {}",
                    parts[2]
                )));
            }
            return Ok(parts[3].clone());
        }
    }

    /// Sends a notification: no reply, so the deadline bounds the write only.
    pub(super) fn notify(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        deadline: Instant,
    ) -> Result<(), RpcFailure> {
        self.send(
            Value::Array(vec![
                Value::from(2),
                Value::from(method),
                Value::Array(arguments),
            ]),
            deadline,
        )
    }

    fn send(&mut self, message: Value, deadline: Instant) -> Result<(), RpcFailure> {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &message)
            .map_err(|error| RpcFailure::Other(error.into()))?;
        if bytes.len() > RPC_MAX_BYTES {
            return Err(RpcFailure::Other(anyhow!(
                "nvim RPC request exceeded {RPC_MAX_BYTES} bytes"
            )));
        }
        let mut writer = DeadlineWrite::new(&mut self.writer, deadline);
        writer
            .write_all(&bytes)
            .map_err(|error| RpcFailure::from_io(error, "nvim RPC write timed out"))?;
        writer
            .flush()
            .map_err(|error| RpcFailure::from_io(error, "nvim RPC flush timed out"))
    }
}

/// A reader that enforces one absolute deadline and one byte budget across
/// however many `read` calls the decoder makes.
struct DeadlineRead<'a> {
    reader: &'a mut BufReader<UnixStream>,
    deadline: Instant,
    remaining: usize,
}

impl<'a> DeadlineRead<'a> {
    fn new(reader: &'a mut BufReader<UnixStream>, deadline: Instant, budget: usize) -> Self {
        Self {
            reader,
            deadline,
            remaining: budget,
        }
    }
}

impl Read for DeadlineRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining_time = self.deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "nvim RPC read deadline expired",
            ));
        }
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "nvim RPC response exceeded byte budget",
            ));
        }
        self.reader
            .get_ref()
            .set_read_timeout(Some(remaining_time))?;
        let capacity = buffer.len().min(self.remaining);
        let count = self.reader.read(&mut buffer[..capacity])?;
        self.remaining -= count;
        Ok(count)
    }
}

struct DeadlineWrite<'a> {
    writer: &'a mut UnixStream,
    deadline: Instant,
}

impl<'a> DeadlineWrite<'a> {
    fn new(writer: &'a mut UnixStream, deadline: Instant) -> Self {
        Self { writer, deadline }
    }
}

impl Write for DeadlineWrite<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.writer
            .set_write_timeout(Some(self.remaining("nvim RPC write deadline expired")?))?;
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer
            .set_write_timeout(Some(self.remaining("nvim RPC flush deadline expired")?))?;
        self.writer.flush()
    }
}

impl DeadlineWrite<'_> {
    fn remaining(&self, expired: &'static str) -> std::io::Result<std::time::Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, expired));
        }
        Ok(remaining)
    }
}

/// Connects to a Unix socket under a deadline.
///
/// `UnixStream::connect` has no timeout, so this opens a non-blocking socket,
/// polls it for writability, and only then turns blocking I/O back on: a
/// socket file left behind by a killed editor must fail fast rather than hang
/// the editor thread.
fn connect_unix(path: &Path, deadline: Instant) -> Result<UnixStream, RpcFailure> {
    let bytes = path.as_os_str().as_bytes();
    ensure_socket_path(bytes).map_err(RpcFailure::Other)?;
    // SAFETY: socket has no pointer arguments and returns a new owned descriptor.
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw_fd == -1 {
        return Err(RpcFailure::Other(std::io::Error::last_os_error().into()));
    }
    // SAFETY: `raw_fd` was just returned by socket and has no other owner.
    let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    // SAFETY: zero is a valid initial representation for sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes.iter().copied()) {
        *target = source as libc::c_char;
    }
    let address_length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: address points to an initialized sockaddr_un of `address_length`
    // bytes, and the descriptor is owned and live for the call.
    let result = unsafe {
        libc::connect(
            owned.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_length,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(RpcFailure::StaleSocket(error));
        }
        wait_for_connect(owned.as_raw_fd(), deadline)?;
    }
    set_blocking(owned.as_raw_fd()).map_err(|error| RpcFailure::Other(error.into()))?;
    Ok(UnixStream::from(owned))
}

fn ensure_socket_path(path: &[u8]) -> Result<()> {
    ensure!(!path.contains(&0), "nvim socket path contains NUL");
    ensure!(
        path.len()
            < std::mem::size_of::<libc::sockaddr_un>()
                - std::mem::offset_of!(libc::sockaddr_un, sun_path),
        "nvim socket path is too long"
    );
    Ok(())
}

fn wait_for_connect(fd: libc::c_int, deadline: Instant) -> Result<(), RpcFailure> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RpcFailure::Timeout(
                "nvim socket connect timed out".to_owned(),
            ));
        }
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor is valid for one pollfd element for this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready == 0 {
            return Err(RpcFailure::Timeout(
                "nvim socket connect timed out".to_owned(),
            ));
        }
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(RpcFailure::Other(error.into()));
        }
        let mut socket_error: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        // SAFETY: the descriptor is live and both pointers reference writable
        // objects of exactly the sizes passed alongside them.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut socket_error).cast(),
                &mut length,
            )
        } == -1
        {
            return Err(RpcFailure::Other(std::io::Error::last_os_error().into()));
        }
        if socket_error != 0 {
            return Err(RpcFailure::StaleSocket(std::io::Error::from_raw_os_error(
                socket_error,
            )));
        }
        return Ok(());
    }
}

fn set_blocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fd is owned and live; fcntl does not retain pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above; flags came from F_GETFL for this descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixListener, thread, time::Duration};

    #[test]
    fn socket_path_must_be_nul_free_and_short_enough() {
        assert!(ensure_socket_path(b"/run/user/1000/spokenpad-nvim.sock").is_ok());
        let nul = ensure_socket_path(b"/run/\0/sock").unwrap_err().to_string();
        assert!(nul.contains("NUL"), "{nul}");
        let long = ensure_socket_path(&[b'a'; 512]).unwrap_err().to_string();
        assert!(long.contains("too long"), "{long}");
    }

    #[test]
    fn a_socket_file_with_no_listener_is_reported_as_stale() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("abandoned.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        drop(listener);
        let failure = RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1))
            .err()
            .expect("nothing is listening");
        assert!(failure.is_stale_socket(), "{failure}");
        assert!(socket.exists(), "the abandoned socket file must remain");
    }

    #[test]
    fn request_has_one_deadline_while_bytes_dribble_in() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("dribble.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            let mut response = Vec::new();
            rmpv::encode::write_value(
                &mut response,
                &Value::Array(vec![
                    Value::from(1),
                    Value::from(1),
                    Value::Nil,
                    Value::from(1),
                ]),
            )
            .unwrap();
            for byte in response {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Instant::now() + Duration::from_millis(100),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(300));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn request_times_out_when_peer_never_replies() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("stalled.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(200));
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Instant::now() + Duration::from_millis(50),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn a_reply_to_another_request_is_skipped_rather_than_returned() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("interleaved.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            let mut response = Vec::new();
            // A notification from the editor, then a reply carrying a
            // different request id, then the one this caller is waiting for.
            for message in [
                Value::Array(vec![
                    Value::from(2),
                    Value::from("redraw"),
                    Value::Array(Vec::new()),
                ]),
                Value::Array(vec![
                    Value::from(1),
                    Value::from(99),
                    Value::Nil,
                    Value::from("stale"),
                ]),
                Value::Array(vec![
                    Value::from(1),
                    Value::from(1),
                    Value::Nil,
                    Value::from("mine"),
                ]),
            ] {
                rmpv::encode::write_value(&mut response, &message).unwrap();
            }
            let _ = stream.write_all(&response);
            thread::sleep(Duration::from_millis(50));
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let value = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(value.as_str(), Some("mine"));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn an_error_reply_becomes_an_error_not_a_value() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("failing.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            let mut response = Vec::new();
            rmpv::encode::write_value(
                &mut response,
                &Value::Array(vec![
                    Value::from(1),
                    Value::from(1),
                    Value::Array(vec![Value::from(0), Value::from("Vim:E5108")]),
                    Value::Nil,
                ]),
            )
            .unwrap();
            let _ = stream.write_all(&response);
            thread::sleep(Duration::from_millis(50));
        });
        let mut client =
            RpcClient::connect(&socket, Instant::now() + Duration::from_secs(1)).unwrap();
        let error = client
            .request(
                "nvim_exec_lua",
                vec![Value::from("error('boom')")],
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Other(_)), "{error}");
        assert!(error.to_string().contains("E5108"), "{error}");
        drop(client);
        peer.join().unwrap();
    }
}
