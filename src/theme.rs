//! Runtime-switchable themes.
//!
//! The bundled JSON files in `src/themes/` are Zed's own standard theme
//! families (One, Ayu, Gruvbox — the set shipped with Zed today, along with
//! their licenses, see `src/themes/LICENSES`), in Zed's theme schema. They
//! are parsed into a flat [`ThemeData`] with one field per color the app
//! actually uses (window chrome, terminal foreground/background, and the
//! terminal's 16-color ANSI palette, which themes carry per theme).
//!
//! The active theme lives in a process-global so the existing `theme::…()`
//! call sites keep working unchanged; the header's theme button opens the
//! switcher. The choice persists to `<config>/aetherium/theme.toml` and is
//! restored at startup.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use gpui::{Hsla, hsla};
use serde::Deserialize;

/// UI font family; matches Zed's default. Falls back to the system UI
/// font when Zed's fonts are not installed — drop the OFL-licensed
/// `Zed Sans`/`Zed Mono` files into `~/.config/aetherium/fonts/` to make
/// the app use them without a system-wide install.
pub const FONT_UI: &str = "Zed Sans";
/// Monospace font family for the terminal. Must be a true monospace:
/// every cell metric (cursor position, selection, mouse mapping) derives
/// from one advance width. Bundled with the app (see `assets.rs`), so it
/// resolves everywhere — unlike "Zed Mono"/"SF Mono", which silently fell
/// back to a proportional font on stock macOS and broke the grid.
/// Users who install Zed Mono into the fonts dir can switch back.
pub const FONT_MONO: &str = "Lilex";

const THEME_FILES: &[&str] = &[
    include_str!("themes/one.json"),
    include_str!("themes/ayu.json"),
    include_str!("themes/gruvbox.json"),
];

const DEFAULT_THEME: &str = "One Dark";

// ── Zed theme JSON (the subset we consume) ─────────────────────────────

#[derive(Deserialize)]
struct ThemeFile {
    themes: Vec<ThemeEntry>,
}

#[derive(Deserialize)]
struct ThemeEntry {
    name: String,
    #[serde(default)]
    appearance: String,
    #[serde(default)]
    style: HashMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    Dark,
    Light,
}

/// Flat colors consumed by the UI chrome and the terminal renderer.
#[derive(Clone, Debug)]
pub struct ThemeData {
    pub bg: Hsla,
    pub panel: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    pub accent: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub hover: Hsla,
    pub drop_target: Hsla,
    pub button: Hsla,
    pub button_hover: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub term_bg: Hsla,
    pub term_fg: Hsla,
    pub ansi_normal: [Hsla; 8],
    pub ansi_bright: [Hsla; 8],
    pub ansi_dim: [Hsla; 8],
}

pub struct ThemeMeta {
    pub name: String,
    pub appearance: Appearance,
    data: ThemeData,
}

fn parse_hex(value: &str) -> Option<Hsla> {
    let hex = value.strip_prefix('#')?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    let (r, g, b, a) = match hex.len() {
        6 => ((v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff, 0xff),
        8 => ((v >> 24) & 0xff, (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff),
        _ => return None,
    };
    Some(
        gpui::Rgba {
            r: r as f32 / 255.,
            g: g as f32 / 255.,
            b: b as f32 / 255.,
            a: a as f32 / 255.,
        }
        .into(),
    )
}

fn style_color(style: &HashMap<String, serde_json::Value>, keys: &[&str]) -> Option<Hsla> {
    keys.iter()
        .find_map(|key| style.get(*key).and_then(|v| v.as_str()))
        .and_then(parse_hex)
}

fn player_color(
    style: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<Hsla> {
    style
        .get("players")
        .and_then(|players| players.as_array())
        .and_then(|players| players.first())
        .and_then(|player| player.get(key))
        .and_then(|value| value.as_str())
        .and_then(parse_hex)
}

fn ansi_set(
    style: &HashMap<String, serde_json::Value>,
    prefix: &str,
    fallback: [Hsla; 8],
) -> [Hsla; 8] {
    let names = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    let mut out = fallback;
    for (index, name) in names.iter().enumerate() {
        if let Some(color) = style_color(style, &[&format!("terminal.ansi.{prefix}{name}")]) {
            out[index] = color;
        }
    }
    out
}

impl ThemeData {
    /// The app's previous hardcoded colors, used as fallbacks for missing
    /// style keys (and as the last resort if a theme file fails to parse).
    fn fallback() -> Self {
        let rgba = |v: u32| -> Hsla {
            let [r, g, b, a] = v.to_be_bytes();
            gpui::Rgba {
                r: r as f32 / 255.,
                g: g as f32 / 255.,
                b: b as f32 / 255.,
                a: a as f32 / 255.,
            }
            .into()
        };
        Self {
            bg: hsla(215. / 360., 0.12, 0.15, 1.),
            panel: hsla(215. / 360., 0.12, 0.15, 1.),
            border: hsla(228. / 360., 0.08, 0.25, 1.),
            text: hsla(221. / 360., 0.11, 0.86, 1.),
            text_dim: hsla(218. / 360., 0.07, 0.46, 1.),
            accent: hsla(207.8 / 360., 0.81, 0.66, 1.),
            cursor: hsla(216. / 360., 0.71, 0.53, 1.),
            selection: hsla(224. / 360., 0.113, 0.261, 1.),
            hover: hsla(225. / 360., 0.118, 0.267, 1.),
            drop_target: hsla(220. / 360., 0.083, 0.214, 1.),
            button: hsla(223. / 360., 0.13, 0.21, 1.),
            button_hover: hsla(225. / 360., 0.118, 0.267, 1.),
            success: hsla(95. / 360., 0.38, 0.62, 1.),
            warning: hsla(39. / 360., 0.67, 0.69, 1.),
            danger: hsla(355. / 360., 0.65, 0.65, 1.),
            term_bg: rgba(0x22252bff),
            term_fg: rgba(0xfffffff2),
            ansi_normal: [
                rgba(0x000000f2),
                rgba(0xff9592ff),
                rgba(0x3dd68cff),
                rgba(0xf5e147ff),
                rgba(0x70b8ffff),
                rgba(0xbaa7ffff),
                rgba(0x4ccce6ff),
                rgba(0xeeeeecff),
            ],
            ansi_bright: [
                rgba(0x000000e6),
                rgba(0xec5d5eff),
                rgba(0x33b074ff),
                rgba(0xffff57ff),
                rgba(0x3b9effff),
                rgba(0x7d66d9ff),
                rgba(0x23afd0ff),
                rgba(0xb5b3adff),
            ],
            ansi_dim: [
                rgba(0x000000cc),
                rgba(0xe5484dff),
                rgba(0x30a46cff),
                rgba(0xffe629ff),
                rgba(0x0090ffff),
                rgba(0x6e56cfff),
                rgba(0x00a2c7ff),
                rgba(0x7c7b74ff),
            ],
        }
    }

    fn from_style(style: &HashMap<String, serde_json::Value>) -> Self {
        let fallback = Self::fallback();
        let or = |color: Option<Hsla>, default: Hsla| color.unwrap_or(default);
        let mut data = Self {
            bg: or(style_color(style, &["background"]), fallback.bg),
            panel: or(
                style_color(style, &["panel.background", "surface.background"]),
                fallback.panel,
            ),
            border: or(style_color(style, &["border"]), fallback.border),
            text: or(style_color(style, &["text"]), fallback.text),
            text_dim: or(
                style_color(style, &["text.muted", "text.placeholder"]),
                fallback.text_dim,
            ),
            accent: or(
                style_color(style, &["icon.accent", "text.accent", "info"]),
                fallback.accent,
            ),
            cursor: or(
                player_color(style, "cursor"),
                or(style_color(style, &["text.accent"]), fallback.cursor),
            ),
            selection: or(
                player_color(style, "selection"),
                or(
                    style_color(style, &["element.selected"]),
                    fallback.selection,
                ),
            ),
            hover: or(style_color(style, &["element.hover"]), fallback.hover),
            drop_target: or(
                style_color(style, &["drop_target.background"]),
                fallback.drop_target,
            ),
            button: or(
                style_color(style, &["element.background"]),
                fallback.button,
            ),
            button_hover: or(
                style_color(style, &["element.hover"]),
                fallback.button_hover,
            ),
            success: or(style_color(style, &["success"]), fallback.success),
            warning: or(style_color(style, &["warning"]), fallback.warning),
            danger: or(style_color(style, &["error"]), fallback.danger),
            term_bg: or(
                style_color(style, &["terminal.background", "editor.background"]),
                fallback.term_bg,
            ),
            term_fg: or(
                style_color(style, &["terminal.foreground", "editor.foreground"]),
                fallback.term_fg,
            ),
            ansi_normal: ansi_set(style, "", fallback.ansi_normal),
            ansi_bright: ansi_set(style, "bright_", fallback.ansi_bright),
            ansi_dim: ansi_set(style, "dim_", fallback.ansi_dim),
        };
        // A transparent cursor would be invisible; themes sometimes carry
        // low-alpha player colors meant for multi-player editors.
        data.cursor.a = data.cursor.a.max(0.5);
        data
    }
}

// ── Registry and active theme ──────────────────────────────────────────

static REGISTRY: OnceLock<Vec<ThemeMeta>> = OnceLock::new();
static ACTIVE: RwLock<Option<Arc<ThemeData>>> = RwLock::new(None);
static ACTIVE_NAME: RwLock<String> = RwLock::new(String::new());

fn registry() -> &'static Vec<ThemeMeta> {
    REGISTRY.get_or_init(|| {
        let mut out = Vec::new();
        for file in THEME_FILES {
            match serde_json::from_str::<ThemeFile>(file) {
                Ok(parsed) => {
                    for theme in parsed.themes {
                        let appearance = match theme.appearance.as_str() {
                            "light" => Appearance::Light,
                            _ => Appearance::Dark,
                        };
                        out.push(ThemeMeta {
                            name: theme.name,
                            appearance,
                            data: ThemeData::from_style(&theme.style),
                        });
                    }
                }
                Err(err) => eprintln!("aetherium: parsing bundled theme file: {err:#}"),
            }
        }
        if out.is_empty() {
            eprintln!("aetherium: no bundled themes parsed; using built-in colors");
            out.push(ThemeMeta {
                name: DEFAULT_THEME.to_string(),
                appearance: Appearance::Dark,
                data: ThemeData::fallback(),
            });
        }
        out
    })
}

/// Every bundled theme, in menu order (darks and lights interleaved as the
/// families define them).
pub fn list() -> Vec<(String, Appearance)> {
    registry()
        .iter()
        .map(|meta| (meta.name.clone(), meta.appearance))
        .collect()
}

pub fn active_name() -> String {
    let name = ACTIVE_NAME.read().unwrap().clone();
    if name.is_empty() {
        DEFAULT_THEME.to_string()
    } else {
        name
    }
}

fn active_data() -> Arc<ThemeData> {
    if let Some(data) = ACTIVE.read().unwrap().as_ref() {
        return data.clone();
    }
    let data = registry()
        .iter()
        .find(|meta| meta.name == DEFAULT_THEME)
        .or_else(|| registry().first())
        .map(|meta| Arc::new(meta.data.clone()))
        .expect("registry is never empty");
    *ACTIVE.write().unwrap() = Some(data.clone());
    data
}

/// Switch the active theme. Persists the choice; returns false when the
/// name is unknown.
pub fn set_active(name: &str) -> bool {
    let Some(meta) = registry().iter().find(|meta| meta.name == name) else {
        return false;
    };
    *ACTIVE.write().unwrap() = Some(Arc::new(meta.data.clone()));
    *ACTIVE_NAME.write().unwrap() = meta.name.clone();
    if let Err(err) = std::fs::write(
        crate::crypto::config_dir().join("theme.toml"),
        format!("theme = {:?}\n", meta.name),
    ) {
        eprintln!("aetherium: saving theme preference: {err:#}");
    }
    true
}

/// Parse the registry and restore the saved theme (defaulting to One Dark).
/// Call once at startup, before any rendering.
// [impl->req~bundled-zed-themes~1]
pub fn init() {
    let saved = std::fs::read_to_string(crate::crypto::config_dir().join("theme.toml"))
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| line.split_once('='))
                .map(|(_, value)| value.trim().trim_matches('"').to_string())
        })
        .filter(|name| !name.is_empty());
    let name = saved.unwrap_or_else(|| DEFAULT_THEME.to_string());
    if !set_active(&name) {
        set_active(DEFAULT_THEME);
    }
}

// ── Color accessors (unchanged signatures for the UI call sites) ───────

fn active() -> ThemeData {
    (*active_data()).clone()
}

/// Window background (`background`).
pub fn bg() -> Hsla {
    active().bg
}
/// Panels (header, sidebar, status bar) — `panel.background`.
pub fn panel() -> Hsla {
    active().panel
}
/// Borders between panels (`border`).
pub fn border() -> Hsla {
    active().border
}
/// Primary text (`text`).
pub fn text() -> Hsla {
    active().text
}
/// Secondary text (`text.muted`).
pub fn text_dim() -> Hsla {
    active().text_dim
}
/// Accent (`icon.accent`).
pub fn accent() -> Hsla {
    active().accent
}
/// Terminal/editor cursor (`players[0].cursor`).
pub fn cursor() -> Hsla {
    active().cursor
}
/// Selected list-row background (`players[0].selection`).
pub fn selection() -> Hsla {
    active().selection
}
/// Hover highlight (`element.hover`).
pub fn hover() -> Hsla {
    active().hover
}
/// Drop-target highlight while dragging over the file tree
/// (`drop_target.background`).
pub fn drop_target() -> Hsla {
    active().drop_target
}
/// Buttons (`element.background`).
pub fn button() -> Hsla {
    active().button
}
pub fn button_hover() -> Hsla {
    active().button_hover
}
/// Status colors.
pub fn success() -> Hsla {
    active().success
}
pub fn warning() -> Hsla {
    active().warning
}
pub fn danger() -> Hsla {
    active().danger
}
/// Terminal background (`terminal.background`).
pub fn term_bg() -> Hsla {
    active().term_bg
}
/// Terminal foreground (`terminal.foreground`).
pub fn term_fg() -> Hsla {
    active().term_fg
}
/// Dim terminal foreground (80% alpha).
pub fn term_fg_dim() -> Hsla {
    let mut color = active().term_fg;
    color.a *= 0.8;
    color
}
/// Bright terminal foreground (90% alpha).
pub fn term_fg_bright() -> Hsla {
    let mut color = active().term_fg;
    color.a *= 0.9;
    color
}
pub fn ansi_normal() -> [Hsla; 8] {
    active().ansi_normal
}
pub fn ansi_bright() -> [Hsla; 8] {
    active().ansi_bright
}
pub fn ansi_dim() -> [Hsla; 8] {
    active().ansi_dim
}
