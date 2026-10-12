//! The time a paragraph of dictation starts with, when it gets one.
//!
//! `nvim.timestamp_format` is parsed once, at config load, into a
//! [`TimestampFormat`]: a few strftime specifiers that make sense for a time
//! of day, and literal text. Everything else derives from that one value —
//! the stamp a capture's paragraph opens with ([`TimestampFormat::render`])
//! and the Vim pattern the dictation editor dims stamps with
//! ([`TimestampFormat::pattern`]) — so the two cannot disagree.
//!
//! Whether a capture gets stamps is its own: the press that began it says
//! so, or leaves it to `nvim.timestamps` ([`Timestamps`]). When it began is
//! the local wall-clock time the daemon read that press ([`Heading`]).
use chrono::{NaiveDateTime, NaiveTime, Timelike};
use std::{borrow::Cow, fmt};

/// Whether the capture a press begins opens its paragraphs with the time.
/// The press's own choice, `spokenpad start --timestamps` or
/// `--no-timestamps`, or none, which leaves it to `nvim.timestamps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Timestamps {
    /// No flag: `nvim.timestamps` decides, as the window has it.
    #[default]
    Configured,
    On,
    Off,
}

impl Timestamps {
    /// Whether paragraphs get a stamp, `configured` being `nvim.timestamps`.
    pub fn on(self, configured: bool) -> bool {
        match self {
            Self::Configured => configured,
            Self::On => true,
            Self::Off => false,
        }
    }
}

/// What a capture's paragraphs open with: when it began, by the local wall
/// clock, and whether they say so. Fixed when the capture begins; nothing a
/// later press does changes it. [`Timestamps::Configured`] is read against
/// `nvim.timestamps` when a paragraph is written, so a config reloaded while
/// the capture's text is still being written applies to its later
/// paragraphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heading {
    /// The local wall-clock time the press that began the capture was read.
    pub started: NaiveDateTime,
    pub timestamps: Timestamps,
}

impl Heading {
    /// `text` as this capture writes it: behind its stamp and one space when
    /// it opens a paragraph that gets one, as it is otherwise. A `continued`
    /// commit extends the paragraph its stamp already opened.
    pub fn paragraph<'a>(
        &self,
        text: &'a str,
        continued: bool,
        format: &TimestampFormat,
        configured: bool,
    ) -> Cow<'a, str> {
        if continued || !self.timestamps.on(configured) {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(format!("{} {text}", format.render(self.started.time())))
        }
    }
}

/// One piece of a [`TimestampFormat`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// `%H`: the hour, 00 to 23.
    Hour,
    /// `%I`: the hour on a 12-hour clock, 01 to 12.
    Hour12,
    /// `%M`
    Minute,
    /// `%S`
    Second,
    /// `%p`: AM or PM.
    Meridiem,
}

/// `nvim.timestamp_format`, parsed: the only shape a stamp can have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampFormat(Vec<Piece>);

/// Why a `nvim.timestamp_format` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    Empty,
    /// A stamp opens a line and stays on it.
    LineBreak,
    /// A specifier that is not one of [`TimestampFormat::SPECIFIERS`].
    Unsupported(char),
    /// A `%` with nothing after it.
    Dangling,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let supported = "%H, %M, %S, %I, %p and %%";
        match self {
            Self::Empty => write!(
                f,
                "nvim.timestamp_format is empty; it is the stamp, such as \"[%H:%M:%S]\""
            ),
            Self::LineBreak => write!(
                f,
                "nvim.timestamp_format must stay on one line: it opens a paragraph"
            ),
            Self::Unsupported(c) => write!(
                f,
                "nvim.timestamp_format: %{c} is not supported; a stamp is a time of day, written with {supported}"
            ),
            Self::Dangling => write!(
                f,
                "nvim.timestamp_format ends in a lone %; write %% for a literal one"
            ),
        }
    }
}

impl std::error::Error for FormatError {}

impl Default for TimestampFormat {
    /// `[14:32:05]`
    fn default() -> Self {
        Self(vec![
            Piece::Text("[".into()),
            Piece::Hour,
            Piece::Text(":".into()),
            Piece::Minute,
            Piece::Text(":".into()),
            Piece::Second,
            Piece::Text("]".into()),
        ])
    }
}

impl TimestampFormat {
    /// What the format may say besides literal text.
    pub const SPECIFIERS: [char; 6] = ['H', 'M', 'S', 'I', 'p', '%'];

    pub fn parse(source: &str) -> Result<Self, FormatError> {
        if source.is_empty() {
            return Err(FormatError::Empty);
        }
        if source.contains(['\n', '\r']) {
            return Err(FormatError::LineBreak);
        }
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut chars = source.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                text.push(c);
                continue;
            }
            let piece = match chars.next().ok_or(FormatError::Dangling)? {
                '%' => {
                    text.push('%');
                    continue;
                }
                'H' => Piece::Hour,
                'I' => Piece::Hour12,
                'M' => Piece::Minute,
                'S' => Piece::Second,
                'p' => Piece::Meridiem,
                other => return Err(FormatError::Unsupported(other)),
            };
            if !text.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut text)));
            }
            pieces.push(piece);
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Ok(Self(pieces))
    }

    /// The stamp for `time`, a local time of day.
    pub fn render(&self, time: NaiveTime) -> String {
        let twelve = match time.hour() % 12 {
            0 => 12,
            hour => hour,
        };
        self.0
            .iter()
            .map(|piece| match piece {
                Piece::Text(text) => text.clone(),
                Piece::Hour => format!("{:02}", time.hour()),
                Piece::Hour12 => format!("{twelve:02}"),
                Piece::Minute => format!("{:02}", time.minute()),
                Piece::Second => format!("{:02}", time.second()),
                Piece::Meridiem => if time.hour() < 12 { "AM" } else { "PM" }.into(),
            })
            .collect()
    }

    /// A Vim pattern that matches a stamp of this format at the start of a
    /// line, and nothing else: `^`, then the rest very nomagic (`\V`), where
    /// only a backslash is special, so literal text is escaped by doubling
    /// its backslashes. A `/` is escaped too, so the pattern also holds in a
    /// `/`-delimited command. Independent of 'magic' and 'ignorecase', as
    /// `matchadd()` reads it.
    pub fn pattern(&self) -> String {
        let mut pattern = String::from(r"^\V");
        for piece in &self.0 {
            match piece {
                Piece::Text(text) => {
                    for c in text.chars() {
                        match c {
                            '\\' => pattern.push_str(r"\\"),
                            '/' => pattern.push_str(r"\/"),
                            c => pattern.push(c),
                        }
                    }
                }
                Piece::Hour | Piece::Hour12 | Piece::Minute | Piece::Second => {
                    pattern.push_str(r"\d\d")
                }
                Piece::Meridiem => pattern.push_str(r"\(AM\|PM\)"),
            }
        }
        pattern
    }
}

impl TryFrom<String> for TimestampFormat {
    type Error = FormatError;

    fn try_from(source: String) -> Result<Self, FormatError> {
        Self::parse(&source)
    }
}

impl<'de> serde::Deserialize<'de> for TimestampFormat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = String::deserialize(deserializer)?;
        Self::parse(&source).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn time(h: u32, m: u32, s: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, s).unwrap()
    }

    #[test]
    fn a_format_takes_time_of_day_specifiers_and_refuses_the_rest() {
        for accepted in ["[%H:%M:%S]", "%I:%M %p", "%%%H", "at %H.%M", "--"] {
            assert!(TimestampFormat::parse(accepted).is_ok(), "{accepted}");
        }
        for (refused, error) in [
            ("", FormatError::Empty),
            ("%H\n", FormatError::LineBreak),
            ("%H\r%M", FormatError::LineBreak),
            ("%Y-%m-%d %H", FormatError::Unsupported('Y')),
            ("%H:%M:%S.%f", FormatError::Unsupported('f')),
            ("%H %ä", FormatError::Unsupported('ä')),
            ("[%H]%", FormatError::Dangling),
        ] {
            assert_eq!(
                TimestampFormat::parse(refused),
                Err(error.clone()),
                "{refused:?}"
            );
        }
        // The message names the specifier and what is allowed instead.
        let message = FormatError::Unsupported('Y').to_string();
        assert!(
            message.contains("%Y")
                && message.contains("%H")
                && message.contains("nvim.timestamp_format"),
            "{message}"
        );
    }

    #[test]
    fn a_stamp_is_rendered_zero_padded_on_either_clock() {
        let format = |source| TimestampFormat::parse(source).unwrap();
        assert_eq!(
            TimestampFormat::default().render(time(9, 5, 7)),
            "[09:05:07]"
        );
        assert_eq!(
            TimestampFormat::default(),
            format("[%H:%M:%S]"),
            "the default is the documented format"
        );
        let twelve = format("%I:%M %p");
        for (at, expected) in [
            (time(0, 0, 0), "12:00 AM"),
            (time(9, 30, 0), "09:30 AM"),
            (time(12, 1, 0), "12:01 PM"),
            (time(23, 59, 59), "11:59 PM"),
        ] {
            assert_eq!(twelve.render(at), expected);
        }
        assert_eq!(format("100%% at %H").render(time(7, 0, 0)), "100% at 07");
    }

    #[test]
    fn the_pattern_matches_the_stamp_literally_at_the_start_of_a_line() {
        let format = |source| TimestampFormat::parse(source).unwrap();
        assert_eq!(TimestampFormat::default().pattern(), r"^\V[\d\d:\d\d:\d\d]");
        // Every character Vim might read as magic is literal after `\V`;
        // only the backslash needs doubling, and `/` is escaped for
        // `/`-delimited commands.
        assert_eq!(format(r"[.*~ %H \ /]").pattern(), r"^\V[.*~ \d\d \\ \/]");
        assert_eq!(format("%I%p").pattern(), r"^\V\d\d\(AM\|PM\)");
        assert_eq!(format("%%%M").pattern(), r"^\V%\d\d");
    }

    #[test]
    fn only_a_paragraph_that_opens_with_stamps_on_gets_one() {
        let heading = |timestamps| Heading {
            started: NaiveDate::from_ymd_opt(2026, 10, 12)
                .unwrap()
                .and_hms_opt(14, 32, 5)
                .unwrap(),
            timestamps,
        };
        let format = TimestampFormat::default();
        for (timestamps, configured, continued, expected) in [
            (Timestamps::On, false, false, "[14:32:05] Hello."),
            (Timestamps::On, true, true, "Hello."),
            (Timestamps::Off, true, false, "Hello."),
            (Timestamps::Configured, true, false, "[14:32:05] Hello."),
            (Timestamps::Configured, false, false, "Hello."),
        ] {
            assert_eq!(
                heading(timestamps).paragraph("Hello.", continued, &format, configured),
                expected,
                "{timestamps:?}, configured {configured}, continued {continued}"
            );
        }
    }
}
