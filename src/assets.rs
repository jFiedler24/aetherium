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

// UI glyphs from Zed's icon set (derived from Lucide, ISC — see
// `zed_icons/LICENSES`). Rendered through gpui's alpha-mask SVG path, so the
// stroke colors baked into the files are ignored; `.text_color()` tints them.
const PLUS: &str = include_str!("../zed_icons/plus.svg");
const PENCIL: &str = include_str!("../zed_icons/pencil.svg");
const TRASH: &str = include_str!("../zed_icons/trash.svg");
const EYE: &str = include_str!("../zed_icons/eye.svg");
const EYE_OFF: &str = include_str!("../zed_icons/eye_off.svg");
const CHEVRON_DOWN: &str = include_str!("../zed_icons/chevron_down.svg");
const SIGNAL_HIGH: &str = include_str!("../zed_icons/signal_high.svg");
const SIGNAL_MEDIUM: &str = include_str!("../zed_icons/signal_medium.svg");
const DISCONNECTED: &str = include_str!("../zed_icons/disconnected.svg");
const FOLDER: &str = include_str!("../zed_icons/folder.svg");
const FOLDER_OPEN: &str = include_str!("../zed_icons/folder_open.svg");
const ARROW_RIGHT: &str = include_str!("../zed_icons/arrow_right.svg");
const ARROW_UP: &str = include_str!("../zed_icons/arrow_up.svg");
const ARROW_DOWN: &str = include_str!("../zed_icons/arrow_down.svg");
const DEBUG_PAUSE: &str = include_str!("../zed_icons/debug_pause.svg");
const PLAY_FILLED: &str = include_str!("../zed_icons/play_filled.svg");
const BOOKMARK: &str = include_str!("../zed_icons/bookmark.svg");
const TOOL_SEARCH: &str = include_str!("../zed_icons/tool_search.svg");
const FILTER_FUNNEL: &str = include_str!("../zed_icons/filter_funnel.svg");
const LOCK: &str = include_str!("../zed_icons/lock.svg");
const LOCK_OFF: &str = include_str!("../zed_icons/lock_off.svg");

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
/// Header: create a new profile.
pub const ICON_PLUS: &str = "aetherium/icons/plus.svg";
/// Header: edit the selected profile.
pub const ICON_PENCIL: &str = "aetherium/icons/pencil.svg";
/// Header: delete the selected profile.
pub const ICON_TRASH: &str = "aetherium/icons/trash.svg";
/// Header: local echo is on.
pub const ICON_EYE: &str = "aetherium/icons/eye.svg";
/// Header: local echo is off.
pub const ICON_EYE_OFF: &str = "aetherium/icons/eye_off.svg";
/// Header: marks the theme button as a dropdown.
pub const ICON_CHEVRON_DOWN: &str = "aetherium/icons/chevron_down.svg";
/// Status bar: connected.
pub const ICON_SIGNAL_HIGH: &str = "aetherium/icons/signal_high.svg";
/// Status bar: connecting.
pub const ICON_SIGNAL_MEDIUM: &str = "aetherium/icons/signal_medium.svg";
/// Status bar: disconnected.
pub const ICON_DISCONNECTED: &str = "aetherium/icons/disconnected.svg";
/// File tree: closed directory.
pub const ICON_FOLDER: &str = "aetherium/icons/folder.svg";
/// File tree: expanded directory.
pub const ICON_FOLDER_OPEN: &str = "aetherium/icons/folder_open.svg";
/// Connect button: log into the selected profile.
pub const ICON_ARROW_RIGHT: &str = "aetherium/icons/arrow_right.svg";
/// Log toolbar: previous match / bookmark.
pub const ICON_ARROW_UP: &str = "aetherium/icons/arrow_up.svg";
/// Log toolbar: next match / bookmark.
pub const ICON_ARROW_DOWN: &str = "aetherium/icons/arrow_down.svg";
/// Log toolbar: follow is paused (click to resume).
pub const ICON_DEBUG_PAUSE: &str = "aetherium/icons/debug_pause.svg";
/// Log toolbar: following the live edge.
pub const ICON_PLAY_FILLED: &str = "aetherium/icons/play_filled.svg";
/// Log toolbar: bookmark toggle/navigation.
pub const ICON_BOOKMARK: &str = "aetherium/icons/bookmark.svg";
/// Log toolbar: search input.
pub const ICON_SEARCH: &str = "aetherium/icons/tool_search.svg";
/// Log toolbar: filter input.
pub const ICON_FILTER: &str = "aetherium/icons/filter_funnel.svg";
/// Sessions list: the profile has a connected session.
pub const ICON_LOCK: &str = "aetherium/icons/lock.svg";
/// Sessions list: no connected session for the profile.
pub const ICON_LOCK_OFF: &str = "aetherium/icons/lock_off.svg";

const BUILD_TOOL_PATH: &str = "aetherium/icons/build_tool.svg";
const UPDATE_MANAGER_PATH: &str = "aetherium/icons/update_manager.svg";

pub struct EmbeddedAssets;

// Lilex (https://github.com/mishamyrt/Lilex), SIL OFL 1.1 — see
// `src/fonts/OFL.txt`. Bundled so the terminal always has a true monospace
// available: system font enumeration on macOS does not reliably expose one
// to gpui's matcher (SF Mono is hidden; a missing "Zed Mono" silently falls
// back to a proportional font and breaks the whole cell grid).
const LILEX_REGULAR: &[u8] = include_bytes!("fonts/Lilex-Regular.ttf");
const LILEX_BOLD: &[u8] = include_bytes!("fonts/Lilex-Bold.ttf");
const LILEX_ITALIC: &[u8] = include_bytes!("fonts/Lilex-Italic.ttf");
const LILEX_BOLD_ITALIC: &[u8] = include_bytes!("fonts/Lilex-BoldItalic.ttf");

/// Register the bundled Lilex family with the text system. Must run before
/// any code resolves `theme::FONT_MONO`.
// [impl->req~bundled-monospace-font~1]
pub fn load_bundled_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(LILEX_REGULAR),
        Cow::Borrowed(LILEX_BOLD),
        Cow::Borrowed(LILEX_ITALIC),
        Cow::Borrowed(LILEX_BOLD_ITALIC),
    ];
    if let Err(err) = cx.text_system().add_fonts(fonts) {
        eprintln!("aetherium: loading bundled Lilex font: {err:#}");
    }
}

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
            ICON_PLUS => Some(PLUS.as_bytes()),
            ICON_PENCIL => Some(PENCIL.as_bytes()),
            ICON_TRASH => Some(TRASH.as_bytes()),
            ICON_EYE => Some(EYE.as_bytes()),
            ICON_EYE_OFF => Some(EYE_OFF.as_bytes()),
            ICON_CHEVRON_DOWN => Some(CHEVRON_DOWN.as_bytes()),
            ICON_SIGNAL_HIGH => Some(SIGNAL_HIGH.as_bytes()),
            ICON_SIGNAL_MEDIUM => Some(SIGNAL_MEDIUM.as_bytes()),
            ICON_DISCONNECTED => Some(DISCONNECTED.as_bytes()),
            ICON_FOLDER => Some(FOLDER.as_bytes()),
            ICON_FOLDER_OPEN => Some(FOLDER_OPEN.as_bytes()),
            ICON_ARROW_RIGHT => Some(ARROW_RIGHT.as_bytes()),
            ICON_ARROW_UP => Some(ARROW_UP.as_bytes()),
            ICON_ARROW_DOWN => Some(ARROW_DOWN.as_bytes()),
            ICON_DEBUG_PAUSE => Some(DEBUG_PAUSE.as_bytes()),
            ICON_PLAY_FILLED => Some(PLAY_FILLED.as_bytes()),
            ICON_BOOKMARK => Some(BOOKMARK.as_bytes()),
            ICON_SEARCH => Some(TOOL_SEARCH.as_bytes()),
            ICON_FILTER => Some(FILTER_FUNNEL.as_bytes()),
            ICON_LOCK => Some(LOCK.as_bytes()),
            ICON_LOCK_OFF => Some(LOCK_OFF.as_bytes()),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
