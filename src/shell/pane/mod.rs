//! The dictation pane: an X11 window spokenpad draws itself, with an embedded
//! Neovim behind it.
//!
//! The window never takes keyboard focus when it appears
//! ([`x11`] explains the properties that make that true), floats on tiling
//! window managers, and needs no window-manager rule. The user can click into
//! it and type; the daemon keeps appending over the editor's own socket, as in
//! every other mode.
//!
//! Nothing in the daemon uses this yet.
pub mod x11;
