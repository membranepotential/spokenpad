//! One policy for a poisoned mutex: on the capture path (the audio callback,
//! the capture buffer, the recorder's writer threads) and in the state the
//! pane's thread shares with the daemon.
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, taking the guard even when another thread panicked while
/// holding it.
///
/// A thread that panicked must not also take the microphone, the recovery
/// WAV or the dictation window down with it: a capture outlives the failure
/// of anything else. That is safe only because the sections behind these locks change
/// their data with single assignments and std's panic-safe collection
/// operations, so what a poisoned lock holds is still a value a reader can
/// use; a section that breaks an invariant across several steps does not
/// belong behind this function.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
