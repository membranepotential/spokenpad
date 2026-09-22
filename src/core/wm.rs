//! The i3 IPC protocol, as values: framing, and the replies spokenpad reads.
//!
//! i3 and sway speak the same protocol over a Unix socket (i3's `ipc.html`,
//! sway's `sway-ipc(7)`): the magic `i3-ipc`, a payload length and a message
//! type as native-endian `u32`s, then a JSON payload. A reply carries the type
//! of the request it answers. Everything here is pure; the socket is in
//! [`shell::wm`](crate::shell::wm).
use crate::core::geometry::{Output, Rect};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::fmt;

pub const MAGIC: &[u8; 6] = b"i3-ipc";
pub const HEADER_LEN: usize = MAGIC.len() + 8;

/// The requests spokenpad sends. The numbers are the protocol's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    RunCommand = 0,
    GetOutputs = 3,
    GetTree = 4,
    GetVersion = 7,
    GetConfig = 9,
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
    /// i3, on X11: windows are matched by their X11 instance.
    I3,
    /// sway, on Wayland: native windows by `app_id`, Xwayland ones by instance.
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

/// `GET_OUTPUTS`: the active outputs, in the order the window manager lists
/// them. i3 also lists a disabled `xroot-0`, which is dropped with every other
/// inactive or empty output.
pub fn parse_outputs(reply: &str) -> Result<Vec<Output>> {
    let document: Value = serde_json::from_str(reply).context("GET_OUTPUTS reply is not JSON")?;
    let outputs = document
        .as_array()
        .context("GET_OUTPUTS reply is not an array")?;
    Ok(outputs
        .iter()
        .filter(|output| output.get("active").and_then(Value::as_bool) == Some(true))
        .filter_map(|output| {
            let rect = output.get("rect")?;
            let number = |key: &str| rect.get(key).and_then(Value::as_i64);
            let rect = Rect {
                x: i32::try_from(number("x")?).ok()?,
                y: i32::try_from(number("y")?).ok()?,
                width: u32::try_from(number("width")?).ok()?,
                height: u32::try_from(number("height")?).ok()?,
            };
            let flag = |key: &str| output.get(key).and_then(Value::as_bool) == Some(true);
            (rect.width > 0 && rect.height > 0).then(|| Output {
                rect,
                primary: flag("primary"),
                focused: flag("focused"),
            })
        })
        .collect())
}

/// The window property a rule or a command selects on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Property {
    /// The instance half of X11 `WM_CLASS` (i3, and Xwayland under sway).
    Instance,
    /// The class half of X11 `WM_CLASS` (i3, and Xwayland under sway).
    Class,
    /// The Wayland `app_id` (sway, native windows).
    AppId,
}

impl Property {
    fn key(self) -> &'static str {
        match self {
            Self::Instance => "instance",
            Self::Class => "class",
            Self::AppId => "app_id",
        }
    }
}

/// One criterion, `[instance="spokenpad"]`. The value is restricted to
/// `[A-Za-z0-9_.-]` when constructed, so interpolating it into a criteria
/// string can never produce window-manager syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Criterion {
    property: Property,
    value: String,
}

impl Criterion {
    pub fn new(property: Property, value: &str) -> Result<Self> {
        ensure!(
            !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "window criterion value must match [A-Za-z0-9_.-]+, got {value:?}"
        );
        Ok(Self {
            property,
            value: value.to_owned(),
        })
    }

    pub fn property(&self) -> Property {
        self.property
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    /// Whether a `GET_TREE` node is a window this criterion selects.
    fn selects(&self, node: &Value) -> bool {
        let actual = match self.property {
            Property::AppId => node.get("app_id"),
            Property::Instance | Property::Class => node
                .get("window_properties")
                .and_then(|properties| properties.get(self.property.key())),
        };
        actual.and_then(Value::as_str) == Some(self.value.as_str())
    }
}

impl fmt::Display for Criterion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}=\"{}\"]", self.property.key(), self.value)
    }
}

/// The configuration text `GET_CONFIG` returns, exactly as the window manager
/// loaded it: the main file, plus — on i3 4.20 and later — every file it
/// included, with i3's variables replaced. sway returns the main file only,
/// so a rule in a file sway includes is not in this text and proves nothing:
/// reading that file from disk could count a rule sway never loaded (a file
/// linked in since the last reload, or a path whose variables expand
/// differently in the daemon's environment).
pub fn parse_config(reply: &str) -> Result<Vec<String>> {
    let document: Value = serde_json::from_str(reply).context("GET_CONFIG reply is not JSON")?;
    let main = document
        .get("config")
        .and_then(Value::as_str)
        .context("GET_CONFIG reply has no config text")?;
    let mut sources = vec![main.to_owned()];
    if let Some(includes) = document.get("included_configs").and_then(Value::as_array) {
        sources.extend(includes.iter().filter_map(|included| {
            included
                .get("variable_replaced_contents")
                .or_else(|| included.get("raw_contents"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        }));
    }
    Ok(sources)
}

/// Whether the configuration refuses focus to exactly this criterion.
///
/// Deliberately accepts only a directive whose criteria are this one property
/// with a literal value (optionally `^`/`$` anchored), rather than trying to
/// interpret arbitrary PCRE and conjunctions the way the window manager does:
/// anything this cannot read with certainty does not count as proof.
pub fn has_no_focus_rule<'a>(
    sources: impl IntoIterator<Item = &'a str>,
    criterion: &Criterion,
) -> bool {
    let expected = format!(
        r#"^\s*{}\s*=\s*"(?:\^)?{}(?:\$)?"\s*$"#,
        criterion.property.key(),
        regex::escape(&criterion.value)
    );
    let pattern = regex::Regex::new(&expected).expect("escaped criterion makes a valid regex");
    sources.into_iter().flat_map(str::lines).any(|raw| {
        let Some(rest) = raw.trim_start().strip_prefix("no_focus") else {
            return false;
        };
        if !rest.starts_with(char::is_whitespace) {
            return false;
        }
        let Some(criteria) = rest.trim_start().strip_prefix('[') else {
            return false;
        };
        let Some(close) = criteria.find(']') else {
            return false;
        };
        let suffix = criteria[close + 1..].trim();
        (suffix.is_empty() || suffix.starts_with('#')) && pattern.is_match(&criteria[..close])
    })
}

/// Every node of a `GET_TREE` reply, tiled and floating alike.
fn nodes(tree: &Value) -> Box<dyn Iterator<Item = &Value> + '_> {
    let children = ["nodes", "floating_nodes"]
        .into_iter()
        .filter_map(|key| tree.get(key).and_then(Value::as_array))
        .flatten()
        .flat_map(nodes);
    Box::new(std::iter::once(tree).chain(children))
}

/// The first criterion, in the order given, that selects a window in the tree.
pub fn find_window<'a>(tree: &str, criteria: &'a [Criterion]) -> Result<Option<&'a Criterion>> {
    let tree: Value = serde_json::from_str(tree).context("GET_TREE reply is not JSON")?;
    Ok(criteria
        .iter()
        .find(|criterion| nodes(&tree).any(|node| criterion.selects(node))))
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

/// The id of the focused node, whatever it is. Used to check that opening a
/// window left focus where it was.
pub fn focused_node(tree: &str) -> Result<Option<i64>> {
    let tree: Value = serde_json::from_str(tree).context("GET_TREE reply is not JSON")?;
    Ok(nodes(&tree)
        .find(|node| node.get("focused").and_then(Value::as_bool) == Some(true))
        .and_then(|node| node.get("id").and_then(Value::as_i64)))
}

/// The command that floats, sizes and places the window `criterion` selects.
/// i3's `move position` is already relative to the whole screen; sway's is
/// relative to the workspace unless `absolute` is given (`sway(5)`).
pub fn placement_command(kind: WmKind, criterion: &Criterion, rect: Rect) -> String {
    let absolute = match kind {
        WmKind::I3 => "",
        WmKind::Sway => "absolute ",
    };
    format!(
        "{criterion} floating enable, resize set {} {}, move {absolute}position {} {}",
        rect.width, rect.height, rect.x, rect.y
    )
}

/// The sway command that adds a `no_focus` rule, while sway runs, for exactly
/// the windows all of `criteria` select.
///
/// sway accepts `no_focus` at runtime (it is in the table of commands valid
/// both in the configuration and over IPC, `sway/commands.c`) and ignores a
/// rule it already holds (`criteria_already_exists`). A rule added this way
/// lasts until the next `reload`, which is why it is sent before every map
/// rather than once.
///
/// sway reads every criteria value as a PCRE pattern that may match anywhere
/// in the name, so each is anchored with `^` and `$`. The value is already
/// `[A-Za-z0-9_.-]+`, and a dot is written `[.]`, so the pattern needs no
/// backslash, which sway's criteria parser would strip.
pub fn no_focus_command(criteria: &[Criterion]) -> Result<String> {
    ensure!(
        !criteria.is_empty(),
        "a no_focus rule without criteria would match every window"
    );
    let criteria = criteria
        .iter()
        .map(|criterion| {
            format!(
                r#"{}="^{}$""#,
                criterion.property.key(),
                criterion.value.replace('.', "[.]")
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!("no_focus [{criteria}]"))
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

    fn instance(value: &str) -> Criterion {
        Criterion::new(Property::Instance, value).unwrap()
    }

    fn app_id(value: &str) -> Criterion {
        Criterion::new(Property::AppId, value).unwrap()
    }

    #[test]
    fn frames_round_trip_and_replies_are_checked_for_their_type() {
        let frame = encode(Message::GetConfig, "{}").unwrap();
        assert_eq!(&frame[..6], MAGIC);
        assert_eq!(frame.len(), HEADER_LEN + 2);
        let header: [u8; HEADER_LEN] = frame[..HEADER_LEN].try_into().unwrap();
        assert_eq!(reply_length(&header, Message::GetConfig).unwrap(), 2);
        assert!(reply_length(&header, Message::GetTree).is_err());
        let mut bad = header;
        bad[0] = b'x';
        assert!(reply_length(&bad, Message::GetConfig).is_err());
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
    fn inactive_and_empty_outputs_are_dropped() {
        let reply = json!([
            {"name":"eDP-1","active":true,"primary":true,"rect":{"x":3840,"y":0,"width":3840,"height":2160}},
            {"name":"xroot-0","active":false,"primary":false,"rect":{"x":0,"y":0,"width":3840,"height":2160}},
            {"name":"HDMI-1","active":true,"focused":true,"rect":{"x":-1280,"y":-40,"width":1280,"height":1024}},
            {"name":"DP-9","active":true,"rect":{"x":0,"y":0,"width":0,"height":0}}
        ]);
        let outputs = parse_outputs(&reply.to_string()).unwrap();
        assert_eq!(outputs.len(), 2);
        assert!(outputs[0].primary && !outputs[0].focused);
        assert_eq!(outputs[1].rect.x, -1280);
        assert!(outputs[1].focused && !outputs[1].primary);
    }

    #[test]
    fn a_criterion_value_cannot_become_window_manager_syntax() {
        for hostile in ["", "x\" ]", "a b", "x]; exec rm", "é"] {
            assert!(
                Criterion::new(Property::Instance, hostile).is_err(),
                "{hostile:?}"
            );
        }
        assert_eq!(
            app_id("spokenpad.io").to_string(),
            r#"[app_id="spokenpad.io"]"#
        );
    }

    #[test]
    fn no_focus_is_read_from_includes_and_needs_a_singleton_criterion() {
        let reply = json!({
            "config": "include ~/.config/i3/conf.d/*\n",
            "included_configs": [{
                "path": "/tmp/spokenpad",
                "raw_contents": "no_focus [instance=\"$spokenpad\"]\n",
                "variable_replaced_contents": concat!(
                    "no_focus [instance=\"^spokenpad$\"]\n",
                    "no_focus [instance=\"other\" title=\"restricted\"]\n",
                    "no_focus [app_id=\"spokenpad\"]\n"
                )
            }]
        });
        let sources = parse_config(&reply.to_string()).unwrap();
        let proves = |criterion| has_no_focus_rule(sources.iter().map(String::as_str), &criterion);
        assert!(proves(instance("spokenpad")));
        assert!(proves(app_id("spokenpad")));
        assert!(!proves(instance("other")));
        assert!(!proves(instance("another")));
        // An instance rule is not an app_id rule.
        assert!(!has_no_focus_rule(
            ["no_focus [instance=\"x\"]"],
            &app_id("x")
        ));
    }

    #[test]
    fn malformed_no_focus_directives_prove_nothing() {
        for directive in [
            "no_focus_typo [instance=\"spokenpad\"]",
            "no_focus garbage [instance=\"spokenpad\"]",
            "no_focus [instance=\"spokenpad\"] garbage",
            "no_focus [instance=\"spokenpad\" title=\"x\"]",
            "# no_focus [instance=\"spokenpad\"]",
            "no_focus [instance=\"spokenpa.\"]",
        ] {
            assert!(
                !has_no_focus_rule([directive], &instance("spokenpad")),
                "accepted {directive:?}"
            );
        }
        assert!(has_no_focus_rule(
            ["no_focus [instance=\"spokenpad\"] # the dictation window"],
            &instance("spokenpad")
        ));
    }

    #[test]
    fn the_tree_is_searched_through_floating_nodes() {
        let tree = json!({
            "id": 1, "focused": false,
            "nodes": [{"id": 2, "focused": true, "app_id": "foot", "nodes": []}],
            "floating_nodes": [{
                "id": 3, "focused": false,
                "window_properties": {"instance": "spokenpad", "class": "Alacritty"}
            }]
        })
        .to_string();
        let criteria = [app_id("spokenpad"), instance("spokenpad")];
        assert_eq!(find_window(&tree, &criteria).unwrap(), Some(&criteria[1]));
        assert_eq!(find_window(&tree, &criteria[..1]).unwrap(), None);
        assert_eq!(focused_node(&tree).unwrap(), Some(2));
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
    fn placement_is_absolute_on_both_window_managers() {
        let rect = Rect {
            x: -1920,
            y: 40,
            width: 600,
            height: 400,
        };
        assert_eq!(
            placement_command(WmKind::I3, &instance("spokenpad"), rect),
            r#"[instance="spokenpad"] floating enable, resize set 600 400, move position -1920 40"#
        );
        assert_eq!(
            placement_command(WmKind::Sway, &app_id("spokenpad"), rect),
            r#"[app_id="spokenpad"] floating enable, resize set 600 400, move absolute position -1920 40"#
        );
    }

    #[test]
    fn a_runtime_no_focus_rule_is_anchored_and_needs_criteria() {
        let class = Criterion::new(Property::Class, "spokenpad-pane").unwrap();
        assert_eq!(
            no_focus_command(&[instance("spokenpad-pane"), class]).unwrap(),
            r#"no_focus [instance="^spokenpad-pane$" class="^spokenpad-pane$"]"#
        );
        assert_eq!(
            no_focus_command(&[app_id("org.spokenpad")]).unwrap(),
            r#"no_focus [app_id="^org[.]spokenpad$"]"#
        );
        assert!(no_focus_command(&[]).is_err());
    }

    #[test]
    fn a_class_criterion_reads_the_class_half_of_wm_class() {
        let tree = json!({
            "id": 1, "focused": false, "nodes": [],
            "floating_nodes": [{
                "id": 3, "focused": false,
                "window_properties": {"instance": "pane", "class": "Pane"}
            }]
        })
        .to_string();
        let class = |value| Criterion::new(Property::Class, value).unwrap();
        let criteria = [class("pane"), class("Pane")];
        assert_eq!(find_window(&tree, &criteria).unwrap(), Some(&criteria[1]));
        assert_eq!(criteria[1].to_string(), r#"[class="Pane"]"#);
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
