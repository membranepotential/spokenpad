//! The functional core: capture policy expressed as values.
//!
//! Nothing here opens a device, a file, a socket or a thread — the modules are
//! total functions and state machines over data the [`shell`](crate::shell)
//! hands them, so every rule they encode is unit-testable without hardware.
//! Code that needs an input device, a sound card, a model, the filesystem, a
//! process, a socket or a thread belongs on the other side.
pub mod decode;
pub mod frames;
pub mod geometry;
pub mod hotkey;
pub mod segments;
pub mod session;
pub mod state;
pub mod terminal;
pub mod text;
pub mod wm;
