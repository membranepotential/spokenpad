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
//!
//! It also writes screenshots and a few timings; run with `-- --nocapture` to
//! see where they went.
mod harness;

use harness::{
    I3, SETTLE, XServer, click, find_key, press_key, press_keysym, screenshot_dir, type_text,
    wait_for, write_png,
};
use rmpv::Value;
use spokenpad::{
    config::{Mode, Nvim},
    core::{geometry::Rect, state::IndicatorPhase},
    shell::{
        nvim::{IndicatorState, NvimSession},
        pane::{self, Options, Pane},
    },
};
use std::{
    path::PathBuf,
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
    let dictation = directory.path().join("dictation");
    std::fs::create_dir_all(&dictation).expect("make the dictation directory");
    let file = dictation.join("dictation-2026-09-21-000000.md");
    std::fs::write(&file, "").expect("create the dictation file");
    let config = Nvim {
        mode: Mode::Attach,
        socket_path: directory.path().join("nvim.sock"),
        dictation_dir: dictation.clone(),
        // The bundled configuration, read from the repository rather than
        // materialised into the user's state directory.
        init: Some(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/lua/dictation_init.lua"
        ))),
        ..Nvim::default()
    };
    let command = pane::editor_command(&config, &file).expect("build the nvim command");
    let mut pane = Pane::open(
        &Options {
            display: Some(server.display.clone()),
            columns: COLUMNS,
            rows: ROWS,
            size: 16.0,
            ..Options::default()
        },
        command,
    )
    .expect("open the pane");
    pane.show().expect("map the pane");
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

    // Idle cost, with the window open and nothing happening.
    let idle = idle_cost(&mut pane, Duration::from_secs(2));
    println!("idle: {idle:.1} ms of processor time over 2 s with the pane open");
    assert!(
        idle < 100.0,
        "the pane burned {idle:.1} ms of processor time while idle"
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
    let escape = find_key(&server, 0xff1b).expect("the layout has Escape");
    press_key(&mut server, escape);
    let landed = wait_for(PATIENCE, "the typed text to reach the buffer", || {
        let _ = pane.step(Duration::from_millis(50));
        let text = buffer_text(&mut pane);
        (text.trim_end() == expected).then_some(text)
    });
    assert_eq!(landed.trim_end(), expected);
    println!(
        "(c) typing on a German layout landed exactly: {expected:?}{}",
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

    drop(pane);
}

// ------------------------------------------------------------------ helpers

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

/// Processor time this process spends while the pane sits idle.
fn idle_cost(pane: &mut Pane, over: Duration) -> f64 {
    let before = cpu_milliseconds();
    let deadline = Instant::now() + over;
    while Instant::now() < deadline {
        let _ = pane.step(deadline.saturating_duration_since(Instant::now()));
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
