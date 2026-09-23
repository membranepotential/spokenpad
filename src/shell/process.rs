//! Short-lived helper processes, such as `systemctl --user`, bounded in time
//! and output: a helper that hangs, or a descendant that keeps its pipe open,
//! cannot hold the caller past its deadline.
use anyhow::{Context, Result, bail};
use std::{
    io::{ErrorKind, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Runs a helper and returns its standard output, bounded in time and output.
pub(crate) fn run(args: &[&str]) -> Result<String> {
    run_bounded(args, TIMEOUT, MAX_OUTPUT_BYTES)
}

fn run_bounded(args: &[&str], timeout: Duration, max_output: usize) -> Result<String> {
    let (program, tail) = args.split_first().context("empty subprocess command")?;
    let mut child = Command::new(program)
        .args(tail)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let mut stdout = child.stdout.take().context("capture subprocess stdout")?;
    set_nonblocking(stdout.as_raw_fd()).context("make subprocess stdout nonblocking")?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut status: Option<ExitStatus> = None;
    let mut eof = false;
    while status.is_none() || !eof {
        drain_output(&mut stdout, &mut bytes, max_output, &mut eof).inspect_err(|_| {
            terminate_group(&mut child);
        })?;
        if status.is_none() {
            status = child.try_wait().context("poll subprocess")?;
        }
        if status.is_some() && eof {
            break;
        }
        if Instant::now() >= deadline {
            terminate_group(&mut child);
            bail!("{program} timed out after {:.3}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = status.context("subprocess ended without an exit status")?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    String::from_utf8(bytes).context("subprocess emitted non-UTF-8 output")
}

fn set_nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `fd` is a live pipe descriptor and F_GETFL/F_SETFL do not retain pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above; the flags came from F_GETFL for this descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn drain_output(
    stdout: &mut impl Read,
    bytes: &mut Vec<u8>,
    max_output: usize,
    eof: &mut bool,
) -> Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                *eof = true;
                return Ok(());
            }
            Ok(count) => {
                if bytes.len().saturating_add(count) > max_output {
                    bail!("subprocess output exceeded {max_output} bytes");
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("read subprocess stdout"),
        }
    }
}

fn terminate_group(child: &mut Child) {
    let process_group = i32::try_from(child.id()).unwrap_or(i32::MAX);
    // SAFETY: a negative PID targets only the child-created process group.
    let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_runner_rejects_excess_output() {
        let error = run_bounded(&["sh", "-c", "printf 123456789"], Duration::from_secs(1), 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeded 8 bytes"), "{error}");
    }

    #[test]
    fn bounded_runner_does_not_wait_on_a_descendants_pipe() {
        let started = Instant::now();
        let error = run_bounded(
            &["sh", "-c", "(sleep 5) &"],
            Duration::from_millis(100),
            1024,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
