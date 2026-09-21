//! The keyboard, as the user's own layout defines it.
//!
//! A key press from X11 is a keycode and a modifier mask, which mean nothing
//! without the layout. `xkbcommon` reads the layout from the X server itself
//! — the same one `setxkbmap` set, per device — and turns a keycode into a
//! keysym and the text it types. It also does dead keys and Compose, so
//! `<dead-acute> e` reaches Neovim as `é` rather than as two presses.
//!
//! The modifier state comes from the mask X11 puts on each event rather than
//! from tracking press and release. That is one fact per event instead of a
//! running total, so a modifier released while the pane had no focus cannot
//! leave the state wrong. The layout itself is re-read on `MappingNotify`, so
//! a `setxkbmap` while the pane is open takes effect.
//!
//! What this hands on is [`core::keys::notation`](crate::core::keys::notation)
//! — the decision about how to spell a key is a pure function, and it is over
//! there.
use crate::core::keys::{self, Keysym, Modifiers};
use anyhow::{Result, bail, ensure};
use x11rb::{protocol::xproto::KeyButMask, xcb_ffi::XCBConnection};
use xkbcommon::xkb;

/// Where in the X11 modifier mask the keyboard group lives (`XKB` puts it in
/// bits 13 and 14 of the `state` field of a key event).
const GROUP_SHIFT: u32 = 13;
const GROUP_MASK: u32 = 0b11;

pub struct Keyboard {
    context: xkb::Context,
    state: xkb::State,
    compose: Option<xkb::compose::State>,
    device: i32,
}

impl Keyboard {
    /// Read the layout the X server has loaded for the core keyboard.
    pub fn new(connection: &XCBConnection) -> Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let (mut major, mut minor, mut event, mut error) = (0, 0, 0, 0);
        let ready = xkb::x11::setup_xkb_extension(
            connection,
            xkb::x11::MIN_MAJOR_XKB_VERSION,
            xkb::x11::MIN_MINOR_XKB_VERSION,
            xkb::x11::SetupXkbExtensionFlags::NoFlags,
            &mut major,
            &mut minor,
            &mut event,
            &mut error,
        );
        ensure!(
            ready,
            "this X server has no usable XKB extension, so the pane cannot read the layout"
        );
        let device = xkb::x11::get_core_keyboard_device_id(connection);
        ensure!(device >= 0, "the X server named no core keyboard device");
        let state = layout(&context, connection, device)?;
        // Compose is optional: a locale with no Compose file is normal, and
        // then dead keys simply do not compose.
        let compose = xkb::compose::Table::new_from_locale(
            &context,
            std::ffi::OsStr::new(&locale()),
            xkb::compose::COMPILE_NO_FLAGS,
        )
        .ok()
        .map(|table| xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS));
        if compose.is_none() {
            log::debug!(
                "no Compose table for locale {:?}; dead keys will not compose",
                locale()
            );
        }
        Ok(Self {
            context,
            state,
            compose,
            device,
        })
    }

    /// Re-read the layout, after the X server said it changed.
    pub fn refresh(&mut self, connection: &XCBConnection) -> Result<()> {
        self.state = layout(&self.context, connection, self.device)?;
        if let Some(compose) = self.compose.as_mut() {
            compose.reset();
        }
        Ok(())
    }

    /// What to send Neovim for this key press, or `None` when the press types
    /// nothing: a modifier, a dead key, or a key still mid-compose.
    pub fn press(&mut self, keycode: u8, mask: KeyButMask) -> Option<String> {
        let raw = u32::from(u16::from(mask));
        self.state
            .update_mask(raw & 0xff, 0, 0, 0, 0, (raw >> GROUP_SHIFT) & GROUP_MASK);
        let code = xkb::Keycode::from(u32::from(keycode));
        let keysym = self.state.key_get_one_sym(code);
        let modifiers = Modifiers {
            alt: self.active(xkb::MOD_NAME_ALT),
            control: self.active(xkb::MOD_NAME_CTRL),
            shift: self.active(xkb::MOD_NAME_SHIFT),
            logo: self.active(xkb::MOD_NAME_LOGO),
        };
        // Read the text before the Compose machine is borrowed: it is the
        // answer for every press that is not part of a sequence.
        let typed = self.state.key_get_utf8(code);
        let (keysym, text) = self.compose(keysym, typed)?;
        keys::notation(Keysym(keysym), &text, modifiers)
    }

    fn active(&self, name: &str) -> bool {
        self.state
            .mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
    }

    /// Feed the keysym to the Compose machine, if there is one.
    ///
    /// `None` means the press was swallowed: it started or continued a
    /// sequence, or cancelled one. Otherwise this is the keysym and text to
    /// spell, which for a finished sequence is the composed result.
    fn compose(&mut self, keysym: xkb::Keysym, typed: String) -> Option<(u32, String)> {
        let Some(compose) = self.compose.as_mut() else {
            return Some((keysym.raw(), typed));
        };
        compose.feed(keysym);
        match compose.status() {
            xkb::compose::Status::Composing => None,
            xkb::compose::Status::Cancelled => {
                compose.reset();
                None
            }
            xkb::compose::Status::Composed => {
                let composed = (
                    compose.keysym().map_or(keysym.raw(), |sym| sym.raw()),
                    compose.utf8().unwrap_or_default(),
                );
                compose.reset();
                Some(composed)
            }
            xkb::compose::Status::Nothing => Some((keysym.raw(), typed)),
        }
    }
}

fn layout(context: &xkb::Context, connection: &XCBConnection, device: i32) -> Result<xkb::State> {
    let keymap =
        xkb::x11::keymap_new_from_device(context, connection, device, xkb::KEYMAP_COMPILE_NO_FLAGS);
    // The crate wraps whatever the C call returned, including nothing.
    if keymap.get_raw_ptr().is_null() {
        bail!("the X server would not give its keyboard layout");
    }
    let state = xkb::x11::state_new_from_device(&keymap, connection, device);
    if state.get_raw_ptr().is_null() {
        bail!("the keyboard layout loaded but its state would not");
    }
    Ok(state)
}

/// The locale Compose tables are named by, as every X client reads it.
fn locale() -> String {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "C".to_owned())
}
