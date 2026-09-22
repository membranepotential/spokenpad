//! xkbcommon, loaded when the pane needs it rather than linked into the
//! binary.
//!
//! The pane is one mode out of three, and the other two open no window.
//! A binary that *listed* libxcb and libxkbcommon as needed would refuse to
//! start at all on a machine that has neither — a laptop with a minimal
//! Wayland install, a server — for a feature that user never asked for. So
//! both are opened with `dlopen` the first time a pane is opened, and a
//! missing one is an error that names the package to install, not a dynamic
//! linker message before `main`.
//!
//! What is here is the thin safe layer over the parts of the C API the pane
//! uses: a context, a keymap and a state read from the X server, and the
//! Compose machine that turns a dead key and a letter into one character.
//! Every object is reference-counted by the library and released in `Drop`.
//! [`keyboard`](super::keyboard) is the only caller.
use anyhow::{Result, anyhow, bail};
use std::{
    ffi::{CStr, CString},
    os::raw::c_char,
    ptr::NonNull,
};
use xkbcommon_dl as ffi;

/// Modifier names, as xkbcommon spells them. These are the *real* modifiers
/// X11 numbers from 0; `Mod1` is what a PC keyboard labels Alt and `Mod4`
/// what it labels Super.
pub const MOD_SHIFT: &[u8] = ffi::XKB_MOD_NAME_SHIFT;
pub const MOD_CONTROL: &[u8] = ffi::XKB_MOD_NAME_CTRL;
pub const MOD_ALT: &[u8] = ffi::XKB_MOD_NAME_ALT;
pub const MOD_LOGO: &[u8] = ffi::XKB_MOD_NAME_LOGO;

/// Which state components a modifier may be active in. "Effective" is the
/// union, which is what decides how a key is spelled.
const STATE_MODS_EFFECTIVE: ffi::xkb_state_component =
    ffi::xkb_state_component::XKB_STATE_MODS_EFFECTIVE;

/// The shared libraries a pane needs, in the order it needs them, with the
/// package that carries each one on a distribution that splits them.
const LIBRARIES: [(&str, &str); 3] = [
    ("libxcb.so.1", "libxcb"),
    ("libxkbcommon.so.0", "libxkbcommon"),
    ("libxkbcommon-x11.so.0", "libxkbcommon-x11"),
];

/// Open every library the pane needs, or say which one is missing.
///
/// Safe to call more than once: each library is opened once and remembered.
/// `spokenpad check` calls it to report readiness before anything is opened.
pub fn load() -> Result<()> {
    x11rb::xcb_ffi::load_libxcb().map_err(|error| missing(LIBRARIES[0], &error.to_string()))?;
    if ffi::xkbcommon_option().is_none() {
        bail!(missing(LIBRARIES[1], "dlopen failed"));
    }
    if ffi::x11::xkbcommon_x11_option().is_none() {
        bail!(missing(LIBRARIES[2], "dlopen failed"));
    }
    Ok(())
}

/// The libraries this build would load, for a readiness report.
pub fn libraries() -> [(&'static str, &'static str); 3] {
    LIBRARIES
}

fn missing((file, package): (&str, &str), reason: &str) -> anyhow::Error {
    anyhow!(
        "{file} could not be loaded ({reason}); \
         nvim.mode = \"pane\" needs it — install the {package} package"
    )
}

/// The handles, once loaded. Every call below goes through one of these, and
/// each panics only if called before [`load`] succeeded, which no path does:
/// [`Context::new`] is the entry point and it loads first.
fn base() -> &'static ffi::XkbCommon {
    ffi::xkbcommon_handle()
}

fn compose_library() -> &'static ffi::XkbCommonCompose {
    ffi::xkbcommon_compose_handle()
}

fn x11() -> &'static ffi::x11::XkbCommonX11 {
    ffi::x11::xkbcommon_x11_handle()
}

/// The text one of xkbcommon's `*_get_utf8` calls writes, `write` being
/// that call with its buffer and the buffer's length. The call reports the
/// length it needs when the buffer is too small, but a keysym's or a
/// sequence's UTF-8 is at most a few bytes: one call with a generous buffer
/// is always enough, and the length is checked anyway. Empty when nothing
/// is typed.
fn utf8_written_by(write: impl FnOnce(*mut c_char, usize) -> std::os::raw::c_int) -> String {
    let mut buffer = [0_u8; 64];
    let written = write(buffer.as_mut_ptr().cast::<c_char>(), buffer.len());
    usize::try_from(written)
        .ok()
        .and_then(|length| buffer.get(..length.min(buffer.len() - 1)))
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default()
}

/// An xkbcommon context: the root object every other one is made from.
pub struct Context(NonNull<ffi::xkb_context>);

impl Context {
    pub fn new() -> Result<Self> {
        load()?;
        // SAFETY: the library is loaded, and `xkb_context_new` takes only
        // flags and returns either a new context or null.
        let raw = unsafe { (base().xkb_context_new)(ffi::xkb_context_flags::XKB_CONTEXT_NO_FLAGS) };
        NonNull::new(raw)
            .map(Self)
            .ok_or_else(|| anyhow!("xkbcommon would not create a context"))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `xkb_context_new`, has not been
        // released before, and is not used again.
        unsafe { (base().xkb_context_unref)(self.0.as_ptr()) };
    }
}

/// A compiled keyboard layout.
pub struct Keymap(NonNull<ffi::xkb_keymap>);

impl Drop for Keymap {
    fn drop(&mut self) {
        // SAFETY: as in `Context::drop`; the keymap owns one reference.
        unsafe { (base().xkb_keymap_unref)(self.0.as_ptr()) };
    }
}

/// Which keys are held, and which layout group is active.
pub struct State(NonNull<ffi::xkb_state>);

impl State {
    /// Replace the modifier and layout state wholesale.
    ///
    /// The pane passes the mask X11 puts on each key event rather than
    /// tracking press and release, so this is a fact about now, not a running
    /// total that could drift.
    pub fn update_mask(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        // SAFETY: a live state, and six integers.
        unsafe {
            (base().xkb_state_update_mask)(self.0.as_ptr(), depressed, latched, locked, 0, 0, group)
        };
    }

    /// The one keysym this keycode produces in the current state.
    pub fn keysym(&self, keycode: u32) -> u32 {
        // SAFETY: a live state, and a keycode the library range-checks itself
        // (an unmapped one yields `XKB_KEY_NoSymbol`).
        unsafe { (base().xkb_state_key_get_one_sym)(self.0.as_ptr(), keycode) }
    }

    /// The text this keycode types in the current state, which is empty for a
    /// key that types nothing.
    pub fn text(&self, keycode: u32) -> String {
        utf8_written_by(|buffer, length| {
            // SAFETY: a live state, and a buffer of exactly the length passed
            // with it. The library writes at most that many bytes and NUL
            // terminates within them.
            unsafe { (base().xkb_state_key_get_utf8)(self.0.as_ptr(), keycode, buffer, length) }
        })
    }

    /// Whether the named modifier is in effect.
    pub fn modifier_is_active(&self, name: &'static [u8]) -> bool {
        // SAFETY: `name` is one of the NUL-terminated constants above, and
        // the state is live. The call only reads.
        let active = unsafe {
            (base().xkb_state_mod_name_is_active)(
                self.0.as_ptr(),
                name.as_ptr().cast::<c_char>(),
                STATE_MODS_EFFECTIVE,
            )
        };
        active > 0
    }
}

impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: as in `Context::drop`; the state owns one reference.
        unsafe { (base().xkb_state_unref)(self.0.as_ptr()) };
    }
}

/// What the Compose machine made of the last key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compose {
    /// Not part of a sequence; use the key as it came.
    Nothing,
    /// The start or middle of a sequence: this press types nothing.
    Composing,
    /// A sequence finished; its result is the keysym and text given.
    Composed,
    /// A sequence was abandoned: this press types nothing either.
    Cancelled,
}

/// The Compose table for a locale, which is what makes a dead key work.
pub struct ComposeTable(NonNull<ffi::xkb_compose_table>);

impl ComposeTable {
    /// The table for `locale`, or `None` where that locale has none — normal,
    /// and then dead keys simply do not compose.
    pub fn new(context: &Context, locale: &str) -> Option<Self> {
        let locale = CString::new(locale).ok()?;
        // SAFETY: a live context and a NUL-terminated locale name that
        // outlives the call. The library copies what it needs.
        let raw = unsafe {
            (compose_library().xkb_compose_table_new_from_locale)(
                context.0.as_ptr(),
                locale.as_ptr(),
                ffi::xkb_compose_compile_flags::XKB_COMPOSE_COMPILE_NO_FLAGS,
            )
        };
        NonNull::new(raw).map(Self)
    }
}

impl Drop for ComposeTable {
    fn drop(&mut self) {
        // SAFETY: as in `Context::drop`.
        unsafe { (compose_library().xkb_compose_table_unref)(self.0.as_ptr()) };
    }
}

/// How far into a Compose sequence the keyboard is.
///
/// The table is kept beside the state because the state borrows it in C: the
/// library's own reference keeps it alive, and holding both in one value means
/// no caller can drop them in the wrong order.
pub struct ComposeState {
    state: NonNull<ffi::xkb_compose_state>,
    _table: ComposeTable,
}

impl ComposeState {
    pub fn new(table: ComposeTable) -> Option<Self> {
        // SAFETY: a live table; the new state takes its own reference to it.
        let raw = unsafe {
            (compose_library().xkb_compose_state_new)(
                table.0.as_ptr(),
                ffi::xkb_compose_state_flags::XKB_COMPOSE_STATE_NO_FLAGS,
            )
        };
        NonNull::new(raw).map(|state| Self {
            state,
            _table: table,
        })
    }

    /// Offer a keysym to the sequence and say what became of it.
    ///
    /// Modifier keysyms are ignored by the library itself, so a held Shift
    /// does not cancel a sequence.
    pub fn feed(&mut self, keysym: u32) -> Compose {
        // SAFETY: a live state and a keysym, which is just a number.
        unsafe { (compose_library().xkb_compose_state_feed)(self.state.as_ptr(), keysym) };
        // SAFETY: as above; this only reads the state.
        let status =
            unsafe { (compose_library().xkb_compose_state_get_status)(self.state.as_ptr()) };
        match status {
            ffi::xkb_compose_status::XKB_COMPOSE_COMPOSING => Compose::Composing,
            ffi::xkb_compose_status::XKB_COMPOSE_COMPOSED => Compose::Composed,
            ffi::xkb_compose_status::XKB_COMPOSE_CANCELLED => Compose::Cancelled,
            ffi::xkb_compose_status::XKB_COMPOSE_NOTHING => Compose::Nothing,
        }
    }

    /// What a finished sequence produced.
    pub fn result(&self) -> (u32, String) {
        // SAFETY: a live state; this only reads.
        let keysym =
            unsafe { (compose_library().xkb_compose_state_get_one_sym)(self.state.as_ptr()) };
        let text = utf8_written_by(|buffer, length| {
            // SAFETY: a live state, and a buffer of exactly the length passed
            // with it. The library writes at most that many bytes and NUL
            // terminates within them.
            unsafe {
                (compose_library().xkb_compose_state_get_utf8)(self.state.as_ptr(), buffer, length)
            }
        });
        (keysym, text)
    }

    /// Forget a finished or abandoned sequence.
    pub fn reset(&mut self) {
        // SAFETY: a live state.
        unsafe { (compose_library().xkb_compose_state_reset)(self.state.as_ptr()) };
    }
}

impl Drop for ComposeState {
    fn drop(&mut self) {
        // SAFETY: as in `Context::drop`. The table is dropped after this,
        // which is the order C wants.
        unsafe { (compose_library().xkb_compose_state_unref)(self.state.as_ptr()) };
    }
}

/// The X11 half: reading the layout the server has loaded, for the device it
/// calls the core keyboard.
pub mod x11_device {
    use super::*;
    use x11rb::xcb_ffi::XCBConnection;

    /// Announce the XKB extension on this connection. Nothing below works
    /// until this has succeeded.
    pub fn setup(connection: &XCBConnection) -> Result<()> {
        let raw = raw_connection(connection);
        let (mut major, mut minor, mut event, mut error) = (0_u16, 0_u16, 0_u8, 0_u8);
        // SAFETY: a live connection, and four out-parameters the library
        // writes exactly one value into each of.
        let ok = unsafe {
            (x11().xkb_x11_setup_xkb_extension)(
                raw,
                ffi::x11::XKB_X11_MIN_MAJOR_XKB_VERSION,
                ffi::x11::XKB_X11_MIN_MINOR_XKB_VERSION,
                ffi::x11::xkb_x11_setup_xkb_extension_flags::XKB_X11_SETUP_XKB_EXTENSION_NO_FLAGS,
                &mut major,
                &mut minor,
                &mut event,
                &mut error,
            )
        };
        if ok == 0 {
            bail!("this X server has no usable XKB extension, so the pane cannot read the layout");
        }
        Ok(())
    }

    /// The device number of the keyboard the server calls the core one.
    pub fn core_keyboard(connection: &XCBConnection) -> Result<i32> {
        // SAFETY: a live connection; the call only reads from the server.
        let device =
            unsafe { (x11().xkb_x11_get_core_keyboard_device_id)(raw_connection(connection)) };
        if device < 0 {
            bail!("the X server named no core keyboard device");
        }
        Ok(device)
    }

    /// The layout that device has loaded, and a state to read it with.
    pub fn layout(
        context: &Context,
        connection: &XCBConnection,
        device: i32,
    ) -> Result<(Keymap, State)> {
        let raw = raw_connection(connection);
        // SAFETY: a live context and connection, and a device number the
        // server gave us. Returns a new keymap or null.
        let keymap = unsafe {
            (x11().xkb_x11_keymap_new_from_device)(
                context.0.as_ptr(),
                raw,
                device,
                ffi::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        let keymap = NonNull::new(keymap)
            .map(Keymap)
            .ok_or_else(|| anyhow!("the X server would not give its keyboard layout"))?;
        // SAFETY: a live keymap and connection; the new state takes its own
        // reference to the keymap.
        let state =
            unsafe { (x11().xkb_x11_state_new_from_device)(keymap.0.as_ptr(), raw, device) };
        let state = NonNull::new(state)
            .map(State)
            .ok_or_else(|| anyhow!("the keyboard layout loaded but its state would not"))?;
        Ok((keymap, state))
    }

    /// The `xcb_connection_t` behind an x11rb connection, which is what the
    /// C library takes.
    fn raw_connection(connection: &XCBConnection) -> *mut ffi::x11::xcb_connection_t {
        x11rb::xcb_ffi::XCBConnection::get_raw_xcb_connection(connection).cast()
    }
}

/// The locale Compose tables are named by, as every X client reads it.
pub fn locale() -> String {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "C".to_owned())
}

/// The name of a keysym, for a log line.
pub fn keysym_name(keysym: u32) -> String {
    let mut buffer = [0_u8; 64];
    // SAFETY: a buffer of exactly the length passed with it; the library NUL
    // terminates inside it or reports that it would not fit.
    let written = unsafe {
        (base().xkb_keysym_get_name)(keysym, buffer.as_mut_ptr().cast::<c_char>(), buffer.len())
    };
    if written <= 0 {
        return format!("{keysym:#x}");
    }
    // SAFETY: the library wrote a NUL-terminated string into the buffer.
    unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) }
        .to_string_lossy()
        .into_owned()
}
