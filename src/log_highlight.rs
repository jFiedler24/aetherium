//! Configurable log syntax highlighting.
//!
//! Rules are regex → color mappings loaded from
//! `<config_dir>/aetherium/log_highlight.toml`; when the file is missing or
//! invalid the embedded default is used. The default rules are adapted from
//! the MIT-licensed vscode-logfile-highlighter
//! (github.com/emilast/LogFileHighlighter).

use std::ops::Range;

use gpui::Hsla;
use regex::Regex;
use serde::Deserialize;

/// Embedded rule set, also the template for the on-disk config.
const DEFAULT_RULES_TOML: &str = r##"
# Log highlighting rules: first matching rule wins, left to right.
# `pattern` is a Rust regex, `color` a "#rrggbb" hex value.
# Default rules adapted from vscode-logfile-highlighter (MIT).
[[rule]]
pattern = '(?i)\b(error|fatal|exception)\b'
color = "#e06c75"
bold = true

[[rule]]
pattern = '(?i)\bwarn(ing)?\b'
color = "#d8a657"

[[rule]]
pattern = '(?i)\binfo\b'
color = "#61afef"

[[rule]]
pattern = '(?i)\b(debug|trace)\b'
color = "#8b8d94"

[[rule]]
pattern = '\b\d{4}-\d{2}-\d{2}([ T]\d{2}:\d{2}(:\d{2}(\.\d+)?)?)?\b'
color = "#56b6c2"

[[rule]]
pattern = '\b\d+\b'
color = "#b5cea8"
"##;

/// On-disk representation of the highlight config.
#[derive(Debug, Deserialize)]
struct RuleFile {
    #[serde(default)]
    rule: Vec<RuleDef>,
}

#[derive(Debug, Deserialize)]
struct RuleDef {
    pattern: String,
    color: String,
    #[serde(default)]
    bold: bool,
}

struct HighlightRule {
    regex: Regex,
    fg: Hsla,
    bold: bool,
}

/// A parsed set of highlighting rules.
pub struct LogHighlighter {
    rules: Vec<HighlightRule>,
}

impl LogHighlighter {
    /// Load from the config dir, falling back to the embedded default.
    pub fn load() -> Self {
        let path = dirs::config_dir()
            .map(|dir| dir.join("aetherium").join("log_highlight.toml"));
        let text = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok());
        match text {
            Some(text) => Self::from_toml(&text),
            None => Self::from_toml(DEFAULT_RULES_TOML),
        }
    }

    fn from_toml(text: &str) -> Self {
        let mut rules = Vec::new();
        match toml::from_str::<RuleFile>(text) {
            Ok(file) => {
                for def in file.rule {
                    match parse_rule(&def) {
                        Some(rule) => rules.push(rule),
                        None => eprintln!(
                            "aetherium: skipping invalid highlight rule {:?} ({:?})",
                            def.pattern, def.color
                        ),
                    }
                }
            }
            Err(err) => eprintln!("aetherium: invalid log_highlight.toml: {err}"),
        }
        Self { rules }
    }

    /// Compute the highlighted segments of one line: (char range, fg, bold).
    /// The first matching rule wins; matches are non-overlapping.
    pub fn highlight_line(&self, text: &str) -> Vec<(Range<usize>, Hsla, bool)> {
        let mut segments = Vec::new();
        let mut cursor = 0;
        while cursor <= text.len() {
            // Leftmost match across all rules; ties resolve to the earlier rule.
            let mut best: Option<(Range<usize>, Hsla, bool)> = None;
            for rule in &self.rules {
                if let Some(found) = rule.regex.find_at(text, cursor) {
                    let better = best
                        .as_ref()
                        .is_none_or(|(range, _, _)| found.start() < range.start);
                    if better {
                        best = Some((found.range(), rule.fg, rule.bold));
                    }
                }
            }
            match best {
                Some((range, fg, bold)) => {
                    if !range.is_empty() {
                        segments.push((range.clone(), fg, bold));
                    }
                    cursor = (range.end).max(range.start + 1);
                }
                None => break,
            }
        }
        segments
    }
}

fn parse_rule(def: &RuleDef) -> Option<HighlightRule> {
    let regex = Regex::new(&def.pattern).ok()?;
    let fg = parse_color(&def.color)?;
    Some(HighlightRule {
        regex,
        fg,
        bold: def.bold,
    })
}

/// "#rrggbb" → Hsla.
fn parse_color(text: &str) -> Option<Hsla> {
    let hex = text.strip_prefix('#')?;
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(gpui::rgb(value).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges_for(text: &str) -> Vec<(Range<usize>, bool)> {
        LogHighlighter::from_toml(DEFAULT_RULES_TOML)
            .highlight_line(text)
            .into_iter()
            .map(|(range, _, bold)| (range, bold))
            .collect()
    }

    #[test]
    fn default_rules_highlight_levels() {
        let segs = ranges_for("2024-05-01 ERROR something failed");
        let text: Vec<_> = segs
            .iter()
            .map(|(range, _)| &"2024-05-01 ERROR something failed"[range.clone()])
            .collect();
        assert!(text.contains(&"2024-05-01"));
        assert!(text.contains(&"ERROR"));
        // ERROR is the bold rule.
        let error_seg = segs
            .iter()
            .find(|(range, _)| &"2024-05-01 ERROR something failed"[range.clone()] == "ERROR")
            .unwrap();
        assert!(error_seg.1);
    }

    #[test]
    fn levels_are_case_insensitive() {
        let segs = ranges_for("Warn: low disk");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].0, 0..4);
    }

    #[test]
    fn custom_toml_is_used() {
        let hl = LogHighlighter::from_toml(
            r##"
            [[rule]]
            pattern = "FOO"
            color = "#ff0000"
            "##,
        );
        let segs = hl.highlight_line("xxFOOyy");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].0, 2..5);
    }

    #[test]
    fn invalid_rules_are_skipped() {
        let hl = LogHighlighter::from_toml(
            r##"
            [[rule]]
            pattern = "(unclosed"
            color = "#ff0000"

            [[rule]]
            pattern = "ok"
            color = "not-a-color"

            [[rule]]
            pattern = "fine"
            color = "#00ff00"
            "##,
        );
        let segs = hl.highlight_line("fine");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].0, 0..4);
    }
}
