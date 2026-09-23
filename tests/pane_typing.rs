//! Typing into the pane while a latched dictation runs.
//!
//! The live check of 2026-09-22: the user latched a dictation, clicked into
//! the pane and typed there, and the window "got stuck" while an append took
//! 2085 ms instead of 20. This drives the real `NvimSession` from a thread of
//! its own, the way the daemon's editor thread does — a latched recording
//! pushing its indicator ten times a second and a committed piece every
//! second — while XTEST types into the pane on a German layout. It measures:
//!
//! - **Insert mode.** What the user types at the end of the text stays one
//!   piece, AltGr and a dead key included, and so does every dictated piece:
//!   the preview following the text used to move the cursor onto the last
//!   character, and the next key landed inside the last dictated word. The
//!   appends keep landing in their usual few milliseconds.
//! - **Normal mode, half a command typed.** Neovim runs no RPC call while a
//!   count, `g` or `"` waits for the rest of its command. The pending keys show
//!   in the corner, and the append waiting behind them is not given up on: it
//!   lands once, in the window, the moment the command is cancelled. It used
//!   to time out after two seconds, reconnect, and time out again, which
//!   diverted the following text away from the window.
//! - **Stopping while a call is held.** Setting the session's `quitting`
//!   flag ends the wait at once, the append is reported as not confirmed,
//!   and it is never written twice.
//! - **Typing is saved as it happens.** What the user typed is in the file
//!   before anything closes the window, with no `:w`.
//! - **Closing the window mid-command.** Everything typed by hand is written,
//!   although the write, too, waits behind a pending command.
//!
//! Run with `-- --nocapture` to see the timings.
mod harness;

use harness::{
    I3, SETTLE, XServer, click, close_window, find_key, press_key, press_keysym, press_with_altgr,
    type_text, wait_for,
};
use rmpv::Value;
use spokenpad::{
    config::{Mode, Nvim},
    core::{font::Points, geometry::Dimensions, state::IndicatorPhase},
    shell::{
        nvim::{IndicatorState, NvimSession, PreviewPlacement, Want, pane_launch},
        pane::{Options, Pane, Status},
    },
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{JoinHandle, sleep},
    time::{Duration, Instant},
};

const PATIENCE: Duration = Duration::from_secs(10);
/// How long the test leaves a command half typed: longer than an append
/// used to be given in all, two seconds for its reply and two more for the
/// reconnection that followed, before it was abandoned.
const HELD: Duration = Duration::from_secs(5);
const ESCAPE: u32 = 0xff1b;
const DEAD_ACUTE: u32 = 0xfe51;

#[test]
fn typing_into_the_pane_during_a_latched_dictation() {
    if !harness::tools_or_skip(&["Xvfb", "i3", "nvim", "setxkbmap", "fc-match"]) {
        return;
    }
    let mut server = XServer::start();
    server.set_layout("de");
    let i3 = I3::start(&server);
    let base = server.plain_window();
    i3.wait_until_managed(base);
    sleep(SETTLE);

    let directory = tempfile::tempdir().expect("a temporary directory");
    let config = dictation_config(directory.path());
    let file = config.dictation_dir.join("dictation-2026-09-22-000000.md");
    std::fs::write(&file, "").expect("create the dictation file");
    let (command, marker) = pane_launch(&config, &file).expect("build the nvim command");
    let mut pane = Pane::open(
        &Options {
            display: server.display.clone(),
            dimensions: Dimensions::new(72, 16).expect("a grid"),
            size: Points::try_from(12.0).expect("a point size"),
            ..Options::default()
        },
        command,
    )
    .expect("open the pane");
    pane.show().expect("map the pane");
    marker.keep();
    i3.wait_until_managed(pane.window().id());
    sleep(SETTLE);

    let daemon = Dictation::start(config.clone());
    let rect = pane.window().geometry().expect("the pane's geometry");
    click(
        &mut server,
        i16::try_from(rect.x + rect.width as i32 / 2).expect("on screen"),
        i16::try_from(rect.y + rect.height as i32 / 2).expect("on screen"),
    );
    wait_for(PATIENCE, "the click to focus the pane", || {
        let _ = pane.step(Duration::from_millis(50));
        i3.node(pane.window().id())
            .filter(|node| node.focused)
            .map(|_| ())
    });
    wait_for(PATIENCE, "the first dictated piece", || {
        let _ = pane.step(Duration::from_millis(50));
        (!daemon.appended().is_empty()).then_some(())
    });

    // ------------------------------------------------------- insert mode
    let append = find_key(&server, u32::from(b'A')).expect("the layout has `A`");
    press_key(&mut server, append);
    run(&mut pane, Duration::from_millis(200));
    let typing_from = Instant::now();
    let mut typed = String::new();
    for character in " Grüße ß ".chars() {
        assert!(type_text(&mut server, &character.to_string()));
        typed.push(character);
        run(&mut pane, Duration::from_millis(150));
    }
    for (character, base) in [('@', b'q'), ('€', b'e'), ('{', b'7'), ('}', b'0')] {
        assert!(press_with_altgr(&mut server, u32::from(base)));
        typed.push(character);
        run(&mut pane, Duration::from_millis(150));
    }
    assert!(press_keysym(&mut server, DEAD_ACUTE) && type_text(&mut server, "e"));
    typed.push('é');
    for character in " zu Ende".chars() {
        assert!(type_text(&mut server, &character.to_string()));
        typed.push(character);
        run(&mut pane, Duration::from_millis(150));
    }
    let pushed = daemon.pushed();
    wait_for(PATIENCE, "the preview to keep up while typing", || {
        let _ = pane.step(Duration::from_millis(50));
        newest_preview(&grid_text(&pane)).filter(|shown| *shown >= pushed)
    });
    let while_typing = daemon.appended_since(typing_from);
    assert!(
        while_typing.len() >= 2,
        "the test should append while the user types: {while_typing:?}"
    );
    let slowest = while_typing
        .iter()
        .map(|piece| piece.took)
        .max()
        .expect("appends");
    println!(
        "insert mode: {} appends while typing, the slowest in {} ms",
        while_typing.len(),
        slowest.as_millis()
    );
    assert!(
        slowest < Duration::from_millis(500),
        "an append stalled while the user typed in insert mode: {while_typing:?}"
    );
    press(&mut server, ESCAPE);
    run(&mut pane, Duration::from_millis(300));
    let buffer = buffer_text(&mut pane);
    assert!(
        buffer.contains(typed.trim_end()),
        "what was typed was split by dictated text: typed {typed:?}, buffer:\n{buffer}"
    );

    // ---------------------------------- normal mode, half a command typed
    let held_from = Instant::now();
    let count = find_key(&server, u32::from(b'2')).expect("the layout has `2`");
    press_key(&mut server, count);
    run(&mut pane, HELD);
    let corner = pane.screen().line(pane.size().1 - 1);
    assert!(
        corner.trim_end().ends_with('2'),
        "the half-typed count should show in the corner: {corner:?}"
    );
    press(&mut server, ESCAPE);
    let behind = wait_for(PATIENCE, "the held append to land", || {
        let _ = pane.step(Duration::from_millis(50));
        daemon
            .appended_since(held_from)
            .into_iter()
            .find(|piece| piece.landed && piece.took >= HELD - Duration::from_secs(1))
    });
    println!(
        "normal mode: an append waited {} ms behind a count held {} ms, then landed",
        behind.took.as_millis(),
        HELD.as_millis()
    );
    run(&mut pane, Duration::from_millis(1_500));

    // ------------------------------------ stopping while a call is held
    let held_from = Instant::now();
    press(&mut server, u32::from(b'2'));
    // Long enough that an append has passed its deadline and is held.
    run(&mut pane, Duration::from_secs(3));
    let stopped = Instant::now();
    let appended = daemon.quit();
    let stopping = stopped.elapsed();
    let unconfirmed: Vec<&Appended> = appended
        .iter()
        .filter(|piece| piece.sent >= held_from && !piece.landed)
        .collect();
    println!(
        "stopping: the thread holding an append for {} ms stopped {} ms after the flag",
        unconfirmed
            .first()
            .map_or(0, |piece| (stopped - piece.sent).as_millis()),
        stopping.as_millis()
    );
    assert_eq!(
        unconfirmed.len(),
        1,
        "exactly the held append should be unconfirmed: {appended:?}"
    );
    assert!(
        stopping < Duration::from_millis(1_500),
        "a stop waited {stopping:?} for a held call"
    );
    press(&mut server, ESCAPE);
    run(&mut pane, Duration::from_millis(500));
    let buffer = buffer_text(&mut pane);
    for piece in &appended {
        let times = buffer.matches(&piece.word).count();
        match piece.landed {
            true => assert_eq!(times, 1, "{} once in the window:\n{buffer}", piece.word),
            // Unconfirmed: it may land when the command ends, but never twice.
            false => assert!(times <= 1, "{} twice in the window:\n{buffer}", piece.word),
        }
    }
    println!(
        "the unconfirmed append is in the window {} time(s) once the command ended",
        buffer.matches(&unconfirmed[0].word).count()
    );

    // --------------------------------- closing the window mid-command
    const BY_HAND: &str = "von Hand";
    press(&mut server, u32::from(b'o'));
    assert!(type_text(&mut server, BY_HAND));
    press(&mut server, ESCAPE);
    // On disk as it is typed, with no `:w`, before anything closes the window.
    wait_for(
        PATIENCE,
        "what was typed to reach the file by itself",
        || {
            run(&mut pane, Duration::from_millis(50));
            std::fs::read_to_string(&file)
                .ok()
                .filter(|text| text.contains(BY_HAND))
                .map(drop)
        },
    );
    let register = find_key(&server, u32::from(b'"')).expect("the layout has `\"`");
    press_key(&mut server, register);
    run(&mut pane, Duration::from_millis(500));
    close_window(&server, pane.window().id());
    wait_for(PATIENCE, "the pane to notice the window closed", || {
        matches!(
            pane.step(Duration::from_millis(50)),
            Ok(Status::Finished(_))
        )
        .then_some(())
    });
    drop(pane);
    let on_disk = std::fs::read_to_string(&file).expect("read the dictation file");
    assert!(
        on_disk.contains(BY_HAND),
        "closing the window mid-command lost what was typed:\n{on_disk}"
    );
    for piece in &appended {
        assert_eq!(
            on_disk.matches(&piece.word).count(),
            buffer.matches(&piece.word).count(),
            "{piece:?}"
        );
    }
    let files: Vec<PathBuf> = std::fs::read_dir(&config.dictation_dir)
        .expect("the dictation directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    assert_eq!(files, vec![file], "text was diverted to another file");
}

/// One committed piece, as the stand-in daemon saw it land.
#[derive(Debug, Clone)]
struct Appended {
    word: String,
    sent: Instant,
    took: Duration,
    landed: bool,
}

/// The daemon's editor thread, reduced to what it does during a latched
/// recording: an indicator ten times a second with a preview that grows,
/// and a committed piece every second continuing the same paragraph.
struct Dictation {
    stop: Arc<AtomicBool>,
    /// The session's own flag, which the daemon sets when it stops.
    quitting: Arc<AtomicBool>,
    tick: Arc<AtomicU64>,
    appended: Arc<Mutex<Vec<Appended>>>,
    thread: JoinHandle<()>,
}

impl Dictation {
    fn start(config: Nvim) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let tick = Arc::new(AtomicU64::new(0));
        let appended = Arc::new(Mutex::new(Vec::new()));
        let mut session = NvimSession::new(config);
        let quitting = session.quitting();
        let thread = {
            let (stop, tick, appended) = (stop.clone(), tick.clone(), appended.clone());
            std::thread::spawn(move || {
                session
                    .ensure(Want::Press)
                    .expect("attach to the pane's editor")
                    .expect("the pane's editor is there");
                let mut pieces = 0;
                let mut last_append = Instant::now();
                while !stop.load(Ordering::Acquire) {
                    let now = tick.fetch_add(1, Ordering::AcqRel) + 1;
                    let state = IndicatorState {
                        phase: IndicatorPhase::Recording,
                        level: (now % 7) as f64 / 7.0,
                        preview: format!("vorschau{now} und mehr"),
                        latched: true,
                        ..IndicatorState::default()
                    };
                    session
                        .set_indicator(&state, PreviewPlacement::NewParagraph)
                        .expect("an indicator push is only a socket write");
                    if last_append.elapsed() >= Duration::from_secs(1) {
                        pieces += 1;
                        let word = format!("diktat{pieces}x");
                        let sent = Instant::now();
                        let landed = session.append(&word, pieces > 1).is_ok();
                        appended.lock().expect("the log").push(Appended {
                            word,
                            sent,
                            took: sent.elapsed(),
                            landed,
                        });
                        last_append = Instant::now();
                    }
                    sleep(Duration::from_millis(100));
                }
                session.close();
            })
        };
        Self {
            stop,
            quitting,
            tick,
            appended,
            thread,
        }
    }

    fn appended(&self) -> Vec<Appended> {
        self.appended.lock().expect("the log").clone()
    }

    fn appended_since(&self, since: Instant) -> Vec<Appended> {
        self.appended()
            .into_iter()
            .filter(|piece| piece.sent >= since)
            .collect()
    }

    /// The number in the preview pushed last.
    fn pushed(&self) -> u64 {
        self.tick.load(Ordering::Acquire)
    }

    /// Stop the way the daemon does: the session's flag, then the thread.
    fn quit(self) -> Vec<Appended> {
        self.quitting.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        self.thread.join().expect("the dictation thread");
        self.appended.lock().expect("the log").clone()
    }
}

fn dictation_config(root: &Path) -> Nvim {
    let dictation = root.join("dictation");
    std::fs::create_dir_all(&dictation).expect("make the dictation directory");
    Nvim {
        mode: Mode::Attach,
        socket_path: root.join("nvim.sock"),
        dictation_dir: dictation,
        init: Some(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/lua/dictation_init.lua"
        ))),
        ..Nvim::default()
    }
}

/// Press the key that types `keysym` on the loaded layout.
fn press(server: &mut XServer, keysym: u32) {
    assert!(
        press_keysym(server, keysym),
        "the layout has keysym {keysym:#x}"
    );
}

/// Let the pane handle whatever arrives for `period`.
fn run(pane: &mut Pane, period: Duration) {
    let until = Instant::now() + period;
    while Instant::now() < until {
        let _ = pane.step(Duration::from_millis(20));
    }
}

/// The newest preview number anywhere on the screen.
fn newest_preview(screen: &str) -> Option<u64> {
    screen
        .split("vorschau")
        .skip(1)
        .filter_map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .max()
}

fn grid_text(pane: &Pane) -> String {
    (0..pane.size().1)
        .map(|row| pane.screen().line(row))
        .collect::<Vec<_>>()
        .join("\n")
}

fn buffer_text(pane: &mut Pane) -> String {
    pane.call(
        "nvim_buf_get_lines",
        vec![
            Value::from(0),
            Value::from(0),
            Value::from(-1),
            Value::from(false),
        ],
        PATIENCE,
    )
    .expect("read the buffer")
    .as_array()
    .expect("lines")
    .iter()
    .map(|line| line.as_str().unwrap_or_default())
    .collect::<Vec<_>>()
    .join("\n")
}
