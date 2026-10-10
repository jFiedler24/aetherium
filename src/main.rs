//! aetherium — a MobaXterm-style SSH/SFTP client built on gpui (Zed's UI
//! framework). Entry point: opens the main window with [`ui::RootView`].
//!
//! The SVG icon set in `aetherium_icons_dark_v2/` is embedded via
//! [`assets::EmbeddedAssets`].

// GUI-subsystem binary on Windows: no console window next to the UI. stderr
// is unavailable there, so the logger mirrors to a file (see below).
// [impl->feat~windows-parity~1]
// [impl->req~windows-gui-subsystem~1]
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod api;
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
#[cfg(windows)]
mod windows_drag;

use gpui::{
    App, Bounds, KeyBinding, WindowBounds, WindowOptions, point, prelude::*, px, size,
};

use crate::ui::{CancelDelete, ConfirmDelete, DeleteEntry, RenameEntry, RootView};

/// Minimal stderr logger so gpui's warnings (font fallback, shaping, asset
/// errors) are visible when running the binary directly. On Windows the
/// binary is a GUI app without a console, so there the lines are appended to
/// `<config dir>/aetherium.log` instead.
struct AppLogger;

impl log::Log for AppLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        let line = format!("[{}] {}", record.level(), record.args());
        #[cfg(windows)]
        {
            let path = crate::crypto::config_dir().join("aetherium.log");
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        eprintln!("{line}");
    }
    fn flush(&self) {}
}

static LOGGER: AppLogger = AppLogger;

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
        // Local REST API for AI/script control of the running UI. The advert
        // goes to the log (stderr on macOS/Linux, <config>/aetherium.log on
        // Windows) and the status bar.
        let (api_tx, api_rx) = std::sync::mpsc::channel();
        let api_info = api::start(api_tx);
        if let Some(info) = api_info.as_ref() {
            log::info!(
                "REST API listening on {} (bearer token: {})",
                api::url(info),
                info.token_path.display()
            );
        }
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
            move |_, cx| cx.new(|cx| RootView::new(cx, api_rx, api_info)),
        )
        .expect("open main window");
        cx.activate(true);
    });
}
