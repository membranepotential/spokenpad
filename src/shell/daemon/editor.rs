//! The editor thread: it owns the connection to the dictation editor, and
//! writes each piece of text there, or to the pending passage when no
//! editor is open.
use super::{Reload, ResultEvent};
use crate::{
    config::Config,
    core::{
        decode::UtteranceId,
        session::{Notice, Session},
    },
    shell::nvim::{AppendFailure, CopyOutcome, Detached, IndicatorState, NvimSession, Want},
};
use std::{
    sync::{
        atomic::Ordering,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

/// Everything the editor's winbar shows, replaced wholesale, never merged.
/// `status` is the speech model's notice, shown when it outranks the
/// capture's own.
pub(super) fn indicator_of(
    session: &Session,
    level: f32,
    status: Option<Notice>,
) -> IndicatorState {
    IndicatorState {
        phase: session.state.indicator_phase(),
        level: f64::from(level),
        preview: session.preview().to_owned(),
        notice: session.shown_notice(status).as_ref().map(Notice::text),
        latched: session.state.latched(),
        previewing: session.previewing(),
    }
}

pub(super) enum EditorWork {
    Ensure,
    Append {
        utterance: UtteranceId,
        text: String,
    },
    Indicator(IndicatorState),
    /// Copy the whole dictation buffer to `+`. Sent once per utterance, after
    /// its `Finished` result -- see the comment where it is sent, in
    /// `serve`, for why every `Append` of that utterance is already ahead of
    /// it in this same queue.
    Copy,
    Quit,
}

/// Where an utterance's last commit went. A later commit of the same
/// utterance continues its line only in the same place: joined onto another
/// file's last line, it would run into unrelated text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    Editor,
    Passage,
}

/// The `[nvim]` settings the next window opens with: what the file says now.
/// A file that no longer loads keeps the settings in use, and the window
/// says why. Either way they take the session the window opens in, such as
/// the display the user manager has now. Changes to any other section only
/// take effect at a restart, which is logged once per distinct set of them.
fn reconfigure(
    nvim: &mut NvimSession,
    running: &Config,
    reload: &mut Reload,
    reported: &mut Vec<&'static str>,
    events: &Sender<ResultEvent>,
) {
    match (reload.load)() {
        Ok(fresh) => {
            let restart = running.restart_needed(&fresh);
            if restart != *reported && !restart.is_empty() {
                log::warn!(
                    "config changes to [{}] take effect after `systemctl --user restart spokenpad`; [nvim] applies to this window",
                    restart.join("], [")
                );
            }
            *reported = restart;
            nvim.reconfigure((reload.session)(fresh.nvim));
        }
        Err(e) => {
            log::error!("config not reloaded: {e:#}; the window keeps the settings in use");
            let _ = events.send(ResultEvent::ConfigInvalid(crate::config::summary(&e)));
            nvim.reconfigure((reload.session)(nvim.settings().clone()));
        }
    }
}

/// Owns the editor connection. It runs until its channel says to stop, never
/// on a shared flag: text already queued must reach a file even while the
/// daemon is shutting down or the capture it came from was cancelled.
///
/// The session's [`quitting`](NvimSession::quitting) flag changes only how:
/// once it is set, an append that is not answered within a quarter second —
/// held behind a half-typed command, or sent to an editor that stopped
/// answering — is given up on without reconnecting and repeating it, and
/// its text goes to the pending passage like any unconfirmed append; an
/// editor that is not connected is not reattached or opened, so what is
/// still queued goes there too, within the shutdown grace instead of
/// waiting on an editor.
pub(super) fn editor_thread(
    mut nvim: NvimSession,
    running: Config,
    mut reload: Reload,
    rx: Receiver<EditorWork>,
    events: Sender<ResultEvent>,
) {
    let quitting = nvim.quitting();
    let mut reported = Vec::new();
    let mut paragraph: Option<(UtteranceId, Sink)> = None;
    let mut shown: Option<IndicatorState> = None;
    let mut quit = false;
    while !quit {
        let first = match rx.recv_timeout(Duration::from_millis(66)) {
            Ok(work) => Some(work),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        report_user_close(&mut nvim, &events);
        // Drain what is already queued, keeping only the newest indicator:
        // the loop offers one 50 times a second and only the last is true.
        let mut indicator = None;
        for work in first.into_iter().chain(rx.try_iter()) {
            // Before each piece of work, so that text arriving just after
            // the user closed the pane does not open another.
            report_user_close(&mut nvim, &events);
            match work {
                EditorWork::Indicator(next) => indicator = Some(next),
                EditorWork::Ensure if quitting.load(Ordering::Acquire) => {}
                EditorWork::Ensure => {
                    // A window about to open, or an editor about to be
                    // attached, takes the settings the file has now.
                    if !nvim.attached() {
                        reconfigure(&mut nvim, &running, &mut reload, &mut reported, &events);
                    }
                    match nvim.ensure(Want::Press) {
                        Ok(Some(path)) => {
                            log::info!("dictating into {}", path.display());
                            shown = None;
                        }
                        Ok(None) => log::info!(
                            "no dictation editor is open; text goes to the dictation file until `spokenpad editor` opens it"
                        ),
                        Err(e) => log::error!("could not open dictation window: {e:#}"),
                    }
                }
                EditorWork::Append { utterance, text } => {
                    // A failed request elsewhere (a clipboard copy that timed
                    // out, an indicator push) drops the connection; text that
                    // is owed to the file reattaches rather than waiting for
                    // the next key-down to do it.
                    if !nvim.connected()
                        && !quitting.load(Ordering::Acquire)
                        && let Err(e) = nvim.ensure(Want::Text)
                    {
                        log::warn!("could not reattach to the dictation window: {e:#}");
                    }
                    // Whether the editor may have this text after all, so
                    // the pending passage is a second copy rather than the
                    // only one.
                    let mut maybe_landed = false;
                    if nvim.connected() {
                        let now = Instant::now();
                        let continued = paragraph == Some((utterance, Sink::Editor));
                        match nvim.append(&text, continued) {
                            Ok(line) => {
                                paragraph = Some((utterance, Sink::Editor));
                                shown = None;
                                log::info!(
                                    "appended in {:.0}ms (line {line})",
                                    now.elapsed().as_secs_f64() * 1000.
                                );
                                continue;
                            }
                            // The editor went away before it got the text —
                            // closed while the tail was decoding — so the
                            // text is certainly not there and goes to the
                            // file below.
                            Err(AppendFailure::NotSent(e)) => {
                                log::warn!("the dictation editor did not receive the text: {e:#}");
                            }
                            // Not confirmed even by the repeat with the same
                            // operation id. Kept only in the log, the text
                            // would be lost to the user if the editor never
                            // took it, which is the usual case: Neovim drops
                            // a request whose connection closed before it ran.
                            // So it goes to the pending passage as well, and
                            // the log says it may be in both.
                            Err(AppendFailure::Unconfirmed(e)) => {
                                log::error!(
                                    "append not confirmed: {e:#}; writing the text to the pending passage too, so if the editor took it after all it is in both"
                                );
                                maybe_landed = true;
                            }
                        }
                    }
                    // With no editor at all, the text goes to the file the
                    // next editor will open on, rather than nowhere.
                    let continued = paragraph == Some((utterance, Sink::Passage));
                    match nvim.append_detached(&text, continued) {
                        Ok(write) => {
                            paragraph = Some((utterance, Sink::Passage));
                            log::info!("no dictation editor: appended to {}", write.path.display());
                            if maybe_landed {
                                nvim.log_detached(&write.path, Detached::Unconfirmed);
                            } else if write.started {
                                nvim.log_detached(&write.path, Detached::NoEditor);
                            }
                        }
                        Err(e) => {
                            paragraph = None;
                            log::error!(
                                "could not write {} characters of transcript to a dictation file: {e:#}; recover from the capture WAV if available",
                                text.chars().count()
                            );
                        }
                    }
                }
                EditorWork::Copy => {
                    // Off by default: with `nvim.copy_to_clipboard` false
                    // nothing is copied. Never on a session with no editor
                    // either: nothing has been spoken into a window that was
                    // never opened, and there is nothing to copy.
                    if nvim.config().copy_to_clipboard && nvim.connected() {
                        match nvim.copy_buffer() {
                            Ok(CopyOutcome::Copied) => {
                                log::debug!("copied the dictation buffer to the clipboard")
                            }
                            Ok(CopyOutcome::Empty) => {}
                            Ok(CopyOutcome::Failed(reason)) => {
                                log::warn!("clipboard copy failed: {reason}")
                            }
                            Err(e) => log::warn!("clipboard copy unavailable: {e:#}"),
                        }
                    }
                }
                EditorWork::Quit => {
                    quit = true;
                    break;
                }
            }
        }
        if nvim.connected()
            && let Some(next) = indicator
            && shown.as_ref() != Some(&next)
        {
            if let Err(e) = nvim.set_indicator(&next) {
                log::warn!("editor indicator unavailable: {e:#}");
            }
            shown = Some(next);
        }
    }
    // Nothing can be written after this point; say what was lost.
    for work in rx.try_iter() {
        if let EditorWork::Append { text, .. } = work {
            log::error!(
                "shutting down with {} characters of undelivered transcript; recover from the capture WAV",
                text.chars().count()
            );
        }
    }
    nvim.close();
}

/// Tells the event loop that the user closed the dictation pane, if they did
/// since the last time the pane was asked, so it can cancel the capture that
/// pane showed.
fn report_user_close(nvim: &mut NvimSession, events: &Sender<ResultEvent>) {
    if let Some(at) = nvim.take_user_close() {
        // The loop is gone only while this thread is being stopped.
        let _ = events.send(ResultEvent::WindowClosed { at });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Nvim;

    /// A config file that no longer loads keeps the settings in use for the
    /// next window, but not their session: a daemon systemd started takes
    /// the display the user manager has now either way.
    #[test]
    fn a_config_that_does_not_load_still_takes_the_managers_session() {
        let running = Config::default();
        let mut nvim = NvimSession::new(Nvim {
            display: Some(":0".into()),
            ..running.nvim.clone()
        });
        let mut reload = Reload {
            load: Box::new(|| anyhow::bail!("expected a table")),
            session: |nvim| Nvim {
                display: Some(":1".into()),
                ..nvim
            },
        };
        let (events, received) = mpsc::channel();
        reconfigure(&mut nvim, &running, &mut reload, &mut Vec::new(), &events);
        assert!(matches!(
            received.try_recv(),
            Ok(ResultEvent::ConfigInvalid(_))
        ));
        assert_eq!(nvim.settings().display.as_deref(), Some(":1"));
    }
}
