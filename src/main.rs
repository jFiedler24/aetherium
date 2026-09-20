//! aetherium — a MobaXterm-style SSH/SFTP client built on gpui (Zed's UI
//! framework). Entry point: opens the main window with [`ui::RootView`].
//!
//! The SVG icon set in `aetherium_icons_dark_v2/` is embedded via
//! [`assets::EmbeddedAssets`].

mod assets;
mod crypto;
mod log_highlight;
mod profiles;
mod recents;
mod session;
mod terminal_model;
mod text_field;
mod ui;

/// Zed-like dark theme constants.
pub(crate) mod theme {
    use gpui::{Hsla, rgb};

    /// Monospace font family; the platform font database falls back to the
    /// system mono font when Zed Plex Mono is not installed.
    pub const FONT_MONO: &str = "Zed Plex Mono";

    /// Window / terminal-area background.
    pub fn bg() -> Hsla {
        rgb(0x1e1e20).into()
    }
    /// Panels (header, sidebar, status bar).
    pub fn panel() -> Hsla {
        rgb(0x28292d).into()
    }
    /// Borders between panels.
    pub fn border() -> Hsla {
        rgb(0x3a3b40).into()
    }
    /// Primary text.
    pub fn text() -> Hsla {
        rgb(0xd7d8da).into()
    }
    /// Secondary text.
    pub fn text_dim() -> Hsla {
        rgb(0x8b8d94).into()
    }
    /// Muted blue accent (selection, cursor, brand).
    pub fn accent() -> Hsla {
        rgb(0x4876d6).into()
    }
    /// Selected list-row background.
    pub fn selection() -> Hsla {
        rgb(0x33415e).into()
    }
    /// Hover highlight.
    pub fn hover() -> Hsla {
        rgb(0x32343a).into()
    }
    /// Buttons.
    pub fn button() -> Hsla {
        rgb(0x3a3b40).into()
    }
    pub fn button_hover() -> Hsla {
        rgb(0x4a4b52).into()
    }
    /// Status colors.
    pub fn success() -> Hsla {
        rgb(0x6fbf73).into()
    }
    pub fn warning() -> Hsla {
        rgb(0xd8a657).into()
    }
}

use gpui::{
    App, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, point, prelude::*, px, size,
};

use crate::ui::RootView;

fn main() {
    Application::new()
        .with_assets(assets::EmbeddedAssets)
        .run(|cx: &mut App| {
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
