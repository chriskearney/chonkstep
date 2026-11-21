//! Renderers: how a dirty scene becomes pixels on a [`Presenter`].
//!
//! [`Renderer`] is the one seam between the scene and the output. Two CPU
//! implementations live here; a GPU renderer would be a third beside them.
//!
//! * [`LegacyRenderer`]: the original path, kept selectable for A/B runs.
//!   Every dirty frame allocates and composites a full-screen pixmap and hands
//!   all of it to the presenter.
//! * [`DamageRenderer`]: keeps one persistent canvas, repaints only what the
//!   display list says changed, and tells the presenter which rectangles to
//!   convert and store.
use crate::damage::{diff, DRect, ItemSig, Region};
use crate::presenter::Presenter;
use crate::scene::{CpuBackend, DrawOp};
use std::time::{Duration, Instant};
use wm_core::{Backend, Point};
use wm_theme_api::DecorationBuffer;

/// Environment variable selecting the renderer: `gl`, `cpu` (alias `damage`),
/// `legacy`, or `auto` (the default: `gl` when it can be brought up, else `cpu`).
pub const RENDERER_ENV: &str = "CHONKSTEP_DARWIN_RENDERER";
/// Environment variable: print a `WMSTAT` line to stderr every N presented frames.
pub const STATS_ENV: &str = "CHONKSTEP_DARWIN_FRAME_STATS";
/// Allocations at least this large count as "large" in [`FrameStats`].
pub const LARGE_ALLOC_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererKind {
    Legacy,
    Damage,
    /// GLES on the GPU, presented by KMS page flip (`glrender`).
    Gl,
}

/// What was asked for, before it is known what the machine can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RendererRequest {
    /// GL when both DRM nodes and the GL libraries work, otherwise CPU.
    Auto,
    Exactly(RendererKind),
}
impl RendererRequest {
    pub fn parse(name: &str) -> Result<Self, String> {
        if name == "auto" { Ok(Self::Auto) } else { RendererKind::parse(name).map(Self::Exactly) }
    }
    /// Unset or empty means `Auto`. Anything else must parse.
    pub fn from_env() -> Result<Self, String> {
        match std::env::var(RENDERER_ENV) {
            Ok(name) if !name.is_empty() => Self::parse(&name),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(error) => Err(format!("{RENDERER_ENV}: {error}")),
        }
    }
    /// The CPU renderer this request means when no GPU output is involved.
    pub fn cpu_kind(self) -> RendererKind {
        match self {
            Self::Exactly(RendererKind::Legacy) => RendererKind::Legacy,
            _ => RendererKind::Damage,
        }
    }
}
impl RendererKind {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "legacy" => Ok(Self::Legacy),
            "damage" | "cpu" => Ok(Self::Damage),
            "gl" => Ok(Self::Gl),
            other => Err(format!("unknown renderer {other:?}; expected gl, cpu, damage or legacy")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Damage => "damage",
            Self::Gl => "gl",
        }
    }
    /// A CPU renderer. The GL renderer owns a GPU output and is built by
    /// `output::select`; asked for here, `Gl` yields the damage renderer.
    pub fn create_cpu(self) -> Box<dyn Renderer> {
        match self {
            Self::Legacy => Box::new(LegacyRenderer),
            Self::Damage | Self::Gl => Box::new(DamageRenderer::default()),
        }
    }
}

/// Cost of one rendered frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Display list, damage computation and compositing.
    pub composite_us: u64,
    /// RGBA to framebuffer format conversion.
    pub convert_us: u64,
    /// Stores into the framebuffer.
    pub present_us: u64,
    pub bytes_written: u64,
    /// Pixels repainted (sum over damage rectangles).
    pub damage_px: u64,
    pub damage_rects: u32,
    /// Bytes of large buffers allocated while producing this frame.
    pub large_alloc_bytes: u64,
}
impl FrameStats {
    pub fn total_us(&self) -> u64 {
        self.composite_us + self.convert_us + self.present_us
    }
    fn add_present(&mut self, presenter: &mut dyn Presenter, elapsed_us: u64) {
        match presenter.take_stats() {
            Some(p) => {
                self.convert_us = p.convert_us;
                self.present_us = p.present_us;
                self.bytes_written = p.bytes_written;
                self.large_alloc_bytes += p.large_alloc_bytes;
            }
            // A presenter that does not measure itself: all of it is "present".
            None => self.present_us = elapsed_us,
        }
    }
}

/// Running totals, cheap enough to keep always.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTotals {
    /// Flip-complete events taken (asynchronous renderers only).
    pub flips: u64,
    /// Flips that completed more than 25 ms after their commit: a missed refresh at 60 Hz.
    pub missed_refreshes: u64,
    pub frames: u64,
    /// Sums over every frame except the first.
    pub sum: FrameStats,
    pub max_total_us: u64,
    pub last: FrameStats,
    /// The first frame, kept apart: it carries one-time work (every texture
    /// uploaded, GPU pipelines compiled on first draw, and on KMS the
    /// blocking modeset commit) and would swamp an average.
    pub first: FrameStats,
    /// Largest composite and present time among the other frames.
    pub max_composite_us: u64,
    pub max_present_us: u64,
}
impl FrameTotals {
    pub fn record(&mut self, s: FrameStats) {
        self.frames += 1;
        self.last = s;
        if self.frames == 1 {
            self.first = s;
            return;
        }
        self.max_composite_us = self.max_composite_us.max(s.composite_us);
        self.max_present_us = self.max_present_us.max(s.present_us);
        self.sum.composite_us += s.composite_us;
        self.sum.convert_us += s.convert_us;
        self.sum.present_us += s.present_us;
        self.sum.bytes_written += s.bytes_written;
        self.sum.damage_px += s.damage_px;
        self.sum.damage_rects += s.damage_rects;
        self.sum.large_alloc_bytes += s.large_alloc_bytes;
        self.max_total_us = self.max_total_us.max(s.total_us());
    }
    pub fn record_presented(&mut self, presented: &Presented) {
        self.flips += 1;
        self.missed_refreshes += u64::from(presented.latency > Duration::from_millis(25));
    }
    /// One line for the frames since `since` (`seconds` of wall time).
    /// Averages are over steady-state frames only; when the very first frame
    /// falls in the window its cost is reported on its own. `present` is the
    /// time to hand the frame over (the commit call, or the framebuffer
    /// copy); waiting for the flip to complete is never included.
    pub fn summary(&self, since: &FrameTotals, renderer: RendererKind, output: &str, seconds: f64) -> String {
        let frames = self.frames - since.frames;
        let has_first = since.frames == 0 && self.frames > 0;
        let steady = frames - u64::from(has_first);
        let d = |a: u64, b: u64| (a - b) / steady.max(1);
        let first = if has_first {
            format!(
                " first_frame_composite_us={} first_frame_present_us={} first_frame_bytes={}",
                self.first.composite_us, self.first.present_us, self.first.bytes_written
            )
        } else {
            String::new()
        };
        format!(
            "WMSTAT renderer={} output={output} frames={frames} seconds={seconds:.2} fps={:.1} flips={} missed_refreshes={} steady_frames={steady} avg_composite_us={} avg_convert_us={} avg_present_us={} max_composite_us={} max_present_us={} avg_bytes={} avg_damage_px={} large_alloc_bytes={}{first}",
            renderer.name(),
            if seconds > 0.0 { frames as f64 / seconds } else { 0.0 },
            self.flips - since.flips,
            self.missed_refreshes - since.missed_refreshes,
            d(self.sum.composite_us, since.sum.composite_us),
            d(self.sum.convert_us, since.sum.convert_us),
            d(self.sum.present_us, since.sum.present_us),
            // Maxima are since start, not per window: they only ever grow.
            self.max_composite_us,
            self.max_present_us,
            d(self.sum.bytes_written, since.sum.bytes_written),
            d(self.sum.damage_px, since.sum.damage_px),
            self.sum.large_alloc_bytes - since.sum.large_alloc_bytes,
        )
    }
}

pub trait Renderer {
    fn kind(&self) -> RendererKind;
    /// Bring the presenter's output up to date with `scene`. On error nothing
    /// is considered shown: the caller keeps the scene dirty and the next call
    /// must deliver everything that is still outstanding.
    fn render_frame(
        &mut self,
        scene: &mut CpuBackend,
        presenter: &mut dyn Presenter,
    ) -> Result<FrameStats, String>;
    /// Forget what is on screen: the next frame repaints and presents all of it.
    fn invalidate(&mut self);
    /// A frame has been handed to the display and its flip has not completed.
    /// `render_frame` must not be called while this is true. Renderers that
    /// present synchronously are never busy.
    fn busy(&self) -> bool {
        false
    }
    /// Take the completion of the frame in flight, waiting up to `timeout`.
    /// This is the frame clock of an asynchronous renderer.
    fn poll_presented(&mut self, timeout: Duration) -> Result<Option<Presented>, String> {
        let _ = timeout;
        Ok(None)
    }
    /// Display sleep: power the output down, or up again. `Ok(false)` when
    /// this renderer's output cannot be powered (nothing changed). Never
    /// called while `busy`. After power-up the next frame is a full one.
    fn set_display_power(&mut self, on: bool) -> Result<bool, String> {
        let _ = on;
        Ok(false)
    }
    /// The frame most recently rendered, as premultiplied RGBA rows from the
    /// top. `None` when this renderer cannot read its output back.
    fn read_back(&mut self) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
    /// Short name of where frames go, for log lines.
    fn output_name(&self) -> &'static str {
        "presenter"
    }
    /// DRM formats and modifiers of client dma-bufs this renderer can show
    /// without a copy. Empty: dma-buf clients are not supported.
    fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        Vec::new()
    }
    /// Take a client dma-buf as a texture. The returned id names it in
    /// `DrawOp::External` until `release_dmabuf`. The fd is only borrowed for
    /// the call; the caller keeps the buffer alive while the id is in use.
    fn import_dmabuf(&mut self, buffer: &DmabufDesc) -> Result<u64, String> {
        let _ = buffer;
        Err("this renderer cannot import dma-bufs".into())
    }
    fn release_dmabuf(&mut self, id: u64) {
        let _ = id;
    }
}

/// One single-plane client dma-buf.
#[derive(Clone, Copy, Debug)]
pub struct DmabufDesc {
    pub fd: std::ffi::c_int,
    pub width: u32,
    pub height: u32,
    /// DRM fourcc.
    pub format: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
}

/// A frame reached the screen.
#[derive(Clone, Copy, Debug)]
pub struct Presented {
    pub sequence: u32,
    pub at: Instant,
    /// Time from handing the frame to the display until its flip completed.
    pub latency: Duration,
}

pub struct LegacyRenderer;
impl Renderer for LegacyRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Legacy
    }
    fn render_frame(
        &mut self,
        scene: &mut CpuBackend,
        presenter: &mut dyn Presenter,
    ) -> Result<FrameStats, String> {
        let start = Instant::now();
        let frame = scene.render()?;
        let composited = Instant::now();
        presenter.present(&frame)?;
        let size = scene.screen_size();
        let pixels = u64::from(size.w) * u64::from(size.h);
        let mut stats = FrameStats {
            composite_us: (composited - start).as_micros() as u64,
            damage_px: pixels,
            damage_rects: 1,
            large_alloc_bytes: pixels * 4,
            ..FrameStats::default()
        };
        stats.add_present(presenter, composited.elapsed().as_micros() as u64);
        scene.ack_damage();
        Ok(stats)
    }
    fn invalidate(&mut self) {}
}

#[derive(Default)]
pub struct DamageRenderer {
    /// The complete current frame, premultiplied RGBA, always opaque.
    canvas: Option<tiny_skia::Pixmap>,
    /// Compositing target for rectangles narrower than the canvas.
    scratch: Vec<u8>,
    /// The display list the canvas holds.
    shown: Vec<ItemSig>,
    /// Canvas regions the presenter has not accepted yet.
    pending: Option<Region>,
}
impl DamageRenderer {
    /// The persistent frame, for tests and screenshots.
    pub fn canvas(&self) -> Option<&tiny_skia::Pixmap> {
        self.canvas.as_ref()
    }
}
impl Renderer for DamageRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Damage
    }
    fn render_frame(
        &mut self,
        scene: &mut CpuBackend,
        presenter: &mut dyn Presenter,
    ) -> Result<FrameStats, String> {
        let start = Instant::now();
        let size = scene.screen_size();
        let mut stats = FrameStats::default();
        let mut fresh = Region::new(size.w, size.h);
        // First frame, output size change or invalidate(): everything.
        if self.canvas.as_ref().is_none_or(|c| c.width() != size.w || c.height() != size.h) {
            crate::pixels::image_len(size.w, size.h)?;
            self.canvas =
                Some(tiny_skia::Pixmap::new(size.w, size.h).ok_or("CPU output allocation failed")?);
            stats.large_alloc_bytes += u64::from(size.w) * u64::from(size.h) * 4;
            self.pending = None;
        }
        let canvas = self.canvas.as_mut().unwrap();
        let pending = match &mut self.pending {
            Some(pending) => pending,
            none => {
                fresh.add_all();
                none.insert(Region::new(size.w, size.h))
            }
        };
        let list = scene.display_list();
        let sigs: Vec<ItemSig> = list.iter().map(|(sig, _)| *sig).collect();
        if fresh.is_empty() {
            diff(&self.shown, &sigs, |key| scene.partial_damage(key), &mut fresh);
        }
        for rect in fresh.rects() {
            stats.large_alloc_bytes += paint(canvas, &mut self.scratch, &list, *rect);
        }
        drop(list);
        self.shown = sigs;
        scene.ack_damage();
        // From here the canvas is ahead of the screen by `pending`. It is only
        // cleared once the presenter has taken it, so a refused frame is
        // delivered in full by a later call.
        pending.add_region(&fresh);
        stats.damage_px = pending.area();
        stats.damage_rects = pending.rects().len() as u32;
        let composited = Instant::now();
        stats.composite_us = (composited - start).as_micros() as u64;
        presenter.present_damage(canvas, pending.rects())?;
        stats.add_present(presenter, composited.elapsed().as_micros() as u64);
        pending.clear();
        Ok(stats)
    }
    fn invalidate(&mut self) {
        self.pending = None;
        self.shown.clear();
    }
}

/// Recomposite `rect` of the canvas from the display list. Returns the bytes
/// of any large allocation this needed (scratch growth only).
fn paint(
    canvas: &mut tiny_skia::Pixmap,
    scratch: &mut Vec<u8>,
    list: &[(ItemSig, DrawOp<'_>)],
    rect: DRect,
) -> u64 {
    let (width, height) = (rect.width(), rect.height());
    if width == 0 || height == 0 {
        return 0;
    }
    let stride = canvas.width() as usize * 4;
    let rows = rect.y0 as usize * stride..rect.y1 as usize * stride;
    if rect.x0 == 0 && width == canvas.width() {
        // Full-width band: the canvas rows are themselves a valid pixmap.
        let mut target = tiny_skia::PixmapMut::from_bytes(&mut canvas.data_mut()[rows], width, height)
            .expect("canvas band");
        draw_items(&mut target, list, rect);
        return 0;
    }
    let need = width as usize * height as usize * 4;
    let mut grown = 0;
    if scratch.len() < need {
        if need - scratch.len() >= LARGE_ALLOC_BYTES {
            grown = need as u64;
        }
        scratch.resize(need, 0);
    }
    let mut target =
        tiny_skia::PixmapMut::from_bytes(&mut scratch[..need], width, height).expect("scratch");
    draw_items(&mut target, list, rect);
    let bytes = width as usize * 4;
    let band = &mut canvas.data_mut()[rows];
    for (source, row) in scratch[..need].chunks_exact(bytes).zip(band.chunks_exact_mut(stride)) {
        row[rect.x0 as usize * 4..][..bytes].copy_from_slice(source);
    }
    grown
}

/// tiny-skia places a pixmap with `Rect::round()`, which truncates toward
/// zero after adding one half: a negative integer origin lands one pixel to
/// the right/below. `CpuBackend::render` inherits that for images hanging off
/// the left or top edge, so the image covers this rectangle on screen.
pub(crate) fn drawn_image_rect(pos: Point, width: u32, height: u32) -> DRect {
    let placed = |n: i32| if n < 0 { n + 1 } else { n };
    DRect::from_xywh(placed(pos.x), placed(pos.y), width, height)
}

/// The same tiny-skia operations as `CpuBackend::render`, into a target whose
/// origin is `rect`'s top-left corner. Images are drawn as the pattern fill
/// `draw_pixmap` performs, but over the exact output rectangle the full-frame
/// render covers and already clipped to the target, so the placement rounding
/// above cannot differ between a tile and the whole frame. Sampling is exact
/// integer translation, so every pixel equals the full-frame render.
fn draw_items(target: &mut tiny_skia::PixmapMut<'_>, list: &[(ItemSig, DrawOp<'_>)], rect: DRect) {
    let draw = |target: &mut tiny_skia::PixmapMut<'_>, pos: Point, image: &DecorationBuffer| {
        let covered = drawn_image_rect(pos, image.width, image.height)
            .intersect(rect)
            .translate(-rect.x0, -rect.y0);
        if covered.is_empty() {
            return;
        }
        let Some(src) = tiny_skia::PixmapRef::from_bytes(&image.pixels, image.width, image.height) else {
            return;
        };
        // Fully opaque source pixels (every XRGB client buffer, most chrome):
        // source-over is then a plain copy, bit for bit. Only taken where no
        // edge padding is sampled, i.e. for images with a non-negative origin.
        if pos.x >= 0 && pos.y >= 0 {
            let (left, width) = ((covered.x0 + rect.x0 - pos.x) as usize, covered.width() as usize);
            let stride = image.width as usize * 4;
            let rows = || {
                (covered.y0..covered.y1).map(|y| {
                    let row = (y + rect.y0 - pos.y) as usize;
                    &src.data()[row * stride + left * 4..][..width * 4]
                })
            };
            if rows().all(|row| row.as_chunks::<4>().0.iter().all(|p| p[3] == 255)) {
                let target_stride = target.width() as usize * 4;
                let data = target.data_mut();
                for (y, row) in (covered.y0..covered.y1).zip(rows()) {
                    data[y as usize * target_stride + covered.x0 as usize * 4..][..width * 4]
                        .copy_from_slice(row);
                }
                return;
            }
        }
        let Some(area) = tiny_skia::Rect::from_ltrb(
            covered.x0 as f32,
            covered.y0 as f32,
            covered.x1 as f32,
            covered.y1 as f32,
        ) else {
            return;
        };
        let defaults = tiny_skia::PixmapPaint::default();
        let paint = tiny_skia::Paint {
            shader: tiny_skia::Pattern::new(
                src,
                tiny_skia::SpreadMode::Pad,
                defaults.quality,
                defaults.opacity,
                tiny_skia::Transform::from_translate((pos.x - rect.x0) as f32, (pos.y - rect.y0) as f32),
            ),
            blend_mode: defaults.blend_mode,
            anti_alias: false,
            force_hq_pipeline: false,
        };
        target.fill_rect(area, &paint, tiny_skia::Transform::identity(), None);
    };
    for (sig, op) in list {
        if !sig.rect.intersects(rect) {
            continue;
        }
        match op {
            DrawOp::Background { color, image } => {
                target.fill(tiny_skia::Color::from_rgba8(color[0], color[1], color[2], 255));
                if let Some(image) = image {
                    draw(target, Point::new(0, 0), image);
                }
            }
            DrawOp::Image { pos, image } => draw(target, *pos, image),
            // GPU memory of a client: only a GPU renderer can show it, and
            // only a GPU renderer ever accepts such a buffer.
            DrawOp::External { .. } => {}
            DrawOp::Decoration { pos, surface } => {
                for solid in &surface.solids {
                    let at = DRect::from_rect(solid.rect).translate(pos.x, pos.y);
                    if !at.intersects(rect) {
                        continue;
                    }
                    let mut paint = tiny_skia::Paint::default();
                    paint.set_color_rgba8(solid.rgb[0], solid.rgb[1], solid.rgb[2], 255);
                    if let Some(area) = tiny_skia::Rect::from_xywh(
                        (pos.x + solid.rect.pos.x - rect.x0) as f32,
                        (pos.y + solid.rect.pos.y - rect.y0) as f32,
                        solid.rect.size.w as f32,
                        solid.rect.size.h as f32,
                    ) {
                        target.fill_rect(area, &paint, tiny_skia::Transform::identity(), None);
                    }
                }
                for part in &surface.parts {
                    draw(target, Point::new(pos.x + part.offset.x, pos.y + part.offset.y), &part.buffer);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presenter::MemoryFramebuffer;
    use wm_core::{Rect, Size};
    use wm_theme_api::{DecorationPart, DecorationSolid, DecorationSurface};

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u32) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as u32) % bound.max(1)
        }
        /// Premultiplied RGBA with a mix of opaque, translucent and clear pixels.
        fn image(&mut self, w: u32, h: u32) -> DecorationBuffer {
            let mut pixels = Vec::with_capacity((w * h * 4) as usize);
            let mode = self.next(3);
            for _ in 0..w * h {
                let a = match mode {
                    0 => 255,
                    1 => self.next(256) as u8,
                    _ => [0, 255, 128][self.next(3) as usize],
                };
                for _ in 0..3 {
                    pixels.push((self.next(256) * u32::from(a) / 255) as u8);
                }
                pixels.push(a);
            }
            DecorationBuffer { width: w, height: h, pixels }
        }
        fn image_within(&mut self, w: u32, h: u32) -> DecorationBuffer {
            let (w, h) = (1 + self.next(w), 1 + self.next(h));
            self.image(w, h)
        }
        fn rect(&mut self, sw: u32, sh: u32) -> Rect {
            Rect::new(
                Point::new(self.next(sw + 60) as i32 - 30, self.next(sh + 60) as i32 - 30),
                Size::new(1 + self.next(sw / 2), 1 + self.next(sh / 2)),
            )
        }
    }

    /// Presenter that keeps what a real screen would hold: only damaged
    /// pixels are taken from each frame.
    struct Screen {
        pixels: Vec<u8>,
        width: usize,
        reject: bool,
        written: u64,
    }
    impl Presenter for Screen {
        fn present(&mut self, frame: &tiny_skia::Pixmap) -> Result<(), String> {
            if self.reject {
                return Err("rejected".into());
            }
            self.pixels.copy_from_slice(frame.data());
            Ok(())
        }
        fn present_damage(&mut self, frame: &tiny_skia::Pixmap, damage: &[DRect]) -> Result<(), String> {
            if self.reject {
                return Err("rejected".into());
            }
            for r in damage {
                for y in r.y0 as usize..r.y1 as usize {
                    let at = (y * self.width + r.x0 as usize) * 4;
                    let n = r.width() as usize * 4;
                    self.pixels[at..at + n].copy_from_slice(&frame.data()[at..at + n]);
                }
                self.written += r.area();
            }
            Ok(())
        }
    }

    const W: u32 = 97;
    const H: u32 = 71;

    /// Drive a scene through every mutation the backend offers that changes
    /// pixels (shell surfaces, popups, background, cursor, pointer) and hold
    /// the damage-tracked screen to the legacy full render after each step.
    #[test]
    fn damage_tracked_screen_equals_legacy_render_after_every_mutation() {
        use wm_theme_api::PopupHost;
        let mut rng = Lcg(0xc0ffee);
        let mut scene = CpuBackend::new(Size::new(W, H)).unwrap();
        let mut renderer = DamageRenderer::default();
        let mut screen = Screen { pixels: vec![0; (W * H * 4) as usize], width: W as usize, reject: false, written: 0 };
        let mut shells: Vec<u64> = Vec::new();
        let mut partial_frames = 0;
        for step in 0..6000 {
            match rng.next(16) {
                0 => {
                    let g = rng.rect(W, H);
                    let above = rng.next(2) == 0;
                    if let Some(id) = scene.create_shell_surface(g, (rng.next(256) as u8, 9, 200), above) {
                        scene.map_shell_surface(id);
                        shells.push(id);
                    }
                }
                1 if !shells.is_empty() => {
                    let id = shells.swap_remove(rng.next(shells.len() as u32) as usize);
                    scene.destroy_shell_surface(id);
                }
                2 | 3 if !shells.is_empty() => {
                    let id = shells[rng.next(shells.len() as u32) as usize];
                    let g = rng.rect(W, H);
                    scene.configure_shell_surface(id, g);
                    let image = rng.image(g.size.w, g.size.h);
                    scene.paint_shell_surface(id, &image);
                }
                4 if !shells.is_empty() => {
                    let id = shells[rng.next(shells.len() as u32) as usize];
                    scene.raise_shell_surface(id);
                }
                5 if !shells.is_empty() => {
                    let id = shells[rng.next(shells.len() as u32) as usize];
                    if rng.next(2) == 0 { scene.unmap_shell_surface(id) } else { scene.map_shell_surface(id) }
                }
                6 if !shells.is_empty() => {
                    // In-place partial edit, as a damaged client commit does.
                    let id = shells[rng.next(shells.len() as u32) as usize];
                    let image = scene.shell_image_mut(id).unwrap();
                    let (w, h) = (image.width, image.height);
                    if image.pixels.len() == (w * h * 4) as usize {
                        let r = DRect::from_xywh(rng.next(w) as i32, rng.next(h) as i32, 1 + rng.next(w), 1 + rng.next(h))
                            .intersect(DRect::from_xywh(0, 0, w, h));
                        let v = rng.next(256) as u8;
                        for y in r.y0..r.y1 {
                            for x in r.x0..r.x1 {
                                let at = ((y as u32 * w + x as u32) * 4) as usize;
                                image.pixels[at..at + 4].copy_from_slice(&[v, v / 2, v / 3, 255]);
                            }
                        }
                        scene.shell_image_damaged(id, &[r]);
                    }
                }
                7 if !shells.is_empty() => {
                    let id = shells[rng.next(shells.len() as u32) as usize];
                    let g = rng.rect(W, H);
                    scene.replace_shell_image(id, rng.image(g.size.w, g.size.h));
                }
                8 => scene.paint_root_color((rng.next(256) as u8, rng.next(256) as u8, 7)),
                9 => {
                    let image = rng.image_within(W + 20, H + 20);
                    scene.paint_root_image(&image);
                }
                10 => {
                    let cursor = (rng.next(4) != 0).then(|| {
                        (Point::new(rng.next(5) as i32, rng.next(5) as i32), rng.image_within(24, 24))
                    });
                    scene.set_cursor(cursor);
                }
                11 => scene.set_cursor_hotspot(Point::new(rng.next(9) as i32 - 2, rng.next(9) as i32 - 2)),
                12 if !shells.is_empty() => {
                    let id = shells.swap_remove(rng.next(shells.len() as u32) as usize);
                    scene.release_shell_buffer(id);
                    shells.push(id);
                }
                13 => {
                    if let Some(id) = scene.create_popup(rng.rect(W, H), (1, 2, 3)) {
                        shells.push(id);
                    }
                }
                _ => {
                    // Pointer motion, mostly small steps as a real device makes.
                    let p = scene.pointer;
                    scene.pointer = if rng.next(8) == 0 {
                        Point::new(rng.next(W) as i32, rng.next(H) as i32)
                    } else {
                        Point::new(
                            (p.x + rng.next(7) as i32 - 3).clamp(0, W as i32 - 1),
                            (p.y + rng.next(7) as i32 - 3).clamp(0, H as i32 - 1),
                        )
                    };
                    scene.dirty = true;
                }
            }
            // Sometimes several mutations accumulate before a frame, and
            // sometimes the presenter refuses a frame.
            if rng.next(4) == 0 {
                continue;
            }
            screen.reject = rng.next(10) == 0;
            let before = screen.written;
            let result = renderer.render_frame(&mut scene, &mut screen);
            assert_eq!(result.is_err(), screen.reject);
            if screen.reject {
                continue;
            }
            let stats = result.unwrap();
            assert_eq!(stats.damage_px, screen.written - before);
            if stats.damage_px < u64::from(W * H) {
                partial_frames += 1;
            }
            let reference = scene.render().unwrap();
            assert!(
                renderer.canvas().unwrap().data() == reference.data(),
                "canvas differs from legacy render at step {step}"
            );
            assert!(screen.pixels == reference.data(), "screen is stale at step {step}");
            assert!(reference.data().as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        }
        assert!(partial_frames > 2000, "damage tracking degenerated to full repaints: {partial_frames}");
    }

    fn decorated_scene() -> (CpuBackend, u64) {
        // Decoration frames need a real ToplevelSurface to own a window, which
        // only a Wayland client can make (tests/wayland_shm.rs covers that
        // end to end). The draw code itself is exercised here directly.
        let mut scene = CpuBackend::new(Size::new(W, H)).unwrap();
        let shell = scene.create_shell_surface(Rect::new(Point::new(5, 5), Size::new(40, 30)), (9, 9, 9), false).unwrap();
        scene.map_shell_surface(shell);
        (scene, shell)
    }

    #[test]
    fn decoration_draw_is_translation_invariant() {
        let mut rng = Lcg(77);
        let surface = DecorationSurface {
            frame_size: Size::new(60, 40),
            parts: (0..6)
                .map(|_| DecorationPart {
                    offset: Point::new(rng.next(50) as i32 - 5, rng.next(30) as i32 - 5),
                    buffer: rng.image_within(20, 12),
                })
                .collect(),
            solids: (0..6)
                .map(|_| DecorationSolid {
                    rect: Rect::new(
                        Point::new(rng.next(50) as i32, rng.next(30) as i32),
                        Size::new(rng.next(20), rng.next(12)),
                    ),
                    rgb: [rng.next(256) as u8, 3, 250],
                })
                .collect(),
            shadow: None,
            shape: None,
        };
        let list_for = |pos: Point| {
            vec![
                (
                    ItemSig { key: crate::damage::ItemKey::Background, rect: DRect::from_xywh(0, 0, W, H), generation: 0 },
                    DrawOp::Background { color: [10, 20, 30], image: None },
                ),
                (
                    ItemSig { key: crate::damage::ItemKey::Decoration(1), rect: DRect::from_xywh(-100, -100, 400, 400), generation: 0 },
                    DrawOp::Decoration { pos, surface: &surface },
                ),
            ]
        };
        let pos = Point::new(17, 9);
        let list = list_for(pos);
        // Whole frame in one piece is the reference...
        let mut whole = tiny_skia::Pixmap::new(W, H).unwrap();
        let mut scratch = Vec::new();
        paint(&mut whole, &mut scratch, &list, DRect::from_xywh(0, 0, W, H));
        assert!(whole.data().as_chunks::<4>().0.iter().any(|p| p[..3] != [10, 20, 30]));
        // ...and painting it as many arbitrary tiles must give the same pixels.
        let mut tiled = tiny_skia::Pixmap::new(W, H).unwrap();
        let mut y = 0;
        while y < H as i32 {
            let h = 1 + rng.next(19) as i32;
            let mut x = 0;
            while x < W as i32 {
                let w = 1 + rng.next(31) as i32;
                let tile = DRect::new(x, y, (x + w).min(W as i32), (y + h).min(H as i32));
                paint(&mut tiled, &mut scratch, &list, tile);
                x += w;
            }
            y += h;
        }
        assert!(tiled.data() == whole.data());
    }

    #[test]
    fn idle_and_unchanged_frames_write_nothing() {
        let (mut scene, shell) = decorated_scene();
        let mut renderer = DamageRenderer::default();
        let mut fb = MemoryFramebuffer::rgb30(W, H).unwrap();
        let first = renderer.render_frame(&mut scene, &mut fb).unwrap();
        assert_eq!(first.bytes_written, u64::from(W * H * 4));
        assert_eq!(first.large_alloc_bytes, u64::from(W * H * 4)); // the canvas, once
        // Dirty flag without a visible change (e.g. a button event).
        let again = renderer.render_frame(&mut scene, &mut fb).unwrap();
        assert_eq!((again.bytes_written, again.damage_px, again.large_alloc_bytes), (0, 0, 0));
        // Repainting a shell with identical geometry damages exactly that shell.
        let image = DecorationBuffer { width: 40, height: 30, pixels: vec![255; 40 * 30 * 4] };
        scene.paint_shell_surface(shell, &image);
        let repaint = renderer.render_frame(&mut scene, &mut fb).unwrap();
        assert_eq!(repaint.damage_px, 40 * 30);
        assert_eq!(repaint.bytes_written, 40 * 30 * 4);
        assert_eq!(repaint.large_alloc_bytes, 0); // scratch below the large threshold
        // invalidate() forces everything once.
        renderer.invalidate();
        let forced = renderer.render_frame(&mut scene, &mut fb).unwrap();
        assert_eq!(forced.bytes_written, u64::from(W * H * 4));
        assert_eq!(forced.large_alloc_bytes, 0);
    }

    #[test]
    fn framebuffer_bytes_match_between_legacy_and_damage_renderers() {
        let mut rng = Lcg(5);
        let (mut scene, shell) = decorated_scene();
        scene.set_cursor(Some((Point::new(0, 0), rng.image(12, 18))));
        let (mut old, mut new) = (LegacyRenderer, DamageRenderer::default());
        let mut old_fb = MemoryFramebuffer::rgb30(W, H).unwrap();
        let mut new_fb = MemoryFramebuffer::rgb30(W, H).unwrap();
        let mut partial_bytes = 0;
        for step in 0..200 {
            match step % 4 {
                0 => scene.pointer = Point::new(rng.next(W) as i32, rng.next(H) as i32),
                1 => scene.paint_shell_surface(shell, &rng.image(40, 30)),
                2 => scene.configure_shell_surface(shell, Rect::new(Point::new(rng.next(60) as i32, rng.next(40) as i32), Size::new(40, 30))),
                _ => scene.pointer.x = (scene.pointer.x + 2).min(W as i32 - 1),
            }
            // Damage renderer first: each renderer acknowledges the scene's
            // partial damage, and the legacy one has no use for it.
            let b = new.render_frame(&mut scene, &mut new_fb).unwrap();
            let a = old.render_frame(&mut scene, &mut old_fb).unwrap();
            assert!(old_fb.bytes() == new_fb.bytes(), "framebuffers differ at step {step}");
            assert_eq!(a.bytes_written, u64::from(W * H * 4));
            assert!(a.large_alloc_bytes >= 2 * u64::from(W * H * 4));
            if step > 0 {
                assert!(b.bytes_written < a.bytes_written);
                assert_eq!(b.large_alloc_bytes, 0);
                partial_bytes += b.bytes_written;
            }
        }
        assert!(partial_bytes > 0);
    }

    #[test]
    fn first_frame_is_reported_apart_from_steady_state() {
        let mut totals = FrameTotals::default();
        let start = totals;
        totals.record(FrameStats { composite_us: 200_000, present_us: 180_000, bytes_written: 4_000_000, ..FrameStats::default() });
        for _ in 0..6 {
            totals.record(FrameStats { composite_us: 1_500, present_us: 200, bytes_written: 100, ..FrameStats::default() });
        }
        let line = totals.summary(&start, RendererKind::Gl, "kms", 38.0);
        assert!(line.contains("frames=7 "), "{line}");
        assert!(line.contains("steady_frames=6 avg_composite_us=1500 "), "{line}");
        assert!(line.contains("avg_present_us=200 max_composite_us=1500 max_present_us=200 "), "{line}");
        assert!(line.contains("first_frame_composite_us=200000 first_frame_present_us=180000 first_frame_bytes=4000000"), "{line}");
        // A later window does not repeat the first frame.
        let mark = totals;
        totals.record(FrameStats { composite_us: 3_000, present_us: 400, ..FrameStats::default() });
        let line = totals.summary(&mark, RendererKind::Gl, "kms", 1.0);
        assert!(line.contains("frames=1 ") && line.contains("steady_frames=1 avg_composite_us=3000 ") && !line.contains("first_frame"), "{line}");
    }

    #[test]
    fn renderer_names_parse_and_reject_unknown() {
        assert_eq!(RendererKind::parse("legacy"), Ok(RendererKind::Legacy));
        assert_eq!(RendererKind::parse("damage"), Ok(RendererKind::Damage));
        assert!(RendererKind::parse("vulkan").is_err());
        assert_eq!(RendererKind::parse("cpu"), Ok(RendererKind::Damage));
        assert_eq!(RendererKind::parse("gl"), Ok(RendererKind::Gl));
        assert_eq!(RendererRequest::parse("auto"), Ok(RendererRequest::Auto));
        assert_eq!(RendererRequest::parse("gl"), Ok(RendererRequest::Exactly(RendererKind::Gl)));
        assert_eq!(RendererRequest::Auto.cpu_kind(), RendererKind::Damage);
        assert_eq!(RendererRequest::Exactly(RendererKind::Legacy).cpu_kind(), RendererKind::Legacy);
        assert_eq!(RendererKind::Legacy.create_cpu().kind(), RendererKind::Legacy);
        assert_eq!(RendererKind::Damage.create_cpu().kind(), RendererKind::Damage);
        assert_eq!(RendererKind::Gl.create_cpu().kind(), RendererKind::Damage);
    }
}
