#![cfg(feature = "darwin-backend")]
//! Host-only wire tests of the display scale: a real Wayland client over a
//! socket pair against the real server, with a memory presenter. What
//! clients are told, what they send back, what reaches the frame, and what
//! the pointer does, at every scale and across live changes.
use std::{
    io::{Seek, SeekFrom, Write},
    os::fd::AsFd,
    os::unix::net::UnixStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    delegate_noop,
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wm_core::{Backend, Point, Size};
use wm_darwin::scale::{Scale, STEPS};
use wm_darwin::server::{ScaleStats, Server, WindowView};
use wm_darwin::smithay::reexports::wayland_server::Display;

const PANEL: Size = Size { w: 1200, h: 800 };

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    /// What the server said: wl_output.scale, preferred_buffer_scale, preferred fractional scale (120ths).
    output_scale: Option<i32>,
    buffer_scale: Option<i32>,
    fractional_scale: Option<u32>,
    configures: Vec<(i32, i32)>,
    frames: u32,
    motions: Vec<(f64, f64)>,
    /// (axis value, value120) per frame of scrolling.
    axes: Vec<f64>,
    axes120: Vec<i32>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, q: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(r.bind(name, version.min(6), q, ())),
                "wl_shm" => s.shm = Some(r.bind(name, 1, q, ())),
                "xdg_wm_base" => s.shell = Some(r.bind(name, version.min(6), q, ())),
                "wl_output" => {
                    let _: wl_output::WlOutput = r.bind(name, version.min(4), q, ());
                }
                "wl_seat" => s.seat = Some(r.bind(name, version.min(9), q, ())),
                "wp_fractional_scale_manager_v1" => s.fractional = Some(r.bind(name, 1, q, ())),
                "wp_viewporter" => s.viewporter = Some(r.bind(name, 1, q, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(_: &mut Self, p: &xdg_wm_base::XdgWmBase, e: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            p.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(_: &mut Self, p: &xdg_surface::XdgSurface, e: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = e {
            p.ack_configure(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Client {
    fn event(s: &mut Self, _: &xdg_toplevel::XdgToplevel, e: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = e {
            s.configures.push((width, height));
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for Client {
    fn event(s: &mut Self, _: &wl_output::WlOutput, e: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Scale { factor } = e {
            s.output_scale = Some(factor);
        }
    }
}
impl Dispatch<wl_surface::WlSurface, ()> for Client {
    fn event(s: &mut Self, _: &wl_surface::WlSurface, e: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_surface::Event::PreferredBufferScale { factor } = e {
            s.buffer_scale = Some(factor);
        }
    }
}
impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for Client {
    fn event(s: &mut Self, _: &wp_fractional_scale_v1::WpFractionalScaleV1, e: wp_fractional_scale_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = e {
            s.fractional_scale = Some(scale);
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for Client {
    fn event(s: &mut Self, _: &wl_callback::WlCallback, e: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = e {
            s.frames += 1;
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(s: &mut Self, seat: &wl_seat::WlSeat, e: wl_seat::Event, _: &(), _: &Connection, q: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = e {
            if caps.contains(wl_seat::Capability::Pointer) && s.pointer.is_none() {
                s.pointer = Some(seat.get_pointer(q, ()));
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(s: &mut Self, _: &wl_pointer::WlPointer, e: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } | wl_pointer::Event::Motion { surface_x, surface_y, .. } => s.motions.push((surface_x, surface_y)),
            wl_pointer::Event::Axis { value, .. } => s.axes.push(value),
            wl_pointer::Event::AxisValue120 { value120, .. } => s.axes120.push(value120),
            _ => {}
        }
    }
}
delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);
delegate_noop!(Client: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(Client: ignore wp_viewporter::WpViewporter);
delegate_noop!(Client: ignore wp_viewport::WpViewport);

struct Memory(Option<tiny_skia::Pixmap>);
impl wm_darwin::presenter::Presenter for Memory {
    fn present(&mut self, frame: &tiny_skia::Pixmap) -> Result<(), String> {
        self.0 = Some(frame.clone());
        Ok(())
    }
}

/// The server's side of things at one moment.
struct Info {
    scale: Scale,
    output_scale: i32,
    windows: Vec<WindowView>,
    stats: ScaleStats,
    pointer: Point,
    cursor: (u32, u32),
    /// The frame on the "panel", RGBA.
    frame: Vec<u8>,
}
enum Request {
    Scale(u32, mpsc::Sender<Result<bool, String>>),
    Mouse([u8; 12]),
    Info(mpsc::Sender<Info>),
    Stop,
}
struct Harness {
    tx: mpsc::Sender<Request>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Harness {
    fn new() -> (Self, Connection) {
        let (server, client) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let mut state = Server::new(&display.handle(), PANEL).unwrap();
            state.enable_sfhid_seat(&display.handle()).unwrap();
            let mut decoder = wm_darwin::hid::Decoder::default();
            Server::add_client(&mut display, server).unwrap();
            let mut out = Memory(None);
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(30) {
                display.dispatch_clients(&mut state).unwrap();
                let ms = started.elapsed().as_millis() as u32;
                state.repaint(&mut out, ms).unwrap();
                display.flush_clients().unwrap();
                match rx.try_recv() {
                    Ok(Request::Scale(v120, reply)) => reply.send(state.set_scale(Scale::from_120(v120).unwrap())).unwrap(),
                    Ok(Request::Mouse(bytes)) => state.deliver_hid(decoder.mouse(&bytes).unwrap(), ms).unwrap(),
                    Ok(Request::Info(reply)) => {
                        state.repaint(&mut out, ms).unwrap();
                        let frame = out.0.as_ref().unwrap().data().to_vec();
                        assert!(frame == state.wm.backend().render().unwrap().data(), "the presented frame is the scene");
                        reply
                            .send(Info {
                                scale: state.scale(),
                                output_scale: state.output_scale_integer(),
                                windows: state.window_views(),
                                stats: state.scale_stats(),
                                pointer: state.wm.backend().pointer_position().unwrap(),
                                cursor: state.cursor_size().unwrap_or((0, 0)),
                                frame,
                            })
                            .unwrap();
                    }
                    Ok(Request::Stop) => break,
                    Err(_) => thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        (Self { tx, thread: Some(thread) }, Connection::from_socket(client).unwrap())
    }
    fn info(&self) -> Info {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Request::Info(tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    }
    fn scale(&self, v120: u32) -> bool {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Request::Scale(v120, tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap()
    }
    fn absolute(&self, sequence: &mut u32, to: Point) {
        let unit = |v: i32, extent: u32| {
            let last = i64::from(extent - 1);
            ((i64::from(v).clamp(0, last) * 65535 + last / 2) / last) as u16
        };
        *sequence += 1;
        let mut record = [0u8; 12];
        record[..4].copy_from_slice(&sequence.to_le_bytes());
        record[6..8].copy_from_slice(&unit(to.x, PANEL.w).to_le_bytes());
        record[9] = 1;
        record[10..12].copy_from_slice(&unit(to.y, PANEL.h).to_le_bytes());
        self.tx.send(Request::Mouse(record)).unwrap();
    }
    /// A relative record: counts and a wheel step, as the trackpad sends.
    fn relative(&self, sequence: &mut u32, dx: i8, dy: i8, wheel: i8) {
        *sequence += 1;
        let mut record = [0u8; 12];
        record[..4].copy_from_slice(&sequence.to_le_bytes());
        record[6] = dx as u8;
        record[7] = dy as u8;
        record[8] = wheel as u8;
        self.tx.send(Request::Mouse(record)).unwrap();
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How the test client renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `wp_fractional_scale_v1` + `wp_viewport`: buffer = round(logical x scale).
    Fractional,
    /// `set_buffer_scale(n)` with the whole scale it was told.
    Whole,
    /// Knows nothing of scales: one buffer pixel per logical unit.
    Unaware,
}
/// A pattern that no resampling leaves intact: single pixels of four
/// colours, the position mixed into the choice.
fn pattern(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            // XRGB8888, little endian: B, G, R, x.
            bytes.extend_from_slice(match (x + 2 * y + (x / 7) * (y / 5)) % 4 {
                0 => &[255, 255, 255, 255],
                1 => &[0, 0, 0, 255],
                2 => &[0, 0, 255, 255],
                _ => &[0, 255, 0, 255],
            });
        }
    }
    bytes
}
struct Window {
    queue: wayland_client::EventQueue<Client>,
    client: Client,
    surface: wl_surface::WlSurface,
    viewport: wp_viewport::WpViewport,
    pool: wl_shm_pool::WlShmPool,
    file: std::fs::File,
    logical: (u32, u32),
}
impl Window {
    fn map(connection: &Connection, logical: (u32, u32)) -> Self {
        let mut queue = connection.new_event_queue();
        let q = queue.handle();
        let mut client = Client::default();
        connection.display().get_registry(&q, ());
        for _ in 0..3 {
            queue.roundtrip(&mut client).unwrap();
        }
        let surface = client.compositor.as_ref().unwrap().create_surface(&q, ());
        let _fractional = client.fractional.as_ref().expect("wp_fractional_scale_manager_v1 is advertised").get_fractional_scale(&surface, &q, ());
        let viewport = client.viewporter.as_ref().expect("wp_viewporter is advertised").get_viewport(&surface, &q, ());
        let xdg = client.shell.as_ref().unwrap().get_xdg_surface(&surface, &q, ());
        let _top = xdg.get_toplevel(&q, ());
        surface.commit();
        for _ in 0..3 {
            queue.roundtrip(&mut client).unwrap();
        }
        let file = tempfile::tempfile().unwrap();
        const POOL: i32 = 2400 * 1600 * 4;
        file.set_len(POOL as u64).unwrap();
        let pool = client.shm.as_ref().unwrap().create_pool(file.as_fd(), POOL, &q, ());
        Self { queue, client, surface, viewport, pool, file, logical }
    }
    /// Render and commit at the scale the server last announced. Returns the buffer size.
    fn draw(&mut self, mode: Mode) -> (u32, u32) {
        let q = self.queue.handle();
        let (lw, lh) = self.logical;
        let (width, height) = match mode {
            Mode::Fractional => {
                let scale = Scale::from_120(self.client.fractional_scale.expect("a preferred fractional scale was sent")).unwrap();
                self.surface.set_buffer_scale(1);
                self.viewport.set_destination(lw as i32, lh as i32);
                (scale.to_physical(lw as i32) as u32, scale.to_physical(lh as i32) as u32)
            }
            Mode::Whole => {
                // (A preferred buffer scale of 1 is the default and is only
                // said once it has been something else.)
                let n = self.client.output_scale.expect("wl_output.scale was sent");
                assert_eq!(self.client.buffer_scale.unwrap_or(n), n, "wl_output.scale and preferred_buffer_scale agree");
                self.surface.set_buffer_scale(n);
                self.viewport.set_destination(-1, -1);
                (lw * n as u32, lh * n as u32)
            }
            Mode::Unaware => {
                self.surface.set_buffer_scale(1);
                self.viewport.set_destination(-1, -1);
                (lw, lh)
            }
        };
        self.file.seek(SeekFrom::Start(0)).unwrap();
        self.file.write_all(&pattern(width, height)).unwrap();
        let buffer = self.pool.create_buffer(0, width as i32, height as i32, width as i32 * 4, wl_shm::Format::Xrgb8888, &q, ());
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage_buffer(0, 0, width as i32, height as i32);
        self.surface.frame(&q, ());
        self.surface.commit();
        for _ in 0..4 {
            self.queue.roundtrip(&mut self.client).unwrap();
        }
        (width, height)
    }
    fn settle(&mut self) {
        for _ in 0..4 {
            self.queue.roundtrip(&mut self.client).unwrap();
        }
    }
}
/// The frame's pixels where the window's content is, as RGB.
fn content_pixels(info: &Info, view: &WindowView) -> Vec<[u8; 3]> {
    let mut out = Vec::new();
    for y in 0..view.content.size.h {
        for x in 0..view.content.size.w {
            let at = (((view.content.pos.y as u32 + y) * PANEL.w + view.content.pos.x as u32 + x) * 4) as usize;
            out.push([info.frame[at], info.frame[at + 1], info.frame[at + 2]]);
        }
    }
    out
}
/// The client's pattern as RGB.
fn pattern_rgb(width: u32, height: u32) -> Vec<[u8; 3]> {
    pattern(width, height).as_chunks::<4>().0.iter().map(|p| [p[2], p[1], p[0]]).collect()
}

#[test]
fn clients_are_told_the_scale_and_a_buffer_at_it_reaches_the_frame_untouched() {
    let (h, connection) = Harness::new();
    let mut window = Window::map(&connection, (201, 123));
    // The pointer image out of the way of the window's pixels.
    h.absolute(&mut 0, Point::new(1150, 780));
    // Before any change: 100%.
    assert_eq!((window.client.output_scale, window.client.buffer_scale, window.client.fractional_scale), (Some(1), None, Some(120)));
    window.draw(Mode::Fractional);
    let home = h.info().windows[0].content.pos;
    let mut exact = h.info().stats.exact_commits;
    let mut resampled = 0;
    // Up through every step, and down again: each change is live.
    for step in STEPS.into_iter().chain(STEPS.into_iter().rev()) {
        let scale = Scale::from_120(step).unwrap();
        let changed = h.scale(step);
        window.settle();
        // All three ways of telling a client agree.
        assert_eq!(window.client.fractional_scale, Some(step), "{step}: wp_fractional_scale_v1.preferred_scale");
        assert_eq!(window.client.output_scale, Some(scale.integer()), "{step}: wl_output.scale");
        if step != 120 || window.client.buffer_scale.is_some() {
            assert_eq!(window.client.buffer_scale, Some(scale.integer()), "{step}: wl_surface.preferred_buffer_scale");
        }
        // The window was asked to keep its logical size.
        // The window fits the panel at every step here, so nothing asks the
        // client for a size: it was left to choose and still is.
        let _ = changed;
        assert_eq!(window.client.configures.last().copied(), Some((0, 0)), "{step}");
        // Before the client answers, the window already has its new size on the panel.
        let before = h.info();
        assert_eq!(before.scale, scale);
        assert_eq!(before.output_scale, scale.integer());
        assert_eq!(before.windows[0].content.size, scale.size_to_physical(Size::new(201, 123)), "{step}");
        assert_eq!(before.cursor, (scale.to_physical(12) as u32, scale.to_physical(18) as u32), "{step}: pointer image");
        // The client renders at the scale: its pixels are the frame's pixels.
        let buffer = window.draw(Mode::Fractional);
        let info = h.info();
        let view = info.windows[0];
        assert_eq!(view.logical, Size::new(201, 123));
        assert_eq!((view.image.w, view.image.h), buffer, "{step}: the buffer is held as it came");
        assert_eq!(view.content.size, view.image, "{step}: the window is its client's buffer");
        // The window keeps its logical place (its frame, drawn at the new
        // scale, may take a few pixels more above it than at 100%).
        let place = Point::new(scale.to_physical(Scale::ONE.to_logical(home.x)), scale.to_physical(Scale::ONE.to_logical(home.y)));
        assert!((view.content.pos.x - place.x).abs() <= scale.to_physical(4) && (view.content.pos.y - place.y).abs() <= scale.to_physical(12), "{step}: at {:?}, logical place {place:?}", view.content.pos);
        assert!(content_pixels(&info, &view) == pattern_rgb(buffer.0, buffer.1), "{step}: the frame shows the client's pixels one for one");
        assert_eq!(info.stats.resampled_commits, resampled, "{step}: nothing was resampled");
        assert!(info.stats.exact_commits > exact, "{step}");
        // A client that knows only whole scales is exact on whole steps...
        let whole = window.draw(Mode::Whole);
        let info = h.info();
        if step % 120 == 0 {
            assert_eq!(info.stats.resampled_commits, resampled, "{step}");
            assert!(content_pixels(&info, &info.windows[0]) == pattern_rgb(whole.0, whole.1), "{step}: whole-scale buffer one for one");
        } else {
            // ...and is brought down to the surface's size in between.
            resampled += 1;
            assert_eq!(info.stats.resampled_commits, resampled, "{step}");
            assert_eq!(info.windows[0].content.size, scale.size_to_physical(Size::new(201, 123)), "{step}");
        }
        exact = h.info().stats.exact_commits;
    }
}

#[test]
fn a_client_that_ignores_the_scale_is_enlarged_not_shrunk() {
    let (h, connection) = Harness::new();
    let mut window = Window::map(&connection, (64, 40));
    h.absolute(&mut 0, Point::new(1150, 780));
    window.draw(Mode::Unaware);
    h.scale(240);
    window.settle();
    window.draw(Mode::Unaware);
    let info = h.info();
    let view = info.windows[0];
    // 64x40 logical is 128x80 on the panel: every pixel of the client's, four times.
    assert_eq!((view.logical, view.content.size, view.image), (Size::new(64, 40), Size::new(128, 80), Size::new(128, 80)));
    assert_eq!(info.stats.resampled_commits, 1);
    let source = pattern_rgb(64, 40);
    let shown = content_pixels(&info, &view);
    for y in 0..80u32 {
        for x in 0..128u32 {
            assert_eq!(shown[(y * 128 + x) as usize], source[((y / 2) * 64 + x / 2) as usize], "{x},{y}");
        }
    }
    // At a fractional scale it is filtered to the surface's size: 96x60.
    h.scale(180);
    window.settle();
    window.draw(Mode::Unaware);
    let info = h.info();
    assert_eq!((info.windows[0].content.size, info.windows[0].image), (Size::new(96, 60), Size::new(96, 60)));
}

#[test]
fn the_pointer_covers_the_same_panel_distance_at_every_scale_and_clients_get_logical_units() {
    let (h, connection) = Harness::new();
    let mut window = Window::map(&connection, (300, 200));
    window.draw(Mode::Fractional);
    let mut sequence = 0;
    // A hand's movement: slow single counts, a diagonal, fast strokes, a reversal.
    let strokes: Vec<(i8, i8)> = std::iter::repeat_n((1, 0), 9)
        .chain(std::iter::repeat_n((0, 1), 7))
        .chain(std::iter::repeat_n((3, -2), 15))
        .chain(std::iter::repeat_n((-1, 1), 11))
        .chain([(127, 64), (-128, -90), (40, 33), (-5, -7)])
        .collect();
    let total: (i32, i32) = strokes.iter().fold((0, 0), |sum, (x, y)| (sum.0 + i32::from(*x), sum.1 + i32::from(*y)));
    let mut ends = Vec::new();
    let mut wheels = Vec::new();
    for step in [120, 150, 180, 240, 360] {
        let scale = Scale::from_120(step).unwrap();
        h.scale(step);
        window.settle();
        window.draw(Mode::Fractional);
        // From the same place on the panel...
        h.absolute(&mut sequence, Point::new(500, 400));
        let start = h.info().pointer;
        for (dx, dy) in &strokes {
            h.relative(&mut sequence, *dx, *dy, 0);
        }
        // ...the same counts end at the same place on the panel, to the pixel.
        let end = h.info().pointer;
        assert_eq!((end.x - start.x, end.y - start.y), total, "{step}: panel pixels moved");
        ends.push(end);
        // Over the window, the client is told logical units: panel pixels over the scale.
        let view = h.info().windows[0];
        let inside = Point::new(view.content.pos.x + 37, view.content.pos.y + 23);
        window.client.motions.clear();
        h.absolute(&mut sequence, inside);
        h.info();
        window.settle();
        let at = h.info().pointer;
        let told = *window.client.motions.last().unwrap_or_else(|| panic!("{step}: the client saw no pointer motion"));
        // Back on the panel, what the client was told is inside the very
        // pixel the pointer is on (the position has a fraction; wl_fixed
        // keeps 1/256 of a unit).
        let panel = (told.0 * scale.factor(), told.1 * scale.factor());
        let pixel = (f64::from(at.x - view.content.pos.x), f64::from(at.y - view.content.pos.y));
        let slack = scale.factor() / 256.0;
        assert!(panel.0 >= pixel.0 - slack && panel.0 < pixel.0 + 1.0 + slack && panel.1 >= pixel.1 - slack && panel.1 < pixel.1 + 1.0 + slack, "{step}: told {told:?} = panel {panel:?}, pointer on {pixel:?}");
        // One wheel step is the same scroll at every scale.
        window.client.axes.clear(); window.client.axes120.clear();
        h.relative(&mut sequence, 0, 0, 1);
        h.info();
        window.settle();
        wheels.push((window.client.axes.clone(), window.client.axes120.clone()));
    }
    assert!(ends.windows(2).all(|pair| pair[0] == pair[1]), "{ends:?}");
    assert_eq!(wheels[0], (vec![-10.0], vec![-120]), "one step up is ten units, one detent");
    assert!(wheels.windows(2).all(|pair| pair[0] == pair[1]), "{wheels:?}");
}

#[test]
fn a_window_too_large_for_the_panel_at_a_scale_is_shrunk_and_gets_its_size_back() {
    let (h, connection) = Harness::new();
    // 500x300 logical: fits the 1200x800 panel up to 200%, not at 250% or 300%.
    let mut window = Window::map(&connection, (500, 300));
    h.absolute(&mut 0, Point::new(1190, 790));
    window.draw(Mode::Fractional);
    // The client follows a configure that names a size, as a terminal does.
    let follow = |window: &mut Window| {
        if let Some((w, h)) = window.client.configures.last().copied().filter(|c| c.0 > 0 && c.1 > 0) {
            window.logical = (w as u32, h as u32);
        }
    };
    let mut at_two = Size::new(0, 0);
    for step in [240, 300, 360, 300, 240, 120] {
        let scale = Scale::from_120(step).unwrap();
        h.scale(step);
        window.settle();
        follow(&mut window);
        window.draw(Mode::Fractional);
        window.settle();
        let view = h.info().windows[0];
        assert_eq!(view.content.size, view.image, "{step}: the frame fits the client");
        assert!(view.content.size.w <= PANEL.w && view.content.size.h <= PANEL.h, "{step}: {:?} is on the panel", view.content.size);
        match step {
            // Room enough: its own size, never asked for another.
            240 if at_two == Size::new(0, 0) => {
                assert_eq!((view.logical, window.client.configures.last().copied()), (Size::new(500, 300), Some((0, 0))));
                at_two = view.content.size;
            }
            // Too large for the panel: asked to be smaller, in logical units.
            300 | 360 => {
                assert!(view.logical.w < 500 || view.logical.h < 300, "{step}: {:?}", view.logical);
                assert!(f64::from(view.logical.w) * scale.factor() <= f64::from(PANEL.w) + 1.0, "{step}");
            }
            // Room again: exactly the size it had at this scale before.
            240 => assert_eq!((view.logical, view.content.size), (Size::new(500, 300), at_two), "back at 200%"),
            _ => assert_eq!(view.logical, Size::new(500, 300), "{step}: back to its own size"),
        }
    }
}
