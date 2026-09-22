//! Under focus-follows-mouse, does only a deliberate move of the pointer
//! into the pane focus it?
//!
//! The pane opens beside the pointer, its outer frame a gap away
//! (`core::geometry::placement`). Before, it opened with its window's corner
//! on the pointer, and a nudge of one to three pixels up or left crossed i3's
//! frame and focused it: `docs/experiments/2026-09-22-pane-hover-focus.md`.
//! Each story here opens the pane the way the daemon does — the monitor and
//! the pointer from `place::target`, `Pane::open`, `Pane::show` — under i3
//! with `focus_follows_mouse yes` or Openbox with `followMouse yes`, with a
//! focused window of the test's own under the pointer, and asserts:
//!
//! 1. **The map:** no focus sample, from before the open until the window
//!    manager has settled, names the pane or its frame; and the frame is
//!    where it belongs — the gap from the pointer, or around it.
//! 2. **A jiggle:** the pointer visits every point within 6 pixels of where
//!    it rested (169 of them), returning to rest after each. No focus.
//! 3. **The pane's own resize:** the window grows and shrinks back, as a
//!    `ConfigureWindow` from the pane. No focus.
//! 4. **A deliberate move:** the pointer glides from outside into the pane.
//!    It is focused. Without this the rows above would measure nothing.
//!
//! Every story runs on its own headless desktop (`tests/harness`); nothing
//! reaches the user's display. Run with `-- --nocapture` for the geometry.
mod harness;

use harness::{
    I3Focus, SETTLE,
    desktops::{
        Desktop, FocusSampler, I3, Openbox, OpenboxFocus, SCREEN, screen_rect, with_frames,
    },
    move_pointer, wait_for,
};
use spokenpad::{
    config::{Mode, Nvim, PaneLayout},
    core::{font::Points, geometry::Dimensions},
    shell::{
        nvim::pane_launch,
        pane::{Options, Pane, place},
    },
};
use std::{
    ffi::CString,
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, Window, WindowClass,
    },
    wrapper::ConnectionExt as _,
};

/// How far the jiggle reaches from where the pointer rested, as in the
/// investigation.
const JIGGLE: i16 = 6;

/// Where the pane's frame is expected, relative to the resting pointer.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Right of and below the pointer, the frame's corner `gap` pixels away
    /// on each axis.
    RightBelow { gap: i32 },
    /// Only below it by `gap`: too wide to go beside it, so centred on it
    /// across.
    Below { gap: i32 },
    /// Beside the pointer somewhere the window manager chose (a tile): only
    /// that the pointer is outside the frame.
    Outside,
    /// Around the pointer: too large to go beside it either way. The window,
    /// not only its frame, holds the pointer, `gap` from every edge the
    /// screen does not hold back.
    Around { gap: i32 },
}

struct Story {
    rest: (i16, i16),
    dimensions: Dimensions,
    layout: PaneLayout,
    expect: Expect,
}

fn tools() -> bool {
    harness::tools_or_skip(&["Xvfb", "i3", "openbox", "nvim", "fc-match"])
}

fn cells(columns: u16, lines: u16) -> Dimensions {
    Dimensions::new(columns, lines).expect("a grid")
}

fn i3(resources: &str) -> I3 {
    I3::start_with(I3Focus::FollowsMouse, resources)
}

#[test]
fn hover_focuses_a_floating_pane_on_i3_only_when_the_pointer_moves_in() {
    if !tools() {
        return;
    }
    run(
        &mut i3("Xft.dpi: 96\n"),
        Story {
            rest: (300, 200),
            dimensions: Dimensions::DEFAULT,
            layout: PaneLayout::Floating,
            expect: Expect::RightBelow { gap: 20 },
        },
    );
}

/// At 192 dpi i3's title bar is 30 pixels, and the gap 40.
#[test]
fn hover_focuses_a_floating_pane_on_i3_at_192_dpi_only_when_the_pointer_moves_in() {
    if !tools() {
        return;
    }
    run(
        &mut i3("Xft.dpi: 192\n"),
        Story {
            rest: (200, 150),
            dimensions: cells(30, 8),
            layout: PaneLayout::Floating,
            expect: Expect::RightBelow { gap: 40 },
        },
    );
}

/// Tiled, i3 decides where the pane goes. With the pointer resting in the
/// window beside the tile, and with it resting where the tile appears.
#[test]
fn hover_focuses_a_tiled_pane_on_i3_only_when_the_pointer_moves_in() {
    if !tools() {
        return;
    }
    run(
        &mut i3("Xft.dpi: 96\n"),
        Story {
            rest: (300, 200),
            dimensions: Dimensions::DEFAULT,
            layout: PaneLayout::Tiled,
            expect: Expect::Outside,
        },
    );
    run(
        &mut i3("Xft.dpi: 96\n"),
        Story {
            rest: (960, 400),
            dimensions: Dimensions::DEFAULT,
            layout: PaneLayout::Tiled,
            expect: Expect::Around { gap: 20 },
        },
    );
}

/// Near the right edge a pane as wide as the screen has room on neither side
/// of the pointer: it is centred across as far as the screen allows, and
/// goes below the pointer. As large as the screen, it has room nowhere, and
/// opens around the pointer resting in its corner.
#[test]
fn a_pane_with_no_room_beside_the_pointer_is_centred_on_it_on_i3() {
    if !tools() {
        return;
    }
    run(
        &mut i3("Xft.dpi: 96\n"),
        Story {
            rest: (1270, 100),
            dimensions: cells(500, 10),
            layout: PaneLayout::Floating,
            expect: Expect::Below { gap: 20 },
        },
    );
    run(
        &mut i3("Xft.dpi: 96\n"),
        Story {
            rest: (1275, 795),
            dimensions: cells(500, 500),
            layout: PaneLayout::Floating,
            expect: Expect::Around { gap: 20 },
        },
    );
}

#[test]
fn hover_focuses_the_pane_on_openbox_only_when_the_pointer_moves_in() {
    if !tools() {
        return;
    }
    for focus in [OpenboxFocus::FollowsMouse, OpenboxFocus::UnderMouse] {
        println!("Openbox, {focus:?}");
        let mut openbox = Openbox::start_with(focus);
        // Set, so the pane cannot read the resolution from the user's own
        // `~/.Xresources`.
        openbox.server.set_resources("Xft.dpi: 96\n");
        run(
            &mut openbox,
            Story {
                rest: (300, 200),
                dimensions: Dimensions::DEFAULT,
                layout: PaneLayout::Floating,
                expect: Expect::RightBelow { gap: 20 },
            },
        );
    }
}

// ------------------------------------------------------------- the story

fn run(desktop: &mut dyn Desktop, story: Story) {
    let Story {
        rest,
        dimensions,
        layout,
        expect,
    } = story;
    println!(
        "{}, pane {layout:?}, pointer at {rest:?}",
        desktop.describe()
    );
    let holder = holder(desktop);
    move_pointer(desktop.server_mut(), rest.0, rest.1);
    desktop.select(holder, rest);
    sleep(SETTLE);
    assert_eq!(
        desktop.focused(),
        Some(holder),
        "the window under the pointer does not have the focus, so no stage can tell a steal"
    );

    // 1. The map, with the pointer resting.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let sampler = FocusSampler::start(&desktop.server().display, desktop.view());
    let mut pane = open(desktop, directory.path(), dimensions, layout);
    wait_for(
        Duration::from_secs(20),
        "the window manager to take the pane",
        || desktop.manages(pane.window().id()).then_some(()),
    );
    pump(&mut pane, SETTLE);
    let frames = with_frames(desktop.server(), pane.window().id());
    let samples = sampler.finish();
    samples.assert_measured("the map");
    assert!(
        !samples.touched(&frames),
        "the pane was focused when it opened beside the resting pointer: {samples:?}"
    );
    let outer = screen_rect(desktop.server(), *frames.last().expect("the pane"));
    let (dx, dy) = distance(outer, rest);
    println!(
        "  window {:?}, outer frame {outer:?}: {dx},{dy} pixels from the pointer",
        screen_rect(desktop.server(), pane.window().id())
    );
    match expect {
        Expect::RightBelow { gap } => {
            assert_eq!(
                (outer.0, outer.1),
                (i32::from(rest.0) + gap, i32::from(rest.1) + gap),
                "the frame is not {gap} pixels right of and below the pointer"
            );
        }
        Expect::Below { gap } => {
            assert_eq!(
                outer.1,
                i32::from(rest.1) + gap,
                "the frame is not {gap} pixels below the pointer"
            );
            // Centred on the pointer as far as the screen allows, and from
            // its left edge when the frame is wider than the screen.
            let width = outer.2 as i32;
            assert_eq!(
                outer.0,
                (i32::from(rest.0) - width / 2)
                    .min(i32::from(SCREEN.0) - width)
                    .max(0),
                "the frame is not centred across on the pointer"
            );
        }
        Expect::Outside => assert!(dx > 0 || dy > 0, "the pointer is inside the pane"),
        Expect::Around { gap } => {
            let (x, y, width, height) = screen_rect(desktop.server(), pane.window().id());
            let (px, py) = (i32::from(rest.0), i32::from(rest.1));
            let (right, bottom) = (x + width as i32, y + height as i32);
            for (edge, screen_edge, depth) in [
                (x, 0, px - x),
                (right, i32::from(SCREEN.0), right - 1 - px),
                (y, 0, py - y),
                (bottom, i32::from(SCREEN.1), bottom - 1 - py),
            ] {
                assert!(
                    depth >= 0 && (edge == screen_edge || depth >= gap),
                    "the pointer is {depth} pixels inside an edge of the window at {edge}"
                );
            }
        }
    }

    // 2. A jiggle around where the pointer rests.
    let sampler = FocusSampler::start(&desktop.server().display, desktop.view());
    for dy in -JIGGLE..=JIGGLE {
        for dx in -JIGGLE..=JIGGLE {
            move_pointer(desktop.server_mut(), rest.0 + dx, rest.1 + dy);
            sleep(Duration::from_millis(2));
            move_pointer(desktop.server_mut(), rest.0, rest.1);
            sleep(Duration::from_millis(2));
        }
    }
    pump(&mut pane, SETTLE);
    let samples = sampler.finish();
    samples.assert_measured("the jiggle");
    assert!(
        !samples.touched(&frames),
        "a jiggle of up to {JIGGLE} pixels around the resting pointer focused the pane"
    );

    // 3. The pane's own resize, with the pointer still resting.
    let sampler = FocusSampler::start(&desktop.server().display, desktop.view());
    let window = pane.window().geometry().expect("the pane's geometry");
    for grow in [40, 0] {
        pane.window()
            .place(spokenpad::core::geometry::Rect {
                width: window.width + grow,
                height: window.height + grow,
                ..window
            })
            .expect("resize the pane");
        pump(&mut pane, Duration::from_millis(300));
    }
    pump(&mut pane, SETTLE);
    let samples = sampler.finish();
    samples.assert_measured("the resize");
    assert!(
        !samples.touched(&frames),
        "the pane's own resize under the resting pointer focused it"
    );

    // 4. The deliberate move: from outside the frame into its middle. Only a
    // pane that covers the whole screen has no outside to come from.
    let outer = screen_rect(desktop.server(), *frames.last().expect("the pane"));
    let Some(start) = outside(outer) else {
        println!("  the pane covers the screen: no move into it from outside");
        return;
    };
    glide(desktop, rest, start);
    sleep(SETTLE);
    let middle = (
        (outer.0 + outer.2 as i32 / 2) as i16,
        (outer.1 + outer.3 as i32 / 2) as i16,
    );
    glide(desktop, start, middle);
    let focused = wait_until(Duration::from_secs(3), || {
        desktop
            .focused()
            .is_some_and(|window| frames.contains(&window))
            && frames.contains(&desktop.server().input_focus())
    });
    pump(&mut pane, Duration::from_millis(100));
    assert!(
        focused,
        "the pointer moved from {start:?} into the pane and did not focus it"
    );
    println!("  a move into it from {start:?} focused it");
}

/// A focusable window of the test's own over the whole screen, holding the
/// focus the way the application the user dictates into does.
fn holder(desktop: &mut dyn Desktop) -> Window {
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
            SCREEN.0,
            SCREEN.1,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(server.connection.setup().roots[0].white_pixel),
        )
        .expect("create the holder");
    server
        .connection
        .change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"focus-holder\0FocusHolder\0",
        )
        .expect("name the holder");
    x11rb::properties::WmHints {
        input: Some(true),
        ..x11rb::properties::WmHints::new()
    }
    .set(&server.connection, id)
    .expect("write WM_HINTS");
    server.connection.flush().expect("flush");
    desktop.map_until_managed(id);
    sleep(SETTLE);
    id
}

/// The pane, opened as the daemon opens it: the monitor and the pointer from
/// the X server, then `open` and `show`.
fn open(desktop: &dyn Desktop, root: &Path, dimensions: Dimensions, layout: PaneLayout) -> Pane {
    let display = &desktop.server().display;
    let name = CString::new(display.as_str()).expect("a display name");
    let (connection, screen) =
        x11rb::xcb_ffi::XCBConnection::connect(Some(&name)).expect("connect to the display");
    let target = place::target(&connection, screen).expect("the monitor and the pointer");
    let config = config(root);
    let file = config.dictation_dir.join("hover.md");
    std::fs::write(&file, "").expect("create the dictation file");
    let (command, marker) = pane_launch(&config, &file).expect("build the nvim command");
    let mut pane = Pane::open(
        &Options {
            display: display.clone(),
            dimensions,
            layout,
            size: Points::try_from(11.0).expect("a point size"),
            target: Some(target),
            ..Options::default()
        },
        command,
    )
    .expect("open the pane");
    pane.show().expect("show the pane");
    marker.keep();
    pane
}

fn config(root: &Path) -> Nvim {
    let dictation = root.join("dictation");
    std::fs::create_dir_all(&dictation).expect("make the dictation directory");
    Nvim {
        mode: Mode::Pane,
        socket_path: root.join("nvim.sock"),
        dictation_dir: dictation,
        init: Some(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/lua/dictation_init.lua"
        ))),
        ..Nvim::default()
    }
}

/// Let the pane handle what happened for `duration`, drawing as it goes.
fn pump(pane: &mut Pane, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        pane.step(Duration::from_millis(10)).expect("run the pane");
    }
}

/// How many pixels `point` is outside `rect` along each axis; 0 on an axis
/// where it is within it.
fn distance((x, y, width, height): (i32, i32, u32, u32), point: (i16, i16)) -> (i32, i32) {
    let along = |start: i32, length: u32, at: i32| {
        let end = start + length as i32 - 1;
        (start - at).max(at - end).max(0)
    };
    (
        along(x, width, i32::from(point.0)),
        along(y, height, i32::from(point.1)),
    )
}

/// A point on the screen well outside `outer`, if there is one.
fn outside(outer: (i32, i32, u32, u32)) -> Option<(i16, i16)> {
    let (width, height) = (SCREEN.0 as i16, SCREEN.1 as i16);
    [
        (5, 5),
        (width - 5, 5),
        (5, height - 5),
        (width - 5, height - 5),
    ]
    .into_iter()
    .find(|point| distance(outer, *point) != (0, 0))
}

/// Move the pointer from `from` to `to` in steps of a few pixels, as a hand
/// moves it.
fn glide(desktop: &mut dyn Desktop, from: (i16, i16), to: (i16, i16)) {
    const STEPS: i32 = 40;
    for step in 1..=STEPS {
        let at =
            |a: i16, b: i16| (i32::from(a) + (i32::from(b) - i32::from(a)) * step / STEPS) as i16;
        move_pointer(desktop.server_mut(), at(from.0, to.0), at(from.1, to.1));
        sleep(Duration::from_millis(5));
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        sleep(Duration::from_millis(20));
    }
    false
}
