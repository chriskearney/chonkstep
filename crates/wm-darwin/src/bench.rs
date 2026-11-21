//! Self-contained rendering benchmark: no Wayland clients, no input devices.
//!
//! `--bench` drives the real `Server::repaint` path (scene, renderer,
//! presenter) against the SFGOP framebuffer, or against a heap framebuffer of
//! the same format with `--offscreen`, through fixed scenarios, and prints one
//! `WMBENCH` line per scenario. It exits by itself and puts back the pixels it
//! found on the framebuffer.
use crate::damage::DRect;
use crate::hid::{Event, Motion};
use crate::pixels::{copy_shm_per_pixel, copy_shm_rows, ShmLayout};
use crate::glrender::{compare_frames, GlRenderer};
use crate::output::Output;
use crate::presenter::{MemoryFramebuffer, Pdgop};
use crate::render::{FrameStats, RendererKind, RendererRequest};
use crate::server::Server;
use smithay::reexports::wayland_server::{protocol::wl_shm, Display};
use std::time::{Duration, Instant};
use wm_core::{Backend, Point, Rect, Size};
use wm_theme_api::DecorationBuffer;

pub const USAGE: &str = "--bench [--renderer gl|cpu|damage|legacy] [--offscreen] [--quick] [--scenario pixelcheck|full|pointer|client|client-partial|animation|idle]...
  --renderer   which render path to measure (default: $CHONKSTEP_DARWIN_RENDERER, else damage).
               gl renders on the GPU and presents by KMS page flip; cpu is the damage renderer
               on the boot framebuffer (SFGOP); legacy is the original full-frame CPU path
  --offscreen  nothing is displayed: gl renders into an ordinary framebuffer object, the CPU
               renderers into a heap framebuffer (2560x1600 x2r10g10b10)
  --quick      one tenth of the frames, 1 s of animation and a 0.2 s idle, for a fast sanity run
  --scenario   run only the named scenario(s); default is all, in the order above";

const SCENARIOS: [&str; 7] = ["pixelcheck", "full", "pointer", "client", "client-partial", "animation", "idle"];

struct Options {
    renderer: RendererKind,
    offscreen: bool,
    quick: bool,
    scenarios: Vec<String>,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Options {
            // Auto means the damage renderer here: a benchmark measures what it is told to.
            renderer: match RendererRequest::from_env()? {
                RendererRequest::Exactly(kind) => kind,
                RendererRequest::Auto => RendererKind::Damage,
            },
            offscreen: false,
            quick: false,
            scenarios: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--renderer" => {
                    options.renderer = RendererKind::parse(args.next().ok_or(USAGE)?)?;
                }
                "--offscreen" => options.offscreen = true,
                "--quick" => options.quick = true,
                "--scenario" => {
                    let name = args.next().ok_or(USAGE)?;
                    if !SCENARIOS.contains(&name.as_str()) {
                        return Err(format!("unknown scenario {name:?}\n{USAGE}"));
                    }
                    options.scenarios.push(name.clone());
                }
                _ => return Err(USAGE.into()),
            }
        }
        if options.scenarios.is_empty() {
            options.scenarios = SCENARIOS.iter().map(|s| s.to_string()).collect();
        }
        Ok(options)
    }
}

/// Per-frame samples of one scenario.
#[derive(Default)]
struct Samples {
    /// Wall time of each step: the scene change plus `Server::repaint`.
    step_us: Vec<u64>,
    /// Stats of the steps that actually rendered a frame.
    frames: Vec<FrameStats>,
    /// When each frame reached the screen (page-flipping output only).
    flips: Vec<Instant>,
    /// Appended to the line as is.
    extra: String,
}
fn spread(values: &mut [u64]) -> (u64, u64, u64) {
    if values.is_empty() {
        return (0, 0, 0);
    }
    values.sort_unstable();
    (values[0], values[values.len() / 2], values[values.len() - 1])
}
impl Samples {
    fn line(&mut self, scenario: &str, renderer: RendererKind, output: &str) -> String {
        let (min, median, max) = spread(&mut self.step_us);
        let median_of = |pick: fn(&FrameStats) -> u64, frames: &[FrameStats]| {
            let mut values: Vec<u64> = frames.iter().map(pick).collect();
            spread(&mut values).1
        };
        let total = |pick: fn(&FrameStats) -> u64, frames: &[FrameStats]| -> u64 {
            frames.iter().map(pick).sum()
        };
        let rendered = self.frames.len() as u64;
        let bytes = total(|f| f.bytes_written, &self.frames);
        format!(
            "WMBENCH scenario={scenario} renderer={} output={output} steps={} rendered={rendered} min_us={min} median_us={median} max_us={max} composite_median_us={} convert_median_us={} present_median_us={} bytes_total={bytes} bytes_per_frame={} damage_px_per_frame={} large_alloc_bytes_total={}",
            renderer.name(),
            self.step_us.len(),
            median_of(|f| f.composite_us, &self.frames),
            median_of(|f| f.convert_us, &self.frames),
            median_of(|f| f.present_us, &self.frames),
            bytes / rendered.max(1),
            total(|f| f.damage_px, &self.frames) / rendered.max(1),
            total(|f| f.large_alloc_bytes, &self.frames),
        ) + &self.extra
    }
}

struct Bench {
    display: Display<Server>,
    server: Server,
    output: Output,
    started: Instant,
    /// Real seat delivery when the SFHID seat (keymap) could be created.
    seat: bool,
    size: Size,
}
impl Bench {
    fn ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
    /// One measured step: `change` the scene, then repaint.
    fn step(
        &mut self,
        samples: &mut Samples,
        change: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let before = self.server.frame_totals().frames;
        let start = Instant::now();
        change(self)?;
        let ms = self.ms();
        self.server.repaint(&mut self.output, ms)?;
        samples.step_us.push(start.elapsed().as_micros() as u64);
        if self.server.frame_totals().frames != before {
            samples.frames.push(self.server.last_frame());
        }
        // Not part of the step time: on a page-flipping output the next frame
        // can only start once this one is on screen.
        if self.wait_presented()? {
            samples.flips.push(Instant::now());
        }
        Ok(())
    }
    /// Wait for the frame in flight, if any. True when one completed.
    fn wait_presented(&mut self) -> Result<bool, String> {
        if !self.server.frame_in_flight() {
            return Ok(false);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.server.frame_in_flight() {
            if Instant::now() >= deadline {
                return Err("no flip-complete event within 2 s of a frame".into());
            }
            let ms = self.ms();
            self.server.idle_wait(Duration::from_millis(100), ms)?;
        }
        Ok(true)
    }
    /// Unmeasured repaint so set-up work is not charged to the first step.
    fn settle(&mut self) -> Result<(), String> {
        let ms = self.ms();
        self.server.repaint(&mut self.output, ms)?;
        self.wait_presented()?;
        Ok(())
    }
    fn move_pointer(&mut self, to: Point, by: (i8, i8)) -> Result<(), String> {
        if self.seat {
            let ms = self.ms();
            self.server.deliver_hid(
                Event::Pointer { motion: Motion::Relative(by.0, by.1), buttons: 0, wheel: 0 },
                ms,
            )
        } else {
            let scene = self.server.wm.backend_mut();
            scene.pointer = to;
            scene.dirty = true;
            Ok(())
        }
    }
}

/// Deterministic opaque test pattern (premultiplied RGBA).
fn pattern(width: u32, height: u32, seed: u32) -> DecorationBuffer {
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let v = (x ^ y).wrapping_mul(seed | 1);
            pixels.extend_from_slice(&[(v >> 3) as u8, (x + seed) as u8, (y + seed) as u8, 255]);
        }
    }
    DecorationBuffer { width, height, pixels }
}

/// A client-side XRGB8888 pool whose content depends on `frame`. `band`
/// limits the rewrite to those rows (what a client with partial damage does).
fn client_draw(pool: &mut [u8], width: usize, frame: u32, band: Option<(usize, usize)>) {
    let rows = pool.len() / (width * 4);
    let (from, to) = band.unwrap_or((0, rows));
    for y in from..to.min(rows) {
        let row = &mut pool[y * width * 4..][..width * 4];
        for (x, p) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let v = (x as u32).wrapping_add(frame.wrapping_mul(7)) ^ (y as u32).wrapping_add(frame);
            p.copy_from_slice(&[v as u8, (v >> 2) as u8, (frame * 3) as u8, 0]);
        }
    }
}

fn run_scenarios(bench: &mut Bench, options: &Options, lines: &mut Vec<String>) -> Result<(), String> {
    let renderer = options.renderer;
    let output = match &bench.output {
        Output::Gpu => bench.server.output_name(),
        other => other.name(),
    };
    let size = bench.size;
    let count = |n: usize| if options.quick { (n / 10).max(2) } else { n };

    // Desktop furniture so a repaint is not just a colour fill: a top bar and
    // two window-sized surfaces that the pointer path crosses.
    {
        let scene = bench.server.wm.backend_mut();
        let furniture = [
            Rect::new(Point::new(0, 0), Size::new(size.w, 28.min(size.h))),
            Rect::new(Point::new(120, 90), Size::new(900.min(size.w), 600.min(size.h))),
            Rect::new(Point::new(size.w as i32 / 2, size.h as i32 / 3), Size::new(900.min(size.w), 600.min(size.h))),
        ];
        for (i, rect) in furniture.into_iter().enumerate() {
            let id = scene.create_shell_surface(rect, (40, 40, 40), false).ok_or("bench surface")?;
            scene.replace_shell_image(id, pattern(rect.size.w, rect.size.h, 17 + i as u32));
            scene.map_shell_surface(id);
        }
    }
    bench.settle()?;

    for scenario in &options.scenarios {
        let mut samples = Samples::default();
        match scenario.as_str() {
            // The same frame from this renderer and from the original CPU
            // full-frame render: a translucent surface and the cursor on top
            // of the furniture, with a nonce colour as background.
            "pixelcheck" => {
                let scene = bench.server.wm.backend_mut();
                scene.paint_root_color((0x33, 0x99, 0xcc));
                let rect = Rect::new(Point::new(300, 200), Size::new(500.min(size.w), 400.min(size.h)));
                let glass = scene.create_shell_surface(rect, (0, 0, 0), true).ok_or("bench glass surface")?;
                let mut image = pattern(rect.size.w, rect.size.h, 91);
                for pixel in image.pixels.as_chunks_mut::<4>().0.iter_mut() {
                    // Premultiplied 50 %: every channel halved with the alpha.
                    for channel in pixel.iter_mut() {
                        *channel /= 2;
                    }
                }
                scene.replace_shell_image(glass, image);
                scene.map_shell_surface(glass);
                bench.server.damage_everything();
                bench.settle()?;
                let reference = bench.server.wm.backend().render()?;
                let line = match bench.server.read_back()? {
                    Some(frame) => {
                        let (pixels, worst) = compare_frames(&frame, reference.data());
                        // Where: the half-transparent surface is the only place a blend happens.
                        let glass_rect = crate::damage::DRect::from_rect(rect);
                        let (bbox, in_glass) = crate::glrender::locate_differences(&frame, reference.data(), size.w, glass_rect);
                        let place = if pixels == 0 {
                            String::new()
                        } else {
                            format!(
                                " diff_bbox={},{}-{},{} translucent_surface={},{}-{},{} differing_inside_translucent_surface={in_glass} differing_elsewhere={}",
                                bbox.x0, bbox.y0, bbox.x1, bbox.y1,
                                glass_rect.x0, glass_rect.y0, glass_rect.x1, glass_rect.y1,
                                pixels - in_glass,
                            )
                        };
                        format!(
                            "WMBENCH pixelcheck renderer={} output={output} pixels={} result={} differing_pixels={pixels} max_channel_diff={worst}{place}",
                            renderer.name(),
                            reference.data().len() / 4,
                            if pixels == 0 { "equal" } else { "different" },
                        )
                    }
                    None => format!(
                        "WMBENCH pixelcheck renderer={} output={output} result=not-applicable (this renderer has no readback; host tests hold it to the same reference)",
                        renderer.name()
                    ),
                };
                bench.server.wm.backend_mut().destroy_shell_surface(glass);
                bench.settle()?;
                lines.push(line);
                continue;
            }
            // Continuous animation: a surface moves every frame for 5 s. On a
            // page-flipping output this is frames presented per second.
            "animation" => {
                let rect = Rect::new(Point::new(0, 100), Size::new(400.min(size.w), 300.min(size.h)));
                let mover = {
                    let scene = bench.server.wm.backend_mut();
                    let id = scene.create_shell_surface(rect, (0, 0, 0), true).ok_or("bench moving surface")?;
                    scene.replace_shell_image(id, pattern(rect.size.w, rect.size.h, 5));
                    scene.map_shell_surface(id);
                    id
                };
                bench.settle()?;
                let length = Duration::from_millis(if options.quick { 1000 } else { 5000 });
                let begin = Instant::now();
                let (mut x, mut dx) = (0i32, 4i32);
                while begin.elapsed() < length {
                    if x + dx < 0 || x + dx + rect.size.w as i32 > size.w as i32 {
                        dx = -dx;
                    }
                    x += dx;
                    bench.step(&mut samples, |b| {
                        b.server.wm.backend_mut().configure_shell_surface(mover, Rect::new(Point::new(x, 100), rect.size));
                        Ok(())
                    })?;
                }
                let seconds = begin.elapsed().as_secs_f64();
                let intervals: Vec<u64> = samples.flips.windows(2).map(|w| (w[1] - w[0]).as_micros() as u64).collect();
                let presented = if samples.flips.is_empty() { samples.frames.len() } else { samples.flips.len() };
                samples.extra = format!(
                    " seconds={seconds:.2} fps={:.1} flips={} missed_refreshes={} interval_ms_min={:.2} interval_ms_max={:.2}",
                    presented as f64 / seconds,
                    samples.flips.len(),
                    intervals.iter().filter(|i| **i > 25_000).count(),
                    intervals.iter().copied().min().unwrap_or(0) as f64 / 1000.0,
                    intervals.iter().copied().max().unwrap_or(0) as f64 / 1000.0,
                );
                bench.server.wm.backend_mut().destroy_shell_surface(mover);
                bench.settle()?;
            }
            // (a) every frame repaints, converts and presents the whole screen.
            "full" => {
                for _ in 0..count(120) {
                    bench.step(&mut samples, |b| {
                        b.server.damage_everything();
                        Ok(())
                    })?;
                }
            }
            // (b) the cursor moves a few pixels per step along a bouncing path.
            "pointer" => {
                let start = Point::new(100.min(size.w as i32 - 1), 60.min(size.h as i32 - 1));
                if bench.seat {
                    // Relative motion starts from 0,0: walk to the start, unmeasured.
                    let (mut x, mut y) = (0, 0);
                    while x < start.x || y < start.y {
                        let by = ((start.x - x).min(100) as i8, (start.y - y).min(100) as i8);
                        bench.move_pointer(start, by)?;
                        x += i32::from(by.0);
                        y += i32::from(by.1);
                    }
                } else {
                    bench.move_pointer(start, (0, 0))?;
                }
                bench.settle()?;
                let (mut at, mut dir) = (start, (3i32, 2i32));
                for _ in 0..count(600) {
                    if at.x + dir.0 < 0 || at.x + dir.0 > size.w as i32 - 1 {
                        dir.0 = -dir.0;
                    }
                    if at.y + dir.1 < 0 || at.y + dir.1 > size.h as i32 - 1 {
                        dir.1 = -dir.1;
                    }
                    at = Point::new(at.x + dir.0, at.y + dir.1);
                    bench.step(&mut samples, |b| b.move_pointer(at, (dir.0 as i8, dir.1 as i8)))?;
                }
            }
            // (c) an 800x600 "client" whose buffer changes every frame. The
            // surface is internal (a shell surface below the windows), but
            // the buffer import is the real SHM copy routine of the selected
            // path, reading an in-process XRGB8888 pool.
            "client" | "client-partial" => {
                let (w, h) = (800.min(size.w), 600.min(size.h));
                let rect = Rect::new(Point::new((size.w - w) as i32 / 2, (size.h - h) as i32 / 2), Size::new(w, h));
                let mut pool = vec![0u8; w as usize * h as usize * 4];
                let layout = ShmLayout::checked(w as i32, h as i32, w as i32 * 4, 0, wl_shm::Format::Xrgb8888, pool.len())?;
                client_draw(&mut pool, w as usize, 0, None);
                let surface = {
                    let scene = bench.server.wm.backend_mut();
                    let id = scene.create_shell_surface(rect, (0, 0, 0), false).ok_or("bench client surface")?;
                    // SAFETY: `pool` is a local allocation built for exactly this `layout`, so every row the copy reads lies inside it.
                    scene.replace_shell_image(id, unsafe { copy_shm_per_pixel(pool.as_ptr(), layout) });
                    scene.map_shell_surface(id);
                    id
                };
                bench.settle()?;
                let partial = scenario == "client-partial";
                for frame in 1..=count(240) as u32 {
                    // The client's own drawing is not compositor work: untimed.
                    let band = partial.then(|| {
                        let top = (frame as usize * 20) % (h as usize - 20).max(1);
                        (top, top + 20)
                    });
                    client_draw(&mut pool, w as usize, frame, band);
                    let damage = match band {
                        Some((top, bottom)) => DRect::new(0, top as i32, w as i32, bottom as i32),
                        None => layout.bounds(),
                    };
                    bench.step(&mut samples, |b| {
                        let scene = b.server.wm.backend_mut();
                        match renderer {
                            // Original commit path: fresh allocation, per-pixel copy.
                            RendererKind::Legacy => {
                                // SAFETY: `pool` is a local allocation built for exactly this `layout`, so every row the copy reads lies inside it.
                                scene.replace_shell_image(surface, unsafe { copy_shm_per_pixel(pool.as_ptr(), layout) });
                            }
                            // New commit path: damaged rows into the held image.
                            RendererKind::Damage | RendererKind::Gl => {
                                let image = scene.shell_image_mut(surface).ok_or("bench client image")?;
                                // SAFETY: `pool` is a local allocation built for exactly this `layout`, so every row the copy reads lies inside it.
                                unsafe { copy_shm_rows(pool.as_ptr(), layout, &mut image.pixels, &[damage]) };
                                scene.shell_image_damaged(surface, &[damage]);
                            }
                        }
                        Ok(())
                    })?;
                }
                bench.server.wm.backend_mut().destroy_shell_surface(surface);
                bench.settle()?;
            }
            // (d) nothing changes: the main loop's per-iteration work without
            // its sleep. No frame may be rendered and no byte written.
            "idle" => {
                let length = Duration::from_millis(if options.quick { 200 } else { 2000 });
                let until = Instant::now() + length;
                while Instant::now() < until {
                    let before = bench.server.frame_totals().frames;
                    let start = Instant::now();
                    bench.display.dispatch_clients(&mut bench.server).map_err(|e| e.to_string())?;
                    let ms = bench.ms();
                    bench.server.repaint(&mut bench.output, ms)?;
                    bench.display.flush_clients().map_err(|e| e.to_string())?;
                    samples.step_us.push(start.elapsed().as_micros() as u64);
                    if bench.server.frame_totals().frames != before {
                        samples.frames.push(bench.server.last_frame());
                    }
                    std::thread::sleep(Duration::from_millis(8));
                }
            }
            _ => unreachable!("scenario names are validated"),
        }
        lines.push(samples.line(scenario, renderer, output));
    }
    Ok(())
}

/// Entry for `chonkstep-darwin --bench ...`: system font discovery, as that
/// binary's normal start-up does.
pub fn run(args: &[String]) -> Result<(), String> {
    run_with_fonts(args, wm_theme::FontState::new)
}

/// The benchmark with the caller choosing the fonts. `fonts` is called only
/// once the arguments are accepted, so `--help` and a bad option cost no font
/// discovery.
pub fn run_with_fonts(
    args: &[String],
    fonts: impl FnOnce() -> wm_theme::FontState,
) -> Result<(), String> {
    if args.iter().any(|a| a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let options = Options::parse(args)?;
    // The GL renderer brings its own output; the CPU renderers write to the
    // boot framebuffer or to heap memory of the same format.
    let (mut gl, mut output) = match (options.renderer, options.offscreen) {
        (RendererKind::Gl, true) => (Some(GlRenderer::offscreen(2560, 1600)?), Output::Gpu),
        (RendererKind::Gl, false) => (Some(GlRenderer::on_kms(crate::output::flip_wait_from_env()?)?), Output::Gpu),
        (_, true) => (None, Output::Memory(MemoryFramebuffer::rgb30(2560, 1600)?)),
        (_, false) => (None, Output::Boot(Pdgop::open()?)),
    };
    // Whatever is on the boot framebuffer now (normally the text console) is
    // put back. A KMS run takes the display over and does not give it back.
    let saved = match &output {
        Output::Boot(p) => Some(p.snapshot()),
        _ => None,
    };
    let (size, format) = match &mut output {
        Output::Boot(p) => (p.size(), format!("stride={} masks={:08x},{:08x},{:08x}", p.layout().stride, p.layout().masks[0], p.layout().masks[1], p.layout().masks[2])),
        Output::Memory(m) => {
            let l = m.scanout().layout();
            (m.size(), format!("stride={} masks={:08x},{:08x},{:08x}", l.stride, l.masks[0], l.masks[1], l.masks[2]))
        }
        Output::Gpu => {
            let g = gl.as_ref().expect("GL renderer");
            (Size::new(g.size().0, g.size().1), String::from("format=XRGB8888"))
        }
    };
    let output_name = match (&output, &gl) {
        (Output::Gpu, Some(g)) => crate::render::Renderer::output_name(g),
        (other, _) => other.name(),
    };
    let mut lines = vec![format!(
        "WMBENCH info renderer={} output={output_name} width={} height={} {format} quick={}",
        options.renderer.name(),
        size.w,
        size.h,
        options.quick,
    )];
    let (result, mut output) = (|| -> Result<(Result<(), String>, Output), String> {
        let display = Display::<Server>::new().map_err(|e| e.to_string())?;
        let mut server = Server::new_with_fonts(&display.handle(), size, fonts())?;
        match gl.take() {
            Some(renderer) => server.set_renderer_boxed(Box::new(renderer)),
            None => server.set_renderer(options.renderer),
        }
        // The real seat gives the real pointer path (focus, hit testing, WM
        // dispatch). It needs only an XKB keymap, never a device. Without it
        // the pointer and cursor are driven directly in the scene.
        let seat = match server.enable_sfhid_seat(&display.handle()) {
            Ok(()) => true,
            Err(error) => {
                lines.push(format!("WMBENCH note seat unavailable ({error}); pointer driven directly"));
                server.wm.backend_mut().set_cursor(Some((Point::new(0, 0), crate::scale::arrow(crate::scale::Scale::ONE))));
                false
            }
        };
        let mut bench = Bench { display, server, output, started: Instant::now(), seat, size };
        let result = run_scenarios(&mut bench, &options, &mut lines);
        // Server and display go before the framebuffer is restored and closed.
        let Bench { display, server, output, .. } = bench;
        drop(server);
        drop(display);
        Ok((result, output))
    })()?;
    let restored = match (&mut output, &saved) {
        (Output::Boot(p), Some(saved)) => p.restore(saved),
        _ => Ok(()),
    };
    drop(output);
    // Printed only now: on the text console, earlier output would have been
    // drawn into the framebuffer mid-run and then overwritten by the restore.
    for line in &lines {
        println!("{line}");
    }
    match (&result, &restored) {
        (Ok(()), Ok(())) => println!("WMBENCH done status=ok"),
        (Err(error), _) => println!("WMBENCH done status=error error={error:?}"),
        (_, Err(error)) => println!("WMBENCH done status=restore-failed error={error:?}"),
    }
    result.and(restored)
}
