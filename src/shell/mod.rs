//! The imperative shell: everything that touches the outside world.
//!
//! Devices, models, processes, sockets, threads and the filesystem live here.
//! Each module keeps the I/O and hands the decisions to [`core`](crate::core);
//! logic that needs none of those belongs on that side instead.
pub mod audio;
pub mod daemon;
pub mod hotkey;
pub mod inference;
pub mod logging;
pub mod nvim;
pub mod recorder;
pub mod wm;
