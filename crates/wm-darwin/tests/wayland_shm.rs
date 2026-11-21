#![cfg(feature = "darwin-backend")]
//! Host-only real Wayland wire test; its memory presenter is NOT native evidence.
use std::{
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsFd,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface, wl_seat, wl_keyboard, wl_pointer,
        wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::{
    decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1},
    shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use wm_core::Size;
use wm_darwin::smithay::reexports::wayland_server::Display;
use wm_darwin::{presenter::Presenter, server::Server};

#[derive(Default)]
struct Client {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    shell: Option<xdg_wm_base::XdgWmBase>,
    decoration: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    releases: u32,
    frames: u32,
    configures: u32,
    server_decor: bool,
    globals: Vec<String>,
    seat: Option<wl_seat::WlSeat>, keyboard:Option<wl_keyboard::WlKeyboard>,pointer:Option<wl_pointer::WlPointer>,
    keys:Vec<(u32,wayland_client::WEnum<wl_keyboard::KeyState>)>,
    buttons:Vec<(u32,wayland_client::WEnum<wl_pointer::ButtonState>)>,
    keymap:String,keyboard_enters:u32,pointer_serial:Option<u32>,button_serial:Option<u32>,modifier_masks:Vec<u32>,closes:u32,
    held_on_enter:Vec<Vec<u32>>,keyboard_leaves:u32,
    /// Sizes from xdg_toplevel.configure, in arrival order (0,0 = client's choice).
    configured_sizes:Vec<(i32,i32)>,
    /// wl_pointer.axis events received (wheel steps).
    axis_events:u32,
    /// Clipboard: the device, the offer that is the current selection with
    /// its types, and requests to send what this client offered.
    data_manager:Option<wl_data_device_manager::WlDataDeviceManager>,
    data_device:Option<wl_data_device::WlDataDevice>,
    offered:Vec<(wl_data_offer::WlDataOffer,Vec<String>)>,
    selection:Option<wl_data_offer::WlDataOffer>,
    selection_events:u32,
    send_requests:Vec<(String,std::os::fd::OwnedFd)>,
    source_cancelled:u32,
}
impl Client {
    fn data_device(&mut self,q:&QueueHandle<Self>){
        if let (None,Some(manager),Some(seat))=(&self.data_device,&self.data_manager,&self.seat){
            self.data_device=Some(manager.get_data_device(seat,q,()));
        }
    }
}
delegate_noop!(Client: ignore wl_data_device_manager::WlDataDeviceManager);
impl Dispatch<wl_data_device::WlDataDevice,()> for Client {
    fn event(s:&mut Self,_:&wl_data_device::WlDataDevice,e:wl_data_device::Event,_:&(),_:&Connection,_:&QueueHandle<Self>){
        match e{
            wl_data_device::Event::DataOffer{id}=>s.offered.push((id,Vec::new())),
            wl_data_device::Event::Selection{id}=>{s.selection=id;s.selection_events+=1;}
            _=>{}
        }
    }
    wayland_client::event_created_child!(Client, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}
impl Dispatch<wl_data_offer::WlDataOffer,()> for Client {
    fn event(s:&mut Self,offer:&wl_data_offer::WlDataOffer,e:wl_data_offer::Event,_:&(),_:&Connection,_:&QueueHandle<Self>){
        if let wl_data_offer::Event::Offer{mime_type}=e{
            if let Some((_,types))=s.offered.iter_mut().find(|(o,_)|o==offer){types.push(mime_type);}
        }
    }
}
impl Dispatch<wl_data_source::WlDataSource,()> for Client {
    fn event(s:&mut Self,_:&wl_data_source::WlDataSource,e:wl_data_source::Event,_:&(),_:&Connection,_:&QueueHandle<Self>){
        match e{
            wl_data_source::Event::Send{mime_type,fd}=>s.send_requests.push((mime_type,fd)),
            wl_data_source::Event::Cancelled=>s.source_cancelled+=1,
            _=>{}
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = e
        {
            s.globals.push(interface.clone());
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(r.bind(name, version.min(5), q, ())),
                "wl_shm" => s.shm = Some(r.bind(name, 1, q, ())),
                "xdg_wm_base" => s.shell = Some(r.bind(name, version.min(6), q, ())),
                "zxdg_decoration_manager_v1" => s.decoration = Some(r.bind(name, 1, q, ())),
                "wl_output" => {
                    let _: wl_output::WlOutput = r.bind(name, version.min(4), q, ());
                }
                "wl_seat" => {s.seat=Some(r.bind(name,version.min(9),q,()));s.data_device(q);}
                "wl_data_device_manager" => {s.data_manager=Some(r.bind(name,version.min(3),q,()));s.data_device(q);}
                _ => {}
            }
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
    fn event(
        _: &mut Self,
        p: &xdg_wm_base::XdgWmBase,
        e: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            p.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for Client {
    fn event(
        s: &mut Self,
        p: &xdg_surface::XdgSurface,
        e: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = e {
            s.configures += 1;
            p.ack_configure(serial);
        }
    }
}
impl Dispatch<wl_buffer::WlBuffer, ()> for Client {
    fn event(
        s: &mut Self,
        _: &wl_buffer::WlBuffer,
        e: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = e {
            s.releases += 1;
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for Client {
    fn event(
        s: &mut Self,
        _: &wl_callback::WlCallback,
        e: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = e {
            s.frames += 1;
        }
    }
}
impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for Client {
    fn event(
        s: &mut Self,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        e: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = e {
            s.server_decor =
                mode == wayland_client::WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        }
    }
}
delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_output::WlOutput);
impl Dispatch<xdg_toplevel::XdgToplevel,()> for Client {
    fn event(s:&mut Self,_:&xdg_toplevel::XdgToplevel,e:xdg_toplevel::Event,_:&(),_:&Connection,_:&QueueHandle<Self>){match e{xdg_toplevel::Event::Close=>s.closes+=1,xdg_toplevel::Event::Configure{width,height,..}=>s.configured_sizes.push((width,height)),_=>{}}}
}
impl Dispatch<wl_seat::WlSeat,()> for Client {
    fn event(s:&mut Self,seat:&wl_seat::WlSeat,e:wl_seat::Event,_:&(),_:&Connection,q:&QueueHandle<Self>){
        if let wl_seat::Event::Capabilities{capabilities:wayland_client::WEnum::Value(caps)}=e{
            if caps.contains(wl_seat::Capability::Keyboard) && s.keyboard.is_none(){s.keyboard=Some(seat.get_keyboard(q,()));}
            if caps.contains(wl_seat::Capability::Pointer) && s.pointer.is_none(){s.pointer=Some(seat.get_pointer(q,()));}
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard,()> for Client {
    fn event(s:&mut Self,_:&wl_keyboard::WlKeyboard,e:wl_keyboard::Event,_:&(),_:&Connection,_:&QueueHandle<Self>){
        match e{
            wl_keyboard::Event::Keymap{fd,size,..}=>{let mut bytes=Vec::new();std::fs::File::from(fd).take(size.into()).read_to_end(&mut bytes).unwrap();s.keymap=String::from_utf8(bytes).unwrap();},
            wl_keyboard::Event::Key{key,state,..}=>s.keys.push((key,state)),
            wl_keyboard::Event::Enter{keys,..}=>{s.keyboard_enters+=1;s.held_on_enter.push(keys.as_chunks::<4>().0.iter().map(|b|u32::from_ne_bytes(*b)).collect());},
            wl_keyboard::Event::Leave{..}=>s.keyboard_leaves+=1,
            wl_keyboard::Event::Modifiers{mods_depressed,..}=>s.modifier_masks.push(mods_depressed),_=>{}
        }
    }
}
impl Dispatch<wl_pointer::WlPointer,()> for Client {
    fn event(s:&mut Self,_:&wl_pointer::WlPointer,e:wl_pointer::Event,_:&(),_:&Connection,_:&QueueHandle<Self>){
        match e{wl_pointer::Event::Enter{serial,..}=>s.pointer_serial=Some(serial),wl_pointer::Event::Button{button,state,serial,..}=>{s.button_serial=Some(serial);s.buttons.push((button,state));},wl_pointer::Event::Axis{..}=>s.axis_events+=1,_=>{}}
    }
}
delegate_noop!(Client: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);

struct MemoryOutput {
    frame: Option<tiny_skia::Pixmap>,
    reject: bool,
}
impl Presenter for MemoryOutput {
    fn present(&mut self, f: &tiny_skia::Pixmap) -> Result<(), String> {
        if self.reject {
            return Err("host test rejection".into());
        }
        self.frame = Some(f.clone());
        Ok(())
    }
    // Like a real screen: only the damaged pixels are taken from the frame, so
    // a missing damage rectangle leaves visibly stale pixels in snapshots.
    fn present_damage(&mut self, f: &tiny_skia::Pixmap, damage: &[wm_darwin::damage::DRect]) -> Result<(), String> {
        if self.reject {
            return Err("host test rejection".into());
        }
        let screen = self
            .frame
            .get_or_insert_with(|| tiny_skia::Pixmap::new(f.width(), f.height()).unwrap());
        let width = f.width() as usize;
        for r in damage {
            for y in r.y0 as usize..r.y1 as usize {
                let (from, to) = ((y * width + r.x0 as usize) * 4, (y * width + r.x1 as usize) * 4);
                screen.data_mut()[from..to].copy_from_slice(&f.data()[from..to]);
            }
        }
        Ok(())
    }
}
#[derive(Debug)]
struct Snapshot {
    managed: usize,
    visible: usize,
    content: Option<wm_core::Rect>,
    pixels: Vec<u8>,
    theme_pixels: usize,
    titlebar:Option<wm_core::Point>,close_button:Option<wm_core::Point>,
    /// A point inside the bottom-right resize handle of the focused window.
    resize_corner:Option<wm_core::Point>,
}
enum Request {
    Snapshot(mpsc::Sender<Snapshot>),
    Reject(bool),
    Workspace(usize),
    Keyboard([u8;8]),Mouse([u8;12]),
    Stop,
}
struct Harness {
    tx: mpsc::Sender<Request>,
    thread: Option<thread::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
}
impl Harness {
    fn new() -> (Self, Connection) {
        Self::with_input(false)
    }
    fn with_input(input:bool)->(Self,Connection){
        Self::sized(input, 320, 240, 15)
    }
    /// A server with an output of this size that lives this many seconds.
    fn sized(input:bool, width:u32, height:u32, seconds:u64)->(Self,Connection){
        let (server, client) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let done = stopped.clone();
        let handle = thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let mut state = Server::new(&display.handle(), Size::new(width, height)).unwrap();
            // CHONKSTEP_DARWIN_TEST_GL=1 runs every wire test on the GL renderer
            // (off-screen) when this machine has GLES 3; snapshots then come
            // from GL readback and are held to the same CPU reference.
            if std::env::var_os("CHONKSTEP_DARWIN_TEST_GL").is_some() {
                match wm_darwin::glrender::GlRenderer::offscreen(width, height) {
                    Ok(renderer) => state.set_renderer_boxed(Box::new(renderer)),
                    Err(error) => eprintln!("GL unavailable ({error}); wire test runs on the CPU renderer"),
                }
            }
            if input{state.enable_sfhid_seat(&display.handle()).unwrap();}
            let mut decoder=wm_darwin::hid::Decoder::default();
            Server::add_client(&mut display, server).unwrap();
            let mut out = MemoryOutput {
                frame: None,
                reject: false,
            };
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(seconds) {
                display.dispatch_clients(&mut state).unwrap();
                let _ = state.repaint(&mut out, started.elapsed().as_millis() as u32);
                display.flush_clients().unwrap();
                match rx.try_recv() {
                    Ok(Request::Stop) => break,
                    Ok(Request::Reject(reject)) => out.reject = reject,
                    Ok(Request::Workspace(workspace)) => state.wm.switch_workspace(workspace),
                    Ok(Request::Keyboard(bytes))=>{if let Some(event)=decoder.keyboard(&bytes).unwrap(){state.deliver_hid(event,started.elapsed().as_millis() as u32).unwrap();}},
                    Ok(Request::Mouse(bytes))=>{state.deliver_hid(decoder.mouse(&bytes).unwrap(),started.elapsed().as_millis() as u32).unwrap();},
                    Ok(Request::Snapshot(reply)) => {
                        let content = state
                            .wm
                            .backend()
                            .windows
                            .values()
                            .find(|w| w.mapped)
                            .map(|w| w.content);
                        let pixels = match state.read_back().unwrap() {
                            Some(frame) => frame,
                            None => out.frame.as_ref().unwrap().data().to_vec(),
                        };
                        // Whatever renderer is selected, the screen must hold
                        // exactly the original full-frame render of the scene.
                        if !out.reject && !state.wm.backend().dirty {
                            let reference = state.wm.backend().render().unwrap();
                            assert!(pixels == reference.data(), "screen differs from full render");
                        }
                        let client=state.wm.focused_client().and_then(|id|state.wm.client(id));
                        let resize_corner=client.and_then(|c|{
                            let content=state.wm.backend().windows[&c.window].content.pos;
                            let origin=wm_core::Point::new(content.x-c.layout.client_offset.x,content.y-c.layout.client_offset.y);
                            c.layout.resize_hitboxes.iter().find(|(k,_)|*k==wm_theme_api::ResizeEdge::SouthEast).map(|(_,r)|wm_core::Point::new(origin.x+r.pos.x+r.size.w as i32/2,origin.y+r.pos.y+r.size.h as i32/2))
                        });
                        let (titlebar,close_button)=client.map(|c|{
                            let content=state.wm.backend().windows[&c.window].content.pos;
                            let origin=wm_core::Point::new(content.x-c.layout.client_offset.x,content.y-c.layout.client_offset.y);
                            let y=c.layout.titlebar_height as i32/2;
                            let x=(c.layout.frame_size.w as i32/2..c.layout.frame_size.w as i32-1).chain(1..c.layout.frame_size.w as i32/2).find(|x|!c.layout.button_hitboxes.iter().map(|(_,r)|r).chain(c.layout.resize_hitboxes.iter().map(|(_,r)|r)).any(|r|*x>=r.pos.x && *x<r.pos.x+r.size.w as i32 && y>=r.pos.y && y<r.pos.y+r.size.h as i32));
                            let close=c.layout.button_hitboxes.iter().find(|(k,_)|*k==wm_theme_api::ButtonKind::Close).map(|(_,r)|wm_core::Point::new(origin.x+r.pos.x+r.size.w as i32/2,origin.y+r.pos.y+r.size.h as i32/2));
                            (x.map(|x|wm_core::Point::new(origin.x+x,origin.y+y)),close)
                        }).unwrap_or((None,None));
                        let theme_pixels = pixels
                            .as_chunks::<4>().0.iter()
                            .filter(|p| p[..3] != [56, 72, 80] && p[..3] != [240, 20, 10])
                            .count();
                        reply
                            .send(Snapshot {
                                managed: state.wm.client_count(),
                                visible: state
                                    .wm
                                    .backend()
                                    .windows
                                    .keys()
                                    .filter(|id| state.wm.backend().visible(**id))
                                    .count(),
                                content,
                                pixels,
                                theme_pixels,
                                titlebar,close_button,resize_corner,
                            })
                            .unwrap();
                    }
                    Err(_) => {}
                }
                thread::sleep(Duration::from_millis(1));
            }
            done.store(true, Ordering::Release);
        });
        (
            Self {
                tx,
                thread: Some(handle),
                stopped,
            },
            Connection::from_socket(client).unwrap(),
        )
    }
    fn snapshot(&self) -> Snapshot {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Request::Snapshot(tx)).unwrap();
        rx.recv_timeout(Duration::from_secs(3)).unwrap()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
        self.thread.take().unwrap().join().unwrap();
        assert!(self.stopped.load(Ordering::Acquire));
    }
}

#[test]
fn actual_shm_client_uses_chonkstep_policy_decorations_and_owned_lifetime() {
    let (h, connection) = Harness::new();
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut client = Client::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut client).unwrap();
    assert!(!client.globals.iter().any(|g| matches!(
        g.as_str(),
        "wl_seat" | "wl_subcompositor" | "zwp_linux_dmabuf_v1" | "zwlr_layer_shell_v1"
    )));
    let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = client
        .shell
        .as_ref()
        .unwrap()
        .get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    top.set_title("Real Chonkstep SHM client".into());
    top.set_app_id("host.shm.test".into());
    let decoration = client
        .decoration
        .as_ref()
        .unwrap()
        .get_toplevel_decoration(&top, &qh, ());
    decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    assert!(client.configures > 0);
    assert!(client.server_decor);
    let mut file = tempfile::tempfile().unwrap();
    let bytes = [10, 20, 240, 255].repeat(80 * 48);
    file.write_all(&bytes).unwrap();
    let pool = client
        .shm
        .as_ref()
        .unwrap()
        .create_pool(file.as_fd(), bytes.len() as i32, &qh, ());
    let buffer = pool.create_buffer(0, 80, 48, 80 * 4, wl_shm::Format::Argb8888, &qh, ());
    h.tx.send(Request::Reject(true)).unwrap();
    h.snapshot(); // ordered barrier before commit
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, 80, 48);
    surface.frame(&qh, ());
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    queue.roundtrip(&mut client).unwrap();
    assert_eq!(client.releases, 1);
    // The GL renderer owns its output; the test presenter cannot refuse its frames.
    if std::env::var_os("CHONKSTEP_DARWIN_TEST_GL").is_none() {
        assert_eq!(
            client.frames, 0,
            "failed presentation must not complete a frame"
        );
    }
    h.tx.send(Request::Reject(false)).unwrap();
    h.snapshot();
    queue.roundtrip(&mut client).unwrap();
    queue.roundtrip(&mut client).unwrap();
    assert_eq!(client.frames, 1);
    let first = h.snapshot();
    assert_eq!(first.managed, 1);
    assert_eq!(first.visible, 1);
    let content = first.content.unwrap();
    assert_eq!(content.size, Size::new(80, 48));
    let pixel = ((content.pos.y as usize + 4) * 320 + content.pos.x as usize + 4) * 4;
    assert_eq!(&first.pixels[pixel..pixel + 4], &[240, 20, 10, 255]);
    assert!(
        first.theme_pixels > 500,
        "actual RasterThemeEngine must paint visible frame chrome"
    );
    h.tx.send(Request::Workspace(1)).unwrap();
    assert_eq!(
        h.snapshot().visible,
        0,
        "wm-core workspace policy hides the actual client"
    );
    h.tx.send(Request::Workspace(0)).unwrap();
    assert_eq!(h.snapshot().visible, 1);
    // Buffer released: overwrite and destroy client storage. Retained rendering stays owned.
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&vec![0; bytes.len()]).unwrap();
    buffer.destroy();
    pool.destroy();
    queue.roundtrip(&mut client).unwrap();
    top.set_title("Changed title after release".into());
    queue.roundtrip(&mut client).unwrap();
    let later = h.snapshot();
    assert_eq!(&later.pixels[pixel..pixel + 4], &[240, 20, 10, 255]);
    surface.attach(None, 0, 0);
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    assert_eq!(h.snapshot().visible, 0);
    let before = client.configures;
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    assert!(
        client.configures > before,
        "unmap must require fresh configure/ack"
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&bytes).unwrap();
    let pool = client
        .shm
        .as_ref()
        .unwrap()
        .create_pool(file.as_fd(), bytes.len() as i32, &qh, ());
    let buffer = pool.create_buffer(0, 80, 48, 320, wl_shm::Format::Xrgb8888, &qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    assert_eq!(h.snapshot().visible, 1);
    decoration.destroy();
    top.destroy();
    xdg.destroy();
    surface.destroy();
    queue.roundtrip(&mut client).unwrap();
    assert_eq!(h.snapshot().managed, 0);
}

#[test]
fn a_buffer_at_another_density_is_shown_at_its_surfaces_size() {
    let (h, connection) = Harness::new();
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut client = Client::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut client).unwrap();
    let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = client
        .shell
        .as_ref()
        .unwrap()
        .get_xdg_surface(&surface, &qh, ());
    let _top = xdg.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&[0; 64]).unwrap();
    let pool = client
        .shm
        .as_ref()
        .unwrap()
        .create_pool(file.as_fd(), 64, &qh, ());
    let buffer = pool.create_buffer(0, 4, 4, 16, wl_shm::Format::Argb8888, &qh, ());
    // A 4x4 buffer declared at scale 2 is a 2x2 surface. On a display at
    // 100% it is resampled to 2x2 panel pixels and shown: a real frame.
    surface.set_buffer_scale(2);
    surface.attach(Some(&buffer), 0, 0);
    surface.frame(&qh, ());
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    queue.roundtrip(&mut client).unwrap();
    assert_eq!(client.frames, 1);
    assert_eq!(h.snapshot().visible, 1);
    // A transform is still refused: nothing here rotates buffers.
    surface.set_buffer_transform(wl_output::Transform::_90);
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    assert!(queue.roundtrip(&mut client).is_err());
}

#[test]
fn sfhid_records_deliver_real_wayland_keys_pointer_cursor_and_wm_drag(){
    let(h,connection)=Harness::with_input(true);let mut queue=connection.new_event_queue();let qh=queue.handle();let mut c=Client::default();
    connection.display().get_registry(&qh,());queue.roundtrip(&mut c).unwrap();queue.roundtrip(&mut c).unwrap();queue.roundtrip(&mut c).unwrap();
    assert!(c.seat.is_some() && c.pointer.is_some() && c.keyboard.is_some());assert!(c.keymap.contains("xkb_keymap"));
    let surface=c.compositor.as_ref().unwrap().create_surface(&qh,());let xdg=c.shell.as_ref().unwrap().get_xdg_surface(&surface,&qh,());let top=xdg.get_toplevel(&qh,());
    let _decor=c.decoration.as_ref().unwrap().get_toplevel_decoration(&top,&qh,());top.set_title("Chonkstep input".into());surface.commit();queue.roundtrip(&mut c).unwrap();
    let mut file=tempfile::tempfile().unwrap();file.write_all(&[10,20,240,255].repeat(160*80)).unwrap();
    let pool=c.shm.as_ref().unwrap().create_pool(file.as_fd(),160*80*4,&qh,());let buffer=pool.create_buffer(0,160,80,640,wl_shm::Format::Xrgb8888,&qh,());
    surface.attach(Some(&buffer),0,0);surface.commit();queue.roundtrip(&mut c).unwrap();queue.roundtrip(&mut c).unwrap();assert!(c.keyboard_enters>0);
    let key=|seq:u32,usage:u8,down:u8|{let mut b=[0;8];b[..4].copy_from_slice(&seq.to_le_bytes());b[4]=usage;b[5]=down;Request::Keyboard(b)};
    for request in [key(1,0xe1,1),key(2,4,1),key(3,4,0),key(4,0xe1,0)]{h.tx.send(request).unwrap();}
    h.snapshot();queue.roundtrip(&mut c).unwrap();
    use wayland_client::WEnum::Value;
    assert_eq!(c.keys,vec![(42,Value(wl_keyboard::KeyState::Pressed)),(30,Value(wl_keyboard::KeyState::Pressed)),(30,Value(wl_keyboard::KeyState::Released)),(42,Value(wl_keyboard::KeyState::Released))]);
    assert!(c.modifier_masks.iter().any(|m|*m!=0));assert_eq!(c.modifier_masks.last(),Some(&0));
    let mouse=|seq:u32,p:wm_core::Point,buttons:u8|{let mut b=[0;12];b[..4].copy_from_slice(&seq.to_le_bytes());b[5]=buttons;b[9]=1;
        let x=((f64::from(p.x)+0.25)/319.*65535.).round().clamp(0.,65535.) as u16;
        let y=((f64::from(p.y)+0.25)/239.*65535.).round().clamp(0.,65535.) as u16;
        b[6..8].copy_from_slice(&x.to_le_bytes());b[10..12].copy_from_slice(&y.to_le_bytes());Request::Mouse(b)};
    let first=h.snapshot();let content=first.content.unwrap();let inside=wm_core::Point::new(content.pos.x+20,content.pos.y+20);
    h.tx.send(mouse(1,inside,0)).unwrap();h.tx.send(mouse(2,inside,1)).unwrap();h.tx.send(mouse(3,inside,0)).unwrap();h.snapshot();queue.roundtrip(&mut c).unwrap();
    assert_eq!(c.buttons,vec![(0x110,Value(wl_pointer::ButtonState::Pressed)),(0x110,Value(wl_pointer::ButtonState::Released))]);
    // Client cursor SHM goes through the same guarded import/release and CPU output.
    let cursor=c.compositor.as_ref().unwrap().create_surface(&qh,());
    let mut cursor_file=tempfile::tempfile().unwrap();cursor_file.write_all(&[0,255,0,255].repeat(4*4)).unwrap();
    let cursor_pool=c.shm.as_ref().unwrap().create_pool(cursor_file.as_fd(),64,&qh,());let cursor_buffer=cursor_pool.create_buffer(0,4,4,16,wl_shm::Format::Argb8888,&qh,());
    c.pointer.as_ref().unwrap().set_cursor(c.pointer_serial.unwrap(),Some(&cursor),0,0);cursor.attach(Some(&cursor_buffer),0,0);cursor.frame(&qh,());cursor.commit();
    queue.roundtrip(&mut c).unwrap();queue.roundtrip(&mut c).unwrap();let shot=h.snapshot();let at=(inside.y as usize*320+inside.x as usize)*4;assert_eq!(&shot.pixels[at..at+4],&[0,255,0,255]);
    assert!(c.releases>=2 && c.frames>=1);
    // Drag on the real RasterThemeEngine titlebar. Client receives no fake click.
    let title=first.titlebar.unwrap();let destination=wm_core::Point::new(title.x+31,title.y+17);
    h.tx.send(mouse(4,title,0)).unwrap();h.tx.send(mouse(5,title,1)).unwrap();h.tx.send(mouse(6,destination,1)).unwrap();h.tx.send(mouse(7,destination,0)).unwrap();
    let moved=h.snapshot().content.unwrap();assert_eq!(moved.pos,wm_core::Point::new(content.pos.x+31,content.pos.y+17));
    queue.roundtrip(&mut c).unwrap();assert_eq!(c.buttons.len(),2,"decoration drag must not leak button events to the client");
    // A forged/old serial cannot start client-initiated movement.
    top._move(c.seat.as_ref().unwrap(),0);queue.roundtrip(&mut c).unwrap();
    let inside=wm_core::Point::new(moved.pos.x+20,moved.pos.y+20);
    h.tx.send(mouse(8,inside,0)).unwrap();h.snapshot();assert_eq!(h.snapshot().content.unwrap().pos,moved.pos);
    h.tx.send(mouse(9,inside,1)).unwrap();h.snapshot();queue.roundtrip(&mut c).unwrap();
    top._move(c.seat.as_ref().unwrap(),c.button_serial.unwrap());queue.roundtrip(&mut c).unwrap();
    let destination=wm_core::Point::new(inside.x+12,inside.y+9);
    h.tx.send(mouse(10,destination,1)).unwrap();h.tx.send(mouse(11,destination,0)).unwrap();
    assert_eq!(h.snapshot().content.unwrap().pos,wm_core::Point::new(moved.pos.x+12,moved.pos.y+9));
    let close=h.snapshot().close_button.unwrap();h.tx.send(mouse(12,close,1)).unwrap();h.tx.send(mouse(13,close,0)).unwrap();h.snapshot();queue.roundtrip(&mut c).unwrap();assert_eq!(c.closes,1);
}

/// Client damage: under the damage renderer only reported rectangles are
/// copied from the client's buffer; no damage means the whole buffer.
#[test]
fn client_damage_limits_the_shm_copy_and_no_damage_copies_everything() {
    let legacy = std::env::var("CHONKSTEP_DARWIN_RENDERER").is_ok_and(|v| v == "legacy");
    let (h, connection) = Harness::new();
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut client = Client::default();
    connection.display().get_registry(&qh, ());
    queue.roundtrip(&mut client).unwrap();
    let surface = client.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = client.shell.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let _top = xdg.get_toplevel(&qh, ());
    surface.commit();
    queue.roundtrip(&mut client).unwrap();
    // Large enough that the two damage rectangles are not worth merging.
    let (w, h_px) = (200usize, 150usize);
    let red = [0, 0, 255, 255].repeat(w * h_px); // BGRA
    let blue = [255, 0, 0, 255].repeat(w * h_px);
    let green = [0, 255, 0, 255].repeat(w * h_px);
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&red).unwrap();
    let pool = client.shm.as_ref().unwrap().create_pool(file.as_fd(), red.len() as i32, &qh, ());
    let buffer = pool.create_buffer(0, 200, 150, 800, wl_shm::Format::Xrgb8888, &qh, ());
    let mut commit = |client: &mut Client, damage: &[(i32, i32, i32, i32)]| {
        surface.attach(Some(&buffer), 0, 0);
        for (x, y, w, h) in damage {
            surface.damage_buffer(*x, *y, *w, *h);
        }
        surface.frame(&qh, ());
        let frames = client.frames;
        surface.commit();
        for _ in 0..50 {
            queue.roundtrip(client).unwrap();
            if client.frames > frames {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("frame callback did not arrive");
    };
    commit(&mut client, &[(0, 0, 200, 150)]);
    let first = h.snapshot();
    let content = first.content.unwrap();
    let at = |snapshot: &Snapshot, x: usize, y: usize| {
        let i = ((content.pos.y as usize + y) * 320 + content.pos.x as usize + x) * 4;
        snapshot.pixels[i..i + 4].to_vec()
    };
    assert_eq!(at(&first, 0, 0), [255, 0, 0, 255]);
    // The whole buffer turns blue, but the client reports two small
    // rectangles, one of them hanging over the edge of the surface.
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&blue).unwrap();
    commit(&mut client, &[(10, 12, 20, 8), (190, 140, 500, 500)]);
    let second = h.snapshot();
    let outside = if legacy { [0, 0, 255, 255] } else { [255, 0, 0, 255] };
    for (x, y, want) in [
        (10, 12, [0, 0, 255, 255]),
        (29, 19, [0, 0, 255, 255]),
        (199, 149, [0, 0, 255, 255]),
        (190, 140, [0, 0, 255, 255]),
        (9, 12, outside),
        (30, 19, outside),
        (10, 11, outside),
        (189, 149, outside),
        (100, 75, outside),
        (0, 0, outside),
    ] {
        assert_eq!(at(&second, x, y), want, "pixel {x},{y} after damaged commit");
    }
    // No damage reported: the complete buffer is taken.
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&green).unwrap();
    commit(&mut client, &[]);
    let third = h.snapshot();
    for (x, y) in [(0, 0), (9, 12), (10, 12), (199, 149), (100, 75)] {
        assert_eq!(at(&third, x, y), [0, 255, 0, 255], "pixel {x},{y} after undamaged commit");
    }
    assert_eq!(client.releases, 3);
}

/// Reported by a user on the M1: resize a window, then drag it by the title
/// bar, and "sizes are reset". The client here follows every configure, as a
/// terminal does.
#[test]
fn a_resized_window_keeps_its_size_when_dragged_by_the_title_bar() {
    let (h, connection) = Harness::with_input(true);
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut c = Client::default();
    connection.display().get_registry(&qh, ());
    for _ in 0..3 { queue.roundtrip(&mut c).unwrap(); }
    let surface = c.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = c.shell.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    let decor = c.decoration.as_ref().unwrap().get_toplevel_decoration(&top, &qh, ());
    decor.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
    top.set_title("Resize then drag".into());
    surface.commit();
    queue.roundtrip(&mut c).unwrap();
    // One pool big enough for any size the test reaches; a fresh wl_buffer per size.
    let file = tempfile::tempfile().unwrap();
    file.set_len(320 * 240 * 4).unwrap();
    let pool = c.shm.as_ref().unwrap().create_pool(file.as_fd(), 320 * 240 * 4, &qh, ());
    let mut current = (120, 70);
    let present = |c: &mut Client, queue: &mut wayland_client::EventQueue<Client>, size: (i32, i32)| {
        let buffer = pool.create_buffer(0, size.0, size.1, size.0 * 4, wl_shm::Format::Xrgb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, size.0, size.1);
        surface.commit();
        queue.roundtrip(c).unwrap();
    };
    present(&mut c, &mut queue, current);
    // Follow configures like a real client: redraw at every size asked for.
    let follow = |c: &mut Client, queue: &mut wayland_client::EventQueue<Client>, current: &mut (i32, i32)| {
        for _ in 0..6 {
            queue.roundtrip(c).unwrap();
            if let Some(&(w, hh)) = c.configured_sizes.last().filter(|s| s.0 > 0 && s.1 > 0 && **s != *current) {
                *current = (w, hh);
                present(c, queue, *current);
            }
            thread::sleep(Duration::from_millis(5));
        }
    };
    follow(&mut c, &mut queue, &mut current);
    let mouse = |seq: u32, p: wm_core::Point, buttons: u8| {
        let mut b = [0; 12];
        b[..4].copy_from_slice(&seq.to_le_bytes());
        b[5] = buttons;
        b[9] = 1;
        let x = ((f64::from(p.x) + 0.25) / 319. * 65535.).round().clamp(0., 65535.) as u16;
        let y = ((f64::from(p.y) + 0.25) / 239. * 65535.).round().clamp(0., 65535.) as u16;
        b[6..8].copy_from_slice(&x.to_le_bytes());
        b[10..12].copy_from_slice(&y.to_le_bytes());
        Request::Mouse(b)
    };
    let before = h.snapshot();
    let start = before.content.unwrap();
    assert_eq!(start.size, Size::new(120, 70));
    // Resize: drag the bottom-right handle by +50,+40 in steps.
    let grip = before.resize_corner.expect("a south-east resize handle");
    let mut seq = 0;
    let mut send = |p: wm_core::Point, buttons: u8| {
        seq += 1;
        h.tx.send(mouse(seq, p, buttons)).unwrap();
    };
    send(grip, 0);
    send(grip, 1);
    for step in 1..=5 {
        send(wm_core::Point::new(grip.x + step * 10, grip.y + step * 8), 1);
        h.snapshot();
        follow(&mut c, &mut queue, &mut current);
    }
    send(wm_core::Point::new(grip.x + 50, grip.y + 40), 0);
    h.snapshot();
    follow(&mut c, &mut queue, &mut current);
    let resized = h.snapshot();
    let after_resize = resized.content.unwrap();
    eprintln!("after resize: content={after_resize:?} client={current:?} configures={:?}", c.configured_sizes);
    let size = after_resize.size;
    assert!(size.w > 150 && size.h > 95, "the resize itself: {size:?}");
    assert_eq!((size.w as i32, size.h as i32), current, "the client drew at the size it was last asked for");
    let configures_after_resize = c.configured_sizes.len();
    // Drag by the title bar.
    let title = resized.titlebar.unwrap();
    send(title, 0);
    send(title, 1);
    for step in 1..=4 {
        send(wm_core::Point::new(title.x + step * 6, title.y + step * 5), 1);
        h.snapshot();
        follow(&mut c, &mut queue, &mut current);
    }
    send(wm_core::Point::new(title.x + 24, title.y + 20), 0);
    h.snapshot();
    follow(&mut c, &mut queue, &mut current);
    let dragged = h.snapshot().content.unwrap();
    let later: Vec<_> = c.configured_sizes[configures_after_resize..].to_vec();
    eprintln!("after drag: content={dragged:?} client={current:?} configures during drag={later:?}");
    assert_eq!(dragged.size, size, "size after the drag; configures during the drag: {later:?}");
    assert_eq!(current, (size.w as i32, size.h as i32), "the client was asked to change size during the drag: {later:?}");
    assert_eq!(dragged.pos, wm_core::Point::new(after_resize.pos.x + 24, after_resize.pos.y + 20));
    assert!(later.iter().all(|s| *s == (size.w as i32, size.h as i32) || *s == (0, 0)), "{later:?}");
}

/// The same with a client that snaps to a cell grid, as the terminal does:
/// it answers each configure with the nearest whole number of 11x22 cells.
/// The frame must fit what the client drew as soon as the resize ends, not
/// at some later commit (which is what looked like the size being reset by
/// the drag), and the drag must not change it.
#[test]
fn a_cell_snapping_client_is_fitted_when_the_resize_ends_and_stays_so_when_dragged() {
    let (h, connection) = Harness::with_input(true);
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let mut c = Client::default();
    connection.display().get_registry(&qh, ());
    for _ in 0..3 { queue.roundtrip(&mut c).unwrap(); }
    let surface = c.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg = c.shell.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    let decor = c.decoration.as_ref().unwrap().get_toplevel_decoration(&top, &qh, ());
    decor.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
    top.set_title("Resize then drag".into());
    surface.commit();
    queue.roundtrip(&mut c).unwrap();
    // One pool big enough for any size the test reaches; a fresh wl_buffer per size.
    let file = tempfile::tempfile().unwrap();
    file.set_len(320 * 240 * 4).unwrap();
    let pool = c.shm.as_ref().unwrap().create_pool(file.as_fd(), 320 * 240 * 4, &qh, ());
    let mut current = (126, 82);
    let present = |c: &mut Client, queue: &mut wayland_client::EventQueue<Client>, size: (i32, i32)| {
        let buffer = pool.create_buffer(0, size.0, size.1, size.0 * 4, wl_shm::Format::Xrgb8888, &qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, size.0, size.1);
        surface.commit();
        queue.roundtrip(c).unwrap();
    };
    present(&mut c, &mut queue, current);
    // Follow configures like a real client: redraw at every size asked for.
    let follow = |c: &mut Client, queue: &mut wayland_client::EventQueue<Client>, current: &mut (i32, i32)| {
        for _ in 0..6 {
            queue.roundtrip(c).unwrap();
            if let Some(&(w, hh)) = c.configured_sizes.last().filter(|s| s.0 > 0 && s.1 > 0 && ((s.0 - 16) / 11 * 11 + 16, (s.1 - 16) / 22 * 22 + 16) != *current) {
                *current = ((w - 16) / 11 * 11 + 16, (hh - 16) / 22 * 22 + 16);
                present(c, queue, *current);
            }
            thread::sleep(Duration::from_millis(5));
        }
    };
    follow(&mut c, &mut queue, &mut current);
    let mouse = |seq: u32, p: wm_core::Point, buttons: u8| {
        let mut b = [0; 12];
        b[..4].copy_from_slice(&seq.to_le_bytes());
        b[5] = buttons;
        b[9] = 1;
        let x = ((f64::from(p.x) + 0.25) / 319. * 65535.).round().clamp(0., 65535.) as u16;
        let y = ((f64::from(p.y) + 0.25) / 239. * 65535.).round().clamp(0., 65535.) as u16;
        b[6..8].copy_from_slice(&x.to_le_bytes());
        b[10..12].copy_from_slice(&y.to_le_bytes());
        Request::Mouse(b)
    };
    let before = h.snapshot();
    let _ = before.content.unwrap();

    // Resize: drag the bottom-right handle by +50,+40 in steps.
    let grip = before.resize_corner.expect("a south-east resize handle");
    let mut seq = 0;
    let mut send = |p: wm_core::Point, buttons: u8| {
        seq += 1;
        h.tx.send(mouse(seq, p, buttons)).unwrap();
    };
    send(grip, 0);
    send(grip, 1);
    for step in 1..=5 {
        send(wm_core::Point::new(grip.x + step * 10, grip.y + step * 8), 1);
        h.snapshot();
        follow(&mut c, &mut queue, &mut current);
    }
    send(wm_core::Point::new(grip.x + 50, grip.y + 40), 0);
    h.snapshot();
    follow(&mut c, &mut queue, &mut current);
    let resized = h.snapshot();
    let after_resize = resized.content.unwrap();
    eprintln!("after resize: content={after_resize:?} client={current:?} configures={:?}", c.configured_sizes);
    let size = after_resize.size;
    assert!(size.w > 150 && size.h > 95, "the resize itself: {size:?}");
    assert_eq!((size.w as i32, size.h as i32), current, "the frame fits what the client drew");
    assert_eq!((current.0 - 16) % 11, 0);
    assert_eq!((current.1 - 16) % 22, 0);

    let configures_after_resize = c.configured_sizes.len();
    // Drag by the title bar.
    let title = resized.titlebar.unwrap();
    send(title, 0);
    send(title, 1);
    for step in 1..=4 {
        send(wm_core::Point::new(title.x + step * 6, title.y + step * 5), 1);
        h.snapshot();
        follow(&mut c, &mut queue, &mut current);
    }
    send(wm_core::Point::new(title.x + 24, title.y + 20), 0);
    h.snapshot();
    follow(&mut c, &mut queue, &mut current);
    let dragged = h.snapshot().content.unwrap();
    let later: Vec<_> = c.configured_sizes[configures_after_resize..].to_vec();
    eprintln!("after drag: content={dragged:?} client={current:?} configures during drag={later:?}");
    assert_eq!(dragged.size, size, "size after the drag; configures during the drag: {later:?}");
    assert_eq!(current, (size.w as i32, size.h as i32), "the client was asked to change size during the drag: {later:?}");
    assert_eq!(dragged.pos, wm_core::Point::new(after_resize.pos.x + 24, after_resize.pos.y + 20));

}
