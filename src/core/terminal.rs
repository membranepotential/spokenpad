//! The terminals spokenpad knows how to open a dictation window in.
//!
//! Each one is a row of facts, taken from its own documentation: which flag
//! names the window for the window manager (the X11 instance on i3, the
//! Wayland `app_id` on sway), whether it can be told where to open, and how
//! it takes the command to run. The window manager's `no_focus` rule keys on
//! that name, so it must be *known* before the window exists: a terminal not
//! in this table cannot be proven not to take focus, and is not accepted.
use crate::core::wm::{Criterion, Property, WmKind};
use anyhow::{Result, bail};
use serde::Deserialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Terminal {
    /// `alacritty --class I -o window.position.x=X -o window.position.y=Y -e`
    /// (`alacritty --help`, `alacritty(1)`): one class value sets both halves
    /// of X11 `WM_CLASS` and, on Wayland, the `app_id`.
    Alacritty,
    /// `kitty --class I --name I --position XxY` (`kitty --help`): `--class`
    /// is the X11 class and the Wayland `app_id`, `--name` the X11 instance;
    /// `--position` works on X11 only.
    Kitty,
    /// `foot --app-id=I` (`foot(1)`). Wayland only; it cannot open a position.
    Foot,
    /// `wezterm start --always-new-process --class I --position screen:X,Y --`
    /// (wezterm.org/cli/start): `--class` is both halves of X11 `WM_CLASS`
    /// and the Wayland `app_id`. Without `--always-new-process` the window
    /// may open in an already running wezterm, and the process spokenpad
    /// started would exit at once.
    Wezterm,
    /// `ghostty --class=spokenpad.I --x11-instance-name=I
    /// --gtk-single-instance=false -e` (ghostty's `Config.zig`): the class is
    /// the Wayland `app_id` and must be a valid GTK application id, hence the
    /// dotted form; GTK builds cannot choose a position.
    Ghostty,
    /// No terminal and no window: nvim runs `--headless`. Nothing can take
    /// focus, so no window manager is involved. For tests, or for a user who
    /// attaches a UI with `nvim --remote-ui --server <socket>`.
    Headless,
}

impl fmt::Display for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Alacritty => "alacritty",
            Self::Kitty => "kitty",
            Self::Foot => "foot",
            Self::Wezterm => "wezterm",
            Self::Ghostty => "ghostty",
            Self::Headless => "headless",
        })
    }
}

/// How a graphical terminal's window is known to the window manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowNames {
    /// The X11 instance, for a terminal that can run on X11 or Xwayland.
    pub x11_instance: Option<String>,
    /// The Wayland `app_id`.
    pub app_id: String,
}

impl Terminal {
    /// Whether this terminal opens a window at all.
    pub fn is_graphical(self) -> bool {
        self != Self::Headless
    }

    /// What the window will be called, for a window instance name that
    /// `config.rs` has already restricted to `[A-Za-z][A-Za-z0-9_-]*`.
    pub fn window_names(self, instance: &str) -> Option<WindowNames> {
        let same = |x11: bool| WindowNames {
            x11_instance: x11.then(|| instance.to_owned()),
            app_id: instance.to_owned(),
        };
        match self {
            Self::Alacritty | Self::Kitty | Self::Wezterm => Some(same(true)),
            Self::Foot => Some(same(false)),
            Self::Ghostty => Some(WindowNames {
                x11_instance: Some(instance.to_owned()),
                app_id: format!("spokenpad.{instance}"),
            }),
            Self::Headless => None,
        }
    }

    /// Every criterion the running window manager must refuse focus to before
    /// this terminal may open a window under it.
    ///
    /// On i3 that is the X11 instance. Under sway it is the `app_id` *and*
    /// the instance: each of these terminals picks Wayland or X11 by itself
    /// (an environment variable or its own configuration can send it to
    /// Xwayland), and a rule for the other one would not apply.
    pub fn focus_criteria(self, instance: &str, wm: WmKind) -> Result<Vec<Criterion>> {
        let Some(names) = self.window_names(instance) else {
            return Ok(Vec::new());
        };
        let x11 = names
            .x11_instance
            .as_deref()
            .map(|name| Criterion::new(Property::Instance, name));
        match (wm, x11) {
            (WmKind::I3, Some(instance)) => Ok(vec![instance?]),
            (WmKind::I3, None) => bail!("{self} runs only on Wayland; i3 is an X11 window manager"),
            (WmKind::Sway, x11) => {
                let mut criteria = vec![Criterion::new(Property::AppId, &names.app_id)?];
                criteria.extend(x11.transpose()?);
                Ok(criteria)
            }
        }
    }

    /// The command line up to, not including, the editor's own argv.
    /// `position` is the window's top-left corner, used where the terminal
    /// can be told it; the window manager is asked to place it afterwards in
    /// any case.
    pub fn argv(self, instance: &str, position: Option<(i32, i32)>) -> Vec<String> {
        let own = |arguments: &[&str]| -> Vec<String> {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect()
        };
        // kitty and wezterm document only non-negative examples; a monitor
        // left of the primary one is placed by the window manager alone.
        let unsigned = position.filter(|(x, y)| *x >= 0 && *y >= 0);
        match self {
            Self::Alacritty => {
                let mut argv = own(&["alacritty", "--class", instance]);
                if let Some((x, y)) = position {
                    argv.extend([
                        "-o".to_owned(),
                        format!("window.position.x={x}"),
                        "-o".to_owned(),
                        format!("window.position.y={y}"),
                    ]);
                }
                argv.push("-e".to_owned());
                argv
            }
            Self::Kitty => {
                let mut argv = own(&["kitty", "--class", instance, "--name", instance]);
                if let Some((x, y)) = unsigned {
                    argv.extend(["--position".to_owned(), format!("{x}x{y}")]);
                }
                argv
            }
            Self::Foot => vec!["foot".to_owned(), format!("--app-id={instance}")],
            Self::Wezterm => {
                let mut argv = own(&[
                    "wezterm",
                    "start",
                    "--always-new-process",
                    "--class",
                    instance,
                ]);
                if let Some((x, y)) = unsigned {
                    argv.extend(["--position".to_owned(), format!("screen:{x},{y}")]);
                }
                argv.push("--".to_owned());
                argv
            }
            Self::Ghostty => vec![
                "ghostty".to_owned(),
                format!("--class=spokenpad.{instance}"),
                format!("--x11-instance-name={instance}"),
                "--gtk-single-instance=false".to_owned(),
                "-e".to_owned(),
            ],
            Self::Headless => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAPHICAL: [Terminal; 5] = [
        Terminal::Alacritty,
        Terminal::Kitty,
        Terminal::Foot,
        Terminal::Wezterm,
        Terminal::Ghostty,
    ];

    #[test]
    fn every_graphical_terminal_names_its_window_before_the_command() {
        for terminal in GRAPHICAL {
            let argv = terminal.argv("spokenpad", Some((10, 20)));
            let names = terminal.window_names("spokenpad").unwrap();
            assert_eq!(argv[0], terminal.to_string());
            assert!(
                argv.iter().any(|argument| argument.contains(&names.app_id)),
                "{terminal}: {argv:?} never sets the app_id {}",
                names.app_id
            );
            if let Some(instance) = &names.x11_instance {
                assert!(
                    argv.iter()
                        .any(|argument| argument.contains(instance.as_str())),
                    "{terminal}: {argv:?}"
                );
            }
        }
        assert!(
            Terminal::Headless
                .argv("spokenpad", Some((1, 2)))
                .is_empty()
        );
        assert_eq!(Terminal::Headless.window_names("spokenpad"), None);
    }

    #[test]
    fn the_exact_argv_of_each_terminal() {
        let argv = |terminal: Terminal, position| terminal.argv("dict", position).join(" ");
        assert_eq!(
            argv(Terminal::Alacritty, Some((-1920, 40))),
            "alacritty --class dict -o window.position.x=-1920 -o window.position.y=40 -e"
        );
        assert_eq!(argv(Terminal::Alacritty, None), "alacritty --class dict -e");
        assert_eq!(
            argv(Terminal::Kitty, Some((10, 20))),
            "kitty --class dict --name dict --position 10x20"
        );
        assert_eq!(
            argv(Terminal::Kitty, Some((-10, 20))),
            "kitty --class dict --name dict"
        );
        assert_eq!(argv(Terminal::Foot, Some((10, 20))), "foot --app-id=dict");
        assert_eq!(
            argv(Terminal::Wezterm, Some((10, 20))),
            "wezterm start --always-new-process --class dict --position screen:10,20 --"
        );
        assert_eq!(
            argv(Terminal::Ghostty, Some((10, 20))),
            "ghostty --class=spokenpad.dict --x11-instance-name=dict --gtk-single-instance=false -e"
        );
    }

    #[test]
    fn i3_proves_the_instance_and_sway_proves_every_name_the_window_can_have() {
        let rules = |terminal: Terminal, wm| {
            terminal
                .focus_criteria("spokenpad", wm)
                .map(|criteria| criteria.iter().map(ToString::to_string).collect::<Vec<_>>())
        };
        assert_eq!(
            rules(Terminal::Alacritty, WmKind::I3).unwrap(),
            [r#"[instance="spokenpad"]"#]
        );
        assert_eq!(
            rules(Terminal::Alacritty, WmKind::Sway).unwrap(),
            [r#"[app_id="spokenpad"]"#, r#"[instance="spokenpad"]"#]
        );
        assert_eq!(
            rules(Terminal::Foot, WmKind::Sway).unwrap(),
            [r#"[app_id="spokenpad"]"#]
        );
        assert_eq!(
            rules(Terminal::Ghostty, WmKind::Sway).unwrap(),
            [
                r#"[app_id="spokenpad.spokenpad"]"#,
                r#"[instance="spokenpad"]"#
            ]
        );
        let error = rules(Terminal::Foot, WmKind::I3).unwrap_err().to_string();
        assert!(error.contains("only on Wayland"), "{error}");
        assert!(rules(Terminal::Headless, WmKind::I3).unwrap().is_empty());
    }

    #[test]
    fn the_packaged_rules_prove_every_terminal_they_claim_to_cover() {
        let i3 = include_str!("../../packaging/i3/spokenpad.conf");
        let sway = include_str!("../../packaging/sway/spokenpad.conf");
        let uncommented: String = sway
            .lines()
            .map(|line| {
                line.strip_prefix("# ")
                    .filter(|l| l.starts_with("no_focus"))
                    .unwrap_or(line)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let proven = |source: &str, terminal: Terminal, wm| {
            terminal
                .focus_criteria("spokenpad", wm)
                .unwrap()
                .iter()
                .all(|criterion| crate::core::wm::has_no_focus_rule([source], criterion))
        };
        for terminal in GRAPHICAL {
            if terminal != Terminal::Foot {
                assert!(proven(i3, terminal, WmKind::I3), "{terminal} on i3");
            }
            // ghostty needs the commented-out lines of the sway file.
            let expected = terminal != Terminal::Ghostty;
            assert_eq!(
                proven(sway, terminal, WmKind::Sway),
                expected,
                "{terminal} on sway"
            );
            assert!(
                proven(&uncommented, terminal, WmKind::Sway),
                "{terminal} on sway"
            );
        }
    }

    #[test]
    fn terminals_are_named_in_lowercase_in_the_config() {
        #[derive(Deserialize)]
        struct Row {
            terminal: Terminal,
        }
        for terminal in GRAPHICAL.into_iter().chain([Terminal::Headless]) {
            let row: Row = toml::from_str(&format!("terminal = \"{terminal}\"")).unwrap();
            assert_eq!(row.terminal, terminal);
        }
        assert!(toml::from_str::<Row>("terminal = \"xterm\"").is_err());
        assert!(toml::from_str::<Row>("terminal = [\"alacritty\", \"-e\"]").is_err());
    }
}
