//! Pure hotkey bookkeeping over the key events a keyboard reports.
//!
//! `WatcherState` folds `EV_KEY` events from any number of devices into the
//! session events they mean — which devices hold the hotkey down, and which
//! latch modifiers each of them reports — so the whole latch and
//! device-loss policy is unit-tested without touching a device.  Opening,
//! polling and reading those devices is [`shell::hotkey`](crate::shell::hotkey).
//!
//! `verdict` is the other half: given what an earlier scan concluded about a
//! `/dev/input/event*` path and the node behind it now, it decides whether the
//! path has to be probed again.

use crate::{config, core::state};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Instant,
};

/// The `value` of an `EV_KEY` event, which the kernel defines exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyAction {
    Release,
    Press,
    Repeat,
}

impl KeyAction {
    pub(crate) fn from_value(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Release),
            1 => Some(Self::Press),
            2 => Some(Self::Repeat),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyEvent {
    pub(crate) code: u16,
    pub(crate) action: KeyAction,
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
pub(crate) struct WatcherState {
    config: config::Hotkey,
    devices: HashMap<PathBuf, DeviceState>,
}

impl WatcherState {
    pub(crate) fn new(config: config::Hotkey) -> Self {
        Self {
            config,
            devices: HashMap::new(),
        }
    }

    /// The hotkey configuration this watcher was built for.
    pub(crate) fn config(&self) -> &config::Hotkey {
        &self.config
    }

    /// At least one watched device advertises the hotkey code.
    pub(crate) fn watches_hotkey(&self) -> bool {
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

    pub(crate) fn register(
        &mut self,
        path: PathBuf,
        supports_hotkey: bool,
        modifiers_held: HashSet<u16>,
    ) {
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
    pub(crate) fn step(
        &mut self,
        path: &Path,
        event: KeyEvent,
        now: Instant,
    ) -> Option<state::Event> {
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
    pub(crate) fn device_lost(&mut self, path: &Path, now: Instant) -> Option<state::Event> {
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
// Rescan bookkeeping
// ---------------------------------------------------------------------------

/// Identity of the node behind a `/dev/input/event*` path.
///
/// A replug can reuse the same name within one rescan interval, so the path
/// alone does not identify a device: devtmpfs creates a fresh node.  Reading
/// the numbers off a device node is the shell's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NodeId {
    rdev: u64,
    ino: u64,
}

impl NodeId {
    pub(crate) fn new(rdev: u64, ino: u64) -> Self {
        Self { rdev, ino }
    }
}

/// What the last scan concluded about a path, and about which node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Known {
    Watched(NodeId),
    /// Opened once and found uninteresting, unreadable, or broken.  Kept so a
    /// rescan does not re-run the open and ioctl battery twice a second.
    Rejected(NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Already examined at this node; nothing to do.
    Skip,
    Probe,
    /// The path now names a different node; forget the old one, then probe.
    Replace,
}

pub(crate) fn verdict(known: Option<Known>, node: NodeId) -> Verdict {
    match known {
        None => Verdict::Probe,
        Some(Known::Rejected(seen)) if seen == node => Verdict::Skip,
        Some(Known::Rejected(_)) => Verdict::Probe,
        Some(Known::Watched(seen)) if seen == node => Verdict::Skip,
        Some(Known::Watched(_)) => Verdict::Replace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal kernel key codes: nothing on this side of the split imports the
    // input-device crate.  `shell::hotkey::tests` ties these numbers to the
    // `KeyCode` names and to the defaults every test below assumes.
    const HOTKEY: u16 = 186; // KEY_F16
    const CANCEL: u16 = 1; // KEY_ESC
    const LEFT_SHIFT: u16 = 42; // KEY_LEFTSHIFT
    const RIGHT_SHIFT: u16 = 54; // KEY_RIGHTSHIFT
    const LEFT_CTRL: u16 = 29; // KEY_LEFTCTRL
    const OTHER: u16 = 30; // KEY_A

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
        let first = NodeId::new(0xd40, 7);
        let second = NodeId::new(0xd40, 8);
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
}
