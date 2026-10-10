//! Log-collection sources for `POST /logs/collect`.
//!
//! A built-in set covers the common Linux logs; users add their own via
//! `<config dir>/aetherium/collect.toml`:
//!
//! ```toml
//! files = ["/app/sovd/sovd.log"]
//! commands = ["systemctl status sovd --no-pager"]
//! ```
//!
//! Collection is a UI-side state machine: each source becomes one
//! `SessionCommand::ApiExec` round trip, so at most one extra exec channel
//! is open beside the interactive session. Every source is attempted
//! independently — a missing file or a failing command is reported per
//! source, never aborting the collection.

use serde::Deserialize;

/// Per-source content cap for collection responses.
pub const COLLECT_MAX_BYTES: usize = 512 * 1024;
/// Wall-clock budget for one source's exec.
pub const COLLECT_STEP_TIMEOUT_SECS: u64 = 45;
/// Wall-clock budget for the whole collection.
pub const COLLECT_JOB_TIMEOUT_SECS: u64 = 300;

/// One thing to collect from the target.
#[derive(Debug, Clone)]
pub enum CollectSource {
    /// A shell command whose stdout is the content.
    Command(String),
    /// A file, fetched with `cat | head -c cap+1` so oversized logs cannot
    /// flood memory; reading one byte past the cap sets `truncated`.
    File(String),
}

impl CollectSource {
    pub fn name(&self) -> String {
        match self {
            CollectSource::Command(command) => command.clone(),
            CollectSource::File(path) => path.clone(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            CollectSource::Command(_) => "command",
            CollectSource::File(_) => "file",
        }
    }

    /// The shell command producing this source's content.
    // [impl->req~configurable-log-sources~1]
    pub fn command(&self) -> String {
        match self {
            CollectSource::Command(command) => command.clone(),
            CollectSource::File(path) => format!(
                "cat -- {} | head -c {}",
                shell_quote(path),
                COLLECT_MAX_BYTES + 1
            ),
        }
    }
}

/// Quote a string for execution by a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Built-in commands: kernel ring buffer and the current boot's journal
/// (the latter simply fails on non-systemd systems — reported per source).
const DEFAULT_COMMANDS: &[&str] = &["dmesg", "journalctl -b --no-pager -n 2000"];

/// Built-in files: the classic syslog destinations across distros.
/// Missing ones are reported, not fatal.
const DEFAULT_FILES: &[&str] = &[
    "/var/log/syslog",
    "/var/log/messages",
    "/var/log/kern.log",
    "/var/log/auth.log",
    "/var/log/daemon.log",
    "/var/log/dmesg",
];

/// The on-disk additions to the built-in set.
#[derive(Debug, Default, Deserialize)]
struct CollectConfig {
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    commands: Vec<String>,
}

/// Parse a `collect.toml` body (pure, for tests).
fn parse_config(text: &str) -> (Vec<String>, Vec<String>) {
    match toml::from_str::<CollectConfig>(text) {
        Ok(config) => (config.files, config.commands),
        Err(err) => {
            eprintln!("aetherium: invalid collect.toml: {err}");
            (Vec::new(), Vec::new())
        }
    }
}

/// The full source list: built-ins first, then the configured additions.
// [impl->req~configurable-log-sources~1]
pub fn sources() -> Vec<CollectSource> {
    let (extra_files, extra_commands) =
        std::fs::read_to_string(crate::crypto::config_dir().join("collect.toml"))
            .map(|text| parse_config(&text))
            .unwrap_or_default();

    let mut sources = Vec::new();
    for command in DEFAULT_COMMANDS
        .iter()
        .map(|command| command.to_string())
        .chain(extra_commands)
    {
        sources.push(CollectSource::Command(command));
    }
    for file in DEFAULT_FILES
        .iter()
        .map(|file| file.to_string())
        .chain(extra_files)
    {
        sources.push(CollectSource::File(file));
    }
    sources
}

#[cfg(test)]
// [utest->req~configurable-log-sources~1]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_common_logs() {
        let sources: Vec<CollectSource> = DEFAULT_COMMANDS
            .iter()
            .map(|command| CollectSource::Command(command.to_string()))
            .chain(
                DEFAULT_FILES
                    .iter()
                    .map(|file| CollectSource::File(file.to_string())),
            )
            .collect();
        let names: Vec<String> = sources.iter().map(|source| source.name()).collect();
        assert!(names.iter().any(|name| name == "dmesg"));
        assert!(
            names
                .iter()
                .any(|name| name == "journalctl -b --no-pager -n 2000")
        );
        assert!(names.iter().any(|name| name == "/var/log/syslog"));
        assert!(names.iter().any(|name| name == "/var/log/messages"));
        assert!(sources.iter().any(|source| source.kind() == "command"));
        assert!(sources.iter().any(|source| source.kind() == "file"));
    }

    #[test]
    fn config_adds_user_sources() {
        let (files, commands) = parse_config(
            r#"
            files = ["/app/sovd/sovd.log", "/data/trace.log"]
            commands = ["systemctl status sovd --no-pager"]
            "#,
        );
        assert_eq!(files, vec!["/app/sovd/sovd.log", "/data/trace.log"]);
        assert_eq!(commands, vec!["systemctl status sovd --no-pager"]);
    }

    #[test]
    fn invalid_config_yields_no_extras() {
        let (files, commands) = parse_config("files = [");
        assert!(files.is_empty());
        assert!(commands.is_empty());
    }

    #[test]
    fn file_source_caps_via_head() {
        let command = CollectSource::File("/app/sovd/sovd.log".into()).command();
        assert!(command.starts_with("cat -- '/app/sovd/sovd.log' | head -c "));
        assert!(command.ends_with(&format!("{}", COLLECT_MAX_BYTES + 1)));
        // Quotes embedded single quotes POSIX-style.
        assert_eq!(
            CollectSource::File("/tmp/it's.log".into()).command(),
            "cat -- '/tmp/it'\\''s.log' | head -c 524289"
        );
    }
}
