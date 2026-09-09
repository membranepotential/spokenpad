//! Read-only evdev hotkey input.
//!
//! Device nodes are deliberately opened with [`File::open`] and handed to
//! [`Device::from_fd`].  Do not replace that sequence with `Device::open`:
//! this process must never gain write access to, grab, or synthesize input on
//! a physical keyboard.

use crate::{config, state};
use anyhow::{Context, Result, bail};
use evdev::{AttributeSet, Device, EventType, KeyCode};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, Write},
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
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
/// [`RESCAN_INTERVAL`] to become active.
pub struct HotkeyWatcher {
    shutdown: Option<UnixStream>,
    worker: Option<JoinHandle<()>>,
}

impl HotkeyWatcher {
    /// Open readable matching input devices and start one polling worker.
    pub fn start(config: config::Hotkey, sender: Sender<state::Event>) -> Result<Self> {
        let devices = initial_devices(&config)?;
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
            .spawn(move || run(config, sender, devices, shutdown_read))
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

struct WatchedDevice {
    device: Device,
    supports_hotkey: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputIntent {
    HotkeyDown,
    HotkeyUp,
    Cancel,
}

fn input_intent(config: &config::Hotkey, code: u16, value: i32) -> Option<InputIntent> {
    // Linux key values are 0 = release, 1 = press, and 2 = auto-repeat.
    // In particular, discard repeats here before they can reach the queue.
    match (code, value) {
        (code, 1) if code == config.key_code => Some(InputIntent::HotkeyDown),
        (code, 0) if code == config.key_code => Some(InputIntent::HotkeyUp),
        (code, 1) if config.cancel_key_code == Some(code) => Some(InputIntent::Cancel),
        _ => None,
    }
}

#[derive(Default)]
struct PressedHotkey {
    sources: HashSet<PathBuf>,
}

impl PressedHotkey {
    fn down(&mut self, source: &Path) -> bool {
        self.sources.insert(source.to_owned()) && self.sources.len() == 1
    }

    fn up(&mut self, source: &Path) -> bool {
        self.sources.remove(source) && self.sources.is_empty()
    }

    /// Returns true when removing this source releases the aggregate key.
    fn remove_source(&mut self, source: &Path) -> bool {
        self.up(source)
    }
}

fn initial_devices(config: &config::Hotkey) -> Result<HashMap<PathBuf, WatchedDevice>> {
    let paths = input_paths().with_context(|| format!("scan {INPUT_DIR} for input devices"))?;
    if paths.is_empty() {
        bail!("no /dev/input/event* devices exist on this system");
    }

    let mut devices = HashMap::new();
    let mut saw_readable = false;
    let mut saw_permission_denied = false;
    for path in paths {
        match open_device(&path, config) {
            Ok(Some(device)) => {
                saw_readable = true;
                devices.insert(path, device);
            }
            Ok(None) => saw_readable = true,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                saw_permission_denied = true;
                log::warn!("no permission to read {}", path.display());
            }
            Err(error) => log::debug!("could not inspect {}: {error}", path.display()),
        }
    }

    if devices.values().any(|device| device.supports_hotkey) {
        return Ok(devices);
    }
    if !saw_readable && saw_permission_denied {
        bail!(
            "no /dev/input/event* device could be opened for reading; add yourself to the 'input' group and log out and back in"
        );
    }
    bail!(
        "no readable keyboard advertising the configured hotkey (evdev code {}) was found under {INPUT_DIR}",
        config.key_code
    )
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

fn open_device(path: &Path, config: &config::Hotkey) -> io::Result<Option<WatchedDevice>> {
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
    let supports_modifier = config
        .latch_modifier
        .codes()
        .iter()
        .any(|code| keys.contains(KeyCode::new(*code)));
    if !(supports_hotkey || supports_cancel || supports_modifier) {
        return Ok(None);
    }

    device.set_nonblocking(true)?;
    log::info!(
        "watching {} ({}) for hotkey input",
        path.display(),
        device.name().unwrap_or("unnamed device")
    );
    Ok(Some(WatchedDevice {
        device,
        supports_hotkey,
    }))
}

fn run(
    config: config::Hotkey,
    sender: Sender<state::Event>,
    mut devices: HashMap<PathBuf, WatchedDevice>,
    shutdown: UnixStream,
) {
    let mut pressed = PressedHotkey::default();
    let mut next_rescan = Instant::now() + RESCAN_INTERVAL;

    loop {
        let mut paths: Vec<_> = devices.keys().cloned().collect();
        paths.sort_unstable();
        let mut poll_fds = Vec::with_capacity(paths.len() + 1);
        poll_fds.push(libc::pollfd {
            fd: shutdown.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        poll_fds.extend(paths.iter().map(|path| libc::pollfd {
            fd: devices[path].device.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }));

        let timeout = next_rescan.saturating_duration_since(Instant::now());
        // SAFETY: `poll_fds` owns `len` initialized pollfd values for the
        // duration of this call; poll neither retains nor changes the pointer.
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

        let mut dropped = Vec::new();
        for (path, descriptor) in paths.iter().zip(&poll_fds[1..]) {
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                dropped.push(path.clone());
                continue;
            }
            if descriptor.revents & libc::POLLIN == 0 {
                continue;
            }
            match read_events(path, &mut devices) {
                Ok(events) => {
                    for (code, value) in events {
                        let Some(intent) = input_intent(&config, code, value) else {
                            continue;
                        };
                        let event = match intent {
                            InputIntent::HotkeyDown if pressed.down(path) => state::Event::Down {
                                at: Instant::now(),
                                latch: latch_held(&devices, config.latch_modifier.codes()),
                            },
                            InputIntent::HotkeyUp if pressed.up(path) => {
                                state::Event::Up { at: Instant::now() }
                            }
                            InputIntent::Cancel => state::Event::Cancel,
                            _ => continue,
                        };
                        if sender.send(event).is_err() {
                            return;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    log::info!("lost {}: {error}", path.display());
                    dropped.push(path.clone());
                }
            }
        }

        for path in dropped {
            if !drop_device(&path, &mut devices, &mut pressed, &sender) {
                return;
            }
        }
        if Instant::now() >= next_rescan {
            if !rescan(&config, &sender, &mut devices, &mut pressed) {
                return;
            }
            next_rescan = Instant::now() + RESCAN_INTERVAL;
        }
    }
}

fn read_events(
    path: &Path,
    devices: &mut HashMap<PathBuf, WatchedDevice>,
) -> io::Result<Vec<(u16, i32)>> {
    let device = &mut devices
        .get_mut(path)
        .expect("polled device must remain registered")
        .device;
    Ok(device
        .fetch_events()?
        .filter(|event| event.event_type() == EventType::KEY)
        .map(|event| (event.code(), event.value()))
        .collect())
}

fn latch_held(devices: &HashMap<PathBuf, WatchedDevice>, codes: &[u16]) -> bool {
    if codes.is_empty() {
        return false;
    }
    // Query every watched device eagerly. A modifier-only keyboard is watched
    // specifically so Shift on one keyboard can latch a hotkey on another.
    let states: Vec<AttributeSet<KeyCode>> = devices
        .iter()
        .filter_map(|(path, device)| match device.device.get_key_state() {
            Ok(state) => Some(state),
            Err(error) => {
                log::debug!("could not read key state from {}: {error}", path.display());
                None
            }
        })
        .collect();
    any_modifier_held(codes, &states, |state, code| {
        state.contains(KeyCode::new(code))
    })
}

fn any_modifier_held<T>(codes: &[u16], states: &[T], contains: impl Fn(&T, u16) -> bool) -> bool {
    states
        .iter()
        .any(|state| codes.iter().any(|code| contains(state, *code)))
}

fn rescan(
    config: &config::Hotkey,
    sender: &Sender<state::Event>,
    devices: &mut HashMap<PathBuf, WatchedDevice>,
    pressed: &mut PressedHotkey,
) -> bool {
    let paths = match input_paths() {
        Ok(paths) => paths,
        Err(error) => {
            log::debug!("could not rescan {INPUT_DIR}: {error}");
            return true;
        }
    };
    let present: HashSet<_> = paths.iter().cloned().collect();
    let missing: Vec<_> = devices
        .keys()
        .filter(|path| !present.contains(*path))
        .cloned()
        .collect();
    for path in missing {
        if !drop_device(&path, devices, pressed, sender) {
            return false;
        }
    }

    for path in paths {
        if devices.contains_key(&path) {
            continue;
        }
        match open_device(&path, config) {
            Ok(Some(device)) => {
                devices.insert(path, device);
            }
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                log::debug!("no permission to read {}", path.display());
            }
            Err(error) => log::debug!("could not inspect {}: {error}", path.display()),
        }
    }
    true
}

fn drop_device(
    path: &Path,
    devices: &mut HashMap<PathBuf, WatchedDevice>,
    pressed: &mut PressedHotkey,
    sender: &Sender<state::Event>,
) -> bool {
    let Some(removed) = devices.remove(path) else {
        return true;
    };
    log::info!("stopped watching {} (device removed)", path.display());

    let released_last_held_source = pressed.remove_source(path);
    let lost_last_hotkey_device =
        removed.supports_hotkey && !devices.values().any(|device| device.supports_hotkey);
    if released_last_held_source || lost_last_hotkey_device {
        // Device loss is not an intentional key release. Cancel rather than
        // decode partial audio, and ensure latched recording cannot get stuck
        // with no remaining hotkey capable device.
        return sender.send(state::Event::Cancel).is_ok();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_repeat_and_non_hotkey_values() {
        let config = config::Hotkey::default();
        assert_eq!(
            input_intent(&config, config.key_code, 1),
            Some(InputIntent::HotkeyDown)
        );
        assert_eq!(
            input_intent(&config, config.key_code, 0),
            Some(InputIntent::HotkeyUp)
        );
        assert_eq!(input_intent(&config, config.key_code, 2), None);
        assert_eq!(input_intent(&config, config.key_code, 3), None);
        assert_eq!(input_intent(&config, 1, 1), Some(InputIntent::Cancel));
        assert_eq!(input_intent(&config, 1, 0), None);
        assert_eq!(input_intent(&config, 30, 1), None);
    }

    #[test]
    fn aggregates_hotkey_state_across_devices() {
        let first = Path::new("event1");
        let second = Path::new("event2");
        let mut pressed = PressedHotkey::default();

        assert!(pressed.down(first));
        assert!(!pressed.down(first));
        assert!(!pressed.down(second));
        assert!(!pressed.up(first));
        assert!(pressed.up(second));
        assert!(!pressed.up(second));
    }

    #[test]
    fn unplug_only_releases_the_last_held_source() {
        let first = Path::new("event1");
        let second = Path::new("event2");
        let mut pressed = PressedHotkey::default();
        pressed.down(first);
        pressed.down(second);

        assert!(!pressed.remove_source(first));
        assert!(pressed.remove_source(second));
    }

    #[test]
    fn latch_modifier_can_be_held_on_another_device() {
        let states = [vec![29_u16], vec![42_u16]];
        assert!(any_modifier_held(&[42, 54], &states, |state, code| {
            state.contains(&code)
        }));
        assert!(!any_modifier_held(&[56, 100], &states, |state, code| {
            state.contains(&code)
        }));
        assert!(!any_modifier_held(&[], &states, |state, code| {
            state.contains(&code)
        }));
    }
}
