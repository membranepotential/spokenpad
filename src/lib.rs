//! Local push-to-talk dictation: a pure [`core`] decides, an imperative
//! [`shell`] talks to the devices, and [`config`] is what both agree on.
pub mod config;
pub mod core;
pub mod shell;
