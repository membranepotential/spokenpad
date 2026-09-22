//! Does the pane stay unfocused, and on top, under window managers other
//! than i3?
//!
//! Verdicts: sway (given the pane's `no_focus` rule over IPC), Openbox, KWin
//! on Wayland and KWin on X11 never focus it, and every one shows it above
//! the window the user is typing in.
//!
//! `tests/pane_window.rs` proves it on i3. This asks the same of sway (with
//! Xwayland), Openbox, KWin (Wayland, with Xwayland) and KWin (X11), each in
//! a headless session the test starts itself (`harness::desktops`), and each
//! with the same story:
//!
//! 1. A window of the test's own holds the focus, the way the application the
//!    user is dictating into does.
//! 2. The pane opens exactly as the daemon opens it — `NvimSession::ensure`
//!    in `nvim.mode = "pane"`, which measures the screen, adds the `no_focus`
//!    rule under sway, starts the pane thread, maps the window and attaches
//!    to the editor inside — and dictated text is appended to it, so it
//!    redraws, while keys are typed into the focused window.
//! 3. The focus is sampled every few milliseconds from before the open to
//!    after the last redraw, both as the X server's input focus and as the
//!    window manager reports it. No sample may name the pane or its frame.
//! 4. The pane is mapped, wholly on screen, and shown above the focused
//!    window.
//! 5. The user selecting the pane — a click, or on KWin Wayland the
//!    activation a click would cause — must focus it: that is the user asking,
//!    and the pane sets `WM_HINTS input = True` so they can type into it
//!    (`src/shell/pane/x11.rs`). Selecting the holder gives the focus back,
//!    and the pane stays above it.
//! 6. The next passage's pane — after the user closed the first — is not
//!    focused either. On an empty workspace the pane opens unfocused, except
//!    under sway, which focuses the first window on a workspace whatever the
//!    rules say: there the pane refuses to open, and no window appears.
//! 7. A positive control: the same window, not matched by the sway rule, then
//!    also without `_NET_WM_USER_TIME`, then also typed
//!    `_NET_WM_WINDOW_TYPE_NORMAL`, must be focused by the same window manager
//!    at one of those steps, or the checks above measure nothing.
//! 8. Ablations, recorded and not asserted: a normal window type with the user
//!    time kept, and the shipped window without `_NET_WM_STATE_ABOVE`.
//!
//! Every row is printed (run with `-- --nocapture`); they are the evidence in
//! `docs/experiments/2026-09-22-pane-stacking-and-sway.md`.
mod harness;

use harness::{
    SETTLE, XServer, close_window,
    desktops::{
        Desktop, FocusSampler, FocusStealingPrevention, KwinWayland, KwinX11, Openbox, SCREEN,
        Samples, Sway, ewmh_stacking_above, find_by_instance, screen_rect, with_frames,
    },
    wait_for,
};
use spokenpad::{
    config::{Mode, Nvim},
    core::{font::Points, geometry::Rect},
    shell::{
        nvim::NvimSession,
        pane::x11::{Display, INSTANCE, Window as PaneWindow},
    },
};
use std::{
    path::{Path, PathBuf},
    thread::sleep,
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, MapState, PropMode, Window,
            WindowClass,
        },
    },
    wrapper::ConnectionExt as _,
};

const PATIENCE: Duration = Duration::from_secs(20);

/// How long a control or ablation window waits, unmapped, between being
/// created and having a property rewritten by the test.
///
/// sway's Xwayland window manager sometimes missed a `WM_CLASS` rewritten
/// right after the window was created: in about one run in four, sway's tree
/// still showed the name the window was created with after the map, so
/// sway's `no_focus` rule for the pane matched a control renamed not to match
/// it. With this pause, ten runs in ten showed the new name. The pane itself
/// never rewrites a property; this is for the test's own rewrites only.
const REWRITE_PAUSE: Duration = SETTLE;

/// sway focuses every window it maps unless a `no_focus` rule matches it; it
/// reads neither `_NET_WM_USER_TIME` nor the window type. spokenpad adds that
/// rule for the pane over sway's IPC before the map, and this proves sway
/// honours it — and that the control, the same window under another
/// `WM_CLASS`, is focused.
#[test]
fn the_pane_never_takes_focus_on_sway() {
    if !harness::tools_or_skip(&["sway", "Xwayland", "nvim", "fc-match"]) {
        return;
    }
    let mut sway = Sway::start();
    let report = story(&mut sway);
    report.assert_never_focused();
}

#[test]
fn the_pane_never_takes_focus_on_openbox() {
    if !harness::tools_or_skip(&["Xvfb", "openbox", "nvim", "fc-match"]) {
        return;
    }
    let mut openbox = Openbox::start();
    let report = story(&mut openbox);
    report.assert_never_focused();
}

#[test]
fn the_pane_never_takes_focus_on_kwin_wayland() {
    if !harness::tools_or_skip(&[
        "kwin_wayland",
        "Xwayland",
        "dbus-daemon",
        "nvim",
        "fc-match",
    ]) {
        return;
    }
    let mut kwin = KwinWayland::start(FocusStealingPrevention::Low);
    let report = story(&mut kwin);
    report.assert_never_focused();
}

/// The same, with KWin's focus stealing prevention turned off, the most
/// permissive setting a user can choose.
#[test]
fn the_pane_never_takes_focus_on_kwin_wayland_without_focus_stealing_prevention() {
    if !harness::tools_or_skip(&[
        "kwin_wayland",
        "Xwayland",
        "dbus-daemon",
        "nvim",
        "fc-match",
    ]) {
        return;
    }
    let mut kwin = KwinWayland::start(FocusStealingPrevention::None);
    let report = story(&mut kwin);
    report.assert_never_focused();
}

#[test]
fn the_pane_never_takes_focus_on_kwin_x11() {
    if !harness::tools_or_skip(&["Xvfb", "kwin_x11", "dbus-daemon", "nvim", "fc-match"]) {
        return;
    }
    let mut kwin = KwinX11::start(FocusStealingPrevention::Low);
    let report = story(&mut kwin);
    report.assert_never_focused();
}

#[test]
fn the_pane_never_takes_focus_on_kwin_x11_without_focus_stealing_prevention() {
    if !harness::tools_or_skip(&["Xvfb", "kwin_x11", "dbus-daemon", "nvim", "fc-match"]) {
        return;
    }
    let mut kwin = KwinX11::start(FocusStealingPrevention::None);
    let report = story(&mut kwin);
    report.assert_never_focused();
}

// ----------------------------------------------------------------- the story

/// What one window manager did, stage by stage.
struct Report {
    desktop: String,
    /// Stages in which a sample named the pane, with what was seen.
    stolen: Vec<String>,
    /// Whether the positive control was focused: without it, "never focused"
    /// means nothing.
    control_focused: bool,
}

impl Report {
    fn new(desktop: String) -> Self {
        println!("\n{desktop}");
        Self {
            desktop,
            stolen: Vec::new(),
            control_focused: false,
        }
    }

    /// Print a row at once, so a run that fails halfway has already said
    /// what it saw.
    fn row(&mut self, row: String) {
        println!("{row}");
    }

    fn assert_never_focused(&self) {
        assert!(
            self.control_focused,
            "{}: the positive control was not focused either, so this run cannot tell \
             focus from no focus",
            self.desktop
        );
        assert!(
            self.stolen.is_empty(),
            "{}: the pane took the focus: {}",
            self.desktop,
            self.stolen.join("; ")
        );
    }
}

fn story(desktop: &mut dyn Desktop) -> Report {
    let mut report = Report::new(desktop.describe());

    // 1. The application the user is dictating into.
    let holder = focus_holder(desktop);
    report.row(format!(
        "focus holder 0x{holder:x}: focused by the window manager: {}",
        desktop.focused() == Some(holder)
    ));

    // 2 and 3. The pane, opened as the daemon opens it, redrawn while the
    // user types elsewhere.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let mut session = NvimSession::new(pane_config(
        directory.path(),
        &desktop.server().display,
        desktop.sway_socket(),
    ));
    let sampler = FocusSampler::start(&desktop.server().display, desktop.view());
    let opened = session.ensure().expect("open the pane");
    assert!(opened.is_some(), "the pane did not attach");
    let pane = pane_window(desktop);
    desktop.wait_until_managed(pane);
    let keys_before = key_presses(desktop.server(), holder);
    let mut typed = 0;
    for sentence in 0..6 {
        session
            .append(&format!("Satz {sentence} im Fenster."), sentence > 0)
            .expect("append to the pane");
        typed += usize::from(desktop.type_key());
        sleep(Duration::from_millis(150));
    }
    sleep(SETTLE);
    let samples = sampler.finish();
    let frames = with_frames(desktop.server(), pane);
    let keys = keys_before + key_presses(desktop.server(), holder);
    record(
        &mut report,
        "open, then six appends with a key typed after each",
        &samples,
        &frames,
    );
    report.row(if typed == 0 {
        "keys typed during the redraws: none, this desktop cannot be typed at headless".to_owned()
    } else {
        format!("keys typed during the redraws that reached the holder: {keys} of {typed}")
    });
    report.row(format!(
        "after the redraws the window manager focuses: {}",
        name(desktop.focused(), holder, &frames)
    ));

    // 4. Mapped, on screen, and above the window the user is typing in.
    let opened = placement(desktop, pane, holder);
    report.row(opened.row("after the open"));
    assert!(opened.viewable, "the pane is not viewable");
    assert!(
        opened.on_screen,
        "the pane is not wholly on the screen: {:?}",
        opened.rect
    );
    opened.assert_above("after the open");

    // 5. Selecting the pane is the user asking for the focus, and it must
    // get it; selecting the holder hands it back. Neither is a steal.
    let (x, y, width, height) = opened.rect;
    let centre = (x + width as i32 / 2, y + height as i32 / 2);
    desktop.select(pane, (centre.0 as i16, centre.1 as i16));
    sleep(SETTLE);
    let after_click = desktop.focused();
    let input_after_click = desktop.server().input_focus();
    report.row(format!(
        "the user selects the pane: the window manager focuses {}, the X input focus is on {}",
        name(after_click, holder, &frames),
        name(Some(input_after_click), holder, &frames)
    ));
    assert!(
        after_click.is_some_and(|window| frames.contains(&window))
            && frames.contains(&input_after_click),
        "{}: the user selected the pane and it did not get the focus",
        desktop.describe()
    );
    let holder_rect = screen_rect(desktop.server(), holder);
    let spot =
        outside(holder_rect, opened.rect).expect("a spot on the holder the pane leaves free");
    desktop.select(holder, spot);
    sleep(SETTLE);
    report.row(format!(
        "the user selects the holder again: the window manager focuses {}",
        name(desktop.focused(), holder, &frames)
    ));
    assert_eq!(
        desktop.focused(),
        Some(holder),
        "selecting the holder did not give it the focus back"
    );
    let reselected = placement(desktop, pane, holder);
    report.row(reselected.row("after the user selected the holder again"));
    reselected.assert_above("after the user selected the holder again");

    // 6a. The next passage: the user closed the pane, the next dictation
    // opens another.
    close_window(desktop.server(), pane);
    wait_for(PATIENCE, "the pane to close", || {
        find_by_instance(desktop.server(), INSTANCE)
            .is_none()
            .then_some(())
    });
    make_focused(desktop, holder);
    let sampler = FocusSampler::start(&desktop.server().display, desktop.view());
    session.ensure().expect("open the next pane");
    let next = pane_window(desktop);
    desktop.wait_until_managed(next);
    session
        .append("Der naechste Absatz.", false)
        .expect("append to the next pane");
    sleep(SETTLE);
    let samples = sampler.finish();
    let frames = with_frames(desktop.server(), next);
    record(
        &mut report,
        "the next passage's pane, after the user selected the first",
        &samples,
        &frames,
    );
    let placed = placement(desktop, next, holder);
    report.row(placed.row("the next passage's pane"));
    placed.assert_above("the next passage's pane");
    session.close();
    wait_for(PATIENCE, "the pane to close", || {
        find_by_instance(desktop.server(), INSTANCE)
            .is_none()
            .then_some(())
    });

    // 7. The positive control, on the holder's workspace.
    make_focused(desktop, holder);
    report.control_focused = control(desktop, &mut report);
    make_focused(desktop, holder);

    // 8. Which of the shipped properties holds on its own, and what
    // `_NET_WM_STATE_ABOVE` does to the stacking: recorded, not asserted.
    ablate(
        desktop,
        &mut report,
        holder,
        "user time 0 with `_NET_WM_WINDOW_TYPE_NORMAL` instead of `_UTILITY`",
        |server, window| {
            set_atoms(
                server,
                window,
                "_NET_WM_WINDOW_TYPE",
                "_NET_WM_WINDOW_TYPE_NORMAL",
            );
        },
    );
    make_focused(desktop, holder);
    ablate(
        desktop,
        &mut report,
        holder,
        "the shipped window without `_NET_WM_STATE_ABOVE`",
        |server, window| {
            server
                .connection
                .delete_property(window, server.atom("_NET_WM_STATE"))
                .expect("delete _NET_WM_STATE");
        },
    );
    make_focused(desktop, holder);

    // 6b. The first window on an empty workspace, where some window managers
    // focus whatever appears.
    desktop.go_to_empty_workspace();
    sleep(SETTLE);
    let before = desktop.server().input_focus();
    let sampler = FocusSampler::start(&desktop.server().display, desktop.view());
    let lone = session.ensure();
    if desktop.sway_socket().is_some() {
        // sway focuses the first window on a workspace whatever the rules
        // say, so the pane must not open there at all.
        let error = format!(
            "{:#}",
            lone.expect_err("the pane opened on an empty sway workspace")
        );
        sleep(SETTLE);
        let samples = sampler.finish();
        report.row(format!(
            "a pane on an empty workspace: refused ({error}); a pane window exists: {}; \
             {} samples, X input focus seen on {:?}",
            find_by_instance(desktop.server(), INSTANCE).is_some(),
            samples.count,
            samples.input.keys().collect::<Vec<_>>()
        ));
        assert!(
            error.contains("the focused workspace is empty"),
            "refused for another reason: {error}"
        );
        assert!(
            find_by_instance(desktop.server(), INSTANCE).is_none(),
            "a refused pane left a window behind"
        );
        return report;
    }
    lone.expect("open a pane on the empty workspace");
    let lone = pane_window(desktop);
    desktop.wait_until_managed(lone);
    session
        .append("Allein auf dem Arbeitsplatz.", false)
        .expect("append to the lone pane");
    sleep(SETTLE);
    let samples = sampler.finish();
    let frames = with_frames(desktop.server(), lone);
    record(
        &mut report,
        &format!(
            "the pane alone on an empty workspace (input focus before: {})",
            name(Some(before), holder, &frames)
        ),
        &samples,
        &frames,
    );
    session.close();
    report
}

fn record(report: &mut Report, stage: &str, samples: &Samples, frames: &[Window]) {
    let touched = samples.touched(frames);
    let row =
        format!(
        "{stage}: {} samples; X input focus seen on {}; the window manager focused {} — {}",
        samples.count,
        samples
            .input
            .keys()
            .map(|window| name_raw(*window, frames))
            .collect::<Vec<_>>()
            .join(", "),
        samples
            .manager
            .keys()
            .map(|window| window.map_or_else(|| "nothing".to_owned(), |id| name_raw(id, frames)))
            .collect::<Vec<_>>()
            .join(", "),
        if touched { "PANE FOCUSED" } else { "never the pane" }
    );
    if touched {
        report.stolen.push(row.clone());
    }
    report.row(row);
}

fn name(window: Option<Window>, holder: Window, frames: &[Window]) -> String {
    match window {
        None => "nothing".to_owned(),
        Some(window) if window == holder => "the holder".to_owned(),
        Some(window) => name_raw(window, frames),
    }
}

fn name_raw(window: Window, frames: &[Window]) -> String {
    match window {
        0 => "None".to_owned(),
        1 => "PointerRoot".to_owned(),
        window if frames.first() == Some(&window) => "THE PANE".to_owned(),
        window if frames.contains(&window) => "THE PANE'S FRAME".to_owned(),
        window => format!("0x{window:x}"),
    }
}

// ------------------------------------------------------------- the holder

/// A plain window of the test's own that takes the focus, as the application
/// the user dictates into would, and counts the keys it is sent.
fn focus_holder(desktop: &mut dyn Desktop) -> Window {
    let server = desktop.server();
    let id = server.connection.generate_id().expect("a window id");
    server
        .connection
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            server.root,
            0,
            0,
            SCREEN.0 / 2,
            SCREEN.1 / 2,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(server.connection.setup().roots[0].white_pixel)
                .event_mask(EventMask::KEY_PRESS),
        )
        .expect("create the focus holder");
    server
        .connection
        .change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"focus-holder\0FocusHolder\0",
        )
        .expect("name the focus holder");
    x11rb::properties::WmHints {
        input: Some(true),
        ..x11rb::properties::WmHints::new()
    }
    .set(&server.connection, id)
    .expect("write WM_HINTS");
    server.connection.map_window(id).expect("map it");
    server.connection.flush().expect("flush");
    desktop.wait_until_managed(id);
    sleep(SETTLE);
    make_focused(desktop, id);
    id
}

/// Make sure `holder` has the focus before a stage starts, clicking it if a
/// window manager did not give it the focus by itself.
fn make_focused(desktop: &mut dyn Desktop, holder: Window) {
    if desktop.focused() != Some(holder) {
        let (x, y, width, height) = screen_rect(desktop.server(), holder);
        desktop.select(
            holder,
            (
                (x + width as i32 / 4) as i16,
                (y + height as i32 / 4) as i16,
            ),
        );
        sleep(SETTLE);
    }
    assert_eq!(
        desktop.focused(),
        Some(holder),
        "{}: the focus holder does not have the focus, so no stage can tell a steal",
        desktop.describe()
    );
}

/// How many key presses reached `holder` since the last call.
fn key_presses(server: &XServer, holder: Window) -> usize {
    let mut count = 0;
    while let Some(event) = server
        .connection
        .poll_for_event()
        .expect("poll the test's events")
    {
        if let Event::KeyPress(press) = event
            && press.event == holder
        {
            count += 1;
        }
    }
    count
}

// --------------------------------------------------------------- the pane

/// The dictation settings a daemon in pane mode would run with, pointed into
/// `root`, at the test's display and, under sway, at the test's sway.
fn pane_config(root: &Path, display: &str, sway_socket: Option<PathBuf>) -> Nvim {
    let dictation = root.join("dictation");
    std::fs::create_dir_all(&dictation).expect("make the dictation directory");
    Nvim {
        mode: Mode::Pane,
        socket_path: root.join("nvim.sock"),
        dictation_dir: dictation,
        display: Some(display.to_owned()),
        sway_socket,
        init: Some(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/lua/dictation_init.lua"
        ))),
        font_size: Points::try_from(11.0).expect("a point size"),
        notify: false,
        ..Nvim::default()
    }
}

fn pane_window(desktop: &dyn Desktop) -> Window {
    wait_for(PATIENCE, "the pane window", || {
        find_by_instance(desktop.server(), INSTANCE)
    })
}

struct Placement {
    rect: (i32, i32, u32, u32),
    viewable: bool,
    on_screen: bool,
    floating: Option<bool>,
    /// Whether the window manager shows the pane above the holder
    /// ([`Desktop::above`]).
    above: Option<bool>,
    /// Whether `_NET_CLIENT_LIST_STACKING` puts the pane above the holder,
    /// when the window manager publishes it.
    ewmh_above: Option<bool>,
}

impl Placement {
    fn row(&self, stage: &str) -> String {
        let (x, y, width, height) = self.rect;
        let show =
            |value: Option<bool>| value.map_or_else(|| "unknown".to_owned(), |v| v.to_string());
        format!(
            "placement {stage}: {width}x{height} at {x},{y} on a {}x{} screen; viewable: {}; \
             wholly on screen: {}; floating: {}; shown above the holder: {}; above it in \
             _NET_CLIENT_LIST_STACKING: {}",
            SCREEN.0,
            SCREEN.1,
            self.viewable,
            self.on_screen,
            show(self.floating),
            show(self.above),
            self.ewmh_above
                .map_or_else(|| "not published".to_owned(), |value| value.to_string())
        )
    }

    fn assert_above(&self, stage: &str) {
        assert_eq!(
            self.above,
            Some(true),
            "{stage}: the pane is not shown above the focused window"
        );
        assert_ne!(
            self.ewmh_above,
            Some(false),
            "{stage}: _NET_CLIENT_LIST_STACKING puts the pane below the focused window"
        );
    }
}

fn placement(desktop: &dyn Desktop, pane: Window, holder: Window) -> Placement {
    let server = desktop.server();
    let rect = screen_rect(server, pane);
    let viewable = server
        .connection
        .get_window_attributes(pane)
        .expect("ask for the attributes")
        .reply()
        .expect("the attributes")
        .map_state
        == MapState::VIEWABLE;
    let (x, y, width, height) = rect;
    let on_screen = x >= 0
        && y >= 0
        && width > 0
        && height > 0
        && x + width as i32 <= i32::from(SCREEN.0)
        && y + height as i32 <= i32::from(SCREEN.1);
    Placement {
        rect,
        viewable,
        on_screen,
        floating: desktop.floating(pane),
        above: desktop.above(pane, holder),
        ewmh_above: ewmh_stacking_above(server, pane, holder),
    }
}

/// A point inside `holder` and outside `pane`, to click the holder with.
fn outside(holder: (i32, i32, u32, u32), pane: (i32, i32, u32, u32)) -> Option<(i16, i16)> {
    let (hx, hy, hw, hh) = holder;
    let (px, py, pw, ph) = pane;
    let inside_pane =
        |x: i32, y: i32| x >= px && x < px + pw as i32 && y >= py && y < py + ph as i32;
    (0..8)
        .flat_map(|row| (0..8).map(move |column| (row, column)))
        .map(|(row, column)| {
            (
                hx + (hw as i32 * (2 * column + 1)) / 16,
                hy + (hh as i32 * (2 * row + 1)) / 16,
            )
        })
        .find(|(x, y)| !inside_pane(*x, *y))
        .map(|(x, y)| (x as i16, y as i16))
}

// ------------------------------------------------------ the positive control

/// The window the pane ships, taken apart one step at a time until the window
/// manager focuses it: first renamed, so sway's `no_focus` rule for the pane
/// does not match it; then also without `_NET_WM_USER_TIME`; then also typed
/// `_NET_WM_WINDOW_TYPE_NORMAL`. Returns whether any step was focused: one
/// must be, or no row above can tell focus from no focus.
fn control(desktop: &mut dyn Desktop, report: &mut Report) -> bool {
    for (label, drop_user_time, normal) in [
        ("control: `WM_CLASS` spokenpad-control", false, false),
        (
            "control: `WM_CLASS` spokenpad-control, no `_NET_WM_USER_TIME`",
            true,
            false,
        ),
        (
            "control: `WM_CLASS` spokenpad-control, no `_NET_WM_USER_TIME`, \
             `_NET_WM_WINDOW_TYPE_NORMAL`",
            true,
            true,
        ),
    ] {
        let window = PaneWindow::open(
            Display::connect(&desktop.server().display).expect("connect for the control"),
            Rect {
                x: 600,
                y: 300,
                width: 400,
                height: 200,
            },
            "spokenpad control",
        )
        .expect("open the control window");
        sleep(REWRITE_PAUSE);
        let server = desktop.server();
        server
            .connection
            .change_property8(
                PropMode::REPLACE,
                window.id(),
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                b"spokenpad-control\0spokenpad-control\0",
            )
            .expect("rename the control");
        if drop_user_time {
            server
                .connection
                .delete_property(window.id(), server.atom("_NET_WM_USER_TIME"))
                .expect("delete _NET_WM_USER_TIME");
        }
        if normal {
            set_atoms(
                server,
                window.id(),
                "_NET_WM_WINDOW_TYPE",
                "_NET_WM_WINDOW_TYPE_NORMAL",
            );
        }
        server.sync();
        let before = server.input_focus();
        // Sampled like the pane, so the control is held to the same measure:
        // a focus that lasted one sample counts.
        let sampler = FocusSampler::start(&desktop.server().display, desktop.view());
        window.map().expect("map the control");
        desktop.wait_until_managed(window.id());
        sleep(SETTLE);
        let samples = sampler.finish();
        let frames = with_frames(desktop.server(), window.id());
        let manager = desktop.focused();
        let input = desktop.server().input_focus();
        let focused = samples.touched(&frames);
        report.row(format!(
            "{label}: focused: {focused} (after the map the window manager focuses {}, the X \
             input focus is on {}; before the map it was on 0x{before:x})",
            manager.map_or_else(|| "nothing".to_owned(), |id| name_raw(id, &frames)),
            name_raw(input, &frames)
        ));
        drop(window);
        sleep(SETTLE);
        if focused {
            return true;
        }
    }
    false
}

/// The window the pane ships, with one property changed before the map by
/// `change`, which writes it over the test's connection. Is it still
/// unfocused, and where is it shown? Recorded, not asserted.
fn ablate(
    desktop: &mut dyn Desktop,
    report: &mut Report,
    holder: Window,
    label: &str,
    change: impl FnOnce(&XServer, Window),
) {
    let window = PaneWindow::open(
        Display::connect(&desktop.server().display).expect("connect for the ablation"),
        Rect {
            x: 500,
            y: 300,
            width: 400,
            height: 200,
        },
        "spokenpad ablation",
    )
    .expect("open the ablation window");
    sleep(REWRITE_PAUSE);
    change(desktop.server(), window.id());
    desktop.server().sync();
    window.map().expect("map the ablation window");
    desktop.wait_until_managed(window.id());
    sleep(SETTLE);
    let frames = with_frames(desktop.server(), window.id());
    let placed = placement(desktop, window.id(), holder);
    report.row(format!(
        "ablation, {label}: on map the window manager focuses {}; shown above the holder: {}; \
         above it in _NET_CLIENT_LIST_STACKING: {}",
        name(desktop.focused(), holder, &frames),
        placed
            .above
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        placed
            .ewmh_above
            .map_or_else(|| "not published".to_owned(), |value| value.to_string())
    ));
    drop(window);
    sleep(SETTLE);
}

/// Replace one atom-list property on `window`.
fn set_atoms(server: &XServer, window: Window, property: &str, value: &str) {
    server
        .connection
        .change_property32(
            PropMode::REPLACE,
            window,
            server.atom(property),
            AtomEnum::ATOM,
            &[server.atom(value)],
        )
        .unwrap_or_else(|error| panic!("write {property}: {error}"));
}
