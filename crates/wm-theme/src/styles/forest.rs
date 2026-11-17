//! The title band of the Forest themes: the modern recipe's band with the
//! window tokens of [`crate::modern::WindowChrome`]: controls drawn as shapes (discs, vias,
//! pads, bond pads) at the left in a Mac's order, the title on a fixed line, centred or after
//! the controls, a top edge, and a trace. Drawn once per title, focus and control state and
//! kept by the recipe's cache (`styles::modern::render`); nothing here runs per frame.
use crate::model::{Color, TextAlign, Theme};
use crate::modern::{Chrome, Controls, TopLine};
use crate::paint;
use tiny_skia::{FillRule, GradientStop, LineCap, LinearGradient, Paint, PathBuilder, Pixmap, Point as SkPoint, SpreadMode, Stroke, Transform};
use wm_theme_api::{ButtonKind, DecorationLayout, DecorationRequest};

fn solid(color: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(color.r, color.g, color.b, 255);
    p.anti_alias = true;
    p
}

/// A band `w` wide from `top` down past the image's bottom, its two upper corners rounded.
fn band(left: f32, top: f32, right: f32, bottom: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.max(0.0).min((right - left) / 2.0);
    let k = r * 0.552_284_8;
    let mut b = PathBuilder::new();
    b.move_to(left, bottom);
    b.line_to(left, top + r);
    if r > 0.0 {
        b.cubic_to(left, top + r - k, left + r - k, top, left + r, top);
    }
    b.line_to(right - r, top);
    if r > 0.0 {
        b.cubic_to(right - r + k, top, right, top + r - k, right, top + r);
    }
    b.line_to(right, bottom);
    b.close();
    b.finish()
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.max(0.0).min(w / 2.0).min(h / 2.0);
    if r <= 0.0 {
        return tiny_skia::Rect::from_xywh(x, y, w, h).map(PathBuilder::from_rect);
    }
    let k = r * 0.552_284_8;
    let (x1, y1) = (x + w, y + h);
    let mut b = PathBuilder::new();
    b.move_to(x + r, y);
    b.line_to(x1 - r, y);
    b.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    b.line_to(x1, y1 - r);
    b.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    b.line_to(x + r, y1);
    b.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    b.line_to(x, y + r);
    b.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    b.close();
    b.finish()
}

fn fill(image: &mut Pixmap, path: Option<tiny_skia::Path>, paint: &Paint) {
    if let Some(path) = path {
        image.fill_path(&path, paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn stroke(image: &mut Pixmap, path: Option<tiny_skia::Path>, paint: &Paint, width: f32) {
    if let Some(path) = path {
        let stroke = Stroke { width, line_cap: LineCap::Round, ..Stroke::default() };
        image.stroke_path(&path, paint, &stroke, Transform::identity(), None);
    }
}

fn line(x0: f32, y0: f32, x1: f32, y1: f32) -> Option<tiny_skia::Path> {
    let mut b = PathBuilder::new();
    b.move_to(x0, y0);
    b.line_to(x1, y1);
    b.finish()
}

/// One control in its box. `u` is device pixels per point; `(cx, cy)` its centre.
#[allow(clippy::too_many_arguments)]
fn control(image: &mut Pixmap, chrome: &Chrome, kind: ButtonKind, cx: f32, cy: f32, u: f32, bar: Color, focused: bool, group_hover: bool, pressed: bool) {
    let window = &chrome.window;
    let black = Color::rgb(0, 0, 0);
    let white = Color::rgb(255, 255, 255);
    let lit = group_hover || (focused && !window.neutral_at_rest);
    let hue = if lit { window.hue(kind) } else { window.idle };
    let hue = match (pressed, chrome.text.relative_luminance() > 0.5) {
        (true, true) => hue.mix(white, 0.18),
        (true, false) => hue.mix(black, 0.14),
        _ => hue,
    };
    // whole pixels for the outer size keep the three alike at every scale
    let (glyph_ink, filled) = match window.controls {
        Controls::Discs => {
            let r = (6.0 * u).round();
            if lit {
                fill(image, PathBuilder::from_circle(cx, cy, r), &solid(hue));
                let rim = (0.5 * u).max(1.0);
                stroke(image, PathBuilder::from_circle(cx, cy, r - rim / 2.0), &solid(hue.mix(black, 0.16)), rim);
            } else {
                let wall = (1.25 * u).max(1.0);
                stroke(image, PathBuilder::from_circle(cx, cy, r - wall / 2.0), &solid(hue), wall);
            }
            (hue.mix(black, 0.58), lit)
        }
        Controls::Vias => {
            let r = (6.0 * u).round();
            let wall = (1.75 * u).max(1.0);
            if group_hover {
                fill(image, PathBuilder::from_circle(cx, cy, r), &solid(hue));
            } else {
                stroke(image, PathBuilder::from_circle(cx, cy, r - wall / 2.0), &solid(hue), wall);
            }
            (bar, group_hover)
        }
        Controls::Pads => {
            let side = (11.0 * u).round();
            let (x, y) = ((cx - side / 2.0).round(), (cy - side / 2.0).round());
            if focused || group_hover {
                fill(image, rounded(x, y, side, side, 3.0 * u), &solid(hue));
            } else {
                let wall = (1.25 * u).max(1.0);
                stroke(image, rounded(x + wall / 2.0, y + wall / 2.0, side - wall, side - wall, 3.0 * u - wall / 2.0), &solid(hue), wall);
            }
            (white, group_hover)
        }
        _ => {
            let side = (12.0 * u).round();
            let (x, y) = ((cx - side / 2.0).round(), (cy - side / 2.0).round());
            if group_hover {
                fill(image, rounded(x, y, side, side, 1.5 * u), &solid(hue));
            } else {
                let wall = (1.5 * u).max(1.0);
                stroke(image, rounded(x + wall / 2.0, y + wall / 2.0, side - wall, side - wall, 1.5 * u - wall / 2.0), &solid(hue), wall);
                let core = (4.0 * u).round();
                fill(image, rounded((cx - core / 2.0).round(), (cy - core / 2.0).round(), core, core, 0.0), &solid(hue));
            }
            (bar, group_hover)
        }
    };
    if !(group_hover && filled) {
        return;
    }
    // the glyphs appear for all three while the pointer is over any of them
    let ink = solid(glyph_ink);
    let width = (1.3 * u).max(1.0);
    let a = 2.6 * u;
    match kind {
        ButtonKind::Close => {
            stroke(image, line(cx - a, cy - a, cx + a, cy + a), &ink, width);
            stroke(image, line(cx - a, cy + a, cx + a, cy - a), &ink, width);
        }
        ButtonKind::Miniaturize => stroke(image, line(cx - a * 1.2, cy, cx + a * 1.2, cy), &ink, width),
        _ => {
            stroke(image, line(cx - a * 1.2, cy, cx + a * 1.2, cy), &ink, width);
            stroke(image, line(cx, cy - a * 1.2, cx, cy + a * 1.2), &ink, width);
        }
    }
}

/// The title band, `w` by `h` device pixels, premultiplied, its upper corners cut to the radius.
pub(crate) fn title(
    theme: &Theme,
    fonts: &mut cosmic_text::FontSystem,
    cache: &mut cosmic_text::SwashCache,
    request: &DecorationRequest,
    layout: &DecorationLayout,
    w: u32,
    h: u32,
) -> Option<Pixmap> {
    let mut image = Pixmap::new(w.max(1), h.max(1))?;
    let chrome = Chrome::from_theme(theme);
    let window = chrome.window;
    let focused = request.focused;
    let b = f32::from(chrome.frame.border.max(1));
    let r = f32::from(chrome.frame.radius.min(chrome.frame.title_height));
    // device pixels per point: the control box is 16 points in the 12-point shapes' themes
    let u = (theme.titlebar.font.size / match window.controls {
        Controls::BondPads => 12.0,
        _ => 13.0,
    })
    .max(0.5);
    let (wf, hf) = (w as f32, h as f32 + r + 2.0);
    let bar = match if focused { &theme.titlebar.active } else { &theme.titlebar.inactive } {
        crate::model::Fill::Solid(c) => *c,
        crate::model::Fill::Gradient(g) => g.from,
    };
    let edge = if focused { theme.border.color_active } else { theme.border.color_inactive };
    // the outline: the band in the border's colour (or its top edge's), the bar inside it
    let mut top = b;
    match (window.top_line, focused) {
        (TopLine::Accent, true) => {
            let stops = vec![
                GradientStop::new(0.0, tiny_skia::Color::from_rgba8(chrome.accent.r, chrome.accent.g, chrome.accent.b, 255)),
                GradientStop::new(1.0, tiny_skia::Color::from_rgba8(edge.r, edge.g, edge.b, 255)),
            ];
            let mut p = solid(edge);
            if let Some(shader) = LinearGradient::new(SkPoint::from_xy(0.0, 0.0), SkPoint::from_xy(0.0, (r * 1.15).max(b * 6.0)), stops, SpreadMode::Pad, Transform::identity()) {
                p.shader = shader;
            }
            fill(&mut image, band(0.0, 0.0, wf, hf, r), &p);
        }
        (TopLine::Interference, _) => {
            // the film's colours along the top edge of every window: three points of them at full
            // strength on the focused one, two points paled toward the hairline on the others
            let pale = |c: u32| {
                let colour = Color::rgb((c >> 16) as u8, (c >> 8) as u8, c as u8);
                let colour = if focused { colour } else { colour.mix(edge, 0.62) };
                tiny_skia::Color::from_rgba8(colour.r, colour.g, colour.b, 255)
            };
            let stops = vec![GradientStop::new(0.0, pale(0x19a3a8)), GradientStop::new(0.55, pale(0x4a5bd0)), GradientStop::new(1.0, pale(0x9a5bd6))];
            let mut p = solid(edge);
            if let Some(shader) = LinearGradient::new(SkPoint::from_xy(0.0, 0.0), SkPoint::from_xy(wf, 0.0), stops, SpreadMode::Pad, Transform::identity()) {
                p.shader = shader;
            }
            fill(&mut image, band(0.0, 0.0, wf, hf, r), &p);
            top = ((if focused { 3.0 } else { 2.0 }) * u).round().max(2.0);
            fill(&mut image, band(0.0, top, wf, hf + top, r), &solid(edge));
            // the hairline under the colour would make three lines of two: the bar starts at the colour
        }
        _ => fill(&mut image, band(0.0, 0.0, wf, hf, r), &solid(edge)),
    }
    let inner = (r - b).max(0.0);
    if window.top_line == TopLine::Highlight {
        let white = Color::rgb(255, 255, 255);
        fill(&mut image, band(b, top, wf - b, hf, inner), &solid(bar.mix(white, if focused { 0.75 } else { 0.5 })));
        fill(&mut image, band(b, top + 1.0, wf - b, hf + 1.0, inner), &solid(bar));
    } else {
        fill(&mut image, band(b, top, wf - b, hf, inner), &solid(bar));
    }
    // the line between the bar and the window's content
    let rule = bar.mix(theme.border.color_inactive, 0.55);
    paint::fill_rect(&mut image, b as i32, h as i32 - 1, w.saturating_sub(2 * b as u32), 1, rule);

    let margin = layout.input_margin as i32;
    let cy = (h as f32 / 2.0).round();
    let group_hover = request.buttons.iter().any(|s| s.hovered || s.pressed);
    let mut controls_right = b as i32 + i32::from(chrome.frame.padding);
    let mut controls_left = w as i32;
    for (kind, hit) in &layout.button_hitboxes {
        let (x, y) = (hit.pos.x - margin, hit.pos.y - margin);
        let _ = y;
        let state = request.buttons.iter().find(|s| s.kind == *kind);
        let cx = x as f32 + hit.size.w as f32 / 2.0;
        control(&mut image, &chrome, *kind, cx, cy, u, bar, focused, group_hover, state.is_some_and(|s| s.pressed));
        controls_right = controls_right.max(x + hit.size.w as i32);
        controls_left = controls_left.min(x);
    }
    // the title
    let ink = if focused { theme.titlebar.text_color_active } else { theme.titlebar.text_color_inactive };
    let gap = (10.0 * u).round() as i32;
    let (text_x, text_w, align) = if window.centered {
        // centred over the window: the same room is kept free on the right as the controls take on the left
        let side = if window.left { controls_right + gap } else { (w as i32 - controls_left) + gap };
        (side, (w as i32 - 2 * side).max(0) as u32, TextAlign::Center)
    } else if window.left {
        let x = controls_right + gap;
        (x, (w as i32 - x - gap).max(0) as u32, TextAlign::Left)
    } else {
        let x = b as i32 + gap;
        (x, (controls_left - gap - x).max(0) as u32, TextAlign::Left)
    };
    let font = &theme.titlebar.font;
    let text = paint::fit_measured_prefix(&request.title, text_w, "\u{2026}", |t| paint::text_width(fonts, font, t));
    let (drawn_x, drawn_w) = paint::draw_title_text(&mut image, fonts, cache, &text, font, ink, text_x, 0, text_w, h, align);
    // the trace: from the title to a via before the right edge
    if window.trace {
        let via_r = (2.5 * u).round();
        let end = w as f32 - b - (9.0 * u).round() - via_r * 2.0;
        let start = (drawn_x + drawn_w as i32 + gap) as f32;
        if end - start >= 24.0 * u {
            let colour = if focused { chrome.accent.mix(bar, 0.35) } else { chrome.line };
            let t = u.round().max(1.0);
            paint::fill_rect(&mut image, start as i32, (cy - t / 2.0).round() as i32, (end - start) as u32, t as u32, colour);
            let wall = (1.25 * u).max(1.0);
            stroke(&mut image, PathBuilder::from_circle(end + via_r, cy, via_r - wall / 2.0 + 0.5), &solid(colour), wall);
        }
    }
    Some(image)
}
