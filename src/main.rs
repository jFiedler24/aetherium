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
///
/// All colors are transcribed from Zed's built-in default dark theme
/// (`fallback_themes.rs` in zed's `theme` crate, MIT-licensed).
pub(crate) mod theme {
    use gpui::{Hsla, hsla};

    /// UI font family; matches Zed's default. Falls back to the system UI
    /// font when Zed's fonts are not installed — drop the OFL-licensed
    /// `Zed Sans`/`Zed Mono` files into `~/.config/aetherium/fonts/` to make
    /// the app use them without a system-wide install.
    pub const FONT_UI: &str = "Zed Sans";
    /// Monospace font family for the terminal; matches Zed's default.
    pub const FONT_MONO: &str = "Zed Mono";

    /// Window / terminal background (`background`).
    pub fn bg() -> Hsla {
        hsla(215. / 360., 0.12, 0.15, 1.)
    }
    /// Panels (header, sidebar, status bar) — `panel_background`.
    pub fn panel() -> Hsla {
        hsla(215. / 360., 0.12, 0.15, 1.)
    }
    /// Borders between panels (`border_variant`).
    pub fn border() -> Hsla {
        hsla(228. / 360., 0.08, 0.25, 1.)
    }
    /// Primary text (`text`).
    pub fn text() -> Hsla {
        hsla(221. / 360., 0.11, 0.86, 1.)
    }
    /// Secondary text (`text_muted`).
    pub fn text_dim() -> Hsla {
        hsla(218. / 360., 0.07, 0.46, 1.)
    }
    /// Accent blue (`icon_accent`).
    pub fn accent() -> Hsla {
        hsla(207.8 / 360., 0.81, 0.66, 1.)
    }
    /// Selected list-row background (`element_selected`).
    pub fn selection() -> Hsla {
        hsla(224. / 360., 0.113, 0.261, 1.)
    }
    /// Hover highlight (`element_hover`).
    pub fn hover() -> Hsla {
        hsla(225. / 360., 0.118, 0.267, 1.)
    }
    /// Drop-target highlight while dragging over the file tree
    /// (`drop_target_background`).
    pub fn drop_target() -> Hsla {
        hsla(220. / 360., 0.083, 0.214, 1.)
    }
    /// Buttons (`element_background`).
    pub fn button() -> Hsla {
        hsla(223. / 360., 0.13, 0.21, 1.)
    }
    pub fn button_hover() -> Hsla {
        hsla(225. / 360., 0.118, 0.267, 1.)
    }
    /// Status colors (the accent hues of the default dark theme).
    pub fn success() -> Hsla {
        hsla(95. / 360., 0.38, 0.62, 1.)
    }
    pub fn warning() -> Hsla {
        hsla(39. / 360., 0.67, 0.69, 1.)
    }
    pub fn danger() -> Hsla {
        hsla(355. / 360., 0.65, 0.65, 1.)
    }
}

use gpui::{
    App, Bounds, KeyBinding, WindowBounds, WindowOptions, point, prelude::*, px, size,
};

use crate::ui::RootView;

fn main() {
    gpui_platform::application()
        .with_assets(assets::EmbeddedAssets)
        .run(|cx: &mut App| {
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
