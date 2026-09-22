//! P0 of the own-window plan: does [`shell::pane::x11::Window`] stay unfocused
//! when it appears, and still accept a click and keystrokes?
//!
//! Both halves of the rule are asserted: the window never takes the focus by
//! itself, and the user's own click does give it the focus, so they can type
//! into it. It is also shown above the window the user is typing in, before
//! and after they click back into that window.
//!
//! The check runs against an X server and a window manager this test starts
//! itself: an `Xvfb` on a free display number and an `i3` with a generated
//! config and its own IPC socket. It never opens anything on the user's
//! display, never reads the user's i3 config, and stops every process it
//! started, also when an assertion fails.
//!
//! The window under test is the one the pane ships, created by the library in
//! this process. Beside the pass criteria the test records an ablation: the
//! same window with one property rewritten before the map, over a second X
//! connection, since a window manager only ever reads what is on the window at
//! map time. Those rows are printed (run with `-- --nocapture`) and are the
//! evidence in
//! `docs/experiments/2026-09-21-own-window-p0-properties.md`.

mod harness;

use harness::{
    I3, SETTLE, XServer, click,
    desktops::{ewmh_stacking_above, x_stacking_above},
    find_key, move_pointer, press_key,
};
use spokenpad::{
    config::PaneLayout,
    core::geometry::Rect,
    shell::pane::x11::{self, Display, Window as Pane},
};
use std::{thread::sleep, time::Duration};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, PropMode, Window},
    },
    wrapper::ConnectionExt as _,
};

/// One map, as the window manager and the X server report it afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observed {
    focused: bool,
    floating: bool,
    stole_focus: bool,
}

impl Observed {
    fn row(&self, label: &str) -> String {
        format!(
            "| {label} | {} | {} | {} |",
            yes_no(self.focused),
            yes_no(self.floating),
            if self.stole_focus {
                "moved"
            } else {
                "unchanged"
            }
        )
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

// -------------------------------------------------- the window under test

/// The pane's window plus the events it has reported so far. The pane selects
/// its own events on its own connection, so this is what the real window sees.
struct Watched {
    window: Pane,
    key_press: usize,
    button_press: usize,
    focus_in: usize,
}

impl Watched {
    fn open(server: &XServer) -> Self {
        Self::at(
            server,
            Rect {
                x: 0,
                y: 0,
                width: 480,
                height: 240,
            },
        )
    }

    fn at(server: &XServer, rect: Rect) -> Self {
        let display = Display::connect(&server.display).expect("connect to the test display");
        let window = Pane::open(display, rect, "spokenpad dictation", PaneLayout::Floating)
            .expect("open the pane window");
        Self {
            window,
            key_press: 0,
            button_press: 0,
            focus_in: 0,
        }
    }

    fn pump(&mut self) {
        while let Some(event) = self.window.poll().expect("poll the pane's events") {
            match event {
                Event::KeyPress(_) => self.key_press += 1,
                Event::ButtonPress(_) => self.button_press += 1,
                Event::FocusIn(_) => self.focus_in += 1,
                _ => {}
            }
        }
    }

    fn id(&self) -> Window {
        self.window.id()
    }
}

// ----------------------------------------------- rewriting properties to ablate

/// The ablation rewrites a property on the pane's window over the *test's*
/// connection, before the window is mapped. A window manager reads what is on
/// the window at map time and does not care which client put it there, so this
/// measures the shipped window with one property changed, rather than a
/// second, similar window the test built itself.
fn set_user_time(server: &XServer, window: Window, value: Option<u32>) {
    let atom = server.atom("_NET_WM_USER_TIME");
    match value {
        Some(time) => server
            .connection
            .change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[time])
            .expect("write _NET_WM_USER_TIME"),
        None => server
            .connection
            .delete_property(window, atom)
            .expect("delete _NET_WM_USER_TIME"),
    };
    server.connection.flush().expect("flush");
}

fn set_window_type(server: &XServer, window: Window, name: &str) {
    let property = server.atom("_NET_WM_WINDOW_TYPE");
    let value = server.atom(name);
    server
        .connection
        .change_property32(
            PropMode::REPLACE,
            window,
            property,
            AtomEnum::ATOM,
            &[value],
        )
        .expect("write _NET_WM_WINDOW_TYPE");
    server.connection.flush().expect("flush");
}

fn set_input_hint(server: &XServer, window: Window, input: bool) {
    x11rb::properties::WmHints {
        input: Some(input),
        initial_state: Some(x11rb::properties::WmHintsState::Normal),
        ..x11rb::properties::WmHints::new()
    }
    .set(&server.connection, window)
    .expect("write WM_HINTS");
    server.connection.flush().expect("flush");
}

fn announce_take_focus(server: &XServer, window: Window) {
    let protocols = server.atom("WM_PROTOCOLS");
    let delete = server.atom("WM_DELETE_WINDOW");
    let take_focus = server.atom("WM_TAKE_FOCUS");
    server
        .connection
        .change_property32(
            PropMode::REPLACE,
            window,
            protocols,
            AtomEnum::ATOM,
            &[delete, take_focus],
        )
        .expect("write WM_PROTOCOLS");
    server.connection.flush().expect("flush");
}

/// The EWMH way to move a window, as a client message to the root. The pane
/// itself uses `ConfigureWindow` ([`Pane::place`]); this is here to record
/// whether a window manager honours the other mechanism too.
fn net_moveresize(server: &XServer, window: Window, rect: Rect) {
    // Flags: window gravity in the low byte (0 = the gravity from
    // WM_NORMAL_HINTS), one bit each for x, y, width and height, then the
    // source indication (1 = a normal application).
    const FLAGS: u32 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 12);
    let message = ClientMessageEvent::new(
        32,
        window,
        server.atom("_NET_MOVERESIZE_WINDOW"),
        [FLAGS, rect.x as u32, rect.y as u32, rect.width, rect.height],
    );
    server
        .connection
        .send_event(
            false,
            server.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            message,
        )
        .expect("send _NET_MOVERESIZE_WINDOW");
    server.connection.flush().expect("flush");
}

/// Map an unmapped window and record what the map did to the focus.
fn map_and_watch(server: &mut XServer, i3: &I3, pane: &mut Watched) -> Observed {
    // Keep the pointer in a corner: with focus_follows_mouse off it cannot
    // move the focus, but a window mapped under it would muddy the reading.
    move_pointer(server, 5, 5);
    let before = server.input_focus();
    pane.window.map().expect("map the pane");
    i3.wait_until_managed(pane.id());
    sleep(SETTLE);
    let node = i3.node(pane.id()).expect("i3 still manages the window");
    pane.pump();
    Observed {
        focused: node.focused,
        floating: node.floating,
        stole_focus: server.input_focus() != before,
    }
}

/// Unmap a window that is already managed, then map it again. The focus is
/// read after the unmap, since the unmap itself hands the focus back.
fn remap(server: &mut XServer, i3: &I3, pane: &mut Watched) -> Observed {
    pane.window.unmap().expect("unmap the pane");
    sleep(SETTLE);
    map_and_watch(server, i3, pane)
}

/// Open a fresh pane, let `ablate` rewrite its properties before the map, and
/// map it.
fn ablate(
    server: &mut XServer,
    i3: &I3,
    ablate: impl FnOnce(&XServer, Window),
) -> (Watched, Observed) {
    let mut pane = Watched::open(server);
    ablate(server, pane.id());
    // The rewrite went over the test's connection and the map goes over the
    // pane's: only a round trip puts them in that order at the server.
    server.sync();
    let observed = map_and_watch(server, i3, &mut pane);
    (pane, observed)
}

// ----------------------------------------------------------------- the check

#[test]
fn the_pane_window_never_takes_focus_on_i3() {
    if !harness::tools_or_skip(&["Xvfb", "i3"]) {
        return;
    }
    let mut server = XServer::start();
    let i3 = I3::start(&server);
    let mut report: Vec<String> = Vec::new();

    // A plain window, so the workspace under test is never empty and the focus
    // has somewhere to be before each map.
    let base = server.plain_window();
    i3.wait_until_managed(base);
    sleep(SETTLE);
    assert!(
        i3.node(base).expect("base window").focused,
        "the plain window should hold the focus before each case"
    );

    // (a) not focused on map, on a workspace that already has a focused window
    let mut pane = Watched::open(&server);
    let observed = map_and_watch(&mut server, &i3, &mut pane);
    assert!(!observed.focused, "i3 focused the pane on map (tree)");
    assert!(
        !observed.stole_focus,
        "the X input focus moved when the pane was mapped"
    );
    assert_eq!(pane.focus_in, 0, "the window itself saw a FocusIn on map");
    // (b) floating, and so above the tiled window that has the focus
    assert!(observed.floating, "i3 did not float the pane");
    assert_eq!(
        x_stacking_above(&server, pane.id(), base),
        Some(true),
        "the pane is not above the focused window after the map"
    );
    report.push(observed.row("the properties the pane ships"));

    // Placement: P2 must be able to put the pane where the pointer is.
    let created = pane.window.geometry().expect("geometry");
    pane.window
        .place(Rect {
            x: 40,
            y: 60,
            width: 320,
            height: 200,
        })
        .expect("place the pane");
    sleep(Duration::from_millis(300));
    let configured = pane.window.geometry().expect("geometry");
    net_moveresize(
        &server,
        pane.id(),
        Rect {
            x: 700,
            y: 420,
            width: 300,
            height: 180,
        },
    );
    sleep(Duration::from_millis(300));
    let moveresized = pane.window.geometry().expect("geometry");
    report.push(format!(
        "| placement | created at {},{} | `Window::place` -> {:?} | `_NET_MOVERESIZE_WINDOW` -> {:?} |",
        created.x,
        created.y,
        (configured.x, configured.y, configured.width, configured.height),
        (
            moveresized.x,
            moveresized.y,
            moveresized.width,
            moveresized.height
        )
    ));
    assert_eq!(
        (
            configured.x,
            configured.y,
            configured.width,
            configured.height
        ),
        (40, 60, 320, 200),
        "`Window::place` did not place the floating pane where it was told"
    );

    // (c) focused after a click inside it
    let rect = pane.window.geometry().expect("geometry");
    let centre = (
        i16::try_from(rect.x + rect.width as i32 / 2).expect("on screen"),
        i16::try_from(rect.y + rect.height as i32 / 2).expect("on screen"),
    );
    click(&mut server, centre.0, centre.1);
    sleep(SETTLE);
    pane.pump();
    assert!(
        i3.node(pane.id()).expect("the pane").focused,
        "a click did not focus the pane"
    );
    assert_eq!(
        server.input_focus(),
        pane.id(),
        "the X input focus is not on the pane after a click"
    );
    assert!(pane.button_press >= 1, "the click never reached the window");

    // (d) typed keys arrive
    let key = find_key(&server, u32::from(b'a')).expect("the layout has an `a`");
    press_key(&mut server, key);
    press_key(&mut server, key);
    sleep(Duration::from_millis(300));
    pane.pump();
    assert_eq!(
        pane.key_press, 2,
        "typed keys did not reach the focused pane"
    );

    // (d2) the user clicks back into the window they were typing in: it gets
    // the focus, and the pane stays above it.
    click(&mut server, 10, 10);
    sleep(SETTLE);
    assert!(
        i3.node(base).expect("the base window").focused,
        "a click on the base window did not give it the focus back"
    );
    let above = x_stacking_above(&server, pane.id(), base);
    let ewmh_above = ewmh_stacking_above(&server, pane.id(), base);
    report.push(format!(
        "| stacking after the user clicked back into the focused window | above it in the X stacking order: {above:?} | in `_NET_CLIENT_LIST_STACKING`: {ewmh_above:?} | |"
    ));
    assert_eq!(
        above,
        Some(true),
        "the pane is not above the focused window"
    );
    assert_ne!(
        ewmh_above,
        Some(false),
        "_NET_CLIENT_LIST_STACKING puts the pane below the focused window"
    );

    // (e) re-map after unmap is unfocused again, and the property that makes
    // that true is still 0 after the user has clicked and typed: the pane
    // never rewrites it, so no interaction can turn it into a real timestamp.
    assert_eq!(
        read_user_time(&server, pane.id()),
        Some(0),
        "_NET_WM_USER_TIME changed while the window was used"
    );
    let again = remap(&mut server, &i3, &mut pane);
    assert!(
        !again.focused,
        "the pane took focus when it was mapped a second time"
    );
    assert!(
        !again.stole_focus,
        "the X input focus moved on the second map"
    );
    report.push(again.row("as shipped, re-mapped after a click and two keys"));
    drop(pane);
    sleep(SETTLE);

    // Ablation: a real timestamp in the same property is enough to lose it.
    // A toolkit would write one on every keystroke; the pane must not.
    let (timestamped, observed) = ablate(&mut server, &i3, |server, window| {
        set_user_time(server, window, Some(1))
    });
    report.push(observed.row("`_NET_WM_USER_TIME` rewritten to 1 before the map"));
    drop(timestamped);
    sleep(SETTLE);

    // Ablation, and the positive control for every check above: without the
    // user time, i3 focuses the same window on map. If this ever stops being
    // true, the test is measuring nothing.
    let (control, observed) = ablate(&mut server, &i3, |server, window| {
        set_user_time(server, window, None)
    });
    assert!(
        observed.focused && observed.stole_focus,
        "without _NET_WM_USER_TIME the window was still not focused: the test \
         cannot tell focus from no focus, so the other rows prove nothing"
    );
    report.push(observed.row("no `_NET_WM_USER_TIME`"));
    drop(control);
    sleep(SETTLE);

    // Ablation: the type only decides floating on i3.
    let (normal, observed) = ablate(&mut server, &i3, |server, window| {
        set_window_type(server, window, "_NET_WM_WINDOW_TYPE_NORMAL")
    });
    assert!(!observed.focused, "user time 0 needs no window type on i3");
    assert!(
        !observed.floating,
        "a normal window floated without being asked to"
    );
    report.push(observed.row("`_NET_WM_WINDOW_TYPE_NORMAL` instead of `_UTILITY`"));
    drop(normal);
    sleep(SETTLE);

    // Ablation: `WM_TAKE_FOCUS` beside the shipped set.
    let (take_focus, observed) = ablate(&mut server, &i3, announce_take_focus);
    report.push(observed.row("as shipped, plus `WM_TAKE_FOCUS`"));
    drop(take_focus);
    sleep(SETTLE);

    // Ablation: the ICCCM "No Input" model, with no user time.
    let (no_input, observed) = ablate(&mut server, &i3, |server, window| {
        set_user_time(server, window, None);
        set_input_hint(server, window, false);
    });
    report.push(observed.row("`WM_HINTS input = False`, no user time"));
    drop(no_input);
    sleep(SETTLE);

    // Placement asked for before the map: P2 wants the pane to appear where
    // the pointer is, not to jump there a frame later.
    let mut placed = Watched::at(
        &server,
        Rect {
            x: 220,
            y: 140,
            width: 400,
            height: 200,
        },
    );
    let observed = map_and_watch(&mut server, &i3, &mut placed);
    assert!(!observed.focused, "the placed pane took focus");
    let landed = placed.window.geometry().expect("geometry");
    report.push(format!(
        "| placement asked for before the map (`WM_NORMAL_HINTS` position 220,140, size 400x200) | landed at {:?} | | |",
        (landed.x, landed.y, landed.width, landed.height)
    ));
    drop(placed);
    sleep(SETTLE);

    // (a, second half) not focused on map on an empty workspace, where i3's
    // `no_focus` rule does not apply.
    i3.command("workspace pane-empty");
    sleep(SETTLE);
    let mut empty = Watched::open(&server);
    let observed = map_and_watch(&mut server, &i3, &mut empty);
    assert!(
        !observed.focused,
        "the pane took focus as the first window on an empty workspace (tree)"
    );
    assert!(
        !observed.stole_focus,
        "the X input focus moved when the pane was mapped on an empty workspace"
    );
    report.push(observed.row("as shipped, empty workspace"));
    drop(empty);

    let manager = x11::manager(&server.connection, 0);
    println!(
        "\ni3 {}, display {}, window manager on the display: {:?}",
        i3.version(),
        server.display,
        manager.name
    );
    assert_eq!(
        manager.name.as_deref(),
        Some("i3"),
        "i3 must not look like sway"
    );
    println!("| window properties | focused on map | floating | X input focus |");
    println!("|---|---|---|---|");
    for line in report {
        println!("{line}");
    }
}

fn read_user_time(server: &XServer, window: Window) -> Option<u32> {
    let atom = server.atom("_NET_WM_USER_TIME");
    server
        .connection
        .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)
        .expect("ask for _NET_WM_USER_TIME")
        .reply()
        .expect("the property")
        .value32()
        .and_then(|mut values| values.next())
}
