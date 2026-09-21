//! P1 of the own-window plan: does the pane draw what Neovim thinks it is
//! showing, and does everything the dictation window has to do still work when
//! spokenpad draws it itself?
//!
//! One test, because it is one window with one editor behind it and the
//! criteria are stages of its life. It runs against an `Xvfb` and an `i3` it
//! starts itself, with a German keyboard layout loaded into that server, and
//! it never touches the user's display, config or state directory.
//!
//! What it asserts, in order:
//!
//! - **(a)** every cell of the pane's grid equals Neovim's own `screenstring`,
//!   after each of 200 seeded random edits, scrolls and jumps.
//! - **(b)** a daemon-style append over the editor's `--listen` socket — the
//!   real `NvimSession` path, not a shortcut — shows in the pane while it is
//!   unfocused, and moves no focus.
//! - **(c)** after a click, keys typed on the German layout land in the buffer
//!   exactly, including `ü`, `ß` and a dead-key sequence.
//! - **(d)** the preview is drawn and is never in the buffer.
//! - **(e)** a resize leaves the pane and Neovim agreeing about the size and
//!   the contents.
//! - **(f)** closing the window writes what was typed into it, a buffer
//!   Neovim refuses to write is kept beside its file instead of thrown away,
//!   and the whole teardown fits inside the budget the daemon's shutdown
//!   gives it, even when the editor has stopped answering.
//!
//! It also writes screenshots and a few timings; run with `-- --nocapture` to
//! see where they went.
mod harness;

use harness::{
    I3, SETTLE, XServer, click, close_window, find_key, press_key, press_keysym, press_with_altgr,
    screenshot_dir, type_text, wait_for, write_png,
};
use rmpv::Value;
use spokenpad::{
    config::{Mode, Nvim},
    core::{geometry::Rect, state::IndicatorPhase},
    shell::{
        daemon::SHUTDOWN_GRACE,
        nvim::pane_launch,
        nvim::{IndicatorState, NvimSession},
        pane::{Options, Pane, Sizing, Status},
    },
};
use std::{
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};

/// How long any one thing the editor has to do may take.
const PATIENCE: Duration = Duration::from_secs(10);
const COLUMNS: u16 = 72;
const ROWS: u16 = 14;

#[test]
fn the_pane_draws_what_neovim_draws() {
    if !harness::tools_or_skip(&["Xvfb", "i3", "nvim", "setxkbmap", "fc-match"]) {
        return;
    }
    let mut server = XServer::start();
    server.set_layout("de");
    let i3 = I3::start(&server);
    // A plain window holds the focus, so "the pane never takes it" can fail.
    let base = server.plain_window();
    i3.wait_until_managed(base);
    sleep(SETTLE);

    let directory = tempfile::tempdir().expect("a temporary directory");
    let config = dictation_config(directory.path());
    let file = config.dictation_dir.join("dictation-2026-09-21-000000.md");
    std::fs::write(&file, "").expect("create the dictation file");
    let (command, marker) = pane_launch(&config, &file).expect("build the nvim command");
    let mut pane = Pane::open(
        &Options {
            display: server.display.clone(),
            sizing: Sizing::Cells {
                columns: COLUMNS,
                rows: ROWS,
            },
            size: 16.0,
            ..Options::default()
        },
        command,
    )
    .expect("open the pane");
    pane.show().expect("map the pane");
    // The pane is up, so the marker stays: it is what lets an attach-mode
    // session recognise this editor as spokenpad's.
    marker.keep();
    i3.wait_until_managed(pane.window().id());
    sleep(SETTLE);
    let focus_before = server.input_focus();
    assert!(
        !i3.node(pane.window().id()).expect("the pane").focused,
        "the pane took the focus when it appeared"
    );

    // ------------------------------------------------ (a) the grid is right
    let metrics = pane.metrics();
    println!(
        "font cell {}x{} px, baseline {}, grid {}x{}",
        metrics.width, metrics.height, metrics.baseline, COLUMNS, ROWS
    );
    seed_buffer(&mut pane);
    same_as_nvim(&mut pane, "the seeded buffer");

    // A double-width character, through the renderer rather than only through
    // the grid model: Neovim leaves the cell after it empty, and the pane has
    // to treat that cell as the right half of the one before rather than as a
    // blank to paint over it.
    call(
        &mut pane,
        "nvim_exec_lua",
        vec![Value::from(WIDE_LINE), Value::Array(Vec::new())],
        PATIENCE,
    );
    same_as_nvim(&mut pane, "a line with double-width characters");
    assert_eq!(
        pane.screen().cell(0, 0).expect("the first cell").text,
        "\u{6f22}"
    );
    assert!(
        pane.screen()
            .cell(0, 1)
            .expect("the cell after it")
            .is_continuation(),
        "the cell after a double-width character must be its right half"
    );
    // And it is actually drawn. The cursor is parked out of the way first:
    // its outline sits in cell 0 and would otherwise make this pass whether
    // or not a single glyph was rasterised. The `✓` is the same question for
    // a character the monospace family has no outline for at all — without a
    // fallback font both of these are blank cells, which in a transcript
    // reads as lost text.
    call(
        &mut pane,
        "nvim_exec_lua",
        vec![
            Value::from("vim.api.nvim_win_set_cursor(0, { 1, 20 })"),
            Value::Array(Vec::new()),
        ],
        PATIENCE,
    );
    for (label, cells) in [("the double-width pair", 0..2_u32), ("a check mark", 9..10)] {
        let painted = painted_pixels(&pane, cells);
        assert!(
            painted > 20,
            "{label} drew only {painted} pixels; the font has no glyph and no fallback was found"
        );
    }
    shoot(&pane, "pane-wide.png");

    let mut random = Prng::new(0x5D0_7E57_5EED);
    for step in 0..200 {
        let operation = OPERATIONS[random.below(OPERATIONS.len())];
        call(
            &mut pane,
            "nvim_input",
            vec![Value::from(operation)],
            PATIENCE,
        );
        same_as_nvim(&mut pane, &format!("step {step}, after {operation:?}"));
    }
    println!("(a) grid matched nvim's screen after each of 200 random operations");

    // ------------------------------- (b) an append over the editor's socket
    call(
        &mut pane,
        "nvim_exec_lua",
        vec![
            Value::from("vim.api.nvim_buf_set_lines(0, 0, -1, false, { '' })"),
            Value::Array(Vec::new()),
        ],
        PATIENCE,
    );
    let mut session = NvimSession::new(config.clone());
    let attached = session
        .ensure()
        .expect("attach to the pane's editor")
        .expect("attach mode found the pane's editor");
    assert_eq!(
        attached, file,
        "the daemon attached to a different file than the pane opened"
    );
    const DICTATED: &str = "Der Hund bellt im Garten.";
    let started = Instant::now();
    session
        .append(DICTATED, false)
        .expect("append over the editor's own socket");
    let shown = wait_for(PATIENCE, "the appended text to be drawn", || {
        let _ = pane.step(Duration::from_millis(50));
        grid_text(&pane).contains(DICTATED).then(Instant::now)
    });
    println!(
        "(b) an append reached the pane's grid in {} ms",
        shown.duration_since(started).as_millis()
    );
    assert!(
        !i3.node(pane.window().id()).expect("the pane").focused,
        "the pane took the focus when text was appended to it"
    );
    assert_eq!(
        server.input_focus(),
        focus_before,
        "the X input focus moved while text was appended"
    );

    // ------------------------------------ (d) the preview, before the click
    // Done here because it needs the daemon session, and it must be checked
    // while the window is still unfocused: a preview is what the pane shows
    // *while* the user is dictating somewhere else.
    const PREVIEW: &str = "und der offene Rest der Äußerung";
    session
        .set_indicator(&IndicatorState {
            phase: IndicatorPhase::Recording,
            preview: PREVIEW.to_owned(),
            ..IndicatorState::default()
        })
        .expect("push an indicator state");
    wait_for(PATIENCE, "the preview to be drawn", || {
        let _ = pane.step(Duration::from_millis(50));
        grid_text(&pane).contains("offene Rest").then_some(())
    });
    let buffer = buffer_text(&mut pane);
    assert!(
        !buffer.contains("offene Rest"),
        "the preview reached the buffer; it must stay virtual text:\n{buffer}"
    );
    assert!(
        buffer.contains(DICTATED),
        "the committed text should still be in the buffer:\n{buffer}"
    );
    println!("(d) the preview is drawn in the grid and is not in the buffer");
    shoot(&pane, "pane-preview.png");

    // Idle cost, with the window open and nothing happening: the loop blocks
    // on one channel, so this is the whole story.
    let idle = idle_cost(&mut pane, Duration::from_secs(2), &mut server, Sweep::Still);
    println!("idle: {idle:.1} ms of processor time over 2 s with the pane open");
    assert!(
        idle < 50.0,
        "the pane burned {idle:.1} ms of processor time while idle"
    );
    // And with the pointer crossing the window, which is what a pane sitting
    // under someone's mouse path actually meets. Sweeping the pointer costs
    // this test something whether or not the pane hears about it, so the same
    // sweep beside the window is the control: the window asks for motion only
    // while a button is down, so the two should not differ.
    let over = idle_cost(&mut pane, Duration::from_secs(2), &mut server, Sweep::Over);
    let beside = idle_cost(
        &mut pane,
        Duration::from_secs(2),
        &mut server,
        Sweep::Beside,
    );
    println!(
        "pointer sweeping for 2 s: {over:.1} ms over the pane, {beside:.1} ms beside it \
         (the difference is what motion costs the pane)"
    );
    assert!(
        over - beside < 40.0,
        "moving the pointer over the pane cost it {:.1} ms more than moving it beside",
        over - beside
    );

    // --------------------------------------------- (c) click, then type
    session
        .set_indicator(&IndicatorState::default())
        .expect("clear the indicator");
    call(
        &mut pane,
        "nvim_exec_lua",
        vec![
            Value::from("vim.api.nvim_buf_set_lines(0, 0, -1, false, { '' })"),
            Value::Array(Vec::new()),
        ],
        PATIENCE,
    );
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
    assert_eq!(
        server.input_focus(),
        pane.window().id(),
        "the X input focus is not on the pane after a click"
    );

    const TYPED: &str = "Grüße ß";
    let insert = find_key(&server, u32::from(b'i')).expect("the layout has an `i`");
    press_key(&mut server, insert);
    assert!(
        type_text(&mut server, TYPED),
        "the German layout should have every character of {TYPED:?}"
    );
    // A dead key, if this layout offers one: `dead_acute` then `e` is `é`.
    let dead_acute = 0xfe51;
    let composed = press_keysym(&mut server, dead_acute) && type_text(&mut server, "e");
    let expected = match composed {
        true => format!("{TYPED}é"),
        false => TYPED.to_owned(),
    };
    // AltGr is `ISO_Level3_Shift`, a level the layout applies, not a modifier
    // Neovim names. These have to arrive as text: if the pane reported AltGr
    // as Alt they would arrive as `<M-q>` and friends and nothing would land.
    let altgr: String = ALTGR
        .iter()
        .filter(|(_, base)| press_with_altgr(&mut server, *base))
        .map(|(character, _)| *character)
        .collect();
    assert_eq!(
        altgr.chars().count(),
        ALTGR.len(),
        "the German layout should have every AltGr key this test presses"
    );
    let expected = format!("{expected}{altgr}");

    let escape = find_key(&server, 0xff1b).expect("the layout has Escape");
    press_key(&mut server, escape);
    let landed = wait_for(PATIENCE, "the typed text to reach the buffer", || {
        let _ = pane.step(Duration::from_millis(50));
        let text = buffer_text(&mut pane);
        (text.trim_end() == expected).then_some(text)
    });
    assert_eq!(landed.trim_end(), expected);
    println!(
        "(c) typing on a German layout landed exactly, AltGr included: {expected:?}{}",
        if composed {
            " (with a dead-key sequence)"
        } else {
            " (this layout offered no dead key)"
        }
    );
    same_as_nvim(&mut pane, "typing");
    shoot(&pane, "pane-typed.png");

    // ------------------------------------------------------- (e) resize
    let smaller = Rect {
        x: rect.x,
        y: rect.y,
        width: u32::from(COLUMNS - 20) * metrics.width,
        height: u32::from(ROWS - 4) * metrics.height,
    };
    pane.window().place(smaller).expect("resize the pane");
    let size = wait_for(PATIENCE, "the pane to follow the new window size", || {
        let _ = pane.step(Duration::from_millis(50));
        (pane.size() == (COLUMNS - 20, ROWS - 4)).then(|| pane.size())
    });
    assert_eq!(size, (COLUMNS - 20, ROWS - 4));
    let (nvim_columns, nvim_rows) = nvim_size(&mut pane);
    assert_eq!(
        (nvim_columns, nvim_rows),
        size,
        "nvim and the pane disagree about the grid after a resize"
    );
    same_as_nvim(&mut pane, "a resize");
    println!("(e) the grid survived a resize to {}x{}", size.0, size.1);
    shoot(&pane, "pane-resized.png");

    // ------------------- closing the window must not lose what was typed
    // Dictated text is on disk already: the Lua side writes after every
    // append. What is not on disk is what the user typed since, and in this
    // mode closing the window ends the editor — so the pane has to write
    // first. Managed mode never faces this: its editor outlives the daemon.
    const BY_HAND: &str = "noch von Hand getippt";
    let insert = find_key(&server, u32::from(b'o')).expect("the layout has an `o`");
    press_key(&mut server, insert);
    assert!(type_text(&mut server, BY_HAND), "the layout has the text");
    let escape = find_key(&server, 0xff1b).expect("the layout has Escape");
    press_key(&mut server, escape);
    wait_for(PATIENCE, "the hand-typed line to reach the buffer", || {
        let _ = pane.step(Duration::from_millis(50));
        buffer_text(&mut pane).contains(BY_HAND).then_some(())
    });

    // Not written yet: Neovim only writes when spokenpad appends.
    let on_disk = std::fs::read_to_string(&file).expect("read the dictation file");
    assert!(
        !on_disk.contains(BY_HAND),
        "this test is not proving anything: the text was already on disk"
    );
    close_window(&server, pane.window().id());
    wait_for(PATIENCE, "the pane to notice the window closed", || {
        matches!(pane.step(Duration::from_millis(50)), Ok(Status::Finished)).then_some(())
    });
    drop(pane);
    let on_disk = std::fs::read_to_string(&file).expect("read the dictation file");
    assert!(
        on_disk.contains(BY_HAND),
        "closing the window lost what was typed into it:\n{on_disk}"
    );
    println!("closing the window wrote the buffer: what was typed by hand is in the file");

    // ------------------- (f) text Neovim will not write is kept, not lost
    // A write can fail: a read-only file here, a full disk or a directory
    // that went away in the field. The pane is the only place that text
    // exists by then, so it goes beside the file instead of into `qall!`.
    const REFUSED: &str = "das hier darf nicht verloren gehen";
    let second = dictation_config(&directory.path().join("unwritable"));
    let unwritable = second.dictation_dir.join("dictation-2026-09-21-000001.md");
    std::fs::write(&unwritable, "").expect("create the dictation file");
    let mut pane = pane_on(&server, &second, &unwritable);
    put_line(&mut pane, REFUSED);
    let mut forbid = std::fs::metadata(&unwritable)
        .expect("the dictation file")
        .permissions();
    forbid.set_readonly(true);
    std::fs::set_permissions(&unwritable, forbid).expect("make the dictation file read-only");
    drop(pane);
    let on_disk = std::fs::read_to_string(&unwritable).expect("read the dictation file");
    assert!(
        !on_disk.contains(REFUSED),
        "the file was writable after all, so this proves nothing:\n{on_disk}"
    );
    let kept = PathBuf::from(format!("{}.unsaved", unwritable.display()));
    let rescued = std::fs::read_to_string(&kept).unwrap_or_else(|error| {
        panic!(
            "a buffer that could not be written left nothing behind: {} is not there ({error})",
            kept.display()
        )
    });
    assert!(
        rescued.contains(REFUSED),
        "the rescued file does not hold what the buffer did:\n{rescued}"
    );
    println!(
        "a buffer Neovim refused to write was kept in {}",
        kept.display()
    );

    // ------------------- (f) and the teardown fits the daemon's budget
    // The daemon gives its editor thread SHUTDOWN_GRACE and then exits,
    // which stops the pane thread wherever it had got to. So writing
    // everything and stopping the editor has to finish inside that even when
    // the editor answers nothing at all -- otherwise a `systemctl --user
    // restart spokenpad` at the wrong moment is what loses the text. This
    // editor is stuck in a shell command and will never reply again.
    let third = dictation_config(&directory.path().join("stalled"));
    let stalled = third.dictation_dir.join("dictation-2026-09-21-000002.md");
    std::fs::write(&stalled, "").expect("create the dictation file");
    let mut pane = pane_on(&server, &third, &stalled);
    put_line(&mut pane, "was nie geschrieben wird");
    let stuck = pane.call(
        "nvim_exec_lua",
        vec![
            Value::from("os.execute('sleep 30')"),
            Value::Array(Vec::new()),
        ],
        Duration::from_millis(250),
    );
    assert!(stuck.is_err(), "the editor answered, so it is not stalled");
    let started = Instant::now();
    drop(pane);
    let took = started.elapsed();
    assert!(
        took < SHUTDOWN_GRACE,
        "tearing down a pane whose editor stopped answering took {took:?}, more than the \
         {SHUTDOWN_GRACE:?} the daemon's shutdown allows: a restart would kill it mid-write"
    );
    println!(
        "a stalled pane tore down in {} ms, inside the daemon's {} ms",
        took.as_millis(),
        SHUTDOWN_GRACE.as_millis()
    );
}

// ------------------------------------------------------------------ helpers

/// The editor configuration these panes run with: a socket and a dictation
/// directory under `root`, and the bundled dictation init read from the
/// repository rather than materialised into the user's state directory.
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

/// Another pane on the same server, editing `file`. Small: what these later
/// stages assert happens when it closes, not on its grid.
fn pane_on(server: &XServer, config: &Nvim, file: &Path) -> Pane {
    let (command, marker) = pane_launch(config, file).expect("build the nvim command");
    let mut pane = Pane::open(
        &Options {
            display: server.display.clone(),
            sizing: Sizing::Cells {
                columns: 40,
                rows: 8,
            },
            size: 16.0,
            ..Options::default()
        },
        command,
    )
    .expect("open the pane");
    pane.show().expect("map the pane");
    marker.keep();
    pane
}

/// Put a line in the buffer without writing it, which is what the user
/// typing into the pane leaves behind.
fn put_line(pane: &mut Pane, text: &str) {
    let lua = format!("vim.api.nvim_buf_set_lines(0, -1, -1, false, {{ {text:?} }})");
    call(
        pane,
        "nvim_exec_lua",
        vec![Value::from(lua), Value::Array(Vec::new())],
        PATIENCE,
    );
    let modified = call(
        pane,
        "nvim_exec_lua",
        vec![
            Value::from("return vim.bo.modified"),
            Value::Array(Vec::new()),
        ],
        PATIENCE,
    );
    assert_eq!(
        modified.as_bool(),
        Some(true),
        "the buffer is not modified, so closing it would have nothing to write"
    );
}

/// One Neovim API call over the pane's own UI channel.
fn call(pane: &mut Pane, method: &str, arguments: Vec<Value>, timeout: Duration) -> Value {
    pane.call(method, arguments, timeout)
        .unwrap_or_else(|error| panic!("nvim {method} failed: {error:#}"))
}

/// Give the buffer enough lines that scrolling and jumping do something.
fn seed_buffer(pane: &mut Pane) {
    let lua = r#"
local lines = {}
for index = 1, 60 do
  lines[index] = string.format("Zeile %02d: Grüße aus dem Diktierfenster, ßßß.", index)
end
vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
vim.api.nvim_win_set_cursor(0, { 1, 0 })
"#;
    call(
        pane,
        "nvim_exec_lua",
        vec![Value::from(lua), Value::Array(Vec::new())],
        PATIENCE,
    );
}

/// How many pixels of the first row's cells `columns` are not the default
/// background, which is how "was anything actually drawn here?" is asked.
fn painted_pixels(pane: &Pane, columns: std::ops::Range<u32>) -> usize {
    let metrics = pane.metrics();
    let background = pane.screen().defaults().background.0;
    let (pixels, width, _) = pane.framebuffer();
    (0..metrics.height)
        .flat_map(|y| {
            (columns.start * metrics.width..columns.end * metrics.width).map(move |x| (x, y))
        })
        .filter(|(x, y)| pixels[(y * u32::from(width) + x) as usize] != background)
        .count()
}

/// The whole grid as one string, for a "does this text show?" question.
fn grid_text(pane: &Pane) -> String {
    let (_, rows) = pane.size();
    (0..rows)
        .map(|row| pane.screen().line(row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Neovim's own view of its screen, one string per row.
fn nvim_screen(pane: &mut Pane) -> Vec<String> {
    let (columns, rows) = pane.size();
    // `:redraw` first, so this is not a race. It makes Neovim flush its UI
    // *now* — the redraw notifications go down the same channel, ahead of the
    // answer to this call — and only then is the screen read. Without it
    // Neovim would defer the flush while the test keeps it busy, and the
    // comparison would be against a screen the pane has not been told about.
    let lua = r#"
local rows, columns = ...
vim.cmd("redraw")
local out = {}
for row = 1, rows do
  local cells = {}
  for column = 1, columns do
    cells[column] = vim.fn.screenstring(row, column)
  end
  out[row] = table.concat(cells)
end
return out
"#;
    let value = call(
        pane,
        "nvim_exec_lua",
        vec![
            Value::from(lua),
            Value::Array(vec![Value::from(rows), Value::from(columns)]),
        ],
        PATIENCE,
    );
    value
        .as_array()
        .expect("screenstring returned an array")
        .iter()
        .map(|row| row.as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Every cell of the pane's grid equals Neovim's own screen.
///
/// Retried until it matches or the patience runs out: Neovim may answer a
/// request before it has flushed the redraw that request's predecessor caused,
/// and that is a race in the test, not a difference in the grid.
fn same_as_nvim(pane: &mut Pane, what: &str) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let theirs = nvim_screen(pane);
        let (_, rows) = pane.size();
        let ours: Vec<String> = (0..rows).map(|row| pane.screen().line(row)).collect();
        if ours == theirs {
            return;
        }
        if Instant::now() >= deadline {
            for (index, (ours, theirs)) in ours.iter().zip(&theirs).enumerate() {
                assert_eq!(ours, theirs, "row {index} differs after {what}");
            }
            panic!(
                "the pane's grid has {} rows and nvim's screen {} after {what}",
                ours.len(),
                theirs.len()
            );
        }
        let _ = pane.step(Duration::from_millis(5));
    }
}

fn buffer_text(pane: &mut Pane) -> String {
    let value = call(
        pane,
        "nvim_buf_get_lines",
        vec![
            Value::from(0),
            Value::from(0),
            Value::from(-1),
            Value::from(false),
        ],
        PATIENCE,
    );
    value
        .as_array()
        .expect("nvim_buf_get_lines returned an array")
        .iter()
        .map(|line| line.as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

fn nvim_size(pane: &mut Pane) -> (u16, u16) {
    let option = |pane: &mut Pane, name: &str| {
        call(
            pane,
            "nvim_get_option_value",
            vec![Value::from(name), Value::Map(Vec::new())],
            PATIENCE,
        )
        .as_i64()
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or_else(|| panic!("nvim gave no usable {name}"))
    };
    (option(pane, "columns"), option(pane, "lines"))
}

/// Where the pointer goes while the idle cost is measured.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sweep {
    /// Left where it is; the loop blocks for the whole period.
    Still,
    /// Back and forth across the pane.
    Over,
    /// The same sweep just above the pane, as the control.
    Beside,
}

/// Processor time this process spends over `period` with the pane open.
fn idle_cost(pane: &mut Pane, period: Duration, server: &mut XServer, sweep: Sweep) -> f64 {
    let rect = pane.window().geometry().expect("the pane's geometry");
    let y = match sweep {
        Sweep::Beside => (rect.y - 40).max(0),
        _ => rect.y + rect.height as i32 / 2,
    };
    let y = i16::try_from(y).expect("on screen");
    let left = i16::try_from(rect.x).expect("on screen");
    let before = cpu_milliseconds();
    let deadline = Instant::now() + period;
    let mut step = 0_i16;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if sweep == Sweep::Still {
            // Nothing to do but wait, which is exactly what is being measured.
            let _ = pane.step(remaining);
            continue;
        }
        // The pointer is moved from this thread, so the cost of moving it is
        // in both the measurement and its control.
        harness::move_pointer(server, left + (step % 64), y);
        step += 4;
        let _ = pane.step(remaining.min(Duration::from_millis(20)));
    }
    cpu_milliseconds() - before
}

/// User plus system time of this process, in milliseconds, from `/proc`.
fn cpu_milliseconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // The comm field can hold spaces and parentheses; everything after the
    // last `)` is fixed-width, and utime and stime are fields 14 and 15.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return 0.0;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |index: usize| {
        fields
            .get(index)
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or_default()
    };
    // Field 3 of `rest` is state, so utime is index 11 and stime index 12.
    (ticks(11) + ticks(12)) * 1000.0 / 100.0
}

/// Write the pane's pixels somewhere a human can look at them.
fn shoot(pane: &Pane, name: &str) {
    let (pixels, width, height) = pane.framebuffer();
    let path: PathBuf = screenshot_dir().join(name);
    match write_png(&path, pixels, width, height) {
        Ok(()) => println!("wrote {}", path.display()),
        Err(error) => println!("could not write {}: {error:#}", path.display()),
    }
}

/// A buffer line whose first character takes two cells, plus a narrow symbol
/// and a letter outside ASCII.
const WIDE_LINE: &str = "vim.api.nvim_buf_set_lines(0, 0, -1, false, { '\u{6f22}\u{5b57} and \u{2713} and \u{df}' })\n\
     vim.api.nvim_win_set_cursor(0, { 1, 0 })";

/// What AltGr types on a German layout, named by the key's unshifted keysym
/// rather than by its position.
const ALTGR: [(char, u32); 9] = [
    ('@', 'q' as u32),
    ('\u{20ac}', 'e' as u32),
    ('{', '7' as u32),
    ('[', '8' as u32),
    (']', '9' as u32),
    ('}', '0' as u32),
    ('\\', 0xdf), // the sharp-s key
    ('~', 0x2b),  // the plus key
    ('|', 0x3c),  // the less-than key
];

/// The operations the random walk picks from: edits, scrolls and jumps, all
/// things that make Neovim redraw differently.
const OPERATIONS: &[&str] = &[
    "ihello<Esc>",
    "oeine neue Zeile<Esc>",
    "Oüber der Zeile<Esc>",
    "A und noch etwas<Esc>",
    "dd",
    "yyp",
    "x",
    "J",
    "<C-d>",
    "<C-u>",
    "<C-e>",
    "<C-y>",
    "<C-f>",
    "<C-b>",
    "gg",
    "G",
    "j",
    "k",
    "w",
    "b",
    "0",
    "$",
    "zz",
    "zt",
    "zb",
    "u",
    "<C-r>",
];

/// A seeded xorshift, so a failing run can be replayed exactly.
struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn below(&mut self, limit: usize) -> usize {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        (state % limit as u64) as usize
    }
}
