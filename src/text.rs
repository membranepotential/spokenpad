use crate::config::Text;
use anyhow::Result;
use regex::{Captures, Regex};

pub struct Processor {
    config: Text,
    fillers: Option<Regex>,
    replacements: Option<Regex>,
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
        Ok(Self {
            config: config.clone(),
            fillers: pattern(
                config.fillers.iter().map(|s| regex::escape(s)),
                r"(?i)\s*\b",
                r"\b,?\s*",
            )?,
            replacements: pattern(
                config.replacements.keys().map(|s| regex::escape(s)),
                r"\b",
                r"\b",
            )?,
        })
    }
    pub fn process(&self, text: &str) -> String {
        let mut out = text.to_owned();
        if self.config.strip_fillers
            && let Some(re) = &self.fillers
        {
            out = re.replace_all(&out, " ").into_owned();
            for (pattern, replacement) in [
                (r"[ \t]+", " "),
                (r"[ \t]+([,.!?;:])", "$1"),
                (r",([.!?;:])", "$1"),
                (r"^[ ,]+", ""),
            ] {
                out = Regex::new(pattern)
                    .expect("constant regex")
                    .replace_all(&out, replacement)
                    .into_owned();
            }
            out = out.trim().to_owned();
        }
        if let Some(re) = &self.replacements {
            out = re
                .replace_all(&out, |c: &Captures<'_>| {
                    self.config.replacements[&c[0]].clone()
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
