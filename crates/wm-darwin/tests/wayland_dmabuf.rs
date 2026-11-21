#![cfg(feature = "darwin-backend")]
//! Host wire test of the server's linux-dmabuf-v1 side. The renderer here is
//! a stand-in that records imports instead of making EGL images (the host
//! has no asahi buffers), and the "dma-bufs" are ordinary files: what is
//! tested is the protocol and the buffer lifetime, not GPU import.
use std::{
    os::fd::AsFd,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    delegate_noop,
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_registry, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wm_core::Size;
use wm_darwin::presenter::Presenter;
use wm_darwin::render::{DmabufDesc, FrameStats, Renderer, RendererKind};
use wm_darwin::scene::CpuBackend;
use wm_darwin::server::Server;
use wm_darwin::smithay::reexports::wayland_server::Display;

const XR24: u32 = 0x3432_5258;
const AR24: u32 = 0x3432_5241;
const NV12: u32 = 0x3231_564e;

#[derive(Default)]
struct Log {
    /// (id, width, height, format, modifier, offset, stride) of accepted imports.
    imported: Vec<(u64, u32, u32, u32, u64, u32, u32)>,
    released: Vec<u64>,
    frames: u32,
}
/// Accepts linear XR24/AR24 like the GL renderer, draws with the CPU renderer.
struct Importing {
    inner: Box<dyn Renderer>,
    log: Arc<Mutex<Log>>,
    next: u64,
}
impl Renderer for Importing {
    fn kind(&self) -> RendererKind {
        RendererKind::Gl
    }
    fn render_frame(&mut self, scene: &mut CpuBackend, presenter: &mut dyn Presenter) -> Result<FrameStats, String> {
        self.log.lock().unwrap().frames += 1;
        self.inner.render_frame(scene, presenter)
    }
    fn invalidate(&mut self) {
        self.inner.invalidate()
    }
    fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        vec![(XR24, 0), (AR24, 0)]
    }
    fn import_dmabuf(&mut self, b: &DmabufDesc) -> Result<u64, String> {
        if !self.dmabuf_formats().contains(&(b.format, b.modifier)) || b.stride < b.width * 4 {
            return Err("refused by the test renderer".into());
        }
        assert!(b.fd >= 0);
        self.next += 1;
        self.log.lock().unwrap().imported.push((self.next, b.width, b.height, b.format, b.modifier, b.offset, b.stride));
        Ok(self.next)
    }
    fn release_dmabuf(&mut self, id: u64) {
        self.log.lock().unwrap().released.push(id);
    }
    // A frame of the harness's 640x480 output that is black everywhere: what
    // a renderer would show for all-zero client buffers on a black screen.
    fn read_back(&mut self) -> Result<Option<Vec<u8>>, String> {
        Ok(Some([0, 0, 0, 255].repeat(640 * 480)))
    }
}
struct Sink;
impl Presenter for Sink {
    fn present(&mut self, _: &tiny_skia::Pixmap) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    dmabuf: Option<(zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, u32)>,
    modifiers: Vec<(u32, u64)>,
    configured: bool,
    frames: u32,
    /// Buffer tags in release order.
    released: Vec<u32>,
    created: u32,
    failed: u32,
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(s: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, q: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = e {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(r.bind(name, version.min(5), q, ())),
                "xdg_wm_base" => s.shell = Some(r.bind(name, version.min(6), q, ())),
                "zwp_linux_dmabuf_v1" => s.dmabuf = Some((r.bind(name, version.min(3), q, ()), version)),
                _ => {}
            }
        }
    }
}
impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for Client {
    fn event(s: &mut Self, _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, e: zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwp_linux_dmabuf_v1::Event::Modifier { format, modifier_hi, modifier_lo } = e {
            s.modifiers.push((format, (u64::from(modifier_hi) << 32) | u64::from(modifier_lo)));
        }
    }
}
impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for Client {
    fn event(s: &mut Self, _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, e: zwp_linux_buffer_params_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            zwp_linux_buffer_params_v1::Event::Created { .. } => s.created += 1,
            zwp_linux_buffer_params_v1::Event::Failed => s.failed += 1,
            _ => {}
        }
    }
    wayland_client::event_created_child!(Client, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, 99),
    ]);
}
impl Dispatch<wl_buffer::WlBuffer, u32> for Client {
    fn event(s: &mut Self, _: &wl_buffer::WlBuffer, e: wl_buffer::Event, tag: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = e {
            s.released.push(*tag);
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
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(_: &mut Self, p: &xdg_wm_base::XdgWmBase, e: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            p.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(s: &mut Self, x: &xdg_surface::XdgSurface, e: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = e {
            x.ack_configure(serial);
            s.configured = true;
        }
    }
}
delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore xdg_toplevel::XdgToplevel);

/// Latest content check (window, hash, result) and the dma-buf counters.
type Checks = (Option<(u64, u64, u8)>, wm_darwin::server::DmabufStats);
struct Harness {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    log: Arc<Mutex<Log>>,
    /// (visible windows, imported dma-bufs alive) as the server sees them.
    state: Arc<Mutex<(usize, usize)>>,
    /// Latest content check and the dma-buf counters.
    checks: Arc<Mutex<Checks>>,
}
impl Harness {
    fn start(importing: bool) -> (Self, Connection) {
        let (server, client) = UnixStream::pair().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let log = Arc::new(Mutex::new(Log::default()));
        let state = Arc::new(Mutex::new((0, 0)));
        let checks = Arc::new(Mutex::new((None, Default::default())));
        let checks2 = checks.clone();
        let (stop2, log2, state2) = (stop.clone(), log.clone(), state.clone());
        let handle = thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let mut server_state = Server::new(&display.handle(), Size::new(640, 480)).unwrap();
            if importing {
                server_state.set_renderer_boxed(Box::new(Importing { inner: RendererKind::Damage.create_cpu(), log: log2, next: 0 }));
            }
            assert_eq!(server_state.enable_dmabuf(&display.handle()).unwrap(), importing);
            Server::add_client(&mut display, server).unwrap();
            let started = Instant::now();
            while !stop2.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(20) {
                display.dispatch_clients(&mut server_state).unwrap();
                server_state.repaint(&mut Sink, started.elapsed().as_millis() as u32).unwrap();
                display.flush_clients().unwrap();
                let backend = server_state.wm.backend();
                *state2.lock().unwrap() =
                    (backend.windows.keys().filter(|id| backend.visible(**id)).count(), server_state.external_buffer_count());
                *checks2.lock().unwrap() = (server_state.last_dmabuf_check(), server_state.dmabuf_stats());
                thread::sleep(Duration::from_millis(1));
            }
        });
        (Self { stop, thread: Some(handle), log, state, checks }, Connection::from_socket(client).unwrap())
    }
    fn wait(&self, what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn fake_dmabuf(bytes: usize) -> std::fs::File {
    let file = tempfile::tempfile().unwrap();
    file.set_len(bytes as u64).unwrap();
    file
}

#[test]
fn cpu_renderers_do_not_advertise_linux_dmabuf() {
    let (_h, connection) = Harness::start(false);
    let mut queue = connection.new_event_queue();
    let mut client = Client::default();
    connection.display().get_registry(&queue.handle(), ());
    queue.roundtrip(&mut client).unwrap();
    assert!(client.compositor.is_some());
    assert!(client.dmabuf.is_none());
}

#[test]
fn dmabuf_buffers_are_imported_shown_without_copy_and_released_when_replaced() {
    let (h, connection) = Harness::start(true);
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut client = Client::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut client).unwrap();
    let (dmabuf, version) = client.dmabuf.clone().expect("zwp_linux_dmabuf_v1 advertised");
    assert_eq!(version, 3);
    queue.roundtrip(&mut client).unwrap();
    client.modifiers.sort_unstable();
    assert_eq!(client.modifiers, [(AR24, 0), (XR24, 0)]);

    let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = client.shell.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let _top = xdg.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    assert!(client.configured);

    let (w, hgt, stride) = (200i32, 120i32, 832u32); // stride wider than the row
    let files: Vec<_> = (0..2).map(|_| fake_dmabuf(stride as usize * hgt as usize)).collect();
    let make = |tag: u32, format: u32, file: &std::fs::File| {
        let params = dmabuf.create_params(&qh, ());
        params.add(file.as_fd(), 0, 0, stride, 0, 0);
        let buffer = params.create_immed(w, hgt, format, zwp_linux_buffer_params_v1::Flags::empty(), &qh, tag);
        params.destroy();
        buffer
    };
    let first = make(1, XR24, &files[0]);
    let second = make(2, AR24, &files[1]);
    queue.roundtrip(&mut client).unwrap();
    {
        let log = h.log.lock().unwrap();
        assert_eq!(log.imported, [(1, 200, 120, XR24, 0, 0, 832), (2, 200, 120, AR24, 0, 0, 832)]);
        assert!(log.released.is_empty());
    }

    // First buffer: the window maps; the frame callback fires; the buffer is
    // NOT released, because the compositor shows the client's own memory.
    let commit = |buffer: &wl_buffer::WlBuffer| {
        surface.attach(Some(buffer), 0, 0);
        surface.damage_buffer(0, 0, w, hgt);
        surface.frame(&qh, ());
        surface.commit();
    };
    commit(&first);
    let mut pump = |client: &mut Client, frames: u32| {
        let started = Instant::now();
        while client.frames < frames {
            queue.roundtrip(client).unwrap();
            assert!(started.elapsed() < Duration::from_secs(5), "frame callback {frames} did not arrive");
            thread::sleep(Duration::from_millis(2));
        }
        queue.roundtrip(client).unwrap();
    };
    pump(&mut client, 1);
    h.wait("the window to become visible", || h.state.lock().unwrap().0 == 1);
    assert!(client.released.is_empty(), "a dma-buf on screen must stay held");

    // Second buffer replaces it: the first is released after the next frame.
    commit(&second);
    pump(&mut client, 2);
    assert_eq!(client.released, [1]);
    // Back to the first, and the same buffer committed twice in a row.
    commit(&first);
    pump(&mut client, 3);
    assert_eq!(client.released, [1, 2]);
    commit(&first);
    pump(&mut client, 4);
    assert_eq!(client.released, [1, 2], "re-committing the buffer on screen releases nothing");
    assert_eq!(h.state.lock().unwrap().1, 2);

    // Destroying a wl_buffer that is not on screen frees its texture at once;
    // the one on screen keeps its texture until the surface lets go of it.
    second.destroy();
    queue.roundtrip(&mut client).unwrap();
    h.wait("texture 2 to be freed", || h.log.lock().unwrap().released == [2]);
    first.destroy();
    queue.roundtrip(&mut client).unwrap();
    thread::sleep(Duration::from_millis(30));
    assert_eq!(h.log.lock().unwrap().released, [2], "the buffer on screen is still sampled");
    assert_eq!(*h.state.lock().unwrap(), (1, 1));
    surface.attach(None, 0, 0);
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    h.wait("texture 1 to be freed", || h.log.lock().unwrap().released == [2, 1]);
    h.wait("the window to unmap", || *h.state.lock().unwrap() == (0, 0));
}

#[test]
fn unsupported_dmabuf_descriptions_fail_without_a_buffer() {
    let (h, connection) = Harness::start(true);
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut client = Client::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut client).unwrap();
    let (dmabuf, _) = client.dmabuf.clone().unwrap();
    let file = fake_dmabuf(4096 * 64);
    // `create` answers with created/failed instead of a protocol error.
    let attempt = |format: u32, stride: u32, modifier: u64, flags: zwp_linux_buffer_params_v1::Flags| {
        let params = dmabuf.create_params(&qh, ());
        params.add(file.as_fd(), 0, 0, stride, (modifier >> 32) as u32, modifier as u32);
        params.create(64, 64, format, flags);
        params
    };
    let none = zwp_linux_buffer_params_v1::Flags::empty();
    let _ok = attempt(XR24, 256, 0, none);
    queue.roundtrip(&mut client).unwrap();
    assert_eq!((client.created, client.failed), (1, 0));
    // Refused by the renderer (stride, a modifier it does not import) or by
    // the flags: `failed`, and the client lives on.
    let _narrow = attempt(XR24, 100, 0, none);
    let _tiled = attempt(XR24, 256, 0x0c00_0000_0000_0002, none);
    let _flipped = attempt(XR24, 256, 0, zwp_linux_buffer_params_v1::Flags::YInvert);
    for _ in 0..3 {
        queue.roundtrip(&mut client).unwrap();
    }
    assert_eq!((client.created, client.failed), (1, 3));
    assert_eq!(h.log.lock().unwrap().imported.len(), 1);
}

#[test]
fn an_unadvertised_format_is_a_protocol_error() {
    let (_h, connection) = Harness::start(true);
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut client = Client::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut client).unwrap();
    let (dmabuf, _) = client.dmabuf.clone().unwrap();
    let file = fake_dmabuf(4096 * 64);
    let params = dmabuf.create_params(&qh, ());
    params.add(file.as_fd(), 0, 0, 256, 0, 0);
    params.create(64, 64, NV12, zwp_linux_buffer_params_v1::Flags::empty());
    let error = (0..3).find_map(|_| queue.roundtrip(&mut client).err()).expect("protocol error");
    assert!(format!("{error:?}").contains("code: 4"), "{error:?}");
}

/// The compositor-side proof that a client's memory is what was rendered: at
/// fixed commit counts the window region of the renderer's frame is compared
/// with the dma-buf read through a CPU mapping.
#[test]
fn window_content_is_checked_against_the_clients_buffer() {
    use std::io::{Seek, SeekFrom, Write};
    let (h, connection) = Harness::start(true);
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut client = Client::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut client).unwrap();
    let (dmabuf, _) = client.dmabuf.clone().unwrap();
    let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = client.shell.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let _top = xdg.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    let (w, hgt, stride) = (64i32, 48i32, 272u32);
    let mut files: Vec<_> = (0..2).map(|_| fake_dmabuf(stride as usize * hgt as usize)).collect();
    let buffers: Vec<_> = files
        .iter()
        .enumerate()
        .map(|(tag, file)| {
            let params = dmabuf.create_params(&qh, ());
            params.add(file.as_fd(), 0, 0, stride, 0, 0);
            let buffer = params.create_immed(w, hgt, XR24, zwp_linux_buffer_params_v1::Flags::empty(), &qh, tag as u32);
            params.destroy();
            buffer
        })
        .collect();
    let mut commits = 0u32;
    let mut commit_until = |client: &mut Client, total: u32| {
        while commits < total {
            surface.attach(Some(&buffers[commits as usize % 2]), 0, 0);
            surface.damage_buffer(0, 0, w, hgt);
            surface.frame(&qh, ());
            surface.commit();
            commits += 1;
            let started = Instant::now();
            while client.frames < commits {
                queue.roundtrip(client).unwrap();
                assert!(started.elapsed() < Duration::from_secs(5), "frame callback {commits}");
                thread::sleep(Duration::from_millis(1));
            }
        }
    };
    // All-zero buffers on the renderer double's black frame: equal at commit 30.
    commit_until(&mut client, 30);
    h.wait("the check at commit 30", || h.checks.lock().unwrap().0.is_some());
    assert_eq!(h.checks.lock().unwrap().0, Some((30, 0, 0)));
    // Three pixels of the buffer shown at commit 150 (index 1) now differ
    // from what the "renderer" shows; padding bytes beyond the row do not count.
    let pixel = |x: u64, y: u64| y * u64::from(stride) + x * 4;
    for (x, y, bgrx) in [(0, 0, [9u8, 0, 0, 0]), (63, 47, [0, 200, 0, 0]), (10, 5, [0, 0, 77, 255])] {
        files[1].seek(SeekFrom::Start(pixel(x, y))).unwrap();
        files[1].write_all(&bgrx).unwrap();
    }
    files[1].seek(SeekFrom::Start(pixel(64, 3))).unwrap();
    files[1].write_all(&[255; 4]).unwrap();
    commit_until(&mut client, 150);
    h.wait("the check at commit 150", || h.checks.lock().unwrap().0.is_some_and(|c| c.0 == 150));
    assert_eq!(h.checks.lock().unwrap().0, Some((150, 3, 200)));
    let stats = h.checks.lock().unwrap().1;
    assert_eq!((stats.imported, stats.refused, stats.commits), (2, 0, 150));
    // Regular files are always readable, so nothing ever waits on a fence here.
    assert_eq!((stats.fence_waits, stats.fence_timeouts, stats.fence_poll_errors), (0, 0, 0));
}
