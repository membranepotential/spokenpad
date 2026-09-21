//! The keyboard, as the user's own layout defines it.
//!
//! A key press from X11 is a keycode and a modifier mask, which mean nothing
//! without the layout. xkbcommon reads the layout from the X server itself —
//! the same one `setxkbmap` set, per device — and turns a keycode into a
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
//! there. The library itself is opened at run time; see [`xkb`](super::xkb).
use super::xkb;
use crate::core::keys::{self, Keysym, Modifiers};
use anyhow::Result;
use x11rb::{protocol::xproto::KeyButMask, xcb_ffi::XCBConnection};

/// Where in the X11 modifier mask the keyboard group lives (XKB puts it in
/// bits 13 and 14 of the `state` field of a key event).
const GROUP_SHIFT: u32 = 13;
const GROUP_MASK: u32 = 0b11;
/// The low byte of that mask is Shift, Lock, Control and Mod1 to Mod5, which
/// are the real modifiers xkbcommon numbers the same way.
const REAL_MODIFIERS: u32 = 0xff;

pub struct Keyboard {
    context: xkb::Context,
    /// The state borrows the keymap in C; holding both keeps them in step and
    /// releases them in the right order.
    _keymap: xkb::Keymap,
    state: xkb::State,
    compose: Option<xkb::ComposeState>,
    device: i32,
}

impl Keyboard {
    /// Read the layout the X server has loaded for the core keyboard.
    pub fn new(connection: &XCBConnection) -> Result<Self> {
        let context = xkb::Context::new()?;
        xkb::x11_device::setup(connection)?;
        let device = xkb::x11_device::core_keyboard(connection)?;
        let (keymap, state) = xkb::x11_device::layout(&context, connection, device)?;
        // Compose is optional: a locale with no Compose file is normal, and
        // then dead keys simply do not compose.
        let locale = xkb::locale();
        let compose = xkb::ComposeTable::new(&context, &locale).and_then(xkb::ComposeState::new);
        if compose.is_none() {
            log::debug!("no Compose table for locale {locale:?}; dead keys will not compose");
        }
        Ok(Self {
            context,
            _keymap: keymap,
            state,
            compose,
            device,
        })
    }

    /// Re-read the layout, after the X server said it changed.
    pub fn refresh(&mut self, connection: &XCBConnection) -> Result<()> {
        let (keymap, state) = xkb::x11_device::layout(&self.context, connection, self.device)?;
        self._keymap = keymap;
        self.state = state;
        if let Some(compose) = self.compose.as_mut() {
            compose.reset();
        }
        Ok(())
    }

    /// What to send Neovim for this key press, or `None` when the press types
    /// nothing: a modifier, a dead key, or a key still mid-compose.
    pub fn press(&mut self, keycode: u8, mask: KeyButMask) -> Option<String> {
        let raw = u32::from(u16::from(mask));
        self.state.update_mask(
            raw & REAL_MODIFIERS,
            0,
            0,
            (raw >> GROUP_SHIFT) & GROUP_MASK,
        );
        let keycode = u32::from(keycode);
        let keysym = self.state.keysym(keycode);
        let modifiers = Modifiers {
            alt: self.state.modifier_is_active(xkb::MOD_ALT),
            control: self.state.modifier_is_active(xkb::MOD_CONTROL),
            shift: self.state.modifier_is_active(xkb::MOD_SHIFT),
            logo: self.state.modifier_is_active(xkb::MOD_LOGO),
        };
        // Read the text before the Compose machine is borrowed: it is the
        // answer for every press that is not part of a sequence.
        let typed = self.state.text(keycode);
        let (keysym, text) = self.compose(keysym, typed)?;
        keys::notation(Keysym(keysym), &text, modifiers)
    }

    /// Feed the keysym to the Compose machine, if there is one.
    ///
    /// `None` means the press was swallowed: it started or continued a
    /// sequence, or cancelled one. Otherwise this is the keysym and text to
    /// spell, which for a finished sequence is the composed result.
    fn compose(&mut self, keysym: u32, typed: String) -> Option<(u32, String)> {
        let Some(compose) = self.compose.as_mut() else {
            return Some((keysym, typed));
        };
        match compose.feed(keysym) {
            xkb::Compose::Composing => None,
            xkb::Compose::Cancelled => {
                compose.reset();
                None
            }
            xkb::Compose::Composed => {
                let (composed, text) = compose.result();
                compose.reset();
                Some((composed, text))
            }
            xkb::Compose::Nothing => Some((keysym, typed)),
        }
    }
}
