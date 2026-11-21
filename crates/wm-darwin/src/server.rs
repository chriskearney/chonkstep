//! Pure Wayland SHM frontend. Protocol success corresponds to real owned pixels.
use std::{collections::HashMap, sync::Arc};
use smithay::{delegate_compositor,delegate_shm,delegate_xdg_shell,delegate_xdg_decoration,delegate_output,delegate_seat};
use smithay::reexports::wayland_server::{Client,Display,DisplayHandle,Resource,
    backend::{ClientData,ClientId,DisconnectReason},
    protocol::{wl_surface::{self,WlSurface},wl_buffer,wl_callback,wl_seat,wl_output}};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::input::{Seat,SeatState,SeatHandler,pointer::CursorImageStatus};
use smithay::output::{Output,PhysicalProperties,Subpixel,Mode,Scale};
use smithay::utils::{Serial,Transform};
use smithay::wayland::{
    buffer::BufferHandler,
    compositor::{CompositorClientState,CompositorHandler,CompositorState,SurfaceAttributes,BufferAssignment,Damage,with_states},
    shell::xdg::{XdgShellHandler,XdgShellState,ToplevelSurface,PopupSurface,PositionerState,XdgToplevelSurfaceData,
        SurfaceCachedState,decoration::{XdgDecorationState,XdgDecorationHandler}},
    shm::{ShmHandler,ShmState}, output::OutputHandler,
    fractional_scale::{FractionalScaleHandler,FractionalScaleManagerState,with_fractional_scale},
    viewporter::{ViewporterState,ViewportCachedState},
};
use crate::scale::Scale as DisplayScale;
use wm_core::{BackendEvent,ClientChrome,WindowManager,Size};
use crate::{damage::DRect,pixels::ShmCopy,scene::ExternalBuffer,render::DmabufDesc};
use smithay::wayland::dmabuf::{DmabufState,DmabufGlobal,DmabufHandler,ImportNotifier};
use smithay::wayland::selection::{SelectionHandler,SelectionSource,SelectionTarget,
    data_device::{ClientDndGrabHandler,DataDeviceHandler,DataDeviceState,ServerDndGrabHandler,set_data_device_focus}};
use smithay::backend::allocator::{dmabuf::Dmabuf,Buffer as _,Format,Fourcc,Modifier};
use smithay::reexports::wayland_server::backend::ObjectId;
use crate::{scene::CpuBackend,presenter::Presenter,render::{Renderer,RendererKind,RendererRequest,FrameStats,FrameTotals}};

#[derive(Default)]
pub struct Peer {
    compositor: CompositorClientState,
}
impl ClientData for Peer {
    fn initialized(&self, _: ClientId) {}
    // Why a client went, on the console: a protocol error names the object
    // and the message, which is otherwise known only to the client.
    fn disconnected(&self, _: ClientId, reason: DisconnectReason) {
        eprintln!("WMCLIENT disconnected reason={reason:?}");
    }
}

pub struct Server {
    compositor: CompositorState,
    shell: XdgShellState,
    shm: ShmState,
    _decoration: XdgDecorationState,
    output: Output,
    pub(crate) seats: SeatState<Self>,
    pub(crate) input: Option<crate::input::InputState>,
    pub(crate) cursor_callbacks: Vec<wl_callback::WlCallback>,
    pub wm: WindowManager<CpuBackend>,
    callbacks: HashMap<u64, Vec<wl_callback::WlCallback>>,
    renderer: Box<dyn Renderer>,
    frames: FrameTotals,
    /// (print a WMSTAT line every N frames, totals at the last line)
    frame_log: Option<(u64, FrameTotals, std::time::Instant)>,
    /// Frame callbacks of a frame that is on its way to the screen; they are
    /// sent when its flip completes.
    awaiting_flip: Vec<wl_callback::WlCallback>,
    dmabuf: DmabufState,
    dmabuf_global: Option<DmabufGlobal>,
    /// wl_buffer -> the texture the renderer made of its dma-buf.
    external_by_buffer: HashMap<ObjectId, ExternalBuffer>,
    /// Users of each imported dma-buf: its wl_buffer while alive, and every
    /// window showing it. The renderer's texture goes with the last one.
    external_refs: HashMap<u64, u32>,
    /// window -> the client buffer it shows (not copied, so not yet released).
    held: HashMap<u64, (wl_buffer::WlBuffer, u64)>,
    /// Replaced client buffers, released once a frame without them is rendered.
    release_after_frame: Vec<wl_buffer::WlBuffer>,
    /// The dma-buf behind each imported id (keeps its fd open: fence polls, checks).
    external_dmabufs: HashMap<u64, Dmabuf>,
    /// Committed dma-bufs whose rendering may not be finished: (id, since).
    /// The next frame waits for them (implicit sync), up to `FENCE_WAIT`.
    pending_fences: Vec<(u64, std::time::Instant)>,
    wait_for_fences: bool,
    /// Brightness keys and idle display sleep.
    pub(crate) power: crate::power::Power,
    /// The clipboard: `wl_data_device_manager` for the seat's clients.
    data_device: DataDeviceState,
    display_handle: DisplayHandle,
    /// Selections set by clients since start (clipboard only).
    pub(crate) selections_set: u32,
    dmabuf_stats: DmabufStats,
    /// Commits per window showing a dma-buf, for the periodic content check.
    external_commits: HashMap<u64, u64>,
    checks_due: Vec<u64>,
    /// Outcome of the latest content check: (commit, differing pixels, largest channel difference).
    last_dmabuf_check: Option<(u64, u64, u8)>,
    /// The display scale and the protocols that tell clients about it.
    scale: DisplayScale,
    _fractional_scale: FractionalScaleManagerState,
    _viewporter: ViewporterState,
    pub(crate) scale_stats: ScaleStats,
}

/// A window's sizes: what its client committed and what is on the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowView {
    /// The surface in logical units.
    pub logical: Size,
    /// The window's content on the panel.
    pub content: wm_core::Rect,
    /// The pixels held for it (the client's buffer, or its resampled copy).
    pub image: Size,
    pub unscaled: bool,
}

/// Counters behind the `WMSCALE` lines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScaleStats {
    /// Scale changes since start.
    pub changes: u32,
    /// Commits whose buffer was at the panel's density and went to the screen untouched.
    pub exact_commits: u64,
    /// Commits whose buffer was resampled (a client that does not render at the display scale).
    pub resampled_commits: u64,
    /// dma-buf commits shown unscaled because they were not at the display scale.
    pub unscaled_commits: u64,
}

/// Edge snapping distance in logical units (wm-core's default at 100%).
pub const SNAP_LOGICAL: i32 = 10;
/// Longest a frame is held back for a client buffer's fence.
const FENCE_WAIT: std::time::Duration = std::time::Duration::from_millis(100);
/// A window's dma-buf content is checked against the rendered frame at these commits.
const CHECK_AT_COMMITS: [u64; 2] = [30, 150];

/// Counters behind the `WMDMABUF` lines.
#[derive(Clone, Copy, Debug, Default)]
pub struct DmabufStats {
    pub imported: u64,
    pub refused: u64,
    pub commits: u64,
    /// Commits whose buffer was not ready at the first look.
    pub fence_waits: u64,
    pub fence_wait_max_us: u64,
    /// Buffers still not ready after `FENCE_WAIT`; the frame was rendered anyway.
    pub fence_timeouts: u64,
    pub fence_poll_errors: u64,
}
impl Server {
    pub fn new(dh: &DisplayHandle, size: Size) -> Result<Self, String> {
        Self::new_with_fonts(dh, size, wm_theme::FontState::new())
    }
    pub fn new_with_fonts(dh: &DisplayHandle, size: Size, fonts: wm_theme::FontState) -> Result<Self, String> {
        let backend = CpuBackend::new(size)?;
        // A CPU renderer until the entry point installs the selected one
        // (`set_renderer_boxed`); the GL renderer owns an output of its own.
        let renderer = RendererRequest::from_env()?.cpu_kind().create_cpu();
        let frame_log = match std::env::var(crate::render::STATS_ENV) {
            Ok(every) => Some((
                every.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                    format!("{} must be a positive frame count", crate::render::STATS_ENV)
                })?,
                FrameTotals::default(),
                std::time::Instant::now(),
            )),
            Err(_) => None,
        };
        // Version 6: `wl_surface.preferred_buffer_scale`.
        let compositor = CompositorState::new_v6::<Self>(dh);
        // Subsurface composition is not in this first increment. Do not advertise it.
        dh.remove_global::<Self>(compositor.subcompositor_global());
        let output = Output::new(
            "SFGOP-0".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "SiliconForest".into(),
                model: "SFGOP mapped framebuffer".into(),
            },
        );
        output.create_global::<Self>(dh);
        // No measured vblank period; refresh=0 is unknown, not a promised 60Hz mode.
        let mode = Mode {
            size: (size.w as i32, size.h as i32).into(),
            refresh: 0,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        Ok(Self {
            compositor,
            shell: XdgShellState::new_with_capabilities::<Self>(dh, []),
            shm: ShmState::new::<Self>(dh, vec![]),
            _decoration: XdgDecorationState::new::<Self>(dh),
            output,
            wm: WindowManager::new(
                backend,
                Box::new(wm_theme::RasterThemeEngine::with_fonts(
                    wm_theme::default_theme::nextstep_classic(),
                    fonts,
                )),
            ),
            seats: SeatState::new(),
            input: None, cursor_callbacks: Vec::new(),
            callbacks: HashMap::new(),
            renderer,
            frames: FrameTotals::default(),
            frame_log,
            awaiting_flip: Vec::new(),
            dmabuf: DmabufState::new(),
            dmabuf_global: None,
            external_by_buffer: HashMap::new(),
            external_refs: HashMap::new(),
            held: HashMap::new(),
            release_after_frame: Vec::new(),
            external_dmabufs: HashMap::new(),
            pending_fences: Vec::new(),
            wait_for_fences: std::env::var("CHONKSTEP_DARWIN_DMABUF_WAIT").map_or(true, |v| v != "off"),
            power: crate::power::Power::new(crate::power::idle_from_env()?),
            data_device: DataDeviceState::new::<Self>(dh),
            display_handle: dh.clone(),
            selections_set: 0,
            dmabuf_stats: DmabufStats::default(),
            external_commits: HashMap::new(),
            checks_due: Vec::new(),
            last_dmabuf_check: None,
            scale: DisplayScale::ONE,
            _fractional_scale: FractionalScaleManagerState::new::<Self>(dh),
            _viewporter: ViewporterState::new::<Self>(dh),
            scale_stats: ScaleStats::default(),
        })
    }
    /// The display scale in force.
    pub fn scale(&self) -> DisplayScale {
        self.scale
    }
    /// Every committed window as the scale machinery sees it.
    pub fn window_views(&self) -> Vec<WindowView> {
        self.wm
            .backend()
            .windows
            .values()
            .filter(|w| w.committed)
            .map(|w| WindowView {
                logical: w.logical,
                content: w.content,
                image: w.image.as_ref().map_or(Size::new(0, 0), |i| Size::new(i.width, i.height)),
                unscaled: w.unscaled,
            })
            .collect()
    }
    /// The size of the pointer image on the panel, if one is shown.
    pub fn cursor_size(&self) -> Option<(u32, u32)> {
        self.wm.backend().cursor().map(|(_, image)| (image.width, image.height))
    }
    /// `wl_output.scale` as clients are told it.
    pub fn output_scale_integer(&self) -> i32 {
        self.output.current_scale().integer_scale()
    }
    pub fn scale_stats(&self) -> ScaleStats {
        self.scale_stats
    }
    /// Tell a surface's client the scale to render at: the whole number
    /// (`preferred_buffer_scale`) and, where it asked, the exact one.
    pub(crate) fn announce_scale(&self, surface: &WlSurface) {
        let scale = self.scale;
        with_states(surface, |states| {
            smithay::wayland::compositor::send_surface_state(surface, states, scale.integer(), Transform::Normal);
            with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale.factor()));
        });
    }
    /// Change the display scale now. Everything follows in this call or in
    /// the frames right after it: the output and every surface are told the
    /// new scale, the pointer arrow is redrawn at it, the shell redraws its
    /// chrome at it, and every window keeps its logical size and place (so it
    /// grows or shrinks on the panel with everything else) until its client
    /// has rendered at the new density. Returns whether anything changed.
    pub fn set_scale(&mut self, scale: DisplayScale) -> Result<bool, String> {
        let old = self.scale;
        if old == scale {
            return Ok(false);
        }
        self.scale = scale;
        self.wm.backend_mut().scale = scale;
        self.scale_stats.changes += 1;
        let surfaces: Vec<WlSurface> = self.wm.backend().windows.values().map(|w| w.surface.wl_surface().clone()).collect();
        // While the desk is redressed and the windows resized, the policy's
        // resizes ask no client for anything: sizes on the panel are in
        // flux, and `rescale_windows` asks the few clients that need it.
        self.wm.backend_mut().rescaling = true;
        // The shell: theme metrics, fonts, frames, menus, notices.
        let redressed: Result<(), String> = Ok(());
        self.rescale_windows(old);
        self.wm.backend_mut().rescaling = false;
        // Clients hear the new scale after any size they are asked for at
        // it: a client that redraws on the scale alone would otherwise draw
        // once at a size it is about to be asked to give up (a terminal's
        // 80 columns at 300% are wider than this panel).
        self.output.change_current_state(None, None, Some(Scale::Fractional(scale.factor())), None);
        for surface in &surfaces {
            self.announce_scale(surface);
        }
        self.rescale_cursor(old);
        redressed?;
        self.scale_thresholds();
        self.renderer.invalidate();
        self.wm.backend_mut().dirty = true;
        eprintln!(
            "WMSCALE change from={} to={} scale120={} wl_output_scale={} windows={}",
            old.label(), scale.label(), scale.v120(), scale.integer(), surfaces.len()
        );
        Ok(true)
    }
    /// Distances on screen that are policy, not pixels, keep their size on
    /// the glass: the edge a dragged window snaps to is ten logical units
    /// away at every scale. (The double-click distance and the resize
    /// handles already follow the scale, through the theme engine.)
    pub(crate) fn scale_thresholds(&mut self) {
        let snap = self.scale.to_physical(SNAP_LOGICAL).max(0) as u32;
        self.wm.set_snap_threshold(snap);
    }
    /// Windows keep their logical geometry across a scale change: the same
    /// place and size in logical units is another rectangle on the panel.
    /// Until the client answers with a buffer at the new density, what it
    /// last drew is shown at the new size.
    fn rescale_windows(&mut self, old: DisplayScale) {
        let new = self.scale;
        let screen = wm_core::Backend::screen_size(self.wm.backend());
        let moved: Vec<(wm_core::ClientId, u64, Size, bool, wm_core::Rect)> = self
            .wm
            .clients()
            .filter_map(|(id, client)| {
                let w = self.wm.backend().windows.get(&client.window)?;
                if w.unscaled || !w.committed {
                    return None;
                }
                // The size it has; or, when a scale too large for the panel
                // shrank it, the size it had at this scale before (else at
                // the scale it was last free at).
                let logical = if w.shrunk {
                    w.sizes_by_scale.iter().find(|(at, _)| *at == new.v120()).or(w.sizes_by_scale.last()).map_or(w.logical, |(_, size)| *size)
                } else {
                    w.logical
                };
                let size = new.size_to_physical(logical);
                // The content's corner keeps its logical place; the reflow
                // puts the (rescaled) frame around it.
                let pos = wm_core::Point::new(
                    new.to_physical(old.to_logical(client.geometry.pos.x)),
                    new.to_physical(old.to_logical(client.geometry.pos.y)),
                );
                // Keep a grip on screen: a strip of the window stays inside
                // the panel on every side, and its title bar below the top.
                let grip = new.to_physical(48);
                let title = new.to_physical(old.to_logical(client.layout.client_offset.y));
                let pos = wm_core::Point::new(
                    pos.x.min(screen.w as i32 - grip).max(grip - size.w as i32),
                    pos.y.min(screen.h as i32 - grip).max(title),
                );
                Some((id, client.window, logical, w.shrunk, wm_core::Rect::new(pos, size)))
            })
            .collect();
        for (id, window, _, was_shrunk, geometry) in moved {
            // The policy places and sizes; nobody is asked anything yet.
            self.wm.set_client_content_geometry(id, geometry);
            // What the policy made of it (a window may not exceed the panel).
            let Some(size) = self.wm.backend().windows.get(&window).map(|w| w.content.size) else { continue };
            let stretched = self
                .wm
                .backend()
                .windows
                .get(&window)
                .and_then(|w| w.image.as_ref())
                .filter(|i| !i.pixels.is_empty())
                .and_then(|image| crate::pixels::resample(image, size.w, size.h).ok());
            if let Some(stretched) = stretched {
                self.wm.backend_mut().set_window_image(window, Some(stretched));
            }
            // The client was told the new scale and redraws for it at the
            // size it chooses: a terminal keeps its columns and rows, and
            // its window follows the buffer it commits. It is asked for a
            // size only when the panel is too small for the window at this
            // scale, and to get back the size it had before that happened.
            let shrunk = size != geometry.size;
            // (Asked for the largest logical size that is no larger on the
            // panel than the room there is: a size rounded up would come
            // back a pixel too large.)
            let asked = if shrunk {
                Size::new(
                    (1..=self.scale.to_logical(size.w as i32).max(1)).rev().find(|l| self.scale.to_physical(*l) <= size.w as i32).unwrap_or(1) as u32,
                    (1..=self.scale.to_logical(size.h as i32).max(1)).rev().find(|l| self.scale.to_physical(*l) <= size.h as i32).unwrap_or(1) as u32,
                )
            } else {
                self.wm.backend().window_logical(window, size)
            };
            if let Some(w) = self.wm.backend_mut().windows.get_mut(&window) {
                w.shrunk = shrunk;
                if shrunk || was_shrunk {
                    w.surface.with_pending_state(|s| s.size = Some((asked.w as i32, asked.h as i32).into()));
                    w.surface.send_configure();
                }
            }
        }
    }
    pub fn add_client(
        display: &mut Display<Self>,
        stream: std::os::unix::net::UnixStream,
    ) -> std::io::Result<Client> {
        display
            .handle()
            .insert_client(stream, Arc::new(Peer::default()))
    }
    /// Frame callbacks are sent only after the presenter accepts the complete CPU frame.
    /// Native SFGOP acceptance is a memory-store fence, not vblank or panel proof.
    pub fn repaint(&mut self, presenter: &mut impl Presenter, time_ms: u32) -> Result<(), String> {
        self.sync_input_focus();
        // The frame clock of an asynchronous renderer: a completed flip
        // releases the callbacks of the frame it showed.
        if let Some(presented) = self.renderer.poll_presented(std::time::Duration::ZERO)? {
            self.presented(&presented, time_ms);
        }
        // Display sleep: while the display is off nothing is rendered and no
        // frame callback is sent, so clients stop drawing too. The scene
        // stays dirty and is shown in full when input wakes the display.
        self.power_tick()?;
        if self.power.off {
            return Ok(());
        }
        // While a frame is in flight the scene stays dirty and is rendered
        // once, with everything that accumulated, after the flip.
        if !self.wm.backend().dirty || self.renderer.busy() {
            return Ok(());
        }
        // Implicit sync: a client may commit before its GPU work is done. The
        // frame is held back until every freshly committed dma-buf is ready.
        if !self.fences_ready() {
            return Ok(());
        }
        let stats = self.renderer.render_frame(self.wm.backend_mut(), presenter)?;
        self.wm.backend_mut().dirty = false;
        // The frame just rendered no longer samples these client buffers.
        for buffer in self.release_after_frame.drain(..) {
            buffer.release();
        }
        for window in std::mem::take(&mut self.checks_due) {
            self.check_external_window(window);
        }
        self.frames.record(stats);
        self.log_frames(false);
        let mut ready: Vec<wl_callback::WlCallback> = Vec::new();
        if self.wm.backend().cursor().is_some() {
            ready.append(&mut self.cursor_callbacks);
        }
        let visible: Vec<u64> = self
            .callbacks
            .keys()
            .copied()
            .filter(|id| self.wm.backend().visible(*id))
            .collect();
        for id in visible {
            if let Some(callbacks) = self.callbacks.remove(&id) {
                ready.extend(callbacks);
            }
        }
        if self.renderer.busy() {
            self.awaiting_flip.extend(ready);
        } else {
            for callback in ready {
                callback.done(time_ms);
            }
        }
        Ok(())
    }
    /// Blank the display after the idle time; power it up when input asked for it.
    fn power_tick(&mut self) -> Result<(), String> {
        if self.power.off {
            if !self.power.wake_requested {
                return Ok(());
            }
            let started = std::time::Instant::now();
            self.renderer.set_display_power(true)?;
            self.renderer.invalidate();
            self.wm.backend_mut().dirty = true;
            self.power.off = false;
            self.power.wake_requested = false;
            self.power.counts.wakes += 1;
            eprintln!(
                "WMPOWER display=on woken_by=input (the waking event is not delivered) was_off_ms={} wake_commit_ms={}",
                self.power.off_since.take().map_or(0, |t| t.elapsed().as_millis()),
                started.elapsed().as_millis()
            );
            return Ok(());
        }
        self.power.wake_requested = false;
        let Some(idle) = self.power.idle else {
            return Ok(());
        };
        // Without a seat nothing could wake it again.
        if self.power.unsupported || self.input.is_none() || self.power.last_input.elapsed() < idle || self.renderer.busy() {
            return Ok(());
        }
        if !self.renderer.set_display_power(false)? && !self.power.without_display {
            self.power.unsupported = true;
            eprintln!("WMPOWER idle_blank=unavailable renderer={} output={} (this output cannot be powered down)", self.renderer.kind().name(), self.renderer.output_name());
            return Ok(());
        }
        self.power.off = true;
        self.power.off_since = Some(std::time::Instant::now());
        self.power.counts.blanks += 1;
        eprintln!("WMPOWER display=off idle_ms={} timeout_ms={}", self.power.last_input.elapsed().as_millis(), idle.as_millis());
        Ok(())
    }
    /// The display is powered down for idleness.
    pub fn display_blanked(&self) -> bool {
        self.power.off
    }
    /// Blank after this long without input (`None`: never). Returns the previous setting.
    pub fn set_idle_blank(&mut self, idle: Option<std::time::Duration>) -> Option<std::time::Duration> {
        self.power.last_input = std::time::Instant::now();
        std::mem::replace(&mut self.power.idle, idle)
    }
    /// Idle blanking can happen here: there is a seat to wake it and the output can be powered.
    pub fn idle_blank_possible(&self) -> bool {
        self.input.is_some() && !self.power.unsupported && (self.power.without_display || self.renderer.output_name() == "kms")
    }
    /// Tests: go through display sleep although the renderer has no display.
    pub fn idle_blank_without_display(&mut self) {
        self.power.without_display = true;
        self.power.unsupported = false;
    }
    pub fn power_counts(&self) -> crate::power::PowerCounts {
        self.power.counts
    }
    pub fn set_backlight(&mut self, backlight: Box<dyn crate::power::Backlight>) {
        self.power.set_backlight(backlight);
    }
    pub fn set_mixer(&mut self, mixer: Box<dyn crate::volume::Mixer>) {
        self.power.set_mixer(mixer);
    }
    pub fn mixer(&self) -> &dyn crate::volume::Mixer {
        self.power.mixer()
    }
    pub fn backlight(&self) -> &dyn crate::power::Backlight {
        self.power.backlight()
    }
    /// Clipboard selections clients have set since start.
    pub fn selections_set(&self) -> u32 {
        self.selections_set
    }
    fn presented(&mut self, presented: &crate::render::Presented, time_ms: u32) {
        self.frames.record_presented(presented);
        for callback in self.awaiting_flip.drain(..) {
            callback.done(time_ms);
        }
    }
    fn log_frames(&mut self, last: bool) {
        if let Some((every, since, at)) = &mut self.frame_log {
            if last || self.frames.frames - since.frames >= *every {
                let line = self.frames.summary(since, self.renderer.kind(), self.renderer.output_name(), at.elapsed().as_secs_f64());
                eprintln!("{line}{}", if last { " final=1" } else { "" });
                *since = self.frames;
                *at = std::time::Instant::now();
            }
        }
    }
    /// Print the closing `WMSTAT` line (when frame logging is on) and one
    /// `WMEXT` line per window showing a client dma-buf.
    pub fn log_final_stats(&mut self) {
        self.log_frames(true);
        self.log_external_windows();
        if self.dmabuf_global.is_some() {
            let d = self.dmabuf_stats;
            eprintln!(
                "WMDMABUF total imported={} refused={} commits={} zero_copy=1 implicit_sync={} fence_waits={} fence_wait_max_us={} fence_timeouts={} fence_poll_errors={} formats=XR24:linear,AR24:linear",
                d.imported,
                d.refused,
                d.commits,
                if self.wait_for_fences { "poll-dmabuf-before-render" } else { "off" },
                d.fence_waits,
                d.fence_wait_max_us,
                d.fence_timeouts,
                d.fence_poll_errors,
            );
        }
    }
    /// For each window showing a client dma-buf: the colour the renderer's
    /// last frame has at the centre of the window. A client that paints a
    /// known colour there can be checked without anyone watching.
    pub fn log_external_windows(&mut self) {
        let windows: Vec<(u64, wm_core::Rect, ExternalBuffer)> = {
            let backend = self.wm.backend();
            backend
                .windows
                .iter()
                .filter(|(id, _)| backend.visible(**id))
                .filter_map(|(id, w)| Some((*id, w.content, w.external?)))
                .collect()
        };
        if windows.is_empty() {
            return;
        }
        let width = wm_core::Backend::screen_size(self.wm.backend()).w as usize;
        let frame = match self.renderer.read_back() {
            Ok(Some(frame)) if !self.wm.backend().dirty => frame,
            Ok(_) => {
                eprintln!("WMEXT windows={} result=no-current-frame-to-read-back", windows.len());
                return;
            }
            Err(error) => {
                eprintln!("WMEXT windows={} result=readback-failed error={error:?}", windows.len());
                return;
            }
        };
        for (id, content, external) in windows {
            let (x, y) = (content.pos.x + external.width as i32 / 2, content.pos.y + external.height as i32 / 2);
            let at = (y.max(0) as usize * width + x.max(0) as usize) * 4;
            match frame.get(at..at + 3) {
                Some(p) if x >= 0 && (x as usize) < width && y >= 0 => eprintln!(
                    "WMEXT window={id} buffer={}x{} at={},{} center={x},{y} center_rgb={:02x},{:02x},{:02x}",
                    external.width, external.height, content.pos.x, content.pos.y, p[0], p[1], p[2]
                ),
                _ => eprintln!("WMEXT window={id} buffer={}x{} center={x},{y} result=center-off-screen", external.width, external.height),
            }
        }
    }
    /// Pause between loop iterations: up to `max`, but return as soon as the
    /// frame in flight reaches the screen, so the next one can start at once.
    pub fn idle_wait(&mut self, max: std::time::Duration, time_ms: u32) -> Result<(), String> {
        if !self.renderer.busy() {
            // Held back for a client buffer: wake as soon as it is ready.
            let fence = self.pending_fences.first().and_then(|(id, _)| {
                self.external_dmabufs.get(id)?.handles().next().map(|h| std::os::fd::AsRawFd::as_raw_fd(&h))
            });
            match fence {
                Some(fd) if self.wait_for_fences => {
                    let _ = crate::kms::poll_readable(fd, Some(max));
                }
                _ => std::thread::sleep(max),
            }
            return Ok(());
        }
        if let Some(presented) = self.renderer.poll_presented(max)? {
            self.presented(&presented, time_ms);
        }
        Ok(())
    }
    /// A frame is on its way to the screen.
    pub fn frame_in_flight(&self) -> bool {
        self.renderer.busy()
    }
    /// The frame most recently rendered, if the renderer can read it back.
    pub fn read_back(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.renderer.read_back()
    }
    pub fn output_name(&self) -> &'static str {
        self.renderer.output_name()
    }
    /// Has this toplevel an unanswered configure? Its buffers are then late
    /// answers to an older size, not a statement about the size it wants.
    fn client_behind(surface: &WlSurface) -> bool {
        with_states(surface, |s| {
            s.data_map.get::<XdgToplevelSurfaceData>().is_some_and(|data| {
                let data = data.lock().unwrap();
                !data.pending_configures().is_empty() || data.current_serial != data.configure_serial
            })
        })
    }
    /// Fit every frame to a client that has caught up and drew another size
    /// than it was asked for (a terminal's cell grid). Called when an
    /// interactive resize ends: the client's last answer may have arrived
    /// while the resize was still in progress and was not adopted then.
    pub(crate) fn fit_caught_up_windows(&mut self) {
        let misfits: Vec<(u64, Size)> = self
            .wm
            .backend()
            .windows
            .iter()
            .filter(|(_, w)| w.committed && w.mapped)
            .filter_map(|(id, w)| {
                let image = w.image.as_ref()?;
                let size = Size::new(image.width, image.height);
                (size != w.content.size && !Self::client_behind(w.surface.wl_surface())).then_some((*id, size))
            })
            .collect();
        for (window, size) in misfits {
            self.wm.dispatch(BackendEvent::ClientSizeCommitted { window, size });
        }
    }
    /// Advertise `zwp_linux_dmabuf_v1` (version 3) with the formats the
    /// installed renderer imports without a copy. Returns false, and
    /// advertises nothing, when it imports none (every CPU renderer).
    pub fn enable_dmabuf(&mut self, dh: &DisplayHandle) -> Result<bool, String> {
        if self.dmabuf_global.is_some() {
            return Err("linux-dmabuf already enabled".into());
        }
        let formats: Vec<Format> = self
            .renderer
            .dmabuf_formats()
            .into_iter()
            .filter_map(|(code, modifier)| Some(Format { code: Fourcc::try_from(code).ok()?, modifier: Modifier::from(modifier) }))
            .collect();
        if formats.is_empty() {
            return Ok(false);
        }
        self.dmabuf_global = Some(self.dmabuf.create_global::<Self>(dh, formats));
        Ok(true)
    }
    fn unref_external(&mut self, id: u64) {
        if let Some(count) = self.external_refs.get_mut(&id) {
            *count -= 1;
            if *count == 0 {
                self.external_refs.remove(&id);
                self.external_dmabufs.remove(&id);
                self.pending_fences.retain(|(pending, _)| *pending != id);
                self.renderer.release_dmabuf(id);
            }
        }
    }
    /// True when no freshly committed dma-buf is still being rendered by its
    /// client. A dma-buf fd polls readable once its write fences have
    /// signalled; the look is non-blocking. A buffer that is not ready after
    /// `FENCE_WAIT` is counted and no longer waited for.
    fn fences_ready(&mut self) -> bool {
        if !self.wait_for_fences {
            self.pending_fences.clear();
            return true;
        }
        let mut still = Vec::new();
        for (id, since) in std::mem::take(&mut self.pending_fences) {
            let Some(fd) = self.external_dmabufs.get(&id).and_then(|d| d.handles().next().map(|h| std::os::fd::AsRawFd::as_raw_fd(&h))) else {
                continue;
            };
            match crate::kms::poll_readable(fd, Some(std::time::Duration::ZERO)) {
                Ok(true) => {
                    let waited = since.elapsed().as_micros() as u64;
                    self.dmabuf_stats.fence_wait_max_us = self.dmabuf_stats.fence_wait_max_us.max(waited);
                }
                Ok(false) if since.elapsed() < FENCE_WAIT => still.push((id, since)),
                Ok(false) => self.dmabuf_stats.fence_timeouts += 1,
                Err(_) => self.dmabuf_stats.fence_poll_errors += 1,
            }
        }
        self.pending_fences = still;
        self.pending_fences.is_empty()
    }
    /// Non-blocking: is this dma-buf ready right now? (Unknown counts as ready.)
    fn fences_peek(&self, id: u64) -> bool {
        self.external_dmabufs
            .get(&id)
            .and_then(|d| d.handles().next().map(|h| std::os::fd::AsRawFd::as_raw_fd(&h)))
            .is_none_or(|fd| crate::kms::poll_readable(fd, Some(std::time::Duration::ZERO)).unwrap_or(true))
    }
    /// (commit number, differing pixels, largest channel difference) of the
    /// latest check that could map the client's buffer.
    pub fn last_dmabuf_check(&self) -> Option<(u64, u64, u8)> {
        self.last_dmabuf_check
    }
    pub fn dmabuf_stats(&self) -> DmabufStats {
        self.dmabuf_stats
    }
    /// Is the client's memory what the renderer put in the frame? Compares
    /// the window's region of the frame just rendered with the dma-buf read
    /// through a CPU mapping; if the buffer cannot be mapped, reports a hash
    /// of the region and whether it is flat instead.
    fn check_external_window(&mut self, window: u64) {
        let Some((content, external)) = self.wm.backend().windows.get(&window).and_then(|w| Some((w.content, w.external?))) else {
            return;
        };
        let commits = self.external_commits.get(&window).copied().unwrap_or(0);
        let head = format!("WMDMABUF check window={window} commit={commits} buffer={}x{} at={},{}", external.width, external.height, content.pos.x, content.pos.y);
        let screen = wm_core::Backend::screen_size(self.wm.backend());
        let frame = match self.renderer.read_back() {
            Ok(Some(frame)) => frame,
            _ => {
                eprintln!("{head} result=no-readback");
                return;
            }
        };
        // The part of the window that is on screen, in buffer coordinates.
        let visible = DRect::from_xywh(content.pos.x, content.pos.y, external.width, external.height)
            .intersect(DRect::from_xywh(0, 0, screen.w, screen.h));
        if visible.is_empty() {
            eprintln!("{head} result=off-screen");
            return;
        }
        let row = |y: i32| {
            let at = (y as usize * screen.w as usize + visible.x0 as usize) * 4;
            &frame[at..at + visible.width() as usize * 4]
        };
        let mut region = Vec::with_capacity(visible.area() as usize * 3);
        for y in visible.y0..visible.y1 {
            region.extend(row(y).as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]));
        }
        let flat = region.as_chunks::<3>().0.iter().all(|p| p == &region[..3]);
        let hash = crate::pixels::fingerprint(&region);
        let shape = format!("visible={}x{} frame_region_hash={hash:016x} flat={flat}", visible.width(), visible.height());
        let Some(dmabuf) = self.external_dmabufs.get(&external.id) else {
            eprintln!("{head} {shape} result=buffer-gone");
            return;
        };
        let (Some(fd), Some(offset), Some(stride)) = (dmabuf.handles().next(), dmabuf.offsets().next(), dmabuf.strides().next()) else {
            return;
        };
        let length = offset as usize + stride as usize * external.height as usize;
        // SAFETY: a read-only shared mapping of the client's dma-buf descriptor, which stays open for the call; failure is checked below.
        let map = unsafe {
            libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ, libc::MAP_SHARED, std::os::fd::AsRawFd::as_raw_fd(&fd), 0)
        };
        if map == libc::MAP_FAILED {
            let error = std::io::Error::last_os_error();
            eprintln!("{head} {shape} result=not-mappable errno={} (frame region reported only)", error.raw_os_error().unwrap_or(0));
            return;
        }
        // XRGB8888/ARGB8888 words, little endian: B, G, R, x. Alpha is not
        // compared: an ARGB client over other content blends.
        // SAFETY: `map` is a successful mapping of exactly `length` bytes, read only and unmapped below after the last use of `client`.
        let client = unsafe { std::slice::from_raw_parts(map as *const u8, length) };
        // Whatever is drawn above this window (the cursor, other windows and
        // their frames, menus) legitimately hides its pixels: those are
        // counted as covered, not compared.
        let covers: Vec<DRect> = {
            use crate::damage::ItemKey;
            let list = self.wm.backend().display_list();
            let at = list.iter().position(|(sig, _)| sig.key == ItemKey::Window(window));
            at.map(|at| {
                list[at + 1..]
                    .iter()
                    .filter(|(sig, _)| sig.key != ItemKey::Decoration(window))
                    .map(|(sig, _)| sig.rect)
                    .filter(|r| r.intersects(visible))
                    .collect()
            })
            .unwrap_or_default()
        };
        let (mut differing, mut worst, mut covered) = (0u64, 0u8, 0u64);
        for (i, y) in (visible.y0..visible.y1).enumerate() {
            let source_row = offset as usize + (y - content.pos.y) as usize * stride as usize + (visible.x0 - content.pos.x) as usize * 4;
            for (x, shown) in region[i * visible.width() as usize * 3..][..visible.width() as usize * 3].as_chunks::<3>().0.iter().enumerate() {
                if covers.iter().any(|r| r.contains_point(visible.x0 + x as i32, y)) {
                    covered += 1;
                    continue;
                }
                let p = &client[source_row + x * 4..][..4];
                let d = shown[0].abs_diff(p[2]).max(shown[1].abs_diff(p[1])).max(shown[2].abs_diff(p[0]));
                differing += u64::from(d != 0);
                worst = worst.max(d);
            }
        }
        // SAFETY: `map` and `length` are the mapping made above, and nothing refers to it after this.
        unsafe { libc::munmap(map, length) };
        self.last_dmabuf_check = Some((commits, differing, worst));
        eprintln!(
            "{head} {shape} zero_copy=1 client_mmap=ok compared_pixels={} covered_by_cursor_or_other_surfaces={covered} differing_pixels={differing} max_channel_diff={worst} result={}",
            visible.area() - covered,
            if differing == 0 { "client-pixels-are-on-the-frame" } else { "DIFFERENT" }
        );
    }
    /// The window stops showing its client buffer, if it held one. The buffer
    /// is released to the client after the next rendered frame (`now`: at once).
    fn drop_held(&mut self, window: u64, replacement: Option<&wl_buffer::WlBuffer>, now: bool) {
        if let Some((buffer, id)) = self.held.remove(&window) {
            if Some(&buffer) != replacement {
                if now {
                    buffer.release();
                } else {
                    self.release_after_frame.push(buffer);
                }
            }
            self.unref_external(id);
        }
    }
    /// Imported dma-bufs currently alive (wl_buffers and windows showing them).
    pub fn external_buffer_count(&self) -> usize {
        self.external_refs.len()
    }
    /// Install a renderer built by the entry point (`output::select`).
    pub fn set_renderer_boxed(&mut self, renderer: Box<dyn Renderer>) {
        self.renderer = renderer;
        self.wm.backend_mut().dirty = true;
    }
    pub fn renderer_kind(&self) -> RendererKind {
        self.renderer.kind()
    }
    /// Replace the renderer with a CPU one; the next frame is a complete repaint.
    pub fn set_renderer(&mut self, kind: RendererKind) {
        self.renderer = kind.create_cpu();
        self.wm.backend_mut().dirty = true;
    }
    /// Repaint and present everything on the next frame.
    pub fn damage_everything(&mut self) {
        self.renderer.invalidate();
        self.wm.backend_mut().dirty = true;
    }
    /// Totals over every frame rendered so far.
    pub fn frame_totals(&self) -> FrameTotals {
        self.frames
    }
    /// Cost of the most recently rendered frame.
    pub fn last_frame(&self) -> FrameStats {
        self.frames.last
    }
    /// Whole-buffer import; the selected renderer decides which copy loop.
    pub(crate) fn import_shm(&self, buffer: &wl_buffer::WlBuffer) -> Result<wm_theme_api::DecorationBuffer, String> {
        match self.renderer.kind() {
            RendererKind::Legacy => crate::pixels::copy_shm(buffer),
            RendererKind::Damage | RendererKind::Gl => crate::pixels::copy_shm_full(buffer),
        }
    }
    fn remove(&mut self, surface: &WlSurface) {
        if let Some(id) = self.wm.backend().window_for_surface(surface) {
            self.drop_held(id, None, true);
            self.external_commits.remove(&id);
            self.wm.dispatch(BackendEvent::Destroyed(id));
            self.wm.backend_mut().remove(id);
            self.callbacks.remove(&id);
            self.output.leave(surface);
        }
    }
    pub(crate) fn refuse(surface: &WlSurface, why: &str) {
        surface.post_error(
            wl_surface::Error::InvalidSize,
            format!("Chonkstep Darwin: {why}"),
        );
    }
    fn metadata(&mut self, surface: &ToplevelSurface) {
        if let Some(id) = self.wm.backend().window_for_surface(surface.wl_surface()) {
            let (title, app_id) = with_states(surface.wl_surface(), |s| {
                let d = s
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap();
                (d.title.clone(), d.app_id.clone())
            });
            let w = self.wm.backend_mut().windows.get_mut(&id).unwrap();
            w.title = title;
            w.app_id = app_id;
            self.wm.dispatch(BackendEvent::TitleChanged(id));
        }
    }
    fn decoration(&mut self, surface: ToplevelSurface, mode: DecorationMode) {
        if let Some(id) = self.wm.backend().window_for_surface(surface.wl_surface()) {
            self.wm.backend_mut().windows.get_mut(&id).unwrap().chrome =
                if mode == DecorationMode::ServerSide {
                    ClientChrome::Full
                } else {
                    ClientChrome::Edges
                };
            surface.with_pending_state(|s| s.decoration_mode = Some(mode));
            if surface.is_initial_configure_sent() {
                surface.send_pending_configure();
            }
            self.wm.dispatch(BackendEvent::ChromeChanged(id));
        }
    }
}
impl BufferHandler for Server {
    // SHM buffers are copied and never retained. A dma-buf's texture lives
    // until its wl_buffer is destroyed and no window shows it any more.
    fn buffer_destroyed(&mut self, buffer: &wl_buffer::WlBuffer) {
        if let Some(external) = self.external_by_buffer.remove(&buffer.id()) {
            self.unref_external(external.id);
        }
    }
}
impl DmabufHandler for Server {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }
    /// A client finished describing a buffer (`create` / `create_immed`).
    /// It is accepted only if the renderer really imports it.
    fn dmabuf_imported(&mut self, _: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if dmabuf.num_planes() != 1 || dmabuf.y_inverted() {
            notifier.failed();
            return;
        }
        let (width, height) = (dmabuf.width(), dmabuf.height());
        if crate::pixels::image_len(width, height).is_err() {
            notifier.invalid_dimensions();
            return;
        }
        let format = dmabuf.format();
        let description = dmabuf.handles().next().zip(dmabuf.offsets().next()).zip(dmabuf.strides().next()).map(
            |((fd, offset), stride)| DmabufDesc {
                fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
                width,
                height,
                format: format.code as u32,
                modifier: u64::from(format.modifier),
                offset,
                stride,
            },
        );
        let Some(description) = description else {
            notifier.incomplete();
            return;
        };
        match self.renderer.import_dmabuf(&description) {
            Ok(id) => match notifier.successful::<Self>() {
                Ok(buffer) => {
                    self.external_by_buffer.insert(buffer.id(), ExternalBuffer { id, width, height });
                    self.external_refs.insert(id, 1);
                    self.dmabuf_stats.imported += 1;
                    // The first few say what clients send; after that the totals do.
                    if self.dmabuf_stats.imported <= 8 {
                        eprintln!(
                            "WMDMABUF import id={id} size={width}x{height} format={} modifier={:#x} stride={} zero_copy=1 (EGL image, no copy)",
                            String::from_utf8_lossy(&description.format.to_le_bytes()),
                            description.modifier,
                            description.stride
                        );
                    }
                    self.external_dmabufs.insert(id, dmabuf);
                }
                Err(_) => self.renderer.release_dmabuf(id), // the client is gone
            },
            Err(why) => {
                self.dmabuf_stats.refused += 1;
                eprintln!("chonkstep-darwin: client dma-buf {width}x{height} refused: {why}");
                notifier.failed();
            }
        }
    }
}
impl ShmHandler for Server {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl OutputHandler for Server {}
// Seat capabilities are created only by the explicit SFHID acquisition path.
impl SeatHandler for Server {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }
    // The clipboard follows the keyboard: only the focused client may set
    // the selection, and it is the one told about offers.
    fn focus_changed(&mut self, seat: &Seat<Self>, focus: Option<&WlSurface>) {
        let client = focus.and_then(|surface| surface.client());
        set_data_device_focus(&self.display_handle, seat, client);
    }
    fn cursor_image(&mut self, _: &Seat<Self>, image: CursorImageStatus) {self.cursor_image_changed(image);}
}
impl CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<Peer>().unwrap().compositor
    }
    fn destroyed(&mut self, surface: &WlSurface) {
        if self.input.as_ref().and_then(|i|i.cursor_surface.as_ref())==Some(surface){
            self.input.as_mut().unwrap().cursor_surface=None;self.wm.backend_mut().set_cursor(None);
            self.cursor_callbacks.clear();
        }
        self.remove(surface);
    }
    fn commit(&mut self, surface: &WlSurface) {
        if self.input.as_ref().and_then(|i|i.cursor_surface.as_ref())==Some(surface){self.import_cursor(surface);return;}
        let Some(id) = self.wm.backend().window_for_surface(surface) else {
            return;
        };
        let (buffer, buffer_scale, transform, delta, callbacks, xdg, raw_damage, viewport) = with_states(surface, |s| {
            let xdg = *s.cached_state.get::<SurfaceCachedState>().current();
            let viewport = *s.cached_state.get::<ViewportCachedState>().current();
            let mut attributes = s.cached_state.get::<SurfaceAttributes>();
            let a = attributes.current();
            // (in buffer coordinates?, rectangle); surface damage is
            // converted once the buffer's density is known.
            let damage: Vec<(bool, DRect)> = a
                .damage
                .drain(..)
                .map(|d| match d {
                    Damage::Surface(r) => (false, (r.loc.x, r.loc.y, r.size.w, r.size.h)),
                    Damage::Buffer(r) => (true, (r.loc.x, r.loc.y, r.size.w, r.size.h)),
                })
                .map(|(buffer, (x, y, w, h))| (buffer, DRect::from_xywh(x, y, w.max(0) as u32, h.max(0) as u32)))
                .collect();
            (
                a.buffer.take(),
                a.buffer_scale,
                a.buffer_transform,
                a.buffer_delta.take(),
                std::mem::take(&mut a.frame_callbacks),
                xdg,
                damage,
                viewport,
            )
        });
        // Size hints are in the surface's logical units; the policy works in
        // panel pixels.
        let density = self.wm.backend().density(id);
        let px = |logical: i32| ((f64::from(logical.max(0)) * density).round() as u32).max(1);
        let hints = wm_core::SizeHints {
            min_size: ((xdg.min_size.w, xdg.min_size.h) != (0, 0))
                .then(|| Size::new(if xdg.min_size.w > 0 { px(xdg.min_size.w) } else { 0 }, if xdg.min_size.h > 0 { px(xdg.min_size.h) } else { 0 })),
            max_size: ((xdg.max_size.w, xdg.max_size.h) != (0, 0)).then(|| {
                Size::new(
                    if xdg.max_size.w > 0 { px(xdg.max_size.w) } else { u32::MAX },
                    if xdg.max_size.h > 0 { px(xdg.max_size.h) } else { u32::MAX },
                )
            }),
            resize_increment: None,
        };
        if self.wm.backend().windows[&id].hints != hints {
            self.wm.backend_mut().windows.get_mut(&id).unwrap().hints = hints;
            self.wm.dispatch(BackendEvent::SizeHintsChanged(id));
        }
        if buffer_scale < 1
            || transform != wl_output::Transform::Normal
            || delta.is_some_and(|d| d.x != 0 || d.y != 0)
            || viewport.src.is_some()
        {
            Self::refuse(
                surface,
                "buffer transform/offset/viewport source not implemented in this increment",
            );
            return;
        }
        let destination = viewport.dst.map(|d| Size::new(d.w.max(1) as u32, d.h.max(1) as u32));
        let display_scale = self.scale;
        match buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                if !self.wm.backend().windows[&id].surface.ensure_configured() {
                    return;
                }
                let external = self.external_by_buffer.get(&buffer.id()).copied();
                let buffer_size = match external {
                    Some(external) => Ok((external.width, external.height)),
                    None => crate::pixels::shm_size(&buffer),
                };
                let (buffer_width, buffer_height) = match buffer_size {
                    Ok(size) => size,
                    Err(error) => {
                        buffer.release();
                        Self::refuse(surface, &error);
                        return;
                    }
                };
                // Where this buffer goes: to the panel as it is when the
                // client rendered at the display scale, resampled otherwise.
                let fitted = crate::scale::fit(display_scale, Size::new(buffer_width, buffer_height), buffer_scale, destination);
                // Client damage in buffer coordinates. Surface damage is
                // widened by a pixel: its edges need not fall on buffer pixels.
                let (ratio_x, ratio_y) = (f64::from(buffer_width) / f64::from(fitted.logical.w.max(1)), f64::from(buffer_height) / f64::from(fitted.logical.h.max(1)));
                let damage: Vec<DRect> = raw_damage
                    .iter()
                    .map(|(in_buffer, r)| if *in_buffer || (ratio_x == 1.0 && ratio_y == 1.0) { *r } else {
                        DRect::new(
                            (f64::from(r.x0) * ratio_x).floor() as i32 - 1,
                            (f64::from(r.y0) * ratio_y).floor() as i32 - 1,
                            (f64::from(r.x1) * ratio_x).ceil() as i32 + 1,
                            (f64::from(r.y1) * ratio_y).ceil() as i32 + 1,
                        )
                    })
                    .collect();
                // xdg window geometry is in logical units.
                let logical = fitted.logical;
                let cropped = |_: u32, _: u32| {
                    if xdg.geometry.is_some_and(|g| {
                        g.loc.x != 0
                            || g.loc.y != 0
                            || g.size.w != logical.w as i32
                            || g.size.h != logical.h as i32
                    }) {
                        Err(String::from("cropped xdg window geometry not implemented"))
                    } else {
                        Ok(())
                    }
                };
                let mut unscaled = false;
                let size = if let Some(external) = external {
                    // A dma-buf: nothing is copied. The window shows the
                    // client's GPU memory, so the buffer stays held (not
                    // released) until a newer one has replaced it on screen.
                    if let Err(error) = cropped(external.width, external.height) {
                        Self::refuse(surface, &error);
                        return;
                    }
                    // GPU memory is never resampled: a dma-buf that is not at
                    // the display scale is shown pixel for pixel (small), and
                    // its window is measured in panel pixels.
                    if fitted.exact {
                        self.scale_stats.exact_commits += 1;
                    } else {
                        unscaled = true;
                        if self.scale_stats.unscaled_commits == 0 {
                            eprintln!("WMSCALE unscaled window={id} buffer={buffer_width}x{buffer_height} buffer_scale={buffer_scale} display={} (a dma-buf is shown pixel for pixel)", display_scale.label());
                        }
                        self.scale_stats.unscaled_commits += 1;
                    }
                    *self.external_refs.entry(external.id).or_insert(0) += 1;
                    self.drop_held(id, Some(&buffer), false);
                    self.held.insert(id, (buffer.clone(), external.id));
                    self.dmabuf_stats.commits += 1;
                    self.pending_fences.retain(|(pending, _)| *pending != external.id);
                    self.pending_fences.push((external.id, std::time::Instant::now()));
                    if !self.fences_peek(external.id) {
                        self.dmabuf_stats.fence_waits += 1;
                    }
                    let count = self.external_commits.entry(id).or_insert(0);
                    *count += 1;
                    if CHECK_AT_COMMITS.contains(count) {
                        self.checks_due.push(id);
                    }
                    self.wm.backend_mut().set_window_external(id, external, &damage);
                    Size::new(external.width, external.height)
                } else if !fitted.exact {
                    // Not rendered at the display scale: the whole buffer is
                    // copied and resampled to the size its surface has on
                    // the panel. A client that drew the logical size it was
                    // asked for fills its window exactly (the two differ by
                    // a pixel of rounding at fractional scales).
                    let content = self.wm.backend().windows[&id].content.size;
                    let target = if self.wm.backend().windows[&id].committed && self.wm.backend().window_logical(id, content) == fitted.logical { content } else { fitted.physical };
                    let copied = cropped(buffer_width, buffer_height)
                        .and_then(|()| self.import_shm(&buffer))
                        .and_then(|image| crate::pixels::resample(&image, target.w, target.h));
                    buffer.release();
                    match copied {
                        Ok(image) => {
                            if self.scale_stats.resampled_commits == 0 {
                                eprintln!("WMSCALE resampled window={id} buffer={buffer_width}x{buffer_height} buffer_scale={buffer_scale} shown={}x{} display={} (the client did not render at the display scale)", image.width, image.height, display_scale.label());
                            }
                            self.scale_stats.resampled_commits += 1;
                            self.drop_held(id, None, false);
                            self.wm.backend_mut().set_window_image(id, Some(image));
                            target
                        }
                        Err(error) => {
                            Self::refuse(surface, &error);
                            return;
                        }
                    }
                } else {
                    self.scale_stats.exact_commits += 1;
                    // Legacy: a fresh per-pixel copy of the whole buffer every
                    // commit. Damage: rows of the rectangles the client reported,
                    // into the image already held (whole buffer when the client
                    // reported none or the size changed). Either way a refused
                    // buffer leaves the previous image untouched.
                    let copied = match self.renderer.kind() {
                        RendererKind::Legacy => crate::pixels::copy_shm(&buffer)
                            .and_then(|image| cropped(image.width, image.height).map(|()| ShmCopy::New(image))),
                        RendererKind::Damage | RendererKind::Gl => crate::pixels::copy_shm_damaged(
                            &buffer,
                            self.wm.backend_mut().window_pixels_mut(id),
                            &damage,
                            cropped,
                        ),
                    };
                    buffer.release(); // all CPU reads finished; retain only the copy
                    match copied {
                        Ok(ShmCopy::New(image)) => {
                            let size = Size::new(image.width, image.height);
                            self.drop_held(id, None, false);
                            self.wm.backend_mut().set_window_image(id, Some(image));
                            size
                        }
                        Ok(ShmCopy::Updated(rects)) => {
                            self.wm.backend_mut().window_image_damaged(id, &rects);
                            let image = self.wm.backend().windows[&id].image.as_ref().unwrap();
                            Size::new(image.width, image.height)
                        }
                        Err(error) => {
                            Self::refuse(surface, &error);
                            return;
                        }
                    }
                };
                let w = self.wm.backend_mut().windows.get_mut(&id).unwrap();
                let first = !w.committed;
                w.committed = true;
                w.logical = logical;
                w.unscaled = unscaled;
                // Its size at this scale, for the way back (see `rescale_windows`).
                if !w.shrunk {
                    let at = display_scale.v120();
                    w.sizes_by_scale.retain(|(scale, _)| *scale != at);
                    w.sizes_by_scale.push((at, logical));
                }
                if first {
                    w.content.size = size;
                    self.wm.dispatch(BackendEvent::MapRequest(id));
                    self.output.enter(surface);
                } else {
                    // Fit the frame to a client that has caught up with the
                    // last configure and still drew another size (a terminal
                    // snapping to its cell grid). A buffer committed while a
                    // newer configure is unanswered is merely late: adopting
                    // its size would undo the resize in progress. This is the
                    // rule of the Linux backend (wm-wayland `resize_pending`).
                    let behind = Self::client_behind(surface);
                    let shown = self.wm.backend().windows[&id].content.size;
                    if !behind && size != shown {
                        self.wm
                            .dispatch(BackendEvent::ClientSizeCommitted { window: id, size });
                    }
                }
            }
            Some(BufferAssignment::Removed) => {
                let was_mapped = self.wm.backend().windows[&id].committed;
                self.wm.dispatch(BackendEvent::Unmapped(id));
                self.drop_held(id, None, false);
                self.wm.backend_mut().set_window_image(id, None);
                let w = self.wm.backend_mut().windows.get_mut(&id).unwrap();
                w.committed = false;
                w.mapped = false;
                self.callbacks.remove(&id);
                self.output.leave(surface);
                if was_mapped {
                    // We do not use Smithay renderer utilities, so reset this role
                    // ourselves. Old acknowledgements cannot authorize a remap.
                    let decoration = w.surface.current_state().decoration_mode;
                    with_states(surface, |s| {
                        let mut role = s
                            .data_map
                            .get::<XdgToplevelSurfaceData>()
                            .unwrap()
                            .lock()
                            .unwrap();
                        *role = Default::default();
                        role.current.decoration_mode = decoration;
                        role.current.capabilities = Vec::new().into();
                        let mut cached = s.cached_state.get::<SurfaceCachedState>();
                        *cached.current() = Default::default();
                        *cached.pending() = Default::default();
                    });
                    w.title = None;
                    w.app_id = None;
                } else {
                    w.surface.send_pending_configure();
                }
            }
            None => {
                if !self.wm.backend().windows[&id].committed {
                    self.wm.backend().windows[&id]
                        .surface
                        .send_pending_configure();
                } else if !self.wm.interactive_drag_active() && !Self::client_behind(surface) {
                    // A commit without a buffer that answers the last
                    // configure: the client keeps what it drew. If that is
                    // another size than it was asked for (a terminal whose
                    // grid did not change), the frame is fitted to it now.
                    let w = &self.wm.backend().windows[&id];
                    let drawn = w.image.as_ref().map(|i| Size::new(i.width, i.height));
                    if let Some(size) = drawn.filter(|size| *size != w.content.size && w.mapped) {
                        self.wm.dispatch(BackendEvent::ClientSizeCommitted { window: id, size });
                    }
                }
            }
        }
        self.callbacks.entry(id).or_default().extend(callbacks);
        self.wm.backend_mut().dirty = true;
    }
}
impl XdgShellHandler for Server {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.announce_scale(surface.wl_surface());
        self.wm.backend_mut().add(surface);
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.remove(surface.wl_surface());
    }
    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.metadata(&surface);
    }
    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.metadata(&surface);
    }
    fn parent_changed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.wm.backend().window_for_surface(surface.wl_surface()) {
            self.wm.dispatch(BackendEvent::ParentChanged(id));
        }
    }
    fn move_request(&mut self,surface:ToplevelSurface,seat:wl_seat::WlSeat,serial:Serial){
        if let Some(id)=self.authorize_client_drag(&surface,&seat,serial){self.wm.dispatch(BackendEvent::MoveRequest(id));}
    }
    fn resize_request(&mut self,surface:ToplevelSurface,seat:wl_seat::WlSeat,serial:Serial,edges:smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge){
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge as E;
        use wm_theme_api::ResizeEdge as R;
        let edge=match edges{E::Top=>R::North,E::Bottom=>R::South,E::Left=>R::West,E::Right=>R::East,
            E::TopLeft=>R::NorthWest,E::TopRight=>R::NorthEast,E::BottomLeft=>R::SouthWest,E::BottomRight=>R::SouthEast,_=>return};
        if let Some(window)=self.authorize_client_drag(&surface,&seat,serial){self.wm.dispatch(BackendEvent::ResizeRequest{window,edge});}
    }
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        eprintln!("unsupported: Darwin popup");
        surface.send_popup_done();
    }
    fn grab(&mut self, surface: PopupSurface, _: wl_seat::WlSeat, _: Serial) {
        surface.send_popup_done();
    }
    fn reposition_request(&mut self, surface: PopupSurface, _: PositionerState, _: u32) {
        surface.send_popup_done();
    }
    // No maximize/fullscreen capability is advertised. Refuse requests by responding
    // with the unchanged configure, never claiming that a mode was entered.
    fn maximize_request(&mut self, s: ToplevelSurface) {
        s.send_configure();
    }
    fn unmaximize_request(&mut self, s: ToplevelSurface) {
        s.send_configure();
    }
    fn fullscreen_request(&mut self, s: ToplevelSurface, _: Option<wl_output::WlOutput>) {
        s.send_configure();
    }
    fn unfullscreen_request(&mut self, s: ToplevelSurface) {
        s.send_configure();
    }
    fn minimize_request(&mut self, _: ToplevelSurface) {
        eprintln!("unsupported: Darwin minimize without restore UI");
    }
}
impl XdgDecorationHandler for Server {
    fn new_decoration(&mut self, s: ToplevelSurface) {
        self.decoration(s, DecorationMode::ServerSide);
    }
    fn request_mode(&mut self, s: ToplevelSurface, mode: DecorationMode) {
        self.decoration(s, mode);
    }
    fn unset_mode(&mut self, s: ToplevelSurface) {
        self.decoration(s, DecorationMode::ServerSide);
    }
}
impl FractionalScaleHandler for Server {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        self.announce_scale(&surface);
    }
}
smithay::delegate_fractional_scale!(Server);
smithay::delegate_viewporter!(Server);
delegate_compositor!(Server);
delegate_shm!(Server);
smithay::delegate_dmabuf!(Server);
delegate_xdg_shell!(Server);
delegate_xdg_decoration!(Server);
delegate_output!(Server);
delegate_seat!(Server);
// Clipboard between clients, as `wm-wayland/src/selection.rs` does it on
// Linux, less what belongs to XWayland and persistence: the compositor never
// owns a selection, so `send_selection` has nothing to send.
impl SelectionHandler for Server {
    type SelectionUserData = ();
    fn new_selection(&mut self, target: SelectionTarget, source: Option<SelectionSource>, _: Seat<Self>) {
        if target == SelectionTarget::Clipboard && source.is_some() {
            self.selections_set += 1;
        }
    }
}
impl DataDeviceHandler for Server {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}
// Drags between clients run on the seat's own grab; no icon is drawn.
impl ClientDndGrabHandler for Server {}
impl ServerDndGrabHandler for Server {}
smithay::delegate_data_device!(Server);
// A session must not end with the panel off: whatever runs next (the
// console, another compositor) would have nothing to show it on.
impl Drop for Server {
    fn drop(&mut self) {
        if self.power.off && !self.renderer.busy() {
            if let Err(error) = self.renderer.set_display_power(true) {
                eprintln!("WMPOWER display=on at_exit=failed error={error:?}");
            } else {
                eprintln!("WMPOWER display=on at_exit=1");
            }
        }
    }
}
