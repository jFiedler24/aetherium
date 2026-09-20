//! Embedded SVG icon assets.
//!
//! gpui's default asset source serves nothing, so the icon set is compiled
//! into the binary and exposed through this tiny [`AssetSource`]. Reference
//! an icon with `svg().path(assets::ICON_...)`.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{App, AssetSource, SharedString};

const MAIN_EXECUTABLE: &str = include_str!("../aetherium_icons_dark_v2/main_executable.svg");
const CLI_TERMINAL: &str = include_str!("../aetherium_icons_dark_v2/cli_terminal.svg");
const BACKGROUND_PROCESS: &str = include_str!("../aetherium_icons_dark_v2/background_process.svg");
const CONFIGURATION: &str = include_str!("../aetherium_icons_dark_v2/configuration.svg");
const BUILD_TOOL: &str = include_str!("../aetherium_icons_dark_v2/build_tool.svg");
const UPDATE_MANAGER: &str = include_str!("../aetherium_icons_dark_v2/update_manager.svg");
const CHEVRON_RIGHT: &str = include_str!("../aetherium_icons_dark_v2/chevron_right.svg");
const FILE: &str = include_str!("../aetherium_icons_dark_v2/file.svg");

/// App icon, shown next to the brand name in the header.
pub const ICON_MAIN_EXECUTABLE: &str = "aetherium/icons/main_executable.svg";
/// Shell tabs: an interactive terminal session.
pub const ICON_CLI_TERMINAL: &str = "aetherium/icons/cli_terminal.svg";
/// Log-follow tabs: a remote process watched in the background.
pub const ICON_BACKGROUND_PROCESS: &str = "aetherium/icons/background_process.svg";
/// The connection-profiles sidebar section.
pub const ICON_CONFIGURATION: &str = "aetherium/icons/configuration.svg";
/// File tree: disclosure chevron for directories.
pub const ICON_CHEVRON_RIGHT: &str = "aetherium/icons/chevron_right.svg";
/// File tree: generic file glyph.
pub const ICON_FILE: &str = "aetherium/icons/file.svg";

const BUILD_TOOL_PATH: &str = "aetherium/icons/build_tool.svg";
const UPDATE_MANAGER_PATH: &str = "aetherium/icons/update_manager.svg";

pub struct EmbeddedAssets;

/// Load user-provided font files (`.ttf`/`.otf`/`.ttc`) from
/// `~/.config/aetherium/fonts/`. This is how Zed's own fonts (Zed Sans /
/// Zed Mono, SIL OFL 1.1) can be used without a system-wide installation:
/// the app's font families are named after them and gpui falls back to the
/// system fonts when they are absent.
pub fn load_user_fonts(cx: &mut App) {
    let Some(config_dir) = dirs::config_dir() else {
        return;
    };
    let fonts_dir = config_dir.join("aetherium").join("fonts");
    let Ok(entries) = std::fs::read_dir(&fonts_dir) else {
        return;
    };
    let fonts: Vec<Cow<'static, [u8]>> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let ext = path.extension()?.to_str()?;
            if ext.eq_ignore_ascii_case("ttf")
                || ext.eq_ignore_ascii_case("otf")
                || ext.eq_ignore_ascii_case("ttc")
            {
                std::fs::read(&path).ok().map(Cow::Owned)
            } else {
                None
            }
        })
        .collect();
    if fonts.is_empty() {
        return;
    }
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        eprintln!(
            "aetherium: loading fonts from {}: {err:#}",
            fonts_dir.display()
        );
    }
}

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            ICON_MAIN_EXECUTABLE => Some(MAIN_EXECUTABLE.as_bytes()),
            ICON_CLI_TERMINAL => Some(CLI_TERMINAL.as_bytes()),
            ICON_BACKGROUND_PROCESS => Some(BACKGROUND_PROCESS.as_bytes()),
            ICON_CONFIGURATION => Some(CONFIGURATION.as_bytes()),
            BUILD_TOOL_PATH => Some(BUILD_TOOL.as_bytes()),
            UPDATE_MANAGER_PATH => Some(UPDATE_MANAGER.as_bytes()),
            ICON_CHEVRON_RIGHT => Some(CHEVRON_RIGHT.as_bytes()),
            ICON_FILE => Some(FILE.as_bytes()),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
