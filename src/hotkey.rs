//! Read-only evdev hotkey input.
//!
//! Device nodes are deliberately opened with [`File::open`] and handed to
//! [`Device::from_fd`].  Do not replace that sequence with `Device::open`:
//! this process must never gain write access to, grab, or synthesize input on
//! a physical keyboard.
//!
//! The watcher thread is split in two.  [`WatcherState`] is pure bookkeeping
//! over the key events it is handed — which devices hold the hotkey down and
//! which latch modifiers each of them reports — and is unit-tested without
//! touching a device.  [`run`] is the poll/read/send shell around it.

use crate::{config, state};
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
/// [`RESCAN_INTERVAL`] to become active.
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
// Pure watcher bookkeeping
// ---------------------------------------------------------------------------

/// The `value` of an `EV_KEY` event, which the kernel defines exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Release,
    Press,
    Repeat,
}

impl KeyAction {
    fn from_value(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Release),
            1 => Some(Self::Press),
            2 => Some(Self::Repeat),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyEvent {
    code: u16,
    action: KeyAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputIntent {
    HotkeyDown,
    HotkeyUp,
    Cancel,
}

/// What one watched device currently reports.
struct DeviceState {
    /// The device advertises the configured hotkey code.
    supports_hotkey: bool,
    /// The hotkey is held down on this device.
    hotkey_down: bool,
    /// Configured latch modifier codes held down on this device.
    modifiers_held: HashSet<u16>,
}

/// Aggregate hotkey state across every watched keyboard.
///
/// Modifier state is derived from the observed event stream — seeded from the
/// kernel when a device is registered — so the latch is evaluated in event
/// order rather than by querying the kernel after a batch has been read.
struct WatcherState {
    config: config::Hotkey,
    devices: HashMap<PathBuf, DeviceState>,
}

impl WatcherState {
    fn new(config: config::Hotkey) -> Self {
        Self {
            config,
            devices: HashMap::new(),
        }
    }

    /// At least one watched device advertises the hotkey code.
    fn watches_hotkey(&self) -> bool {
        self.devices.values().any(|device| device.supports_hotkey)
    }

    fn any_hotkey_down(&self) -> bool {
        self.devices.values().any(|device| device.hotkey_down)
    }

    /// A latch modifier is held on *some* keyboard: a modifier-only keyboard is
    /// watched specifically so Shift on one can latch the hotkey on another.
    fn latch_held(&self) -> bool {
        self.devices
            .values()
            .any(|device| !device.modifiers_held.is_empty())
    }

    fn register(&mut self, path: PathBuf, supports_hotkey: bool, modifiers_held: HashSet<u16>) {
        let was_missing = !self.watches_hotkey();
        self.devices.insert(
            path,
            DeviceState {
                supports_hotkey,
                hotkey_down: false,
                modifiers_held,
            },
        );
        if supports_hotkey && was_missing {
            log::info!(
                "a keyboard advertising the hotkey (evdev code {}) is being watched",
                self.config.key_code
            );
        }
    }

    /// Fold one key event from `path` in, returning the session event it means.
    fn step(&mut self, path: &Path, event: KeyEvent, now: Instant) -> Option<state::Event> {
        self.track_modifier(path, event);
        match self.intent(event)? {
            InputIntent::HotkeyDown => {
                let latch = self.latch_held();
                self.set_hotkey_down(path, true)
                    .then_some(state::Event::Down { at: now, latch })
            }
            InputIntent::HotkeyUp => self
                .set_hotkey_down(path, false)
                .then_some(state::Event::Up { at: now }),
            InputIntent::Cancel => Some(state::Event::Cancel),
        }
    }

    /// Stop watching `path`, reporting whether the hotkey went with it.
    ///
    /// Two losses end a recording, and both for the same reason — the key that
    /// would have ended it is gone: the keyboard holding the hotkey down, and
    /// the last keyboard that can report the hotkey at all.  The latter is
    /// what a *latched* recording depends on: its key is already up, so
    /// nothing else would ever stop it, and the cancel key left on the same
    /// keyboard.  What was said is decoded, not thrown away.
    fn device_lost(&mut self, path: &Path, now: Instant) -> Option<state::Event> {
        let lost = self.devices.remove(path)?;
        let released = lost.hotkey_down && !self.any_hotkey_down();
        if released {
            log::warn!(
                "{} disappeared while the hotkey was held; decoding what was said",
                path.display()
            );
        }
        let unreachable = lost.supports_hotkey && !self.watches_hotkey();
        if unreachable {
            log::warn!(
                "no watched keyboard advertises the hotkey (evdev code {}) any more; \
                 ending any recording",
                self.config.key_code
            );
        }
        (released || unreachable).then_some(state::Event::HotkeyLost { at: now })
    }

    fn intent(&self, event: KeyEvent) -> Option<InputIntent> {
        match event.action {
            KeyAction::Press if event.code == self.config.key_code => Some(InputIntent::HotkeyDown),
            KeyAction::Release if event.code == self.config.key_code => Some(InputIntent::HotkeyUp),
            KeyAction::Press if self.config.cancel_key_code == Some(event.code) => {
                Some(InputIntent::Cancel)
            }
            // Auto-repeat is discarded here, before it can reach the queue.
            _ => None,
        }
    }

    fn track_modifier(&mut self, path: &Path, event: KeyEvent) {
        if !self.config.latch_modifier.codes().contains(&event.code) {
            return;
        }
        let Some(device) = self.devices.get_mut(path) else {
            return;
        };
        match event.action {
            KeyAction::Press | KeyAction::Repeat => device.modifiers_held.insert(event.code),
            KeyAction::Release => device.modifiers_held.remove(&event.code),
        };
    }

    /// Returns whether this changed the aggregate across all keyboards.
    fn set_hotkey_down(&mut self, path: &Path, down: bool) -> bool {
        let before = self.any_hotkey_down();
        let Some(device) = self.devices.get_mut(path) else {
            return false;
        };
        device.hotkey_down = down;
        before != self.any_hotkey_down()
    }
}

// ---------------------------------------------------------------------------
// Device discovery
// ---------------------------------------------------------------------------

/// Identity of the node behind a `/dev/input/event*` path.
///
/// A replug can reuse the same name within one [`RESCAN_INTERVAL`], so the
/// path alone does not identify a device: devtmpfs creates a fresh node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeId {
    rdev: u64,
    ino: u64,
}

impl NodeId {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            rdev: metadata.rdev(),
            ino: metadata.ino(),
        }
    }
}

/// What the last scan concluded about a path, and about which node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    Watched(NodeId),
    /// Opened once and found uninteresting, unreadable, or broken.  Kept so a
    /// rescan does not re-run the open and ioctl battery twice a second.
    Rejected(NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Already examined at this node; nothing to do.
    Skip,
    Probe,
    /// The path now names a different node; forget the old one, then probe.
    Replace,
}

fn verdict(known: Option<Known>, node: NodeId) -> Verdict {
    match known {
        None => Verdict::Probe,
        Some(Known::Rejected(seen)) if seen == node => Verdict::Skip,
        Some(Known::Rejected(_)) => Verdict::Probe,
        Some(Known::Watched(seen)) if seen == node => Verdict::Skip,
        Some(Known::Watched(_)) => Verdict::Replace,
    }
}

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
        state.config.key_code
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
            Ok(metadata) => NodeId::of(&metadata),
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
        match open_device(&path, &state.config) {
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

    const HOTKEY: u16 = KeyCode::KEY_F16.code();
    const CANCEL: u16 = KeyCode::KEY_ESC.code();
    const LEFT_SHIFT: u16 = KeyCode::KEY_LEFTSHIFT.code();
    const RIGHT_SHIFT: u16 = KeyCode::KEY_RIGHTSHIFT.code();
    const LEFT_CTRL: u16 = KeyCode::KEY_LEFTCTRL.code();
    const OTHER: u16 = KeyCode::KEY_A.code();

    /// Comparable shape of what a step emitted; `state::Event` carries an
    /// `Instant` and is not `PartialEq`.
    #[derive(Debug, PartialEq, Eq)]
    enum Emitted {
        Nothing,
        Down { latch: bool },
        Up,
        Lost,
        Cancel,
    }

    impl From<Option<state::Event>> for Emitted {
        fn from(event: Option<state::Event>) -> Self {
            match event {
                None => Self::Nothing,
                Some(state::Event::Down { latch, .. }) => Self::Down { latch },
                Some(state::Event::Up { .. }) => Self::Up,
                Some(state::Event::HotkeyLost { .. }) => Self::Lost,
                Some(state::Event::Cancel) => Self::Cancel,
                Some(state::Event::Finished) => unreachable!("the watcher never emits Finished"),
            }
        }
    }

    fn watcher(paths: &[&str]) -> WatcherState {
        let mut state = WatcherState::new(config::Hotkey::default());
        for path in paths {
            state.register(PathBuf::from(path), true, HashSet::new());
        }
        state
    }

    fn feed(state: &mut WatcherState, path: &str, code: u16, action: KeyAction) -> Emitted {
        state
            .step(Path::new(path), KeyEvent { code, action }, Instant::now())
            .into()
    }

    fn press(state: &mut WatcherState, path: &str, code: u16) -> Emitted {
        feed(state, path, code, KeyAction::Press)
    }

    fn release(state: &mut WatcherState, path: &str, code: u16) -> Emitted {
        feed(state, path, code, KeyAction::Release)
    }

    #[test]
    fn config_default_matches_the_key_codes_under_test() {
        let config = config::Hotkey::default();
        assert_eq!(config.key_code, HOTKEY);
        assert_eq!(config.cancel_key_code, Some(CANCEL));
        assert_eq!(config.latch_modifier.codes(), [LEFT_SHIFT, RIGHT_SHIFT]);
    }

    #[test]
    fn key_actions_cover_press_release_and_repeat_only() {
        assert_eq!(KeyAction::from_value(0), Some(KeyAction::Release));
        assert_eq!(KeyAction::from_value(1), Some(KeyAction::Press));
        assert_eq!(KeyAction::from_value(2), Some(KeyAction::Repeat));
        assert_eq!(KeyAction::from_value(3), None);
        assert_eq!(KeyAction::from_value(-1), None);
    }

    #[test]
    fn maps_presses_releases_and_cancel() {
        let mut state = watcher(&["event1"]);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: false }
        );
        assert_eq!(release(&mut state, "event1", HOTKEY), Emitted::Up);
        assert_eq!(press(&mut state, "event1", CANCEL), Emitted::Cancel);
        assert_eq!(release(&mut state, "event1", CANCEL), Emitted::Nothing);
        assert_eq!(press(&mut state, "event1", OTHER), Emitted::Nothing);
    }

    #[test]
    fn ignores_auto_repeat_of_the_hotkey() {
        let mut state = watcher(&["event1"]);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: false }
        );
        assert_eq!(
            feed(&mut state, "event1", HOTKEY, KeyAction::Repeat),
            Emitted::Nothing
        );
        assert_eq!(
            feed(&mut state, "event1", CANCEL, KeyAction::Repeat),
            Emitted::Nothing
        );
        assert_eq!(release(&mut state, "event1", HOTKEY), Emitted::Up);
    }

    #[test]
    fn ignores_events_from_an_unregistered_device() {
        let mut state = watcher(&["event1"]);
        assert_eq!(press(&mut state, "event9", HOTKEY), Emitted::Nothing);
        assert!(!state.any_hotkey_down());
    }

    #[test]
    fn aggregates_the_hotkey_across_devices() {
        let mut state = watcher(&["event1", "event2"]);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: false }
        );
        assert_eq!(press(&mut state, "event2", HOTKEY), Emitted::Nothing);
        assert_eq!(release(&mut state, "event1", HOTKEY), Emitted::Nothing);
        assert_eq!(release(&mut state, "event2", HOTKEY), Emitted::Up);
        assert_eq!(release(&mut state, "event2", HOTKEY), Emitted::Nothing);
    }

    #[test]
    fn latch_follows_the_order_of_the_observed_events() {
        let mut state = watcher(&["event1"]);
        assert_eq!(press(&mut state, "event1", LEFT_SHIFT), Emitted::Nothing);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: true }
        );
        release(&mut state, "event1", HOTKEY);

        // A modifier released earlier in the same batch loses the latch.
        assert_eq!(release(&mut state, "event1", LEFT_SHIFT), Emitted::Nothing);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: false }
        );
    }

    #[test]
    fn latch_survives_auto_repeat_and_a_second_modifier() {
        let mut state = watcher(&["event1"]);
        press(&mut state, "event1", LEFT_SHIFT);
        press(&mut state, "event1", RIGHT_SHIFT);
        feed(&mut state, "event1", LEFT_SHIFT, KeyAction::Repeat);
        release(&mut state, "event1", LEFT_SHIFT);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: true }
        );
    }

    #[test]
    fn latch_modifier_can_be_held_on_another_device() {
        let mut state = watcher(&["event1", "event2"]);
        press(&mut state, "event2", LEFT_SHIFT);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: true }
        );
    }

    #[test]
    fn unconfigured_modifiers_never_latch() {
        let mut state = watcher(&["event1"]);
        press(&mut state, "event1", LEFT_CTRL);
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: false }
        );
    }

    #[test]
    fn modifier_state_is_seeded_at_registration() {
        let mut state = WatcherState::new(config::Hotkey::default());
        state.register(PathBuf::from("event1"), true, HashSet::from([LEFT_SHIFT]));
        assert_eq!(
            press(&mut state, "event1", HOTKEY),
            Emitted::Down { latch: true }
        );
    }

    #[test]
    fn losing_the_device_that_holds_the_hotkey_ends_the_recording() {
        let mut state = watcher(&["event1", "event2"]);
        press(&mut state, "event1", HOTKEY);
        let lost = state.device_lost(Path::new("event1"), Instant::now());
        assert_eq!(Emitted::from(lost), Emitted::Lost);
        assert!(!state.any_hotkey_down());
        assert!(state.watches_hotkey(), "event2 can still report the hotkey");
    }

    /// The latched case: nothing is held, so only the disappearance of the
    /// last keyboard that can report the hotkey can end the recording — and
    /// the cancel key went with it.
    #[test]
    fn losing_the_last_hotkey_keyboard_ends_the_recording() {
        let mut state = watcher(&["event1"]);
        assert_eq!(
            Emitted::from(state.device_lost(Path::new("event1"), Instant::now())),
            Emitted::Lost
        );
        assert!(!state.watches_hotkey());
    }

    /// A keyboard that never advertised the hotkey is only a modifier source.
    #[test]
    fn losing_a_modifier_only_keyboard_emits_nothing() {
        let mut state = watcher(&["event1"]);
        state.register(PathBuf::from("event2"), false, HashSet::new());
        assert_eq!(
            Emitted::from(state.device_lost(Path::new("event2"), Instant::now())),
            Emitted::Nothing
        );
    }

    #[test]
    fn losing_a_device_while_idle_emits_nothing() {
        let mut state = watcher(&["event1", "event2"]);
        assert_eq!(
            Emitted::from(state.device_lost(Path::new("event1"), Instant::now())),
            Emitted::Nothing
        );
        assert_eq!(
            Emitted::from(state.device_lost(Path::new("event9"), Instant::now())),
            Emitted::Nothing
        );
        assert!(state.watches_hotkey());
    }

    #[test]
    fn losing_one_of_two_held_devices_keeps_the_key_down() {
        let mut state = watcher(&["event1", "event2"]);
        press(&mut state, "event1", HOTKEY);
        press(&mut state, "event2", HOTKEY);
        assert_eq!(
            Emitted::from(state.device_lost(Path::new("event1"), Instant::now())),
            Emitted::Nothing
        );
        assert!(state.any_hotkey_down());
        assert_eq!(release(&mut state, "event2", HOTKEY), Emitted::Up);
    }

    #[test]
    fn a_path_is_reprobed_only_when_its_node_changes() {
        let first = NodeId {
            rdev: 0xd40,
            ino: 7,
        };
        let second = NodeId {
            rdev: 0xd40,
            ino: 8,
        };
        assert_eq!(verdict(None, first), Verdict::Probe);
        assert_eq!(verdict(Some(Known::Rejected(first)), first), Verdict::Skip);
        assert_eq!(
            verdict(Some(Known::Rejected(first)), second),
            Verdict::Probe
        );
        assert_eq!(verdict(Some(Known::Watched(first)), first), Verdict::Skip);
        assert_eq!(
            verdict(Some(Known::Watched(first)), second),
            Verdict::Replace
        );
    }

    #[test]
    fn a_rejected_path_is_forgotten_when_it_disappears() {
        let mut devices = Devices::default();
        let path = PathBuf::from("/dev/input/event1");
        let node = NodeId {
            rdev: 0xd40,
            ino: 7,
        };
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
