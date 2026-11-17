//! The Forest themes' window chrome as PNG files, for looking at:
//!   forest_preview <out-dir> [scale]
//! One file per theme: a column of windows (focused; focused with the pointer on the controls;
//! close pressed; without focus; a long title; a fixed-size window), each over the theme's desk colour.
use wm_theme::{FontState, RasterThemeEngine};
use wm_theme_api::{ButtonKind, ButtonRuntimeState, DecorationRequest, DecorationSurface, Size, ThemeEngine};

fn put(canvas: &mut tiny_skia::Pixmap, at: (i32, i32), surface: &DecorationSurface, content: [u8; 3], layout: &wm_theme_api::DecorationLayout, size: Size) {
    let mut paint = tiny_skia::Paint { anti_alias: false, ..Default::default() };
    paint.set_color_rgba8(content[0], content[1], content[2], 255);
    if let Some(r) = tiny_skia::Rect::from_xywh((at.0 + layout.client_offset.x) as f32, (at.1 + layout.client_offset.y) as f32, size.w as f32, size.h as f32) {
        canvas.fill_rect(r, &paint, tiny_skia::Transform::identity(), None);
    }
    for solid in &surface.solids {
        paint.set_color_rgba8(solid.rgb[0], solid.rgb[1], solid.rgb[2], 255);
        if let Some(r) = tiny_skia::Rect::from_xywh((at.0 + solid.rect.pos.x) as f32, (at.1 + solid.rect.pos.y) as f32, solid.rect.size.w as f32, solid.rect.size.h as f32) {
            canvas.fill_rect(r, &paint, tiny_skia::Transform::identity(), None);
        }
    }
    for part in &surface.parts {
        let Some(image) = tiny_skia::PixmapRef::from_bytes(&part.buffer.pixels, part.buffer.width, part.buffer.height) else { continue };
        canvas.draw_pixmap(at.0 + part.offset.x, at.1 + part.offset.y, image, &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("out dir"));
    let scale: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.5);
    std::fs::create_dir_all(&dir).unwrap();
    let fonts = FontState::new();
    for (id, _) in wm_theme::forest::CHOICES {
        let theme = wm_theme::default_theme::theme_by_id(id).unwrap();
        let chrome = theme.chrome.unwrap();
        let engine = RasterThemeEngine::with_fonts_at_scale(theme.scaled(scale), fonts.clone(), scale).with_style(wm_theme::DecorationStyle::Modern).unwrap();
        let button = |kind, hovered, pressed| ButtonRuntimeState { kind, hovered, pressed };
        let rest = vec![button(ButtonKind::Close, false, false), button(ButtonKind::Miniaturize, false, false), button(ButtonKind::Maximize, false, false)];
        let hover = vec![button(ButtonKind::Close, false, false), button(ButtonKind::Miniaturize, true, false), button(ButtonKind::Maximize, false, false)];
        let press = vec![button(ButtonKind::Close, true, true), button(ButtonKind::Miniaturize, false, false), button(ButtonKind::Maximize, false, false)];
        let cases: Vec<(&str, bool, Vec<ButtonRuntimeState>, bool)> = vec![
            ("Terminal \u{2014} zsh \u{2014} 120\u{00d7}30", true, rest.clone(), true),
            ("Activity Monitor", true, hover.clone(), true),
            ("Firefox", true, press, true),
            ("zsh", false, rest.clone(), true),
            ("Activity Monitor", false, hover, true),
            ("A very long title that does not fit into the bar of a window this narrow at all, however it is cut", true, rest.clone(), true),
            ("About This Machine", true, rest, false),
        ];
        let content = Size::new((520.0 * scale) as u32, (70.0 * scale) as u32);
        let step = (content.h as f32 + 62.0 * scale) as u32;
        let (w, h) = (content.w + (60.0 * scale) as u32, step * cases.len() as u32 + (20.0 * scale) as u32);
        let mut canvas = tiny_skia::Pixmap::new(w, h).unwrap();
        canvas.fill(tiny_skia::Color::from_rgba8(chrome.background.r, chrome.background.g, chrome.background.b, 255));
        for (n, (title, focused, buttons, resizable)) in cases.into_iter().enumerate() {
            let request = DecorationRequest { content_size: content, title: title.into(), focused, resizable, buttons };
            let layout = engine.layout(&request);
            let surface = engine.render_surface(&request, &layout);
            let body = if n % 2 == 0 { [chrome.panel.r, chrome.panel.g, chrome.panel.b] } else { [0x1e, 0x1e, 0x1e] };
            put(&mut canvas, ((24.0 * scale) as i32, (16.0 * scale) as i32 + (n as u32 * step) as i32), &surface, body, &layout, content);
        }
        // the canvas is opaque: premultiplied equals straight
        canvas.save_png(dir.join(format!("{id}-chrome.png"))).unwrap();
    }
}
