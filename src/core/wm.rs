//! The i3 IPC protocol, as values: framing, and the replies spokenpad reads.
//!
//! i3 and sway speak the same protocol over a Unix socket (i3's `ipc.html`,
//! sway's `sway-ipc(7)`): the magic `i3-ipc`, a payload length and a message
//! type as native-endian `u32`s, then a JSON payload. A reply carries the type
//! of the request it answers. spokenpad speaks it for one thing: on sway,
//! which reads none of the properties that keep a pane unfocused elsewhere,
//! it adds the pane's own `no_focus` rule before the pane maps. Everything
//! here is pure; the socket is in [`shell::wm`](crate::shell::wm).
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::fmt;

pub const MAGIC: &[u8; 6] = b"i3-ipc";
pub const HEADER_LEN: usize = MAGIC.len() + 8;

/// The requests spokenpad sends. The numbers are the protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    RunCommand = 0,
    GetTree = 4,
    GetVersion = 7,
}

impl Message {
    fn code(self) -> u32 {
        self as u32
    }
}

/// A request frame: header and payload.
pub fn encode(message: Message, payload: &str) -> Result<Vec<u8>> {
    let length = u32::try_from(payload.len()).context("IPC payload too long")?;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&length.to_ne_bytes());
    frame.extend_from_slice(&message.code().to_ne_bytes());
    frame.extend_from_slice(payload.as_bytes());
    Ok(frame)
}

/// The payload length a reply header announces, after checking that it is a
/// reply to `expected`. A window manager sends events only to a client that
/// subscribed, and spokenpad never does, so any other type is a protocol error.
pub fn reply_length(header: &[u8; HEADER_LEN], expected: Message) -> Result<usize> {
    ensure!(header.starts_with(MAGIC), "IPC reply has no i3-ipc magic");
    let field = |at: usize| u32::from_ne_bytes(header[at..at + 4].try_into().expect("4 bytes"));
    let length = field(MAGIC.len());
    let kind = field(MAGIC.len() + 4);
    ensure!(
        kind == expected.code(),
        "IPC reply of type {kind} to a request of type {}",
        expected.code()
    );
    usize::try_from(length).context("IPC reply length does not fit usize")
}

/// Which implementation of the protocol answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmKind {
    /// i3, on X11.
    I3,
    /// sway, on Wayland; the pane is one of its Xwayland windows.
    Sway,
}

impl fmt::Display for WmKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::I3 => "i3",
            Self::Sway => "sway",
        })
    }
}

/// `GET_VERSION`. sway adds `"variant": "sway"`; i3 has no such field.
pub fn parse_version(reply: &str) -> Result<WmKind> {
    let document: Value = serde_json::from_str(reply).context("GET_VERSION reply is not JSON")?;
    ensure!(document.is_object(), "GET_VERSION reply is not an object");
    match document.get("variant").and_then(Value::as_str) {
        Some("sway") => Ok(WmKind::Sway),
        None => Ok(WmKind::I3),
        Some(other) => bail!("unsupported i3 IPC implementation {other:?}"),
    }
}

/// The pane's X11 `WM_CLASS`, instance and class alike: what the pane's
/// window carries ([`shell::pane::x11`](crate::shell::pane::x11)) and what
/// its `no_focus` rule on sway matches.
pub const PANE_WM_CLASS: &str = "spokenpad-pane";

/// Every node of a `GET_TREE` reply, tiled and floating alike.
fn nodes(tree: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    let children = ["nodes", "floating_nodes"]
        .into_iter()
        .filter_map(|key| tree.get(key).and_then(Value::as_array))
        .flatten()
        .flat_map(nodes);
    Box::new(std::iter::once(tree).chain(children))
}

/// Whether the focused workspace holds no window at all, tiled or floating.
///
/// i3 and sway both ignore `no_focus` for the first window on a workspace
/// (i3 userguide, "no_focus"; `sway(5)`), and a new window lands on the
/// focused one. So a window opened onto an empty focused workspace takes
/// focus whatever the rules say. A tree with no focused workspace is an
/// error rather than `false`: nothing then says where the window would land.
pub fn focused_workspace_is_empty(tree: &str) -> Result<bool> {
    let tree: Value = serde_json::from_str(tree).context("GET_TREE reply is not JSON")?;
    let focused = |node: &Value| node.get("focused").and_then(Value::as_bool) == Some(true);
    let workspace = nodes(&tree)
        .filter(|node| node.get("type").and_then(Value::as_str) == Some("workspace"))
        .find(|workspace| nodes(workspace).any(focused))
        .context("GET_TREE shows no focused workspace")?;
    // i3 gives every window its X11 id in `window`; sway gives every view a
    // `pid`, which no split container or workspace has.
    let is_window = |node: &Value| {
        ["window", "pid"]
            .into_iter()
            .any(|key| node.get(key).is_some_and(Value::is_number))
    };
    Ok(!nodes(workspace).skip(1).any(is_window))
}

/// The sway command that adds a `no_focus` rule, while sway runs, for exactly
/// the pane: the windows whose instance and class are both
/// [`PANE_WM_CLASS`].
///
/// sway accepts `no_focus` at runtime (it is in the table of commands valid
/// both in the configuration and over IPC, `sway/commands.c`) and ignores a
/// rule it already holds (`criteria_already_exists`). A rule added this way
/// lasts until the next `reload`, which is why it is sent before every map
/// rather than once.
///
/// sway reads every criteria value as a PCRE pattern that may match anywhere
/// in the name, so each is anchored with `^` and `$`. [`PANE_WM_CLASS`] holds
/// no other pattern syntax, and no backslash, which sway's criteria parser
/// would strip.
pub fn pane_no_focus_command() -> String {
    format!(r#"no_focus [instance="^{PANE_WM_CLASS}$" class="^{PANE_WM_CLASS}$"]"#)
}

/// `RUN_COMMAND`: one result per command, each of which must have succeeded.
pub fn check_command_reply(reply: &str) -> Result<()> {
    let document: Value = serde_json::from_str(reply).context("RUN_COMMAND reply is not JSON")?;
    let results = document
        .as_array()
        .context("RUN_COMMAND reply is not an array")?;
    ensure!(!results.is_empty(), "RUN_COMMAND matched no command");
    for result in results {
        if result.get("success").and_then(Value::as_bool) != Some(true) {
            bail!(
                "window manager refused the command: {}",
                result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_and_replies_are_checked_for_their_type() {
        let frame = encode(Message::GetTree, "{}").unwrap();
        assert_eq!(&frame[..6], MAGIC);
        assert_eq!(frame.len(), HEADER_LEN + 2);
        let header: [u8; HEADER_LEN] = frame[..HEADER_LEN].try_into().unwrap();
        assert_eq!(reply_length(&header, Message::GetTree).unwrap(), 2);
        assert!(reply_length(&header, Message::GetVersion).is_err());
        let mut bad = header;
        bad[0] = b'x';
        assert!(reply_length(&bad, Message::GetTree).is_err());
    }

    #[test]
    fn the_version_says_which_window_manager_answered() {
        let i3 = parse_version(
            r#"{"major":4,"minor":25,"patch":1,"loaded_config_file_name":"/home/u/.config/i3/config"}"#,
        )
        .unwrap();
        assert_eq!(i3, WmKind::I3);
        let sway = parse_version(
            r#"{"variant":"sway","major":1,"loaded_config_file_name":"/home/u/.config/sway/config"}"#,
        )
        .unwrap();
        assert_eq!(sway, WmKind::Sway);
        assert!(parse_version(r#"{"variant":"hyprland"}"#).is_err());
        assert!(parse_version("[]").is_err());
    }

    #[test]
    fn an_empty_focused_workspace_is_found_on_i3_and_sway() {
        let root = |workspaces: serde_json::Value| {
            json!({"id": 1, "type": "root", "focused": false, "nodes": [
                {"id": 2, "type": "output", "focused": false, "nodes": workspaces}
            ]})
            .to_string()
        };
        // i3: the focused workspace is itself focused when it is empty; a
        // workspace elsewhere holding windows does not count.
        let i3_empty = root(json!([
            {"id": 3, "type": "workspace", "focused": true, "nodes": [], "floating_nodes": []},
            {"id": 4, "type": "workspace", "focused": false, "nodes": [
                {"id": 5, "type": "con", "focused": false, "window": 4194305, "nodes": []}
            ]}
        ]));
        assert!(focused_workspace_is_empty(&i3_empty).unwrap());
        // A floating window is a window: i3 counts it, and so does sway.
        let i3_floating = root(json!([
            {"id": 3, "type": "workspace", "focused": false, "nodes": [], "floating_nodes": [
                {"id": 6, "type": "floating_con", "focused": false, "window": null, "nodes": [
                    {"id": 7, "type": "con", "focused": true, "window": 4194306, "nodes": []}
                ]}
            ]}
        ]));
        assert!(!focused_workspace_is_empty(&i3_floating).unwrap());
        // sway: views carry a pid; a split container alone is no window.
        let sway_tiled = root(json!([
            {"id": 3, "type": "workspace", "focused": false, "nodes": [
                {"id": 8, "type": "con", "focused": false, "nodes": [
                    {"id": 9, "type": "con", "focused": true, "pid": 4242, "app_id": "foot", "nodes": []}
                ]}
            ]}
        ]));
        assert!(!focused_workspace_is_empty(&sway_tiled).unwrap());
        let sway_empty = root(json!([
            {"id": 3, "type": "workspace", "focused": true, "nodes": [], "floating_nodes": []}
        ]));
        assert!(focused_workspace_is_empty(&sway_empty).unwrap());
        // Nothing focused: where the window would land is unknown.
        let unfocused = root(json!([
            {"id": 3, "type": "workspace", "focused": false, "nodes": []}
        ]));
        assert!(focused_workspace_is_empty(&unfocused).is_err());
    }

    #[test]
    fn the_pane_rule_is_anchored_on_both_halves_of_its_wm_class() {
        assert_eq!(
            pane_no_focus_command(),
            r#"no_focus [instance="^spokenpad-pane$" class="^spokenpad-pane$"]"#
        );
    }

    #[test]
    fn a_refused_command_is_an_error() {
        check_command_reply(r#"[{"success":true},{"success":true}]"#).unwrap();
        let error = check_command_reply(
            r#"[{"success":true},{"success":false,"error":"No window matches"}]"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("No window matches"), "{error}");
        assert!(check_command_reply("[]").is_err());
    }
}
