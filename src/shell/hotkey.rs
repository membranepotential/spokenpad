//! Read-only evdev hotkey input: the device half of the watcher.
//!
//! Device nodes are deliberately opened with [`File::open`] and handed to
//! [`Device::from_fd`].  Do not replace that sequence with `Device::open`:
//! this process must never gain write access to, grab, or synthesize input on
//! a physical keyboard.
//!
//! The watcher thread is split in two.  Everything here opens, scans, polls
//! and reads devices; the bookkeeping it folds those reads into —
//! `WatcherState` and `verdict` — is pure and lives in
//! [`core::hotkey`](crate::core::hotkey).

use crate::{
    config,
    core::{
        hotkey::{KeyAction, KeyEvent, Known, NodeId, Verdict, WatcherState, verdict},
        state,
    },
};
use anyhow::{Context, Result, bail};
use evdev::{Device, EventType, KeyCode};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Write},
    os::fd::AsRawFd,
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const INPUT_DIR: &str = "/dev/input";
const RESCAN_INTERVAL: Duration = Duration::from_millis(500);

/// Background watcher for the configured evdev hotkey.
///
/// A lightweight periodic directory scan supplies hotplug support without a
/// udev dependency.  Consequently, a newly attached keyboard can take up to
/// `RESCAN_INTERVAL` to become active.
pub struct HotkeyWatcher {
    shutdown: Option<UnixStream>,
    worker: Option<JoinHandle<()>>,
}

impl HotkeyWatcher {
    /// Open readable matching input devices and start one polling worker.
    pub fn start(config: config::Hotkey, sender: Sender<state::Event>) -> Result<Self> {
        let (devices, state) = start_watching(config)?;
        let (shutdown_read, shutdown_write) =
            UnixStream::pair().context("create hotkey watcher shutdown socket")?;
        shutdown_read
            .set_nonblocking(true)
            .context("make hotkey watcher shutdown socket nonblocking")?;
        shutdown_write
            .set_nonblocking(true)
            .context("make hotkey watcher shutdown socket nonblocking")?;

        let worker = thread::Builder::new()
            .name("spokenpad-hotkey".to_owned())
            .spawn(move || run(sender, devices, state, shutdown_read))
            .context("start hotkey watcher thread")?;

        Ok(Self {
            shutdown: Some(shutdown_write),
            worker: Some(worker),
        })
    }

    /// Stop the worker and close every watched input device.
    pub fn stop(&mut self) {
        if let Some(mut shutdown) = self.shutdown.take() {
            let _ = shutdown.write_all(&[1]);
            let _ = shutdown.shutdown(std::net::Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !worker.is_finished() {
                log::error!("hotkey watcher did not stop within 1s; detaching for process exit");
            } else if worker.join().is_err() {
                log::error!("hotkey watcher thread panicked");
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for HotkeyWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Device discovery
// ---------------------------------------------------------------------------

struct OpenDevice {
    device: Device,
    node: NodeId,
}

/// Open descriptors plus the verdicts of earlier scans, keyed by path.
#[derive(Default)]
struct Devices {
    open: HashMap<PathBuf, OpenDevice>,
    rejected: HashMap<PathBuf, NodeId>,
}

impl Devices {
    fn known(&self, path: &Path) -> Option<Known> {
        if let Some(open) = self.open.get(path) {
            return Some(Known::Watched(open.node));
        }
        self.rejected.get(path).copied().map(Known::Rejected)
    }
}

/// A device worth watching, with the state it reports at open time.
struct Watched {
    device: Device,
    supports_hotkey: bool,
    modifiers_held: HashSet<u16>,
}

/// What one pass of [`scan`] probed; read for the startup diagnostics only.
#[derive(Default)]
struct ScanOutcome {
    paths: usize,
    readable: usize,
    permission_denied: usize,
}

fn start_watching(config: config::Hotkey) -> Result<(Devices, WatcherState)> {
    let mut devices = Devices::default();
    let mut state = WatcherState::new(config);
    let mut events = Vec::new();
    let outcome = scan(&mut devices, &mut state, &mut events)
        .with_context(|| format!("scan {INPUT_DIR} for input devices"))?;
    debug_assert!(events.is_empty(), "the first scan cannot lose a device");

    if state.watches_hotkey() {
        return Ok((devices, state));
    }
    if outcome.paths == 0 {
        bail!("no /dev/input/event* devices exist on this system");
    }
    if outcome.readable == 0 && outcome.permission_denied > 0 {
        bail!(
            "no /dev/input/event* device could be opened for reading; add yourself to the 'input' group and log out and back in"
        );
    }
    bail!(
        "no readable keyboard advertising the configured hotkey (evdev code {}) was found under {INPUT_DIR}",
        state.config().key_code
    )
}

/// One pass over [`INPUT_DIR`]: forget devices whose node vanished or was
/// replaced, probe paths not yet examined at their current node, and push any
/// session events those losses imply onto `events`.
fn scan(
    devices: &mut Devices,
    state: &mut WatcherState,
    events: &mut Vec<state::Event>,
) -> io::Result<ScanOutcome> {
    let paths = input_paths()?;
    let present: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let vanished: Vec<PathBuf> = devices
        .open
        .keys()
        .filter(|path| !present.contains(path.as_path()))
        .cloned()
        .collect();
    for path in vanished {
        events.extend(forget(&path, devices, state));
    }
    devices
        .rejected
        .retain(|path, _| present.contains(path.as_path()));

    let mut outcome = ScanOutcome {
        paths: paths.len(),
        ..ScanOutcome::default()
    };
    for path in paths {
        let node = match fs::metadata(&path) {
            Ok(metadata) => NodeId::new(metadata.rdev(), metadata.ino()),
            Err(error) => {
                log::debug!("could not stat {}: {error}", path.display());
                continue;
            }
        };
        match verdict(devices.known(&path), node) {
            Verdict::Skip => continue,
            Verdict::Probe => {}
            Verdict::Replace => {
                log::info!("{} now names a different device; reopening", path.display());
                events.extend(forget(&path, devices, state));
            }
        }
        match open_device(&path, state.config()) {
            Ok(Some(watched)) => {
                outcome.readable += 1;
                devices.rejected.remove(&path);
                devices.open.insert(
                    path.clone(),
                    OpenDevice {
                        device: watched.device,
                        node,
                    },
                );
                state.register(path, watched.supports_hotkey, watched.modifiers_held);
            }
            Ok(None) => {
                outcome.readable += 1;
                devices.rejected.insert(path, node);
            }
            Err(error) => {
                if error.kind() == io::ErrorKind::PermissionDenied {
                    outcome.permission_denied += 1;
                    log::debug!("no permission to read {}", path.display());
                } else {
                    log::debug!("could not inspect {}: {error}", path.display());
                }
                // Re-examined once the node behind the path is replaced; a
                // permission change alone is picked up on the next start.
                devices.rejected.insert(path, node);
            }
        }
    }
    Ok(outcome)
}

fn input_paths() -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(INPUT_DIR)? {
        let entry = entry?;
        if entry.file_name().as_encoded_bytes().starts_with(b"event") {
            paths.push(entry.path());
        }
    }
    paths.sort_unstable();
    Ok(paths)
}

fn open_device(path: &Path, config: &config::Hotkey) -> io::Result<Option<Watched>> {
    // File::open requests O_RDONLY. Device::open is intentionally forbidden
    // here because this safety boundary must remain explicit and reviewable.
    let file = File::open(path)?;
    let device = Device::from_fd(file.into())?;
    let Some(keys) = device.supported_keys() else {
        return Ok(None);
    };

    let supports_hotkey = keys.contains(KeyCode::new(config.key_code));
    let supports_cancel = config
        .cancel_key_code
        .is_some_and(|code| keys.contains(KeyCode::new(code)));
    let modifiers: Vec<u16> = config
        .latch_modifier
        .codes()
        .iter()
        .copied()
        .filter(|code| keys.contains(KeyCode::new(*code)))
        .collect();
    if !supports_hotkey && !supports_cancel && modifiers.is_empty() {
        return Ok(None);
    }

    device.set_nonblocking(true)?;
    // Seed the derived modifier state: a modifier may be held down already.
    let modifiers_held = match device.get_key_state() {
        Ok(held) => modifiers
            .into_iter()
            .filter(|code| held.contains(KeyCode::new(*code)))
            .collect(),
        Err(error) => {
            log::debug!("could not read key state from {}: {error}", path.display());
            HashSet::new()
        }
    };
    log::info!(
        "watching {} ({}) for hotkey input",
        path.display(),
        device.name().unwrap_or("unnamed device")
    );
    Ok(Some(Watched {
        device,
        supports_hotkey,
        modifiers_held,
    }))
}

/// Close and deregister one device, returning the session event that implies.
fn forget(path: &Path, devices: &mut Devices, state: &mut WatcherState) -> Option<state::Event> {
    devices.open.remove(path)?;
    log::info!("stopped watching {} (device removed)", path.display());
    state.device_lost(path, Instant::now())
}

// ---------------------------------------------------------------------------
// Poll / read / send shell
// ---------------------------------------------------------------------------

fn run(
    sender: Sender<state::Event>,
    mut devices: Devices,
    mut state: WatcherState,
    shutdown: UnixStream,
) {
    let mut next_rescan = Instant::now() + RESCAN_INTERVAL;
    let mut events = Vec::new();

    loop {
        let mut paths: Vec<PathBuf> = devices.open.keys().cloned().collect();
        paths.sort_unstable();
        let mut poll_fds = Vec::with_capacity(paths.len() + 1);
        poll_fds.push(libc::pollfd {
            fd: shutdown.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        poll_fds.extend(paths.iter().map(|path| libc::pollfd {
            fd: devices.open[path].device.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }));

        let timeout = next_rescan.saturating_duration_since(Instant::now());
        // SAFETY: `poll_fds` owns `poll_fds.len()` initialized pollfd values
        // for the duration of this call, and every fd in them is owned by
        // `shutdown` or by a device still in `devices.open`; poll neither
        // retains the pointer nor changes its length.
        let ready = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                timeout.as_millis() as libc::c_int,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("hotkey poll failed: {error}");
            return;
        }
        if poll_fds[0].revents != 0 {
            return;
        }

        let mut lost = Vec::new();
        for (path, descriptor) in paths.iter().zip(&poll_fds[1..]) {
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                lost.push(path.clone());
                continue;
            }
            if descriptor.revents & libc::POLLIN == 0 {
                continue;
            }
            let device = &mut devices
                .open
                .get_mut(path)
                .expect("polled device must remain registered")
                .device;
            match read_key_events(device) {
                Ok(keys) => events.extend(
                    keys.into_iter()
                        .filter_map(|key| state.step(path, key, Instant::now())),
                ),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    log::warn!("lost {}: {error}", path.display());
                    lost.push(path.clone());
                }
            }
        }
        for path in lost {
            events.extend(forget(&path, &mut devices, &mut state));
        }

        if Instant::now() >= next_rescan {
            if let Err(error) = scan(&mut devices, &mut state, &mut events) {
                log::debug!("could not rescan {INPUT_DIR}: {error}");
            }
            next_rescan = Instant::now() + RESCAN_INTERVAL;
        }

        for event in events.drain(..) {
            if sender.send(event).is_err() {
                return;
            }
        }
    }
}

fn read_key_events(device: &mut Device) -> io::Result<Vec<KeyEvent>> {
    Ok(device
        .fetch_events()?
        .filter(|event| event.event_type() == EventType::KEY)
        .filter_map(|event| {
            KeyAction::from_value(event.value()).map(|action| KeyEvent {
                code: event.code(),
                action,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// `core::hotkey::tests` drives the watcher with these codes as literals;
    /// this is what ties those numbers to the `KeyCode` names and the defaults.
    #[test]
    fn config_default_matches_the_key_codes_under_test() {
        let config = config::Hotkey::default();
        assert_eq!(config.key_code, KeyCode::KEY_F16.code());
        assert_eq!(config.cancel_key_code, Some(KeyCode::KEY_ESC.code()));
        assert_eq!(
            config.latch_modifier.codes(),
            [
                KeyCode::KEY_LEFTSHIFT.code(),
                KeyCode::KEY_RIGHTSHIFT.code()
            ]
        );
    }

    #[test]
    fn a_rejected_path_is_forgotten_when_it_disappears() {
        let mut devices = Devices::default();
        let path = PathBuf::from("/dev/input/event1");
        let node = NodeId::new(0xd40, 7);
        devices.rejected.insert(path.clone(), node);
        assert_eq!(devices.known(&path), Some(Known::Rejected(node)));

        let present: HashSet<&Path> = HashSet::new();
        devices
            .rejected
            .retain(|path, _| present.contains(path.as_path()));
        assert_eq!(devices.known(&path), None);
    }

    #[test]
    fn shutdown_unblocks_the_poll_loop() {
        let (sender, receiver) = mpsc::channel();
        let (read, mut write) = UnixStream::pair().expect("socket pair");
        read.set_nonblocking(true).expect("nonblocking");
        write.write_all(&[1]).expect("signal shutdown");

        let started = Instant::now();
        let worker = thread::spawn(move || {
            run(
                sender,
                Devices::default(),
                WatcherState::new(config::Hotkey::default()),
                read,
            )
        });
        let deadline = Instant::now() + Duration::from_millis(100);
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            worker.is_finished(),
            "run() must return as soon as the shutdown socket is readable"
        );
        worker.join().expect("worker thread");
        assert!(started.elapsed() < RESCAN_INTERVAL);
        drop(receiver);
    }
}
