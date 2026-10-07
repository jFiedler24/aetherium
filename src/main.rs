//! aetherium — a MobaXterm-style SSH/SFTP client built on gpui (Zed's UI
//! framework). Entry point: opens the main window with [`ui::RootView`].
//!
//! The SVG icon set in `aetherium_icons_dark_v2/` is embedded via
//! [`assets::EmbeddedAssets`].

mod assets;
mod crypto;
mod history;
mod log_highlight;
mod profiles;
mod recents;
mod session;
mod terminal_model;
mod text_field;
mod theme;
mod ui;

use gpui::{
    App, Bounds, KeyBinding, WindowBounds, WindowOptions, point, prelude::*, px, size,
};

use crate::ui::{CancelDelete, ConfirmDelete, DeleteEntry, RenameEntry, RootView};

/// Minimal stderr logger so gpui's warnings (font fallback, shaping, asset
/// errors) are visible when running the binary directly.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }
    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

fn main() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
    // Bundled Zed themes + the saved preference, before any rendering.
    theme::init();
    gpui_platform::application()
        .with_assets(assets::EmbeddedAssets)
        .run(|cx: &mut App| {
        // Bundled Lilex guarantees a true monospace for the terminal; user
        // fonts may still override/extend afterwards.
        assets::load_bundled_fonts(cx);
        // Optional user fonts (e.g. Zed Sans/Mono) from the config dir.
        assets::load_user_fonts(cx);
        // Key bindings for the text fields (profile form).
        cx.bind_keys([
            KeyBinding::new("backspace", text_field::Backspace, Some("TextField")),
            KeyBinding::new("delete", text_field::Delete, Some("TextField")),
            KeyBinding::new("left", text_field::Left, Some("TextField")),
            KeyBinding::new("right", text_field::Right, Some("TextField")),
            KeyBinding::new("home", text_field::Home, Some("TextField")),
            KeyBinding::new("end", text_field::End, Some("TextField")),
            KeyBinding::new("ctrl-v", text_field::Paste, Some("TextField")),
            KeyBinding::new("cmd-v", text_field::Paste, Some("TextField")),
            KeyBinding::new("tab", text_field::Tab, Some("TextField")),
            KeyBinding::new("shift-tab", text_field::Backtab, Some("TextField")),
        ]);
        // File-tree shortcuts: Delete/Backspace ask to delete the selected
        // entry; while the confirmation dialog is up, Enter confirms and
        // Escape cancels. F2 renames inline.
        cx.bind_keys([
            KeyBinding::new("delete", DeleteEntry, Some("FileTree")),
            KeyBinding::new("backspace", DeleteEntry, Some("FileTree")),
            KeyBinding::new("enter", ConfirmDelete, Some("FileTree")),
            KeyBinding::new("escape", CancelDelete, Some("FileTree")),
            KeyBinding::new("f2", RenameEntry, Some("FileTree")),
        ]);

        let bounds = Bounds {
            origin: point(px(60.), px(60.)),
            size: size(px(1200.), px(760.)),
        };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.), px(480.))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("aetherium".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(RootView::new),
        )
        .expect("open main window");
        cx.activate(true);
    });
}
