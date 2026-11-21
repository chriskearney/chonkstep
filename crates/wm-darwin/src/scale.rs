//! The display scale: one number for the whole desktop.
//!
//! The scene, the window manager and the renderer work in the panel's own
//! pixels at every scale; nothing is ever drawn small and stretched. The
//! scale decides how large things are *made*: the shell's theme and fonts
//! are rasterised at it (`chonk-shell`, `wm-theme`), the pointer arrow is
//! drawn at it, and clients are told it (`wl_output.scale`,
//! `wl_surface.preferred_buffer_scale`, `wp_fractional_scale_v1`) so they
//! render their buffers at the panel's pixel density. A surface is measured
//! in logical units, `scale` panel pixels each.
//!
//! The scale is kept in 120ths, the unit of `wp_fractional_scale_v1`, so the
//! steps are exact and both sides round the same way.

use wm_core::Size;

/// The steps on offer, in 120ths: 100% to 300%. Quarter steps up to 200%,
/// where the eye still tells them apart; halves beyond.
pub const STEPS: [u32; 7] = [120, 150, 180, 210, 240, 300, 360];

/// `CHONKSTEP_SCALE`, as on every other Chonkstep session: `2`, `1.5`, `150%`.
pub const SCALE_ENV: &str = "CHONKSTEP_SCALE";

/// A display scale, one of [`STEPS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scale(u32);

impl Scale {
    pub const ONE: Scale = Scale(120);

    /// The step with exactly this many 120ths.
    pub fn from_120(value: u32) -> Option<Self> {
        STEPS.contains(&value).then_some(Self(value))
    }
    /// The step nearest to a factor (ties go up).
    pub fn nearest(factor: f64) -> Self {
        let wanted = if factor.is_finite() { factor * 120.0 } else { 120.0 };
        let mut best = STEPS[0];
        for step in STEPS {
            if (f64::from(step) - wanted).abs() <= (f64::from(best) - wanted).abs() {
                best = step;
            }
        }
        Self(best)
    }
    /// `2`, `1.5`, `1.25`, `150%`: a step, exactly. Anything else is an error
    /// naming the steps, not a silent rounding.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (number, percent) = match text.strip_suffix('%') {
            Some(number) => (number.trim(), true),
            None => (text, false),
        };
        let value: f64 = number.parse().map_err(|_| format!("{text:?} is not a scale"))?;
        let v120 = if percent { value * 1.2 } else { value * 120.0 };
        let rounded = v120.round();
        if (v120 - rounded).abs() < 1e-6 && rounded >= 0.0 {
            if let Some(scale) = Self::from_120(rounded as u32) {
                return Ok(scale);
            }
        }
        Err(format!("scale {text:?} is not one of {}", Self::step_labels().join(", ")))
    }
    pub fn step_labels() -> Vec<String> {
        STEPS.iter().map(|s| Scale(*s).label()).collect()
    }
    /// The scale for a panel nobody chose one for: the step that makes the
    /// desktop about 1280 logical units wide, never under 100%. The J293's
    /// 2560x1600 panel (227 dpi) gets 200%, the classic Retina layout.
    pub fn automatic(panel: Size) -> Self {
        Self::nearest((f64::from(panel.w) / 1280.0).max(1.0))
    }
    pub fn v120(self) -> u32 {
        self.0
    }
    pub fn factor(self) -> f64 {
        f64::from(self.0) / 120.0
    }
    /// What `wl_output.scale` and `preferred_buffer_scale` say: the next
    /// whole number, so a client that knows only integers renders at least
    /// as many pixels as are shown.
    pub fn integer(self) -> i32 {
        self.0.div_ceil(120) as i32
    }
    /// "200%".
    pub fn label(self) -> String {
        format!("{}%", (self.0 * 100 + 60) / 120)
    }
    /// Logical units to panel pixels, halves away from zero: the rounding
    /// `wp_fractional_scale_v1` prescribes for buffer sizes.
    pub fn to_physical(self, logical: i32) -> i32 {
        let scaled = i64::from(logical) * i64::from(self.0);
        (if scaled >= 0 { (scaled + 60) / 120 } else { (scaled - 60) / 120 }) as i32
    }
    /// Panel pixels to the nearest logical unit (at least 1 for a positive length).
    pub fn to_logical(self, physical: i32) -> i32 {
        let scaled = i64::from(physical) * 120;
        let half = i64::from(self.0) / 2;
        let value = if scaled >= 0 { (scaled + half) / i64::from(self.0) } else { (scaled - half) / i64::from(self.0) };
        if physical > 0 { value.max(1) as i32 } else { value as i32 }
    }
    pub fn size_to_physical(self, logical: Size) -> Size {
        Size::new(self.to_physical(logical.w as i32).max(1) as u32, self.to_physical(logical.h as i32).max(1) as u32)
    }
    pub fn size_to_logical(self, physical: Size) -> Size {
        Size::new(self.to_logical(physical.w as i32).max(1) as u32, self.to_logical(physical.h as i32).max(1) as u32)
    }
}

/// How a committed client buffer becomes the window's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    /// The surface's size in logical units.
    pub logical: Size,
    /// The window's size on the panel.
    pub physical: Size,
    /// The buffer is already `physical` pixels: it is shown as it is.
    /// Otherwise it has to be resampled to `physical`.
    pub exact: bool,
}

/// The size of a surface and whether its buffer can be shown pixel for
/// pixel. `buffer` is the buffer's size, `buffer_scale` what the client
/// declared with `set_buffer_scale`, `destination` its `wp_viewport`
/// destination if it set one.
///
/// A client that renders at the output's density sends a buffer of
/// `logical x scale` pixels (give or take the pixel a cell grid or a
/// rounding costs): that buffer goes to the panel untouched. Any other
/// buffer (a client that ignores the scale, or knows only whole scales on a
/// fractional output) is resampled to the size its surface has on the panel.
pub fn fit(scale: Scale, buffer: Size, buffer_scale: i32, destination: Option<Size>) -> Fit {
    let per = buffer_scale.max(1) as u32;
    let logical = destination.unwrap_or_else(|| Size::new(buffer.w.div_ceil(per), buffer.h.div_ceil(per)));
    let wanted = scale.size_to_physical(logical);
    let exact = buffer.w.abs_diff(wanted.w) <= 1 && buffer.h.abs_diff(wanted.h) <= 1;
    Fit { logical, physical: if exact { buffer } else { wanted }, exact }
}

/// Where the choice comes from, strongest first: the environment, what the
/// menu last persisted, the panel.
pub fn resolve(env: Option<&str>, persisted: Option<u32>, panel: Size) -> (Scale, &'static str) {
    if let Some(text) = env.filter(|t| !t.trim().is_empty()) {
        match Scale::parse(text) {
            Ok(scale) => return (scale, "environment"),
            Err(error) => eprintln!("chonkstep-darwin: {SCALE_ENV}: {error}; ignored"),
        }
    }
    if let Some(scale) = persisted.and_then(Scale::from_120) {
        return (scale, "persisted");
    }
    (Scale::automatic(panel), "automatic")
}

/// The pointer arrow at a scale: the classic 12x18 arrow, drawn (not
/// stretched) at the panel's density. The hotspot is its tip, (0, 0), at
/// every scale.
pub fn arrow(scale: Scale) -> wm_theme_api::DecorationBuffer {
    use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
    let s = scale.factor() as f32;
    let (width, height) = (scale.to_physical(12).max(1) as u32, scale.to_physical(18).max(1) as u32);
    let mut pixmap = Pixmap::new(width, height).expect("arrow dimensions are small and non-zero");
    // The outline of the old pixel arrow: down the left edge, along the
    // bottom, back up the diagonal.
    let mut path = PathBuilder::new();
    path.move_to(0.5, 0.5);
    path.line_to(0.5, 15.5);
    path.line_to(10.5, 15.5);
    path.close();
    let path = path.finish().expect("a triangle is a path");
    let transform = Transform::from_scale(s, s);
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    paint.set_color_rgba8(255, 255, 255, 255);
    pixmap.fill_path(&path, &paint, FillRule::Winding, transform, None);
    paint.set_color_rgba8(0, 0, 0, 255);
    let stroke = Stroke { width: 1.0, ..Stroke::default() };
    pixmap.stroke_path(&path, &paint, &stroke, transform, None);
    wm_theme_api::DecorationBuffer { width, height, pixels: pixmap.take() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_steps_parse_exactly_and_nothing_else_does() {
        for (text, v120) in [("1", 120), ("1.25", 150), ("1.5", 180), ("1.75", 210), ("2", 240), ("2.5", 300), ("3", 360), ("200%", 240), (" 150% ", 180), ("2.0", 240)] {
            assert_eq!(Scale::parse(text).map(Scale::v120), Ok(v120), "{text}");
        }
        for text in ["", "0", "1.1", "4", "two", "-2", "2x", "175", "nan", "inf"] {
            assert!(Scale::parse(text).is_err(), "{text}");
        }
        assert_eq!(Scale::step_labels(), ["100%", "125%", "150%", "175%", "200%", "250%", "300%"]);
        assert_eq!(STEPS.map(|s| Scale::from_120(s).unwrap().integer()), [1, 2, 2, 2, 2, 3, 3]);
        assert_eq!(Scale::from_120(200), None);
    }

    #[test]
    fn the_macbook_panel_gets_two_and_small_outputs_get_one() {
        assert_eq!(Scale::automatic(Size::new(2560, 1600)), Scale(240));
        assert_eq!(Scale::automatic(Size::new(640, 480)), Scale::ONE);
        assert_eq!(Scale::automatic(Size::new(1280, 800)), Scale::ONE);
        assert_eq!(Scale::automatic(Size::new(1920, 1080)), Scale(180));
        assert_eq!(Scale::automatic(Size::new(3840, 2160)), Scale(360));
        assert_eq!(Scale::automatic(Size::new(7680, 4320)), Scale(360));
    }

    #[test]
    fn the_environment_beats_the_menu_which_beats_the_panel() {
        let panel = Size::new(2560, 1600);
        assert_eq!(resolve(Some("1.5"), Some(300), panel), (Scale(180), "environment"));
        assert_eq!(resolve(None, Some(300), panel), (Scale(300), "persisted"));
        assert_eq!(resolve(Some(""), None, panel), (Scale(240), "automatic"));
        // A bad value is reported and ignored, never guessed at.
        assert_eq!(resolve(Some("huge"), Some(150), panel), (Scale(150), "persisted"));
        assert_eq!(resolve(None, Some(200), panel), (Scale(240), "automatic"));
    }

    #[test]
    fn lengths_round_as_the_fractional_scale_protocol_says() {
        // Halves away from zero.
        assert_eq!(Scale(180).to_physical(1), 2);
        assert_eq!(Scale(180).to_physical(3), 5);
        assert_eq!(Scale(150).to_physical(2), 3);
        assert_eq!(Scale(150).to_physical(-2), -3);
        assert_eq!(Scale(240).to_physical(640), 1280);
        for step in STEPS {
            let scale = Scale(step);
            for logical in 1..4000 {
                let physical = scale.to_physical(logical);
                // There and back again loses nothing: a window keeps its
                // logical size through any number of scale changes.
                assert_eq!(scale.to_logical(physical), logical, "{step} {logical}");
                assert!((f64::from(physical) - f64::from(logical) * scale.factor()).abs() <= 0.5);
            }
        }
        assert_eq!(Scale(360).to_logical(1), 1);
        assert_eq!(Scale(360).to_logical(0), 0);
    }

    #[test]
    fn a_buffer_at_the_panels_density_is_shown_untouched() {
        for step in STEPS {
            let scale = Scale(step);
            // The terminal's rule: logical = ceil(grid / scale), buffer = round(logical x scale).
            for grid in [(640u32, 384u32), (1281, 769), (957, 1203), (9, 17)] {
                let logical = Size::new((f64::from(grid.0) / scale.factor()).ceil() as u32, (f64::from(grid.1) / scale.factor()).ceil() as u32);
                let buffer = scale.size_to_physical(logical);
                assert!(buffer.w >= grid.0 && buffer.h >= grid.1);
                assert_eq!(fit(scale, buffer, 1, Some(logical)), Fit { logical, physical: buffer, exact: true }, "{step} {grid:?}");
            }
            // A whole-number client on a whole-number scale.
            if step % 120 == 0 {
                let n = scale.integer();
                let fitted = fit(scale, Size::new(300 * n as u32, 200 * n as u32), n, None);
                assert_eq!(fitted, Fit { logical: Size::new(300, 200), physical: Size::new(300 * n as u32, 200 * n as u32), exact: true });
            }
        }
    }

    #[test]
    fn a_buffer_at_another_density_is_resampled_to_the_surfaces_size() {
        // Ignores the scale altogether: 300x200 logical, shown at 600x400.
        assert_eq!(fit(Scale(240), Size::new(300, 200), 1, None), Fit { logical: Size::new(300, 200), physical: Size::new(600, 400), exact: false });
        // Knows whole scales only, on 150%: renders at 2, shown at 1.5.
        assert_eq!(fit(Scale(180), Size::new(600, 400), 2, None), Fit { logical: Size::new(300, 200), physical: Size::new(450, 300), exact: false });
        // At 100% nothing is ever resampled.
        assert!(fit(Scale::ONE, Size::new(301, 203), 1, None).exact);
        // A viewport that stretches its buffer.
        assert!(!fit(Scale(240), Size::new(100, 100), 1, Some(Size::new(300, 200))).exact);
    }

    #[test]
    fn the_arrow_is_drawn_at_the_scale_with_its_tip_at_the_origin() {
        let mut heights = Vec::new();
        for step in STEPS {
            let scale = Scale(step);
            let arrow = arrow(scale);
            assert_eq!((arrow.width, arrow.height), (scale.to_physical(12) as u32, scale.to_physical(18) as u32));
            let opaque = |x: u32, y: u32| arrow.pixels[((y * arrow.width + x) * 4 + 3) as usize] > 0;
            // The tip is the hotspot: something is drawn within a pixel of (0, 0)
            // and nothing in the far top-right corner.
            assert!(opaque(0, 0) || opaque(0, 1) || opaque(1, 1), "{step}: tip");
            assert!(!opaque(arrow.width - 1, 0), "{step}: top right is empty");
            let rows = (0..arrow.height).filter(|y| (0..arrow.width).any(|x| opaque(x, *y))).count() as f64;
            // 16 logical rows of ink, at the panel's density.
            assert!((rows - 16.0 * scale.factor()).abs() <= 1.5, "{step}: {rows} rows");
            heights.push(rows);
            // Drawn, not stretched: an edge is one or two pixels of blend wide
            // at every scale. Count the partly transparent pixels on the middle row.
            let y = arrow.height / 2;
            let soft = (0..arrow.width).filter(|x| { let a = arrow.pixels[((y * arrow.width + x) * 4 + 3) as usize]; a > 0 && a < 255 }).count();
            assert!(soft <= 4, "{step}: {soft} soft pixels on one row");
        }
        assert!(heights.windows(2).all(|w| w[0] < w[1]));
    }
}
