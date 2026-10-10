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

/// Embedded rule set for shell tabs: heuristic coloring for output the
/// remote program left uncolored (the terminal equivalent of `grc`).
/// Same first-match-wins semantics as the log rules; users can override
/// the set via `shell_highlight.toml` in the config directory.
const SHELL_RULES_TOML: &str = r##"
# Shell highlighting rules: first matching rule wins, left to right.
# These apply ONLY to cells the program did not color itself — anything
# SGR-colored (ls --color, vim, htop, ...) renders untouched.
[[rule]]
pattern = '(?i)\b(error|fatal|exception|failed|failure|denied|refused|panic)\b'
color = "#e06c75"
bold = true

[[rule]]
pattern = '(?i)\bwarn(ing)?\b'
color = "#d8a657"

[[rule]]
pattern = '(?i)\b(ok|success(ful)?|passed|done|connected|active|running|enabled|healthy)\b'
color = "#98c379"

[[rule]]
pattern = '(?i)\b(info|notice)\b'
color = "#61afef"

[[rule]]
pattern = '(?i)\b(debug|trace)\b'
color = "#8b8d94"

[[rule]]
pattern = '\b\d{4}-\d{2}-\d{2}([ T]\d{2}:\d{2}(:\d{2}(\.\d+)?)?)?\b'
color = "#56b6c2"

[[rule]]
pattern = '\b\d{1,3}(\.\d{1,3}){3}(:\d+)?\b'
color = "#b5cea8"

[[rule]]
pattern = '\b\d+(\.\d+)?\b'
color = "#b5cea8"

[[rule]]
pattern = "https?://[^\\s'\"]+"
color = "#61afef"

[[rule]]
pattern = "'[^']*'"
color = "#98c379"

[[rule]]
pattern = '"[^"]*"'
color = "#e5c07b"

[[rule]]
pattern = '/(\w|@|~|,|\.|\+|-)+(/(\w|@|~|,|\.|\+|-)*)*'
color = "#56b6c2"
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

// [impl->req~log-highlighting~1]
impl LogHighlighter {
    /// Load the log rule set from the config dir, falling back to the
    /// embedded default.
    pub fn load() -> Self {
        Self::load_from("log_highlight.toml", DEFAULT_RULES_TOML)
    }

    /// Load the shell rule set (`shell_highlight.toml` in the config dir,
    /// embedded shell defaults when missing or invalid).
    // [impl->req~uncolored-cell-coloring~1]
    pub fn load_shell() -> Self {
        Self::load_from("shell_highlight.toml", SHELL_RULES_TOML)
    }

    fn load_from(file_name: &str, default_toml: &str) -> Self {
        let path =
            dirs::config_dir().map(|dir| dir.join("aetherium").join(file_name));
        let text = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok());
        match text {
            Some(text) => Self::from_toml(&text),
            None => Self::from_toml(default_toml),
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
// [utest->req~log-highlighting~1]
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

    // [utest->req~uncolored-cell-coloring~1]
    #[test]
    fn shell_rules_color_common_output() {
        let hl = LogHighlighter::from_toml(SHELL_RULES_TOML);
        let segs = hl.highlight_line("ssh: connect to host 192.168.7.2 port 22: Connection refused");
        let text = "ssh: connect to host 192.168.7.2 port 22: Connection refused";
        let mut refused_bold = false;
        let mut ip_whole = false;
        for (range, _, bold) in &segs {
            match &text[range.clone()] {
                "refused" => refused_bold = *bold,
                "192.168.7.2" => ip_whole = true,
                _ => {}
            }
        }
        assert!(refused_bold, "error words are bold");
        assert!(ip_whole, "IP addresses stay one segment (not 192.168 + 7.2)");

        let segs = hl.highlight_line("ok, downloaded /tmp/aetherium/x.tar.gz");
        let text = "ok, downloaded /tmp/aetherium/x.tar.gz";
        assert!(segs.iter().any(|(r, _, _)| &text[r.clone()] == "ok"));
        assert!(
            segs.iter()
                .any(|(r, _, _)| text[r.clone()].starts_with("/tmp/aetherium"))
        );

        let segs = hl.highlight_line("open https://example.com/docs?id=1 now");
        let text = "open https://example.com/docs?id=1 now";
        assert!(
            segs.iter()
                .any(|(r, _, _)| &text[r.clone()] == "https://example.com/docs?id=1")
        );

        let segs = hl.highlight_line("args: '--force' \"--dry-run\"");
        let text = "args: '--force' \"--dry-run\"";
        assert!(segs.iter().any(|(r, _, _)| &text[r.clone()] == "'--force'"));
        assert!(segs.iter().any(|(r, _, _)| &text[r.clone()] == "\"--dry-run\""));
    }
}
