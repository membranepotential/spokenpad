use crate::config::Text;
use anyhow::Result;
use regex::{Captures, Regex};

/// Whitespace and punctuation repair applied after fillers are removed.
struct Cleanup {
    runs: Regex,
    before_punctuation: Regex,
    comma_before_punctuation: Regex,
    leading: Regex,
}
impl Cleanup {
    fn new() -> Result<Self> {
        Ok(Self {
            runs: Regex::new(r"[ \t]+")?,
            before_punctuation: Regex::new(r"[ \t]+([,.!?;:])")?,
            comma_before_punctuation: Regex::new(r",([.!?;:])")?,
            leading: Regex::new(r"^[ ,]+")?,
        })
    }
    fn apply(&self, text: &str) -> String {
        let out = self.runs.replace_all(text, " ");
        let out = self.before_punctuation.replace_all(&out, "$1");
        let out = self.comma_before_punctuation.replace_all(&out, "$1");
        self.leading.replace_all(&out, "").trim().to_owned()
    }
}

pub struct Processor {
    config: Text,
    fillers: Option<Regex>,
    replacements: Option<Regex>,
    cleanup: Cleanup,
}
impl Processor {
    pub fn new(config: &Text) -> Result<Self> {
        fn pattern(
            words: impl Iterator<Item = String>,
            prefix: &str,
            suffix: &str,
        ) -> Result<Option<Regex>> {
            let words: Vec<_> = words.collect();
            Ok(if words.is_empty() {
                None
            } else {
                Some(Regex::new(&format!(
                    "{prefix}(?:{}){suffix}",
                    words.join("|")
                ))?)
            })
        }
        // A key matches as a word of its own: nothing that belongs to a word
        // on either side of it. The half boundaries say exactly that, also
        // for a key that begins or ends with punctuation ("e.g.", "c++"),
        // which `\b` would require to touch a word.
        Ok(Self {
            config: config.clone(),
            fillers: pattern(
                config.fillers.iter().map(|s| regex::escape(s)),
                r"(?i)\s*\b{start-half}",
                r"\b{end-half},?\s*",
            )?,
            replacements: pattern(
                config.replacements.keys().map(|s| regex::escape(s)),
                r"\b{start-half}",
                r"\b{end-half}",
            )?,
            cleanup: Cleanup::new()?,
        })
    }
    pub fn process(&self, text: &str) -> String {
        let mut out = text.to_owned();
        if self.config.strip_fillers
            && let Some(re) = &self.fillers
        {
            out = self.cleanup.apply(&re.replace_all(&out, " "));
        }
        if let Some(re) = &self.replacements {
            out = re
                .replace_all(&out, |c: &Captures<'_>| {
                    // The pattern is built from these keys, but never index a
                    // map with recognizer output: leave anything unknown alone.
                    self.config
                        .replacements
                        .get(&c[0])
                        .cloned()
                        .unwrap_or_else(|| c[0].to_owned())
                })
                .into_owned();
        }
        if self.config.trailing_space && !out.is_empty() {
            out.push(' ');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_replacements_follow_toml_order() {
        let c = crate::config::Config::parse("[text.replacements]\n'foo bar'='X'\nfoo='Y'", None)
            .unwrap();
        assert_eq!(
            Processor::new(&c.text).unwrap().process("foo bar foo"),
            "X Y"
        );
    }
    /// A key that begins or ends with punctuation matches where it stands
    /// as a word of its own, and never inside another word.
    #[test]
    fn keys_edged_with_punctuation_match_as_words() {
        let c = crate::config::Config::parse(
            "[text]\nfillers=['hm…']\n[text.replacements]\n'e.g.'='for example'\n'c++'='C++'\n'#todo'='TODO'",
            None,
        )
        .unwrap();
        let p = Processor::new(&c.text).unwrap();
        for (input, output) in [
            ("see e.g. this", "see for example this"),
            ("see e.g.", "see for example"),
            ("e.g., that", "for example, that"),
            ("write c++ and c++", "write C++ and C++"),
            ("a #todo here", "a TODO here"),
            ("hm… right", "right"),
            // Glued to a word, a key is part of that word.
            ("e.g.x", "e.g.x"),
            ("abc++", "abc++"),
            ("x#todo", "x#todo"),
        ] {
            assert_eq!(p.process(input), output, "{input:?}");
        }
    }
    #[test]
    fn fillers_and_literal_replacements() {
        let mut c = Text::default();
        c.replacements.insert("teh".into(), "$1 \\ text".into());
        let p = Processor::new(&c).unwrap();
        for (input, output) in [
            ("Um, hello uh world, erm.", "hello world."),
            ("umbrella hmmm", "umbrella hmmm"),
            ("teh set reset", "$1 \\ text set reset"),
            ("UM", ""),
            ("Hello\nworld", "Hello\nworld"),
        ] {
            assert_eq!(p.process(input), output);
        }
    }
}
