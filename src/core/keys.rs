//! One key press, in the notation Neovim reads.
//!
//! The pane hands over what the keyboard layout made of a key: the keysym it
//! resolved to, the text it produced (empty for a key that types nothing, and
//! already composed, so `<dead-acute> e` arrives as `é`), and which modifiers
//! were held. [`notation`] turns that into the string for `nvim_input`:
//! `<C-w>`, `<S-Tab>`, `<M-ü>`, `<kPlus>`, `<lt>`, or the text itself.
//!
//! Two rules do most of the work.
//!
//! - **The layout has already applied Shift and AltGr.** `Shift`+`a` arrives
//!   as the text `A`, and on a German layout `AltGr`+`q` arrives as `@`. So
//!   text is sent as text and no `S-` is added — except where the modifier
//!   changes the notation, because `<C-A>` means Ctrl-A in Neovim and would
//!   lose the Shift. There `Shift`+`a` becomes `<C-S-a>`.
//! - **Ctrl produces a control character, which is not what Neovim wants.**
//!   `Ctrl`+`a` yields the text `U+0001`; the name of the key is in the
//!   keysym, so with a modifier held the keysym decides and the text is only
//!   the fallback for characters outside Latin-1.
//!
//! Every name below was checked against the installed Neovim with
//! `nvim_replace_termcodes`, including the modifier order it prints back.
//! Nothing here does I/O; the X11 and xkbcommon half is in `shell::pane`.

/// An X11 keysym: the number a keyboard layout maps a key to at the level the
/// held modifiers select. The values are `keysymdef.h`'s and never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Keysym(pub u32);

/// The modifiers that change how a key is spelled. `AltGr` is deliberately
/// absent: it is a level shift the layout has already applied, not a modifier
/// Neovim names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub alt: bool,
    pub control: bool,
    pub shift: bool,
    /// The Super/Windows key, which Neovim spells `D-`.
    pub logo: bool,
}

impl Modifiers {
    /// Whether any modifier forces the `<…>` form. Shift alone does not: the
    /// layout has already put it into the text.
    fn need_brackets(self) -> bool {
        self.alt || self.control || self.logo
    }

    /// Neovim prints modifiers in this order, so this emits them in it.
    fn prefix(self, shift: bool) -> String {
        let mut prefix = String::new();
        for (held, name) in [
            (self.alt, "M-"),
            (self.control, "C-"),
            (shift, "S-"),
            (self.logo, "D-"),
        ] {
            if held {
                prefix.push_str(name);
            }
        }
        prefix
    }
}

/// What to send to `nvim_input` for this key press, or `None` when the press
/// is not one: a modifier key on its own, a dead key waiting for the next
/// press, or a key the layout gives neither text nor a name.
pub fn notation(keysym: Keysym, text: &str, modifiers: Modifiers) -> Option<String> {
    if is_modifier(keysym) || is_dead(keysym) {
        return None;
    }
    if let Some(name) = named(keysym) {
        return Some(format!("<{}{name}>", modifiers.prefix(modifiers.shift)));
    }
    if !modifiers.need_brackets() {
        // Nothing to name: send what the layout produced. `<` is the one
        // character that would otherwise start a key name.
        return match text {
            "" => None,
            text => Some(text.replace('<', "<lt>")),
        };
    }
    let character = character(keysym, text)?;
    // Neovim reads `<C-A>` as Ctrl-A, so an upper-case letter has to carry its
    // Shift as a modifier instead of as its case. Every other character keeps
    // whatever the layout made of it, and needs no `S-`.
    let (character, shift) = match character {
        'A'..='Z' => (character.to_ascii_lowercase(), true),
        character => (character, false),
    };
    Some(format!("<{}{}>", modifiers.prefix(shift), spell(character)))
}

/// The character a key press stands for when a modifier is held.
///
/// The keysym comes first: with Ctrl held the text is a control character, and
/// with Alt held some X servers produce nothing at all. Latin-1 keysyms *are*
/// their code points, and everything else is either a Unicode keysym
/// (`0x01000000 | code point`) or not a character at all.
fn character(keysym: Keysym, text: &str) -> Option<char> {
    match keysym.0 {
        value @ (0x20..=0x7e | 0xa0..=0xff) => return char::from_u32(value),
        value @ 0x0100_0020..=0x0110_ffff => return char::from_u32(value - 0x0100_0000),
        _ => {}
    }
    let mut characters = text.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) if !character.is_control() => Some(character),
        _ => None,
    }
}

/// A character inside `<…>`, where four of them would be read as syntax.
fn spell(character: char) -> String {
    match character {
        ' ' => "Space".to_owned(),
        '<' => "lt".to_owned(),
        '\\' => "Bslash".to_owned(),
        '|' => "Bar".to_owned(),
        character => character.to_string(),
    }
}

/// Neovim's name for a key that is not text.
///
/// `ISO_Left_Tab` is the keysym X11 sends for Shift+Tab; naming it `Tab` is
/// right because the held Shift is added as a modifier anyway.
fn named(keysym: Keysym) -> Option<&'static str> {
    let name = match keysym.0 {
        0xff08 => "BS",
        0xff09 | 0xfe20 => "Tab",
        0xff0d => "CR",
        0xff1b => "Esc",
        0xffff => "Del",
        0xff50 => "Home",
        0xff51 => "Left",
        0xff52 => "Up",
        0xff53 => "Right",
        0xff54 => "Down",
        0xff55 => "PageUp",
        0xff56 => "PageDown",
        0xff57 => "End",
        0xff63 => "Insert",
        0xff65 => "Undo",
        0xff6a => "Help",
        // The keypad. With Num Lock off the layout sends the navigation
        // keysyms instead, and Neovim names those too.
        0xff8d => "kEnter",
        0xff95 => "kHome",
        0xff96 => "kLeft",
        0xff97 => "kUp",
        0xff98 => "kRight",
        0xff99 => "kDown",
        0xff9a => "kPageUp",
        0xff9b => "kPageDown",
        0xff9c => "kEnd",
        0xff9d => "kOrigin",
        0xff9e => "kInsert",
        0xff9f => "kDel",
        0xffaa => "kMultiply",
        0xffab => "kPlus",
        0xffac => "kComma",
        0xffad => "kMinus",
        0xffae => "kPoint",
        0xffaf => "kDivide",
        0xffbd => "kEqual",
        value @ 0xffb0..=0xffb9 => return Some(KEYPAD_DIGITS[(value - 0xffb0) as usize]),
        // F1 to F35, which is as far as X11 counts; Neovim names F1 to F37.
        value @ 0xffbe..=0xffe0 => return Some(FUNCTION_KEYS[(value - 0xffbe) as usize]),
        _ => return None,
    };
    Some(name)
}

const KEYPAD_DIGITS: [&str; 10] = ["k0", "k1", "k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9"];

const FUNCTION_KEYS: [&str; 35] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24", "F25", "F26", "F27", "F28",
    "F29", "F30", "F31", "F32", "F33", "F34", "F35",
];

/// A key that only modifies other keys. Pressing one produces nothing.
fn is_modifier(keysym: Keysym) -> bool {
    matches!(
        keysym.0,
        0xffe1
            ..=0xffee    // Shift, Control, Caps Lock, Meta, Alt, Super, Hyper
            | 0xfe03       // ISO_Level3_Shift, the AltGr most layouts use
            | 0xfe11       // ISO_Level5_Shift
            | 0xff7e       // Mode_switch
            | 0xff7f       // Num Lock
            | 0xff14       // Scroll Lock
            | 0xff20 // Multi_key, the Compose key
    )
}

/// A dead key: it types nothing by itself and modifies the next press. The
/// composed result arrives as the text of that next press.
fn is_dead(keysym: Keysym) -> bool {
    matches!(keysym.0, 0xfe50..=0xfe93)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers {
        alt: false,
        control: false,
        shift: false,
        logo: false,
    };
    const CONTROL: Modifiers = Modifiers {
        control: true,
        ..NONE
    };
    const ALT: Modifiers = Modifiers { alt: true, ..NONE };
    const SHIFT: Modifiers = Modifiers {
        shift: true,
        ..NONE
    };

    fn key(keysym: u32, text: &str, modifiers: Modifiers) -> Option<String> {
        notation(Keysym(keysym), text, modifiers)
    }

    #[test]
    fn plain_text_is_sent_as_the_layout_produced_it() {
        assert_eq!(key(0x61, "a", NONE).as_deref(), Some("a"));
        assert_eq!(key(0x41, "A", SHIFT).as_deref(), Some("A"));
        // German layout: the umlaut key, and AltGr+q for the at sign. Neither
        // is a modifier Neovim names.
        assert_eq!(key(0xfc, "ü", NONE).as_deref(), Some("ü"));
        assert_eq!(key(0x40, "@", NONE).as_deref(), Some("@"));
        // Composed with a dead key: the dead press itself types nothing, and
        // the next press arrives already composed.
        assert_eq!(key(0xfe51, "", NONE), None);
        assert_eq!(key(0xe9, "é", NONE).as_deref(), Some("é"));
        // The sharp s the criterion asks for, and a space.
        assert_eq!(key(0xdf, "ß", NONE).as_deref(), Some("ß"));
        assert_eq!(key(0x20, " ", NONE).as_deref(), Some(" "));
    }

    #[test]
    fn a_less_than_sign_is_escaped_so_it_is_not_read_as_a_key_name() {
        assert_eq!(key(0x3c, "<", NONE).as_deref(), Some("<lt>"));
        assert_eq!(key(0x3c, "<", SHIFT).as_deref(), Some("<lt>"));
        assert_eq!(key(0x3c, "<", CONTROL).as_deref(), Some("<C-lt>"));
    }

    #[test]
    fn control_names_the_key_rather_than_the_control_character_it_typed() {
        // X11 delivers U+0001 as the text of Ctrl+a.
        assert_eq!(key(0x61, "\u{1}", CONTROL).as_deref(), Some("<C-a>"));
        // Ctrl+Shift+a: the layout upper-cases the keysym, and `<C-A>` would
        // mean Ctrl-A, so the Shift has to become a modifier.
        assert_eq!(
            key(
                0x41,
                "\u{1}",
                Modifiers {
                    shift: true,
                    ..CONTROL
                }
            )
            .as_deref(),
            Some("<C-S-a>")
        );
        assert_eq!(key(0x20, " ", CONTROL).as_deref(), Some("<C-Space>"));
        assert_eq!(key(0x5c, "\u{1c}", CONTROL).as_deref(), Some("<C-Bslash>"));
        assert_eq!(key(0x7c, "|", CONTROL).as_deref(), Some("<C-Bar>"));
    }

    #[test]
    fn alt_and_super_keep_the_character_the_layout_gave() {
        assert_eq!(key(0x61, "a", ALT).as_deref(), Some("<M-a>"));
        assert_eq!(key(0xfc, "ü", ALT).as_deref(), Some("<M-ü>"));
        assert_eq!(
            key(0x41, "A", Modifiers { shift: true, ..ALT }).as_deref(),
            Some("<M-S-a>")
        );
        assert_eq!(
            key(0x61, "a", Modifiers { logo: true, ..NONE }).as_deref(),
            Some("<D-a>")
        );
        assert_eq!(
            key(
                0x61,
                "a",
                Modifiers {
                    alt: true,
                    control: true,
                    shift: false,
                    logo: true
                }
            )
            .as_deref(),
            Some("<M-C-D-a>")
        );
    }

    #[test]
    fn named_keys_carry_their_modifiers_including_shift() {
        assert_eq!(key(0xff1b, "\u{1b}", NONE).as_deref(), Some("<Esc>"));
        assert_eq!(key(0xff0d, "\r", NONE).as_deref(), Some("<CR>"));
        assert_eq!(key(0xff09, "\t", NONE).as_deref(), Some("<Tab>"));
        // X11 sends ISO_Left_Tab for Shift+Tab.
        assert_eq!(key(0xfe20, "", SHIFT).as_deref(), Some("<S-Tab>"));
        assert_eq!(key(0xff52, "", NONE).as_deref(), Some("<Up>"));
        assert_eq!(key(0xff52, "", ALT).as_deref(), Some("<M-Up>"));
        assert_eq!(key(0xffbe, "", NONE).as_deref(), Some("<F1>"));
        assert_eq!(key(0xffe0, "", NONE).as_deref(), Some("<F35>"));
        assert_eq!(
            key(
                0xffbe,
                "",
                Modifiers {
                    shift: true,
                    ..CONTROL
                }
            )
            .as_deref(),
            Some("<C-S-F1>")
        );
    }

    #[test]
    fn the_keypad_has_names_of_its_own() {
        assert_eq!(key(0xffb0, "0", NONE).as_deref(), Some("<k0>"));
        assert_eq!(key(0xffb9, "9", NONE).as_deref(), Some("<k9>"));
        assert_eq!(key(0xffab, "+", NONE).as_deref(), Some("<kPlus>"));
        assert_eq!(key(0xffaf, "/", NONE).as_deref(), Some("<kDivide>"));
        assert_eq!(key(0xff8d, "\r", NONE).as_deref(), Some("<kEnter>"));
        // Num Lock off: the layout sends the navigation keysyms.
        assert_eq!(key(0xff9c, "", NONE).as_deref(), Some("<kEnd>"));
        assert_eq!(key(0xff9d, "", NONE).as_deref(), Some("<kOrigin>"));
    }

    #[test]
    fn a_key_that_types_nothing_and_names_nothing_sends_nothing() {
        for modifier in [0xffe1, 0xffe3, 0xffe9, 0xffeb, 0xfe03, 0xff20, 0xff7f] {
            assert_eq!(key(modifier, "", NONE), None, "keysym {modifier:#x}");
            assert_eq!(key(modifier, "", CONTROL), None, "keysym {modifier:#x}");
        }
        // A dead key, before and while composing.
        assert_eq!(key(0xfe50, "", NONE), None);
        assert_eq!(key(0xfe93, "", NONE), None);
        // Something with no text, no name and no character.
        assert_eq!(key(0xff7a, "", NONE), None);
        assert_eq!(key(0xff7a, "", CONTROL), None);
    }

    #[test]
    fn a_unicode_keysym_outside_latin_1_still_resolves_under_a_modifier() {
        // `0x01000000 | code point` is how X11 carries anything else.
        assert_eq!(key(0x0100_0416, "Ж", ALT).as_deref(), Some("<M-Ж>"));
        // With no keysym to go by, the text is the fallback.
        assert_eq!(key(0x0, "€", ALT).as_deref(), Some("<M-€>"));
        assert_eq!(key(0x0, "not one character", ALT), None);
    }
}
