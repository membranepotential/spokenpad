//! Timeout-bounded X11/i3 queries used only when opening a dictation window.
use crate::geometry::{Output, Rect};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;
use std::{
    io::{ErrorKind, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

fn run(args: &[&str]) -> Result<String> {
    run_bounded(args, TIMEOUT, MAX_OUTPUT_BYTES)
}

fn run_bounded(args: &[&str], timeout: Duration, max_output: usize) -> Result<String> {
    let (program, tail) = args.split_first().context("empty subprocess command")?;
    let mut child = Command::new(program)
        .args(tail)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let mut stdout = child.stdout.take().context("capture subprocess stdout")?;
    set_nonblocking(stdout.as_raw_fd()).context("make subprocess stdout nonblocking")?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut status: Option<ExitStatus> = None;
    let mut eof = false;
    while status.is_none() || !eof {
        drain_output(&mut stdout, &mut bytes, max_output, &mut eof).inspect_err(|_| {
            terminate_group(&mut child);
        })?;
        if status.is_none() {
            status = child.try_wait().context("poll subprocess")?;
        }
        if status.is_some() && eof {
            break;
        }
        if Instant::now() >= deadline {
            terminate_group(&mut child);
            bail!("{program} timed out after {:.3}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = status.context("subprocess ended without an exit status")?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    String::from_utf8(bytes).context("subprocess emitted non-UTF-8 output")
}

fn set_nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `fd` is a live pipe descriptor and F_GETFL/F_SETFL do not retain pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above; the flags came from F_GETFL for this descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn drain_output(
    stdout: &mut impl Read,
    bytes: &mut Vec<u8>,
    max_output: usize,
    eof: &mut bool,
) -> Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                *eof = true;
                return Ok(());
            }
            Ok(count) => {
                if bytes.len().saturating_add(count) > max_output {
                    bail!("subprocess output exceeded {max_output} bytes");
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("read subprocess stdout"),
        }
    }
}

fn terminate_group(child: &mut Child) {
    let process_group = i32::try_from(child.id()).unwrap_or(i32::MAX);
    // SAFETY: a negative PID targets only the child-created process group.
    let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

/// The monitor layout, queried afresh. Once per window spawn, so a monitor
/// plugged in a moment ago is on the list.
pub(crate) fn outputs() -> Vec<Output> {
    let Ok(text) = run(&["xrandr", "--query"]) else {
        return Vec::new();
    };
    let pattern =
        Regex::new(r"(?m)^\S+\s+connected\s+(?:(primary)\s+)?(\d+)x(\d+)\+(-?\d+)\+(-?\d+)")
            .expect("static output regex");
    pattern
        .captures_iter(&text)
        .filter_map(|capture| {
            let width = capture.get(2)?.as_str().parse().ok()?;
            let height = capture.get(3)?.as_str().parse().ok()?;
            if width == 0 || height == 0 {
                return None;
            }
            Some(Output {
                primary: capture.get(1).is_some(),
                rect: Rect {
                    width,
                    height,
                    x: capture.get(4)?.as_str().parse().ok()?,
                    y: capture.get(5)?.as_str().parse().ok()?,
                },
            })
        })
        .collect()
}

pub(crate) fn pointer_position() -> Option<(i32, i32)> {
    let text = run(&["xdotool", "getmouselocation", "--shell"]).ok()?;
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|raw| raw.parse().ok())
    };
    Some((value("X=")?, value("Y=")?))
}

/// Proves that the active i3 configuration refuses this exact instance focus.
/// Deliberately accepts only a literal instance (optionally `^`/`$` anchored),
/// rather than trying to interpret arbitrary PCRE like i3 does.
pub(crate) fn has_no_focus_rule(instance: &str) -> bool {
    let Ok(config) = run(&["i3-msg", "-r", "-t", "get_config"]) else {
        return false;
    };
    has_no_focus_rule_in_json(&config, instance)
}

fn has_no_focus_rule_in_json(config: &str, instance: &str) -> bool {
    let Ok(document) = serde_json::from_str::<Value>(config) else {
        return false;
    };
    let mut sources = document
        .get("config")
        .and_then(Value::as_str)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(includes) = document.get("included_configs").and_then(Value::as_array) {
        sources.extend(includes.iter().filter_map(|included| {
            included
                .get("variable_replaced_contents")
                .or_else(|| included.get("raw_contents"))
                .and_then(Value::as_str)
        }));
    }
    let escaped = regex::escape(instance);
    let criterion = Regex::new(&format!(
        r#"^\s*instance\s*=\s*"(?:\^)?{escaped}(?:\$)?"\s*$"#
    ))
    .expect("escaped instance makes a valid regex");
    sources.into_iter().flat_map(str::lines).any(|raw| {
        let line = raw.trim_start();
        let Some(rest) = line.strip_prefix("no_focus") else {
            return false;
        };
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            return false;
        }
        let criteria = rest.trim_start();
        let Some(criteria) = criteria.strip_prefix('[') else {
            return false;
        };
        let Some(close) = criteria.find(']') else {
            return false;
        };
        let suffix = criteria[close + 1..].trim();
        (suffix.is_empty() || suffix.starts_with('#')) && criterion.is_match(&criteria[..close])
    })
}

/// Whether i3 currently holds a window with this X11 instance name. Public
/// because the manual window smoke check must refuse to run beside a live
/// dictation window.
pub fn i3_window_exists(instance: &str) -> bool {
    let Ok(text) = run(&["i3-msg", "-t", "get_tree"]) else {
        return false;
    };
    fn contains(node: &Value, instance: &str) -> bool {
        if node
            .get("window_properties")
            .and_then(|properties| properties.get("instance"))
            .and_then(Value::as_str)
            == Some(instance)
        {
            return true;
        }
        ["nodes", "floating_nodes"].into_iter().any(|key| {
            node.get(key)
                .and_then(Value::as_array)
                .is_some_and(|children| children.iter().any(|child| contains(child, instance)))
        })
    }
    serde_json::from_str::<Value>(&text)
        .ok()
        .is_some_and(|tree| contains(&tree, instance))
}

pub(crate) fn place_window(instance: &str, rect: Rect) -> Result<()> {
    let criteria = format!("[instance=\"{instance}\"]");
    let command = format!(
        "floating enable, resize set {} {}, move position {} {}",
        rect.width, rect.height, rect.x, rect.y
    );
    run(&["i3-msg", &format!("{criteria} {command}")]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_regex_accepts_negative_coordinates() {
        let text = "DP-1 connected primary 1920x1080+0+0 normal\nHDMI-1 connected 1280x1024+-1280+-40 normal";
        let pattern =
            Regex::new(r"(?m)^\S+\s+connected\s+(?:(primary)\s+)?(\d+)x(\d+)\+(-?\d+)\+(-?\d+)")
                .unwrap();
        let found: Vec<_> = pattern
            .captures_iter(text)
            .map(|capture| {
                (
                    capture[4].parse::<i32>().unwrap(),
                    capture[5].parse::<i32>().unwrap(),
                )
            })
            .collect();
        assert_eq!(found, [(0, 0), (-1280, -40)]);
    }

    #[test]
    fn no_focus_parser_reads_includes_and_requires_singleton_instance() {
        let config = serde_json::json!({
            "config": "include ~/.config/i3/conf.d/*\n",
            "included_configs": [{
                "path": "/tmp/spokenpad",
                "raw_contents": "no_focus [instance=\"$spokenpad\"]\n",
                "variable_replaced_contents": concat!(
                    "no_focus [instance=\"^spokenpad$\"]\n",
                    "no_focus [instance=\"spokenpad\" title=\"restricted\"]\n"
                )
            }]
        });
        assert!(has_no_focus_rule_in_json(&config.to_string(), "spokenpad"));
        assert!(!has_no_focus_rule_in_json(&config.to_string(), "another"));
    }

    #[test]
    fn no_focus_parser_rejects_a_conjunction() {
        let config = serde_json::json!({
            "config": "no_focus [instance=\"spokenpad\" title=\"restricted\"]",
            "included_configs": []
        });
        assert!(!has_no_focus_rule_in_json(&config.to_string(), "spokenpad"));
    }

    #[test]
    fn no_focus_parser_rejects_malformed_directives() {
        for directive in [
            "no_focus_typo [instance=\"spokenpad\"]",
            "no_focus garbage [instance=\"spokenpad\"]",
            "no_focus [instance=\"spokenpad\"] garbage",
        ] {
            let config = serde_json::json!({ "config": directive, "included_configs": [] });
            assert!(
                !has_no_focus_rule_in_json(&config.to_string(), "spokenpad"),
                "accepted {directive:?}"
            );
        }
    }

    #[test]
    fn bounded_runner_rejects_excess_output() {
        let error = run_bounded(&["sh", "-c", "printf 123456789"], Duration::from_secs(1), 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeded 8 bytes"), "{error}");
    }

    #[test]
    fn bounded_runner_does_not_wait_on_a_descendants_pipe() {
        let started = Instant::now();
        let error = run_bounded(
            &["sh", "-c", "(sleep 5) &"],
            Duration::from_millis(100),
            1024,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
