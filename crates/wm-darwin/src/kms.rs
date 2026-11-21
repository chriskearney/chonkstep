//! KMS output on the display driver, with scanout buffers the GPU renders into.
//!
//! This is the call sequence of the lab's working reference
//! (`m1-mesa-asahi/test/agx-panel-demo.c`, its `kms` path), ported one call
//! for one call: buffers are created on the render node with the asahi GEM
//! ioctl (there is no GBM), exported as dma-bufs, imported into GL as render
//! targets (AR24, linear) and into the card node as XRGB8888 framebuffers;
//! the first atomic commit sets the mode, later ones flip `FB_ID` and are
//! answered by a flip-complete event, which is the frame clock.
use crate::gpu::*;
use std::ffi::{c_int, c_void, CStr, CString};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A line on stdout and, unbuffered, on `/dev/console`: when the kernel dies
/// in the next call, the last step is still on the wire.
pub struct Steps {
    console: c_int,
    prefix: &'static str,
}
impl Steps {
    pub fn new(prefix: &'static str) -> Self {
        // SAFETY: the path is a NUL-terminated string that lives across the call.
        let console = unsafe { libc::open(c"/dev/console".as_ptr(), libc::O_WRONLY | libc::O_NOCTTY) };
        Self { console, prefix }
    }
    pub fn step(&mut self, what: &str) {
        let line = format!("{} step: {what}\n", self.prefix);
        print!("{line}");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        if self.console >= 0 {
            // SAFETY: `console` is a descriptor this value opened and `line` is a live buffer of the length passed.
            unsafe { libc::write(self.console, line.as_ptr() as *const c_void, line.len()) };
        }
    }
}
impl Drop for Steps {
    fn drop(&mut self) {
        if self.console >= 0 {
            // SAFETY: this value owns `console` and closes it once, when dropped.
            unsafe { libc::close(self.console) };
        }
    }
}

fn os_error(call: &str) -> String {
    let e = std::io::Error::last_os_error();
    format!("{call}: errno {} ({e})", e.raw_os_error().unwrap_or(0))
}

/// Open the `/dev/dri/<prefix>*` node whose driver is `driver`. Nodes are
/// numbered in load order, so the name is the only reliable selector.
pub fn open_by_driver(drm: &Drm, prefix: &str, driver: &str) -> Result<(c_int, String), String> {
    let entries = std::fs::read_dir("/dev/dri").map_err(|e| format!("/dev/dri: {e}"))?;
    let mut seen = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(prefix) {
            continue;
        }
        let path = format!("/dev/dri/{name}");
        let c = CString::new(path.clone()).map_err(|_| "node path")?;
        // SAFETY: the path is a NUL-terminated string that lives across the call.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            seen.push(format!("{path}: open failed"));
            continue;
        }
        // SAFETY: `fd` is an open DRM node; libdrm returns null or a version it owns until drmFreeVersion.
        let version = unsafe { (drm.drmGetVersion)(fd) };
        // SAFETY: `version` is non-null (checked first in this condition) and points to libdrm's version struct.
        let found = if version.is_null() || unsafe { (*version).name.is_null() } {
            String::from("(no version)")
        } else {
            // SAFETY: `version` is non-null (checked first in this condition) and points to libdrm's version struct.
            unsafe { CStr::from_ptr((*version).name) }.to_string_lossy().into_owned()
        };
        if !version.is_null() {
            // SAFETY: `version` came from drmGetVersion, is non-null, and is freed exactly once here.
            unsafe { (drm.drmFreeVersion)(version) };
        }
        if found == driver {
            return Ok((fd, path));
        }
        seen.push(format!("{path}: driver {found}"));
        // SAFETY: `fd` was opened above, is owned here, and is not used after this close.
        unsafe { libc::close(fd) };
    }
    Err(format!("no /dev/dri/{prefix}* with driver {driver} ({})", seen.join(", ")))
}

/// True when both DRM nodes exist: the cheap test behind "default to GL".
pub fn nodes_present(drm: &Drm) -> Result<(), String> {
    for (prefix, driver) in [("renderD", "asahi"), ("card", "apple")] {
        let (fd, _) = open_by_driver(drm, prefix, driver)?;
        // SAFETY: `fd` was opened above, is owned here, and is not used after this close.
        unsafe { libc::close(fd) };
    }
    Ok(())
}

/// One flip-complete event.
#[derive(Clone, Copy, Debug)]
pub struct Flip {
    pub sequence: u32,
    pub at: Instant,
}

/// Where flip-complete events come from. `PollRead` is the normal source: the
/// card fd is pollable, so it can also sit in the compositor's event loop
/// (`Kms::event_fd`). The two blocking-`read()` sources date from a kernel that
/// did not wake `poll` on DRM fds and are kept for comparison.
pub trait FlipSource {
    /// Wait up to `timeout` (forever when `None`) for the next event.
    fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<Flip>, String>;
    fn name(&self) -> &'static str;
}

/// Parse `struct drm_event` records: `{u32 type, u32 length}`; a vblank event
/// is 32 bytes with its sequence number at offset 24.
fn parse_events(buffer: &[u8], mut found: impl FnMut(u32)) {
    let mut at = 0;
    while at + 8 <= buffer.len() {
        let word = |o: usize| u32::from_ne_bytes(buffer[at + o..at + o + 4].try_into().unwrap());
        let (kind, length) = (word(0), word(4) as usize);
        if kind == DRM_EVENT_FLIP_COMPLETE {
            found(if length >= 32 && at + 28 <= buffer.len() { word(24) } else { 0 });
        }
        if length < 8 {
            break;
        }
        at += length;
    }
}
fn read_flips(fd: c_int, mut found: impl FnMut(u32)) -> Result<(), String> {
    let mut buffer = [0u8; 256];
    loop {
        // SAFETY: `buffer` is a live local of the length passed and `fd` is the descriptor this reader owns.
        let n = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut c_void, buffer.len()) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if n < 8 {
            return Err(os_error("read of the DRM event (card node)"));
        }
        let mut any = false;
        parse_events(&buffer[..n as usize], |sequence| {
            any = true;
            found(sequence)
        });
        if any {
            return Ok(());
        }
    }
}

/// `poll()` for readability with the caller's timeout, then `read()`.
pub struct PollRead {
    fd: c_int,
    queued: std::collections::VecDeque<Flip>,
    /// `poll` said readable but waits that then timed out; should stay 0.
    pub timeouts: u32,
}
/// Wait until `fd` is readable. `Ok(false)` is a timeout.
pub fn poll_readable(fd: c_int, timeout: Option<Duration>) -> Result<bool, String> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let ms = match deadline {
            None => -1,
            // Round up so a short timeout does not become a busy loop.
            Some(d) => d.saturating_duration_since(Instant::now()).as_micros().div_ceil(1000).min(60_000) as c_int,
        };
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: `pfd` is a live local and the count passed is 1.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(os_error("poll (card node)"));
        }
        if n > 0 {
            if pfd.revents & libc::POLLIN != 0 {
                return Ok(true);
            }
            return Err(format!("poll (card node): revents {:#x}", pfd.revents));
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(false);
        }
    }
}
impl FlipSource for PollRead {
    fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<Flip>, String> {
        if self.queued.is_empty() {
            if !poll_readable(self.fd, timeout)? {
                self.timeouts += 1;
                return Ok(None);
            }
            let queued = &mut self.queued;
            read_flips(self.fd, |sequence| queued.push_back(Flip { sequence, at: Instant::now() }))?;
        }
        Ok(self.queued.pop_front())
    }
    fn name(&self) -> &'static str {
        "poll"
    }
}

/// Blocking `read()` on the calling thread, exactly as the reference does.
/// It cannot honour a timeout; use it only where blocking is acceptable.
pub struct InlineRead {
    fd: c_int,
    queued: std::collections::VecDeque<Flip>,
}
impl FlipSource for InlineRead {
    fn wait(&mut self, _: Option<Duration>) -> Result<Option<Flip>, String> {
        if self.queued.is_empty() {
            let queued = &mut self.queued;
            read_flips(self.fd, |sequence| queued.push_back(Flip { sequence, at: Instant::now() }))?;
        }
        Ok(self.queued.pop_front())
    }
    fn name(&self) -> &'static str {
        "inline"
    }
}

/// A dedicated thread blocks in `read()` and forwards events over a channel,
/// so the compositor's loop can wait with a timeout and keep serving clients.
/// The thread lives until the process exits; it owns nothing but the fd number.
pub struct ReaderThread {
    events: mpsc::Receiver<Result<Flip, String>>,
}
impl ReaderThread {
    fn spawn(fd: c_int) -> Result<Self, String> {
        let (tx, events) = mpsc::channel();
        std::thread::Builder::new()
            .name("drm-flip-events".into())
            .spawn(move || loop {
                let mut failed = false;
                let result = read_flips(fd, |sequence| {
                    failed |= tx.send(Ok(Flip { sequence, at: Instant::now() })).is_err();
                });
                if let Err(error) = result {
                    let _ = tx.send(Err(error));
                    return;
                }
                if failed {
                    return; // the compositor dropped its end
                }
            })
            .map_err(|e| format!("flip event thread: {e}"))?;
        Ok(Self { events })
    }
}
impl FlipSource for ReaderThread {
    fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<Flip>, String> {
        let event = match timeout {
            None => self.events.recv().map_err(|_| "flip event thread ended".to_string())?,
            Some(timeout) => match self.events.recv_timeout(timeout) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("flip event thread ended".into()),
            },
        };
        event.map(Some)
    }
    fn name(&self) -> &'static str {
        "thread"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipWait {
    Poll,
    Inline,
    Thread,
}

/// One scanout buffer: GPU memory that is both a GL framebuffer and a KMS
/// framebuffer. Row 0 is the top scanline.
pub struct ScanoutBuffer {
    pub fbo: GLuint,
    pub texture: GLuint,
    pub fb: u32,
    pub pitch: u32,
    /// CPU mapping through the render node (for checks; never used to draw).
    pub map: *mut u8,
    pub size: usize,
    /// The dma-buf; kept open for the life of the buffer.
    pub prime_fd: c_int,
}

#[repr(C)]
#[derive(Default)]
struct AsahiGemCreate {
    size: u64,
    flags: u32,
    vm_id: u32,
    handle: u32,
    pad: u32,
}
#[repr(C)]
#[derive(Default)]
struct AsahiGemMmapOffset {
    handle: u32,
    flags: u32,
    offset: u64,
}

/// The display: card node, connector/CRTC/primary plane and their property ids.
pub struct Kms {
    card: c_int,
    render: c_int,
    pub card_path: String,
    pub render_path: String,
    crtc_id: u32,
    plane_id: u32,
    connector_id: u32,
    prop_connector_crtc: u32,
    prop_mode: u32,
    prop_active: u32,
    plane_props: [u32; 10],
    mode: DrmModeModeInfo,
    mode_blob: u32,
    modeset_done: bool,
    pub ebusy_retries: u32,
    gl_fourcc: u32,
    flips: Box<dyn FlipSource>,
}

const PLANE_PROPERTIES: [&str; 10] =
    ["FB_ID", "CRTC_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H"];

impl Kms {
    pub fn width(&self) -> u32 {
        u32::from(self.mode.hdisplay)
    }
    pub fn height(&self) -> u32 {
        u32::from(self.mode.vdisplay)
    }
    pub fn refresh(&self) -> u32 {
        self.mode.vrefresh
    }
    pub fn flip_source(&self) -> &'static str {
        self.flips.name()
    }
    /// The card fd: readable when a flip-complete event is waiting. Register
    /// it in an event loop, then take the event with `wait_flip(Some(ZERO))`.
    pub fn event_fd(&self) -> c_int {
        self.card
    }

    fn property(drm: &Drm, fd: c_int, object: u32, kind: u32, name: &str) -> Option<(u32, u64)> {
        // SAFETY: libdrm calls on a DRM descriptor opened by this code. Every resource it returns is checked for null before it is read and released with its matching free call before the block ends; the ioctl arguments are live locals of the structure the ioctl number encodes.
        unsafe {
            let props = (drm.drmModeObjectGetProperties)(fd, object, kind);
            if props.is_null() {
                return None;
            }
            let mut found = None;
            for i in 0..(*props).count_props as usize {
                let p = (drm.drmModeGetProperty)(fd, *(*props).props.add(i));
                if p.is_null() {
                    continue;
                }
                if found.is_none() && CStr::from_ptr((*p).name.as_ptr()).to_bytes() == name.as_bytes() {
                    found = Some(((*p).prop_id, *(*props).prop_values.add(i)));
                }
                (drm.drmModeFreeProperty)(p);
            }
            (drm.drmModeFreeObjectProperties)(props);
            found
        }
    }

    /// Open both nodes and find connector, mode, CRTC, primary plane and the
    /// property ids. Nothing on screen changes until the first commit.
    pub fn open(libs: &Libraries, wait: FlipWait, steps: &mut Steps) -> Result<Self, String> {
        let drm = &libs.drm;
        steps.step("open the asahi render node");
        let (render, render_path) = open_by_driver(drm, "renderD", "asahi")?;
        steps.step("open the apple card node");
        let (card, card_path) = open_by_driver(drm, "card", "apple")?;
        // SAFETY: libdrm calls on a DRM descriptor opened by this code. Every resource it returns is checked for null before it is read and released with its matching free call before the block ends; the ioctl arguments are live locals of the structure the ioctl number encodes.
        unsafe {
            if (drm.drmSetClientCap)(card, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1) != 0 {
                return Err(os_error("DRM_CLIENT_CAP_UNIVERSAL_PLANES"));
            }
            if (drm.drmSetClientCap)(card, DRM_CLIENT_CAP_ATOMIC, 1) != 0 {
                return Err(os_error("DRM_CLIENT_CAP_ATOMIC"));
            }
            steps.step("drmModeGetResources");
            let res = (drm.drmModeGetResources)(card);
            if res.is_null() {
                return Err(os_error("drmModeGetResources"));
            }
            let mut connector = std::ptr::null_mut::<DrmModeConnector>();
            for i in 0..(*res).count_connectors.max(0) as usize {
                let c = (drm.drmModeGetConnector)(card, *(*res).connectors.add(i));
                if !c.is_null() && (*c).connection == DRM_MODE_CONNECTED && (*c).count_modes > 0 {
                    connector = c;
                    break;
                }
                if !c.is_null() {
                    (drm.drmModeFreeConnector)(c);
                }
            }
            if connector.is_null() {
                return Err("no connected connector with a mode".into());
            }
            let connector_id = (*connector).connector_id;
            let mut mode = *(*connector).modes;
            for i in 0..(*connector).count_modes as usize {
                let m = *(*connector).modes.add(i);
                if m.kind & DRM_MODE_TYPE_PREFERRED != 0 {
                    mode = m;
                }
            }
            let (mut crtc_id, mut crtc_index) = (0u32, 0usize);
            for e in 0..(*connector).count_encoders.max(0) as usize {
                let encoder = (drm.drmModeGetEncoder)(card, *(*connector).encoders.add(e));
                if encoder.is_null() {
                    continue;
                }
                for c in 0..((*res).count_crtcs.max(0) as usize).min(32) {
                    if crtc_id == 0 && (*encoder).possible_crtcs & (1 << c) != 0 {
                        crtc_id = *(*res).crtcs.add(c);
                        crtc_index = c;
                    }
                }
                (drm.drmModeFreeEncoder)(encoder);
            }
            (drm.drmModeFreeConnector)(connector);
            (drm.drmModeFreeResources)(res);
            if crtc_id == 0 {
                return Err("no CRTC for the connector".into());
            }
            steps.step("drmModeGetPlaneResources");
            let planes = (drm.drmModeGetPlaneResources)(card);
            if planes.is_null() {
                return Err(os_error("drmModeGetPlaneResources"));
            }
            let mut plane_id = 0;
            for i in 0..(*planes).count_planes as usize {
                let plane = (drm.drmModeGetPlane)(card, *(*planes).planes.add(i));
                if plane.is_null() {
                    continue;
                }
                let id = (*plane).plane_id;
                let kind = Self::property(drm, card, id, DRM_MODE_OBJECT_PLANE, "type").map(|p| p.1);
                if plane_id == 0
                    && kind == Some(DRM_PLANE_TYPE_PRIMARY)
                    && (*plane).possible_crtcs & (1 << crtc_index) != 0
                {
                    plane_id = id;
                }
                (drm.drmModeFreePlane)(plane);
            }
            (drm.drmModeFreePlaneResources)(planes);
            if plane_id == 0 {
                return Err("no primary plane for the CRTC".into());
            }
            let id = |object, kind, name: &str| {
                Self::property(drm, card, object, kind, name)
                    .map(|p| p.0)
                    .ok_or_else(|| format!("object {object} has no property {name}"))
            };
            let prop_connector_crtc = id(connector_id, DRM_MODE_OBJECT_CONNECTOR, "CRTC_ID")?;
            let prop_mode = id(crtc_id, DRM_MODE_OBJECT_CRTC, "MODE_ID")?;
            let prop_active = id(crtc_id, DRM_MODE_OBJECT_CRTC, "ACTIVE")?;
            let mut plane_props = [0u32; 10];
            for (slot, name) in plane_props.iter_mut().zip(PLANE_PROPERTIES) {
                *slot = id(plane_id, DRM_MODE_OBJECT_PLANE, name)?;
            }
            let mut mode_blob = 0;
            if (drm.drmModeCreatePropertyBlob)(
                card,
                &mode as *const DrmModeModeInfo as *const c_void,
                std::mem::size_of::<DrmModeModeInfo>(),
                &mut mode_blob,
            ) != 0
            {
                return Err(os_error("drmModeCreatePropertyBlob (mode)"));
            }
            let flips: Box<dyn FlipSource> = match wait {
                FlipWait::Poll => Box::new(PollRead { fd: card, queued: Default::default(), timeouts: 0 }),
                FlipWait::Inline => Box::new(InlineRead { fd: card, queued: Default::default() }),
                FlipWait::Thread => {
                    steps.step("start the flip event thread (blocking read on the card node)");
                    Box::new(ReaderThread::spawn(card)?)
                }
            };
            Ok(Self {
                card,
                render,
                card_path,
                render_path,
                crtc_id,
                plane_id,
                connector_id,
                prop_connector_crtc,
                prop_mode,
                prop_active,
                plane_props,
                mode,
                mode_blob,
                modeset_done: false,
                ebusy_retries: 0,
                gl_fourcc: 0,
                flips,
            })
        }
    }

    /// GEM object on the render node -> dma-buf -> EGLImage -> texture -> FBO,
    /// and -> card node -> XRGB8888 framebuffer.
    pub fn create_buffer(
        &mut self,
        libs: &Libraries,
        context: &Context,
        index: usize,
        steps: &mut Steps,
    ) -> Result<ScanoutBuffer, String> {
        let (drm, gl) = (&libs.drm, &libs.gl);
        let (create_image, image_target) = context.image_functions(libs)?;
        let (width, height) = (self.width(), self.height());
        let pitch = width * 4;
        let size = (pitch as usize * height as usize + 0x3fff) & !0x3fff;
        // SAFETY: libdrm calls on a DRM descriptor opened by this code. Every resource it returns is checked for null before it is read and released with its matching free call before the block ends; the ioctl arguments are live locals of the structure the ioctl number encodes.
        unsafe {
            let mut create = AsahiGemCreate { size: size as u64, ..Default::default() };
            steps.step(&format!("buffer {index}: asahi GEM_CREATE {size} bytes on the render node"));
            if (drm.drmIoctl)(self.render, DRM_IOCTL_ASAHI_GEM_CREATE, &mut create as *mut _ as *mut c_void) != 0 {
                return Err(os_error("DRM_IOCTL_ASAHI_GEM_CREATE (render node)"));
            }
            let mut offset = AsahiGemMmapOffset { handle: create.handle, ..Default::default() };
            if (drm.drmIoctl)(self.render, DRM_IOCTL_ASAHI_GEM_MMAP_OFFSET, &mut offset as *mut _ as *mut c_void) != 0 {
                return Err(os_error("DRM_IOCTL_ASAHI_GEM_MMAP_OFFSET (render node)"));
            }
            steps.step(&format!("buffer {index}: mmap through the render node"));
            let map = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.render,
                offset.offset as libc::off_t,
            );
            if map == libc::MAP_FAILED {
                return Err(os_error("mmap of the GPU buffer (render node)"));
            }
            steps.step(&format!("buffer {index}: PRIME export from the render node (handle {})", create.handle));
            let mut prime_fd = -1;
            if (drm.drmPrimeHandleToFD)(self.render, create.handle, DRM_CLOEXEC | DRM_RDWR, &mut prime_fd) != 0 {
                return Err(os_error("drmPrimeHandleToFD (render node)"));
            }
            // GL side: ARGB8888 first (renderable, with alpha), XRGB8888 if that
            // is refused. Both describe the same bytes; the display reads XRGB8888.
            let mut image = std::ptr::null_mut();
            for fourcc in [DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888] {
                let fourcc = if self.gl_fourcc != 0 { self.gl_fourcc } else { fourcc };
                let attribs = [
                    EGL_WIDTH, width as EGLint, EGL_HEIGHT, height as EGLint,
                    EGL_LINUX_DRM_FOURCC_EXT, fourcc as EGLint,
                    EGL_DMA_BUF_PLANE0_FD_EXT, prime_fd,
                    EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
                    EGL_DMA_BUF_PLANE0_PITCH_EXT, pitch as EGLint,
                    EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, 0, // DRM_FORMAT_MOD_LINEAR
                    EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, 0,
                    EGL_NONE,
                ];
                steps.step(&format!(
                    "buffer {index}: eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT, {}, linear, pitch {pitch}, fd {prime_fd})",
                    String::from_utf8_lossy(&fourcc.to_le_bytes())
                ));
                image = create_image(context.display, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attribs.as_ptr());
                if !image.is_null() {
                    self.gl_fourcc = fourcc;
                    break;
                }
                println!("WMGL note eglCreateImageKHR refused: EGL error {:#x}", (libs.egl.eglGetError)());
                if self.gl_fourcc != 0 {
                    break;
                }
            }
            if image.is_null() {
                return Err("eglCreateImageKHR(EGL_LINUX_DMA_BUF_EXT) failed".into());
            }
            steps.step(&format!("buffer {index}: glEGLImageTargetTexture2DOES and framebuffer"));
            let (mut texture, mut fbo) = (0, 0);
            (gl.glGenTextures)(1, &mut texture);
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            image_target(GL_TEXTURE_2D, image);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            let error = (gl.glGetError)();
            (gl.glGenFramebuffers)(1, &mut fbo);
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fbo);
            (gl.glFramebufferTexture2D)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
            let status = (gl.glCheckFramebufferStatus)(GL_FRAMEBUFFER);
            if error != GL_NO_ERROR || status != GL_FRAMEBUFFER_COMPLETE {
                return Err(format!(
                    "imported texture as framebuffer: GL error {error:#x}, framebuffer status {status:#x}"
                ));
            }
            steps.step(&format!("buffer {index}: PRIME import into the card node (fd {prime_fd})"));
            let mut card_handle = 0;
            if (drm.drmPrimeFDToHandle)(self.card, prime_fd, &mut card_handle) != 0 {
                return Err(os_error("drmPrimeFDToHandle (card node)"));
            }
            let (handles, pitches, offsets) = ([card_handle, 0, 0, 0], [pitch, 0, 0, 0], [0u32; 4]);
            let mut fb = 0;
            steps.step(&format!("buffer {index}: ADDFB2 XRGB8888"));
            if (drm.drmModeAddFB2)(self.card, width, height, DRM_FORMAT_XRGB8888, handles.as_ptr(), pitches.as_ptr(), offsets.as_ptr(), &mut fb, 0) != 0 {
                return Err(os_error("drmModeAddFB2 (XRGB8888, card node)"));
            }
            println!(
                "WMGL buffer index={index} render_handle={} card_handle={card_handle} fb={fb} gl_format={} texture={texture} fbo={fbo}",
                create.handle,
                String::from_utf8_lossy(&self.gl_fourcc.to_le_bytes())
            );
            Ok(ScanoutBuffer { fbo, texture, fb, pitch, map: map as *mut u8, size, prime_fd })
        }
    }

    /// Show `buffer`. The first commit sets the mode (blocking); later ones
    /// are non-blocking flips of `FB_ID` only. Either way exactly one
    /// flip-complete event follows a queued commit; do not commit again
    /// before taking it. `Busy`: the display has not finished its previous
    /// commit (EBUSY); nothing was queued and no event will follow this call.
    pub fn commit(&mut self, libs: &Libraries, buffer: &ScanoutBuffer, steps: &mut Steps) -> Result<Committed, String> {
        let drm = &libs.drm;
        // SAFETY: libdrm calls on a DRM descriptor opened by this code. Every resource it returns is checked for null before it is read and released with its matching free call before the block ends; the ioctl arguments are live locals of the structure the ioctl number encodes.
        unsafe {
            if !self.modeset_done {
                let (w, h) = (u64::from(self.width()), u64::from(self.height()));
                let values = [u64::from(buffer.fb), u64::from(self.crtc_id), 0, 0, w << 16, h << 16, 0, 0, w, h];
                let request = (drm.drmModeAtomicAlloc)();
                (drm.drmModeAtomicAddProperty)(request, self.connector_id, self.prop_connector_crtc, u64::from(self.crtc_id));
                (drm.drmModeAtomicAddProperty)(request, self.crtc_id, self.prop_mode, u64::from(self.mode_blob));
                (drm.drmModeAtomicAddProperty)(request, self.crtc_id, self.prop_active, 1);
                for (property, value) in self.plane_props.iter().zip(values) {
                    (drm.drmModeAtomicAddProperty)(request, self.plane_id, *property, value);
                }
                steps.step(&format!("atomic commit with ALLOW_MODESET, fb {}", buffer.fb));
                let result = (drm.drmModeAtomicCommit)(
                    self.card,
                    request,
                    DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT,
                    std::ptr::null_mut(),
                );
                let error = os_error("drmModeAtomicCommit (ALLOW_MODESET | PAGE_FLIP_EVENT)");
                (drm.drmModeAtomicFree)(request);
                if result != 0 {
                    return Err(error);
                }
                self.modeset_done = true;
                return Ok(Committed::Queued);
            }
            let request = (drm.drmModeAtomicAlloc)();
            (drm.drmModeAtomicAddProperty)(request, self.plane_id, self.plane_props[0], u64::from(buffer.fb));
            let result = (drm.drmModeAtomicCommit)(
                self.card,
                request,
                DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK,
                std::ptr::null_mut(),
            );
            let errno = std::io::Error::last_os_error().raw_os_error();
            let error = os_error("drmModeAtomicCommit (PAGE_FLIP_EVENT | NONBLOCK)");
            (drm.drmModeAtomicFree)(request);
            if result == 0 {
                return Ok(Committed::Queued);
            }
            if errno == Some(libc::EBUSY) {
                // Not an error and not something to sleep on here: the caller
                // keeps the frame and offers it again (`FlipQueue`).
                self.ebusy_retries += 1;
                return Ok(Committed::Busy);
            }
            Err(error)
        }
    }

    /// Display sleep: the CRTC's ACTIVE property, off or on again. The plane
    /// keeps its framebuffer. Not before the first frame, and not while a
    /// flip is pending.
    ///
    /// Off: a blocking commit without an event. Nothing is flipped while the
    /// display is off, and the commit that follows is the blocking power-up
    /// commit, which the kernel itself orders behind this one.
    ///
    /// On: the commit returns before the panel is back (the driver finishes
    /// in its commit tail; about 0.3 s on this panel), and until then a
    /// non-blocking flip is refused with EBUSY. So this commit carries
    /// PAGE_FLIP_EVENT: one flip-complete event follows it, and the caller
    /// must take that event before the next flip (`FlipQueue::powered_up`).
    pub fn set_active(&mut self, libs: &Libraries, on: bool) -> Result<(), String> {
        if !self.modeset_done {
            return Err("display power: no mode is set yet".into());
        }
        let drm = &libs.drm;
        // SAFETY: libdrm calls on a DRM descriptor opened by this code. Every resource it returns is checked for null before it is read and released with its matching free call before the block ends; the ioctl arguments are live locals of the structure the ioctl number encodes.
        unsafe {
            let request = (drm.drmModeAtomicAlloc)();
            (drm.drmModeAtomicAddProperty)(request, self.crtc_id, self.prop_active, u64::from(on));
            let flags = if on { DRM_MODE_ATOMIC_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT } else { DRM_MODE_ATOMIC_ALLOW_MODESET };
            let result = (drm.drmModeAtomicCommit)(self.card, request, flags, std::ptr::null_mut());
            let error = os_error(if on { "drmModeAtomicCommit (ACTIVE=1, ALLOW_MODESET | PAGE_FLIP_EVENT)" } else { "drmModeAtomicCommit (ACTIVE=0, ALLOW_MODESET)" });
            (drm.drmModeAtomicFree)(request);
            if result != 0 {
                return Err(error);
            }
        }
        Ok(())
    }

    /// The next flip-complete event; `None` when `timeout` passed first.
    pub fn wait_flip(&mut self, timeout: Option<Duration>) -> Result<Option<Flip>, String> {
        self.flips.wait(timeout)
    }
}

/// What became of a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Committed {
    /// Taken; one flip-complete event follows.
    Queued,
    /// Refused with EBUSY: the display is still finishing an earlier commit.
    Busy,
}

/// The display as the flip queue sees it (the real one, or a test's).
pub trait Scanout {
    /// Flip to scanout buffer `index`.
    fn flip(&mut self, index: usize) -> Result<Committed, String>;
    /// The next flip-complete event, waiting up to `timeout`.
    fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<Flip>, String>;
}

/// A frame that reached the screen: its buffer, the event, commit-to-event time.
#[derive(Debug, Clone, Copy)]
pub struct Shown {
    pub index: usize,
    pub flip: Flip,
    pub latency: Duration,
}

/// Which commit the display owes an event for, and what waits behind it.
///
/// One thing at a time is in flight: a flip (`pending`), a power-up
/// (`waking`), or a frame the display refused for the moment (`held`). While
/// any of them is, the queue is busy and no new frame may be started. Every
/// wait is the caller's poll timeout on the display's event fd: nothing here
/// sleeps or counts retries.
pub struct FlipQueue {
    front: Option<usize>,
    pending: Option<(usize, Instant)>,
    /// A power-up commit whose event has not arrived: since when.
    waking: Option<Instant>,
    /// A rendered frame the display refused (EBUSY): its buffer, since when.
    held: Option<(usize, Instant)>,
    /// Longest wait for the power-up event before flips are tried without it.
    pub wake_wait: Duration,
    /// Longest a frame is held for a busy display before that is an error.
    pub busy_wait: Duration,
    /// Power-up events that did not arrive within `wake_wait`.
    pub wake_timeouts: u32,
    /// Frames that were refused at least once and shown later.
    pub held_frames: u32,
}
impl Default for FlipQueue {
    fn default() -> Self {
        Self::new()
    }
}
impl FlipQueue {
    pub fn new() -> Self {
        Self {
            front: None,
            pending: None,
            waking: None,
            held: None,
            wake_wait: Duration::from_secs(3),
            busy_wait: Duration::from_secs(5),
            wake_timeouts: 0,
            held_frames: 0,
        }
    }
    /// The buffer on screen.
    pub fn front(&self) -> Option<usize> {
        self.front
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.waking.is_some() || self.held.is_some()
    }
    /// Hand a rendered frame to the display. A busy display keeps it here.
    pub fn submit(&mut self, display: &mut dyn Scanout, index: usize) -> Result<(), String> {
        if self.busy() {
            return Err("flip queue: a frame was submitted while another commit is in flight".into());
        }
        match display.flip(index)? {
            Committed::Queued => self.pending = Some((index, Instant::now())),
            Committed::Busy => self.held = Some((index, Instant::now())),
        }
        Ok(())
    }
    /// A power-up commit was made; its event comes before any flip.
    pub fn powered_up(&mut self) {
        self.waking = Some(Instant::now());
    }
    /// Take what the display has to report, waiting up to `timeout`.
    pub fn poll(&mut self, display: &mut dyn Scanout, timeout: Duration) -> Result<Option<Shown>, String> {
        if let Some(since) = self.waking {
            if display.wait(Some(timeout))?.is_some() {
                // The panel is back. Not a frame: nothing new is on screen.
                self.waking = None;
            } else if since.elapsed() >= self.wake_wait {
                // Reported, not fatal: flips are tried anyway, and a display
                // that is still busy holds the frame as usual.
                self.waking = None;
                self.wake_timeouts += 1;
                eprintln!(
                    "chonkstep-gl: error: no flip-complete event within {} ms of the display power-up commit; continuing without it",
                    self.wake_wait.as_millis()
                );
            }
            return Ok(None);
        }
        if let Some((index, since)) = self.held {
            match display.flip(index)? {
                Committed::Queued => {
                    self.held = None;
                    self.held_frames += 1;
                    self.pending = Some((index, Instant::now()));
                }
                Committed::Busy => {
                    if since.elapsed() >= self.busy_wait {
                        return Err(format!("drmModeAtomicCommit: the display refused a flip with EBUSY for {} ms", since.elapsed().as_millis()));
                    }
                    // Whatever the display is finishing announces itself on
                    // the event fd; otherwise this is the caller's own pace.
                    let _stray = display.wait(Some(timeout))?;
                    return Ok(None);
                }
            }
        }
        let Some((index, committed)) = self.pending else {
            return Ok(None);
        };
        let Some(flip) = display.wait(Some(timeout))? else {
            return Ok(None);
        };
        self.front = Some(index);
        self.pending = None;
        Ok(Some(Shown { index, flip, latency: flip.at.duration_since(committed) }))
    }
}

fn median(values: &mut [u64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_unstable();
    values[values.len() / 2] as f64 / 1000.0
}

pub const PROBE_USAGE: &str = "--gl-probe [--seconds N] [--flip-wait poll|inline|thread] [--offscreen]
  First GPU step: load the Mesa bundle, make a surfaceless GLES 3 context, set the mode on the
  display node, create three scanout buffers, check one cleared frame by readback and through the
  CPU mapping, then clear to a slowly changing colour and page-flip for N seconds (default 5).
  --flip-wait  poll: poll() on the card fd with a 1 s timeout, then read() (default);
               inline: blocking read() on the main thread, as the C reference;
               thread: a dedicated reader thread forwarding events over a channel
  --offscreen  no DRM nodes: context, one ordinary GL framebuffer, clear and readback only";

/// Entry for `--gl-probe`. Prints `WMGL` lines; step lines also go to `/dev/console`.
pub fn probe(args: &[String]) -> Result<(), String> {
    let (mut seconds, mut wait, mut offscreen) = (5.0f64, FlipWait::Poll, false);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" => {
                println!("{PROBE_USAGE}");
                return Ok(());
            }
            "--seconds" => {
                seconds = iter.next().and_then(|s| s.parse().ok()).filter(|s| (0.0..=600.0).contains(s)).ok_or(PROBE_USAGE)?;
            }
            "--flip-wait" => {
                wait = match iter.next().map(String::as_str) {
                    Some("poll") => FlipWait::Poll,
                    Some("inline") => FlipWait::Inline,
                    Some("thread") => FlipWait::Thread,
                    _ => return Err(PROBE_USAGE.into()),
                };
            }
            "--offscreen" => offscreen = true,
            _ => return Err(PROBE_USAGE.into()),
        }
    }
    let result = probe_run(seconds, wait, offscreen);
    match &result {
        Ok(()) => println!("WMGL done status=ok"),
        Err(error) => println!("WMGL done status=error error={error:?}"),
    }
    result
}

/// Clear the bound framebuffer to `rgb` and wait for the GPU.
unsafe fn clear(gl: &Gl, fbo: GLuint, width: u32, height: u32, rgb: [f32; 3]) {
    // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
    unsafe {
        (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fbo);
        (gl.glViewport)(0, 0, width as GLsizei, height as GLsizei);
        (gl.glClearColor)(rgb[0], rgb[1], rgb[2], 1.0);
        (gl.glClear)(GL_COLOR_BUFFER_BIT);
        (gl.glFinish)();
    }
}
/// RGBA of the pixel at the bottom-left of the bound framebuffer.
unsafe fn read_pixel(gl: &Gl) -> [u8; 4] {
    let mut pixel = [0u8; 4];
    // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
    unsafe { (gl.glReadPixels)(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel.as_mut_ptr() as *mut c_void) };
    pixel
}

fn probe_run(seconds: f64, wait: FlipWait, offscreen: bool) -> Result<(), String> {
    let mut steps = Steps::new("chonkstep-gl:");
    steps.step("dlopen libdrm, libEGL, libGLESv2");
    let libs = Libraries::load()?;
    let context = Context::create(&libs, &mut |s| steps.step(s))?;
    let gl = &libs.gl;
    println!("WMGL info renderer={:?} version={:?}", context.renderer, context.version);
    let import = context.has_display_extension("EGL_EXT_image_dma_buf_import");
    let oes = context.has_gl_extension("GL_OES_EGL_image");
    println!("WMGL info EGL_EXT_image_dma_buf_import={import} GL_OES_EGL_image={oes}");
    // A fixed colour whose 8-bit values are exact, for the readback check.
    let nonce = [0x33u8, 0x99, 0xcc];
    let nonce_f = nonce.map(|c| f32::from(c) / 255.0);
    if offscreen {
        let (width, height) = (256u32, 256u32);
        let (mut texture, mut fbo) = (0, 0);
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            (gl.glGenTextures)(1, &mut texture);
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            (gl.glTexImage2D)(GL_TEXTURE_2D, 0, GL_RGBA8 as GLint, width as GLsizei, height as GLsizei, 0, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null());
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.glGenFramebuffers)(1, &mut fbo);
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fbo);
            (gl.glFramebufferTexture2D)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
            let status = (gl.glCheckFramebufferStatus)(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                return Err(format!("offscreen framebuffer status {status:#x}"));
            }
            clear(gl, fbo, width, height, nonce_f);
            let pixel = read_pixel(gl);
            let ok = pixel[..3] == nonce;
            println!(
                "WMGL check target=offscreen expected={nonce:02x?} gl_readback={:02x?} result={} gl_error={:#x}",
                &pixel[..3],
                if ok { "equal" } else { "DIFFERENT" },
                (gl.glGetError)()
            );
            return if ok { Ok(()) } else { Err("offscreen readback differs".into()) };
        }
    }
    if !import || !oes {
        return Err("EGL_EXT_image_dma_buf_import / GL_OES_EGL_image not advertised".into());
    }
    let mut kms = Kms::open(&libs, wait, &mut steps)?;
    let (width, height) = (kms.width(), kms.height());
    println!(
        "WMGL info card={} render={} mode={}x{}@{} flip_wait={}",
        kms.card_path, kms.render_path, width, height, kms.refresh(), kms.flip_source()
    );
    let mut buffers = Vec::new();
    for index in 0..3 {
        buffers.push(kms.create_buffer(&libs, &context, index, &mut steps)?);
    }
    // One cleared frame, seen three ways: what was asked for, what GL reads
    // back, and the bytes of the same memory through the CPU mapping
    // (little-endian ARGB8888/XRGB8888 words: B, G, R, x).
    // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given. `buffers[0].map` is a live CPU mapping of that buffer, at least one pixel long.
    let (pixel, mapped) = unsafe {
        clear(gl, buffers[0].fbo, width, height, nonce_f);
        let pixel = read_pixel(gl);
        let m = std::slice::from_raw_parts(buffers[0].map, 4);
        (pixel, [m[2], m[1], m[0]])
    };
    let check = pixel[..3] == nonce && mapped == nonce;
    println!(
        "WMGL check target=scanout expected={nonce:02x?} gl_readback={:02x?} cpu_map={mapped:02x?} result={}",
        &pixel[..3],
        if check { "equal" } else { "DIFFERENT" }
    );
    // The checked frame is shown first (this is the modeset), then the animation.
    let (mut front, mut pending): (Option<usize>, Option<usize>) = (None, None);
    let (mut render_us, mut wait_us, mut interval_us) = (Vec::new(), Vec::new(), Vec::new());
    let (mut flips, mut missed, mut first_sequence, mut last_sequence) = (0u32, 0u32, 0u32, 0u32);
    let mut last_flip: Option<Instant> = None;
    let begin = Instant::now();
    let mut frame: i64 = -1;
    loop {
        let start = Instant::now();
        let mut target = 0;
        if frame >= 0 {
            target = (0..3).find(|i| Some(*i) != front && Some(*i) != pending).unwrap();
            // Triangle waves of different periods (no libm call): a slow colour drift.
            let wave = |period: i64| {
                let t = (frame % period) as f32 / period as f32;
                1.0 - (2.0 * t - 1.0).abs()
            };
            let rgb = [wave(240), wave(360), wave(600)];
            // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
            unsafe { clear(gl, buffers[target].fbo, width, height, rgb) };
            render_us.push(start.elapsed().as_micros() as u64);
        }
        let rendered = Instant::now();
        if pending.is_some() {
            // Bounded for the sources that can time out: a lost event is then
            // an error line, not a hung guest.
            let flip = kms
                .wait_flip(Some(Duration::from_secs(1)))?
                .ok_or("no flip-complete event within 1 s of a commit")?;
            wait_us.push(rendered.elapsed().as_micros() as u64);
            if let Some(last) = last_flip {
                let interval = flip.at.duration_since(last).as_micros() as u64;
                interval_us.push(interval);
                missed += u32::from(interval > 25_000);
            }
            if flips == 0 {
                first_sequence = flip.sequence;
                steps.step(&format!("first flip-complete event, sequence {}", flip.sequence));
            }
            flips += 1;
            last_sequence = flip.sequence;
            last_flip = Some(flip.at);
            front = pending.take();
        }
        if frame < 0 {
            steps.step("first commit (sets the mode)");
        }
        // Every commit here follows the event of the one before: busy is an error.
        if kms.commit(&libs, &buffers[target], &mut steps)? == Committed::Busy {
            return Err("drmModeAtomicCommit: EBUSY although the previous flip had completed".into());
        }
        pending = Some(target);
        if frame < 0 {
            steps.step("waiting for flip-complete events from here on");
        }
        if begin.elapsed().as_secs_f64() >= seconds && frame >= 0 {
            break;
        }
        frame += 1;
    }
    kms.wait_flip(Some(Duration::from_secs(1)))?;
    let elapsed = begin.elapsed().as_secs_f64();
    let (min, max) = (
        interval_us.iter().copied().min().unwrap_or(0) as f64 / 1000.0,
        interval_us.iter().copied().max().unwrap_or(0) as f64 / 1000.0,
    );
    // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
    let gl_error = unsafe { (gl.glGetError)() };
    println!(
        "WMGL scene=clear frames={} seconds={elapsed:.2} fps={:.1} render_ms_median={:.2} flip_wait_ms_median={:.2} missed_refreshes={missed} ebusy_retries={} flips={flips} interval_ms_min={min:.2} interval_ms_max={max:.2} event_sequence_span={} flip_wait={} gl_error={gl_error:#x}",
        render_us.len(),
        if elapsed > 0.0 { f64::from(flips) / elapsed } else { 0.0 },
        median(&mut render_us),
        median(&mut wait_us),
        kms.ebusy_retries,
        last_sequence.wrapping_sub(first_sequence),
        kms.flip_source(),
    );
    if !check {
        return Err("scanout pixel check differs".into());
    }
    if gl_error != GL_NO_ERROR {
        return Err(format!("GL error {gl_error:#x}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u32, length: u32, sequence: u32) -> Vec<u8> {
        let mut e = vec![0u8; length as usize];
        e[0..4].copy_from_slice(&kind.to_ne_bytes());
        e[4..8].copy_from_slice(&length.to_ne_bytes());
        if length >= 28 {
            e[24..28].copy_from_slice(&sequence.to_ne_bytes());
        }
        e
    }

    #[test]
    fn drm_event_records_are_parsed_like_the_reference() {
        // vblank (type 1), flip complete, an unknown longer event, flip complete.
        let mut buffer = event(1, 32, 7);
        buffer.extend(event(DRM_EVENT_FLIP_COMPLETE, 32, 41));
        buffer.extend(event(0x8000_0000, 48, 0));
        buffer.extend(event(DRM_EVENT_FLIP_COMPLETE, 32, 42));
        let mut seen = Vec::new();
        parse_events(&buffer, |s| seen.push(s));
        assert_eq!(seen, [41, 42]);
        // A short flip event carries no sequence; a zero length cannot loop forever.
        let mut seen = Vec::new();
        parse_events(&event(DRM_EVENT_FLIP_COMPLETE, 8, 0), |s| seen.push(s));
        assert_eq!(seen, [0]);
        let mut bad = event(1, 32, 0);
        bad[4..8].copy_from_slice(&0u32.to_ne_bytes());
        parse_events(&bad, |_| panic!("no flip in this buffer"));
        parse_events(&[1, 2, 3], |_| panic!("too short"));
    }

    /// A display that refuses the next `busy` flips and delivers the events it is given.
    #[derive(Default)]
    struct FakeDisplay {
        busy: u32,
        events: std::collections::VecDeque<u32>,
        flips: Vec<usize>,
        refused: u32,
        /// Queue the event of an accepted flip at once (an instant display).
        instant: bool,
    }
    impl Scanout for FakeDisplay {
        fn flip(&mut self, index: usize) -> Result<Committed, String> {
            if self.busy > 0 {
                self.busy -= 1;
                self.refused += 1;
                return Ok(Committed::Busy);
            }
            self.flips.push(index);
            if self.instant {
                self.events.push_back(self.flips.len() as u32);
            }
            Ok(Committed::Queued)
        }
        fn wait(&mut self, _: Option<Duration>) -> Result<Option<Flip>, String> {
            Ok(self.events.pop_front().map(|sequence| Flip { sequence, at: Instant::now() }))
        }
    }
    const NOW: Duration = Duration::ZERO;

    #[test]
    fn a_flip_is_in_flight_until_its_event() {
        let (mut queue, mut display) = (FlipQueue::new(), FakeDisplay::default());
        assert!(!queue.busy());
        queue.submit(&mut display, 1).unwrap();
        assert!(queue.busy() && queue.front().is_none());
        assert!(queue.poll(&mut display, NOW).unwrap().is_none());
        // A second frame before the event is the caller's mistake.
        assert!(queue.submit(&mut display, 2).is_err());
        display.events.push_back(7);
        let shown = queue.poll(&mut display, NOW).unwrap().unwrap();
        assert_eq!((shown.index, shown.flip.sequence), (1, 7));
        assert!(!queue.busy());
        assert_eq!(queue.front(), Some(1));
    }

    /// The failure on the M1: the power-up commit returns before the panel is
    /// back, and a flip sent straight after it is refused with EBUSY.
    #[test]
    fn no_flip_is_sent_between_the_power_up_commit_and_its_event() {
        let (mut queue, mut display) = (FlipQueue::new(), FakeDisplay::default());
        display.instant = true;
        queue.submit(&mut display, 0).unwrap();
        queue.poll(&mut display, NOW).unwrap().unwrap();
        // Off, then on: the display stays busy until the power-up event.
        display.instant = false;
        display.busy = u32::MAX;
        queue.powered_up();
        for _ in 0..50 {
            assert!(queue.busy(), "the queue must hold frames back until the event");
            assert!(queue.poll(&mut display, NOW).unwrap().is_none());
        }
        // Nothing was offered to the display meanwhile: not one EBUSY.
        assert_eq!(display.refused, 0);
        // The event: the panel is back. It is not a frame.
        display.busy = 0;
        display.events.push_back(100);
        assert!(queue.poll(&mut display, NOW).unwrap().is_none());
        assert!(!queue.busy());
        assert_eq!(queue.front(), Some(0));
        queue.submit(&mut display, 1).unwrap();
        display.events.push_back(101);
        assert_eq!(queue.poll(&mut display, NOW).unwrap().unwrap().index, 1);
        assert_eq!((queue.wake_timeouts, queue.held_frames, display.refused), (0, 0, 0));
    }

    #[test]
    fn a_lost_power_up_event_is_reported_and_a_busy_display_holds_the_frame() {
        let (mut queue, mut display) = (FlipQueue::new(), FakeDisplay::default());
        queue.wake_wait = Duration::from_millis(20);
        queue.powered_up();
        assert!(queue.poll(&mut display, NOW).unwrap().is_none());
        assert!(queue.busy());
        std::thread::sleep(Duration::from_millis(25));
        // Bounded: an error line and a count, not a failed session.
        assert!(queue.poll(&mut display, NOW).unwrap().is_none());
        assert!(!queue.busy());
        assert_eq!(queue.wake_timeouts, 1);
        // The display is still finishing: the frame is kept, not dropped and not an error.
        display.busy = 3;
        queue.submit(&mut display, 2).unwrap();
        assert!(queue.busy());
        for _ in 0..3 {
            assert!(queue.poll(&mut display, NOW).unwrap().is_none());
            assert!(queue.busy());
        }
        assert_eq!((display.refused, display.flips.as_slice()), (3, &[2usize][..]));
        display.events.push_back(5);
        let shown = queue.poll(&mut display, NOW).unwrap().unwrap();
        assert_eq!(shown.index, 2);
        assert_eq!(queue.held_frames, 1);
        assert!(!queue.busy());
    }

    #[test]
    fn a_display_that_stays_busy_is_an_error_after_the_bound() {
        let (mut queue, mut display) = (FlipQueue::new(), FakeDisplay::default());
        queue.busy_wait = Duration::from_millis(20);
        display.busy = u32::MAX;
        queue.submit(&mut display, 0).unwrap();
        assert!(queue.poll(&mut display, NOW).unwrap().is_none());
        std::thread::sleep(Duration::from_millis(25));
        let error = queue.poll(&mut display, NOW).unwrap_err();
        assert!(error.contains("EBUSY"), "{error}");
    }

    #[test]
    fn asahi_ioctl_arguments_have_the_reference_layout() {
        assert_eq!(std::mem::size_of::<AsahiGemCreate>(), 24);
        assert_eq!(std::mem::size_of::<AsahiGemMmapOffset>(), 16);
        // _IOWR('d', 0x40 + n, size): direction 0xC0000000, size << 16, 'd' << 8.
        let iowr = |n: u64, size: u64| 0xC000_0000u64 | (size << 16) | (0x64 << 8) | (0x40 + n);
        assert_eq!(DRM_IOCTL_ASAHI_GEM_CREATE, iowr(5, 24));
        assert_eq!(DRM_IOCTL_ASAHI_GEM_MMAP_OFFSET, iowr(6, 16));
    }

    #[test]
    fn poll_source_times_out_then_delivers_every_queued_event() {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a two-element array, as pipe requires.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut source = PollRead { fd: fds[0], queued: Default::default(), timeouts: 0 };
        let start = Instant::now();
        assert!(source.wait(Some(Duration::from_millis(30))).unwrap().is_none());
        assert!(start.elapsed() >= Duration::from_millis(30));
        assert!(source.wait(Some(Duration::ZERO)).unwrap().is_none());
        assert_eq!(source.timeouts, 2);
        // Two events in one read: both are delivered, the second without polling.
        let mut bytes = event(DRM_EVENT_FLIP_COMPLETE, 32, 5);
        bytes.extend(event(DRM_EVENT_FLIP_COMPLETE, 32, 6));
        // SAFETY: `fds[1]` is the write end of the pipe made above and `bytes` is a live buffer of the length passed.
        assert_eq!(unsafe { libc::write(fds[1], bytes.as_ptr() as *const c_void, bytes.len()) }, 64);
        assert_eq!(source.wait(Some(Duration::from_secs(5))).unwrap().unwrap().sequence, 5);
        assert_eq!(source.wait(Some(Duration::ZERO)).unwrap().unwrap().sequence, 6);
        assert!(source.wait(Some(Duration::ZERO)).unwrap().is_none());
        // SAFETY: both ends of the pipe made above are owned by this test and closed once.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[test]
    fn reader_thread_forwards_events_and_honours_timeouts() {
        let mut fds = [0; 2];
        // SAFETY: `fds` is a two-element array, as pipe requires.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let mut source = ReaderThread::spawn(fds[0]).unwrap();
        assert!(source.wait(Some(Duration::from_millis(20))).unwrap().is_none());
        let bytes = event(DRM_EVENT_FLIP_COMPLETE, 32, 99);
        // SAFETY: `fds[1]` is the write end of the pipe made above and `bytes` is a live buffer of the length passed.
        assert_eq!(unsafe { libc::write(fds[1], bytes.as_ptr() as *const c_void, bytes.len()) }, 32);
        let flip = source.wait(Some(Duration::from_secs(5))).unwrap().unwrap();
        assert_eq!(flip.sequence, 99);
        // Closing the write end ends the thread with an error, not a hang.
        // SAFETY: both ends of the pipe made above are owned by this test and closed once.
        unsafe { libc::close(fds[1]) };
        assert!(source.wait(Some(Duration::from_secs(5))).is_err());
    }
}
