//! The Forest themes: Clearing, Canopy, Wafer and Die Shot.
//! They wear the modern recipe (`styles::modern`) with the window tokens of
//! [`crate::modern::WindowChrome`]: controls at the left in a Mac's order, each theme its own
//! shape of control, shadow and motion. `docs/forest-themes.md` says what each back end draws
//! of them.
use crate::model::{Appearance, Color, Fill, FontStyle, FontWeight, TextAlign, Theme};
use crate::modern::{Chrome, Controls, InstrumentChrome, Shadow, TopLine, WindowChrome};
use wm_theme_api::{ButtonKind, FrameMetrics, OverviewMetrics};

pub const CHOICES: [(&str, &str); 4] = [
    ("clearing", "Clearing"),
    ("canopy", "Canopy"),
    ("wafer", "Wafer"),
    ("die-shot", "Die Shot"),
];

/// The ids these themes had on the desktop they were first drawn for. A saved theme, a saved
/// wallpaper or a configuration that names one of them means the theme beside it, so state
/// written there is read here. They are never written: every id this crate hands out is one
/// of [`CHOICES`].
pub const EARLIER_IDS: [(&str, &str); 4] = [
    ("sf-clearing", "clearing"),
    ("sf-canopy", "canopy"),
    ("sf-wafer", "wafer"),
    ("sf-dieshot", "die-shot"),
];

/// `id`, or the id of [`CHOICES`] that an earlier id stands for.
pub fn current_id(id: &str) -> &str {
    EARLIER_IDS.iter().find(|(earlier, _)| *earlier == id).map_or(id, |(_, current)| *current)
}

fn c(hex: u32) -> Color {
    Color::rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// background, panel, surface (title), raised (title without focus), text, muted, line, accent,
/// accent_text, selection, success, danger; then close, minimise, zoom, idle; then the shadow's
/// colour and its alpha with and without focus.
struct Palette {
    tokens: [u32; 12],
    controls: [u32; 4],
    shadow: (u32, u8, u8),
    light: bool,
}

const CLEARING: Palette = Palette {
    tokens: [0xdfe5dc, 0xfbfcf9, 0xe9ede5, 0xf1f3ee, 0x1f2a24, 0x56625a, 0xb9c3b8, 0x2c7a54, 0xffffff, 0xcfe6d8, 0x2f8a52, 0xb3402e],
    controls: [0xcf4a3a, 0xa87004, 0x2f8a52, 0x7c877d],
    shadow: (0x16241b, 102, 61),
    light: true,
};
const CANOPY: Palette = Palette {
    tokens: [0x0b1210, 0x111a16, 0x17221d, 0x121a16, 0xe6ede4, 0x9aa89d, 0x2e3d35, 0xdd8f55, 0x1a120b, 0x2b3d33, 0x82c98f, 0xec7c60],
    controls: [0xec7c60, 0xe2b65c, 0x82c98f, 0x5d6e64],
    shadow: (0x010403, 178, 115),
    light: false,
};
const WAFER: Palette = Palette {
    tokens: [0xe9edf2, 0xffffff, 0xf1f3f7, 0xf6f7f9, 0x1c2330, 0x566074, 0xc3cad6, 0x4a5bd0, 0xffffff, 0xdde2fa, 0x1f8a5b, 0xc43c4e],
    controls: [0xd2475a, 0xa87400, 0x1f8a5b, 0x737c8c],
    shadow: (0x141d38, 87, 51),
    light: true,
};
const DIESHOT: Palette = Palette {
    tokens: [0x07090c, 0x0e1116, 0x12161c, 0x0e1116, 0xe9eef2, 0x93a0ad, 0x2c3540, 0xe7c15a, 0x14110a, 0x26303a, 0x58d6a0, 0xff7a6b],
    controls: [0xff7a6b, 0xe7c15a, 0x58d6a0, 0x5b6876],
    shadow: (0x000000, 204, 128),
    light: false,
};

/// One of the four, in either appearance. Each is drawn for one (Clearing and Wafer light,
/// Canopy and Die Shot dark; `default_theme::native_appearance`); in the other it keeps its
/// shapes, type and motion and takes its sibling's palette (Clearing <-> Canopy, Wafer <-> Die
/// Shot).
pub fn theme(id: &str, appearance: Appearance) -> Option<Theme> {
    let id = current_id(id);
    let (name, _) = CHOICES.iter().find(|(known, _)| *known == id).map(|(_, name)| (*name, ()))?;
    let light = appearance == Appearance::Light;
    let palette = match (id, light) {
        ("clearing", true) | ("canopy", true) => &CLEARING,
        ("clearing", false) | ("canopy", false) => &CANOPY,
        ("wafer", true) | ("die-shot", true) => &WAFER,
        _ => &DIESHOT,
    };
    debug_assert_eq!(palette.light, light);
    let [background, panel, surface, raised, text, muted, line, accent, accent_text, selection, success, danger] = palette.tokens.map(c);
    let [close, minimise, zoom, idle] = palette.controls.map(c);
    // (title height, radius, family, weight, size, controls, button box, gap, padding)
    let (title, radius, family, weight, size, controls, button, gap, padding) = match id {
        "clearing" => (28, 10, "Inter", FontWeight::Medium, 13.0, Controls::Discs, 16, 4, 6),
        "canopy" => (30, 12, "Inter", FontWeight::SemiBold, 13.0, Controls::Vias, 16, 4, 7),
        "wafer" => (28, 7, "IBM Plex Sans", FontWeight::Medium, 13.0, Controls::Pads, 15, 4, 6),
        _ => (30, 3, "Source Code Pro", FontWeight::Medium, 12.0, Controls::BondPads, 16, 4, 7),
    };
    // (offset y, blur, appear ms, rise, curve, cross-fade ms)
    let (drop, blur, open_ms, open_rise, open_curve, switch_ms) = match id {
        "clearing" => (10, 40, 180, 8, 3, 220),
        "canopy" => (14, 48, 200, 10, 5, 260),
        "wafer" => (7, 28, 140, 4, 3, 200),
        _ => (10, 32, 120, 0, 10, 180),
    };
    let (shadow_rgb, shadow_focused, shadow_unfocused) = palette.shadow;
    let shadow_rgb = c(shadow_rgb);
    let window = WindowChrome {
        controls,
        left: true,
        centered: id != "die-shot",
        neutral_at_rest: id == "wafer",
        close,
        minimise,
        zoom,
        idle,
        top_line: match id {
            "clearing" => TopLine::Highlight,
            "canopy" => TopLine::Accent,
            "wafer" => TopLine::Interference,
            _ => TopLine::None,
        },
        trace: id == "die-shot",
        hairline: id != "canopy",
        effects: true,
        shadow_unfocused,
        open_ms,
        open_rise,
        open_curve,
        switch_ms,
    };
    let chrome = Chrome {
        instrument: InstrumentChrome { background: Some(panel), border: Some(line), accent_edge: 0 },
        window,
        shadow: Shadow { x: 0, y: drop, blur, color: Color { r: shadow_rgb.r, g: shadow_rgb.g, b: shadow_rgb.b, a: shadow_focused } },
        frame: FrameMetrics {
            title_height: title,
            border: 1,
            radius,
            input_margin: 6,
            padding,
            button_size: button,
            button_gap: gap,
            focus_edge: 0,
            round_focus_mark: false,
        },
        overview: OverviewMetrics { radius, ..OverviewMetrics::default() },
        background,
        panel,
        surface,
        raised,
        text,
        muted,
        line,
        accent,
        accent_text,
        selection,
        success,
        danger,
    };
    let mut theme = crate::default_theme::nextstep_classic();
    theme.id = id.into();
    theme.name = name.into();
    theme.appearance = appearance;
    theme.chrome = Some(chrome);
    // the artwork of the same name (chonk-shell `Wallpaper::Forest`)
    theme.wallpaper = id.into();
    theme.titlebar.height = title;
    theme.titlebar.font.family = family.into();
    theme.titlebar.font.size = size;
    theme.titlebar.font.weight = weight;
    theme.titlebar.font.style = FontStyle::Normal;
    theme.titlebar.text_align = if window.centered { TextAlign::Center } else { TextAlign::Left };
    theme.titlebar.active = Fill::Solid(surface);
    theme.titlebar.inactive = Fill::Solid(raised);
    theme.titlebar.text_color_active = text;
    theme.titlebar.text_color_inactive = muted;
    theme.titlebar.bevel.width = 0;
    theme.titlebar.button_margin = padding;
    let button_style = theme.titlebar.buttons[0].clone();
    theme.titlebar.buttons = [ButtonKind::Close, ButtonKind::Miniaturize, ButtonKind::Maximize]
        .map(|kind| crate::model::ButtonStyle { kind, size: button, ..button_style.clone() })
        .to_vec();
    theme.border.color_active = if id == "die-shot" { accent } else { line };
    theme.border.color_inactive = line;
    theme.resize_bar.height = 0;
    theme.resize_bar.bevel.width = 0;
    theme.tile.fill = Fill::Solid(raised);
    theme.tile.bevel.width = 0;
    let mut menu_font = theme.titlebar.font.clone();
    // menus in the titles' medium weight
    menu_font.weight = FontWeight::Medium;
    if id == "die-shot" {
        // the monospaced face is the title's alone; menus and the dock's names are in Inter
        menu_font.family = "Inter".into();
        menu_font.size = 13.0;
    }
    theme.menu.title_font = menu_font.clone();
    theme.menu.item_font = menu_font;
    theme.menu.title_bar = Fill::Solid(surface);
    theme.menu.title_text_color = muted;
    // Not pure white: dialogs draw their wells as paper lighter than the menu's ground
    // (chonk-btpair, chonk-netjoin), and on white there is nothing lighter. Wafer's panel is
    // white, so its menus take the unfocused title's tone, one step down.
    theme.menu.background = Fill::Solid(if panel == c(0xffffff) { raised } else { panel });
    theme.menu.text_color = text;
    theme.menu.highlight_background = Fill::Solid(if palette.light { accent } else { selection });
    theme.menu.highlight_text_color = if palette.light { accent_text } else { text };
    theme.menu.bevel.width = 0;
    theme.menu.item_height = 28;
    theme.terminal.bg = panel;
    theme.terminal.fg = text;
    theme.terminal.cursor = accent;
    let (blue, violet, teal) = if palette.light { (c(0x3b5fc4), c(0x7a4fb8), c(0x1c7f86)) } else { (c(0x7aa2f7), c(0xb9a3d9), c(0x81b6ac)) };
    theme.terminal.ansi = [
        if palette.light { text } else { surface },
        danger,
        success,
        minimise,
        blue,
        violet,
        teal,
        if palette.light { muted } else { text },
        muted,
        danger,
        success,
        accent,
        blue,
        violet,
        teal,
        text,
    ];
    theme.terminal.opacity = None;
    Some(theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_resolve_in_both_appearances_and_keep_their_shapes() {
        for (id, name) in CHOICES {
            let native = crate::default_theme::theme_by_id(id).expect("registered");
            assert_eq!(native.name, name);
            for appearance in [Appearance::Light, Appearance::Dark] {
                let t = theme(id, appearance).unwrap();
                assert_eq!(t.appearance, appearance);
                let (a, b) = (t.chrome.unwrap().window, native.chrome.unwrap().window);
                assert_eq!((a.controls, a.left, a.centered, a.open_ms), (b.controls, b.left, b.centered, b.open_ms));
                assert_eq!(t.titlebar.buttons.iter().map(|b| b.kind).collect::<Vec<_>>(), [ButtonKind::Close, ButtonKind::Miniaturize, ButtonKind::Maximize]);
                let decoded: Theme = toml::from_str(&toml::to_string(&t).unwrap()).unwrap();
                assert_eq!(decoded, t);
            }
        }
        assert!(theme("obsidian", Appearance::Dark).is_none());
    }

    /// State written under the earlier ids resolves to the same themes, under today's ids.
    #[test]
    fn earlier_ids_are_read_and_never_handed_out() {
        for (earlier, current) in EARLIER_IDS {
            assert!(CHOICES.iter().any(|(id, _)| *id == current));
            for appearance in [Appearance::Light, Appearance::Dark] {
                assert_eq!(theme(earlier, appearance), theme(current, appearance));
                assert_eq!(crate::default_theme::theme_variant(earlier, appearance).unwrap().id, current);
            }
            assert_eq!(crate::default_theme::theme_by_id(earlier), crate::default_theme::theme_by_id(current));
            assert_eq!(crate::default_theme::native_appearance(earlier), crate::default_theme::native_appearance(current));
        }
        assert!(crate::default_theme::CHOICES.iter().all(|(id, _)| current_id(id) == *id), "no menu row carries an earlier id");
        assert_eq!(current_id("obsidian"), "obsidian");
    }

    /// The brief's promises: text at 4.5:1 or better on its bar, controls at 3:1 or better.
    #[test]
    fn text_and_controls_keep_their_contrast() {
        for (id, _) in CHOICES {
            for appearance in [Appearance::Light, Appearance::Dark] {
                let t = theme(id, appearance).unwrap();
                let k = t.chrome.unwrap();
                assert!(k.text.contrast_ratio(k.surface) >= 4.5, "{id} title");
                assert!(k.muted.contrast_ratio(k.raised) >= 4.5, "{id} title without focus");
                assert!(k.text.contrast_ratio(k.panel) >= 4.5 && k.muted.contrast_ratio(k.panel) >= 4.5, "{id} menus");
                assert!(k.accent_text.contrast_ratio(k.accent) >= 4.5, "{id} accent text");
                for hue in [k.window.close, k.window.minimise, k.window.zoom] {
                    assert!(hue.contrast_ratio(k.surface) >= 3.0, "{id} control {hue:?}");
                }
                assert!(k.window.idle.contrast_ratio(k.raised) >= 3.0, "{id} idle controls");
            }
        }
    }
}
