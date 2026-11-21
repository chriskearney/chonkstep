//! wm-core's real policy adapter. Geometry is global; decoration parts are local.
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::shell::xdg::ToplevelSurface;
use std::collections::HashMap;
use wm_core::*;
use wm_theme_api::{DecorationBuffer, DecorationLayout, DecorationSurface, ResizeEdge};
use crate::damage::{DRect, ItemKey, ItemSig};

pub struct Window {
    pub surface: ToplevelSurface,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub content: Rect,
    /// Replace or edit only through `CpuBackend::set_window_image` /
    /// `window_image_mut` + `window_image_damaged`, which version the pixels.
    pub(crate) image: Option<DecorationBuffer>,
    generation: u64,
    /// Image-space rectangles changed since the renderer last looked;
    /// `None` means the whole image.
    partial: Option<Vec<DRect>>,
    /// The window shows a client dma-buf (GPU memory, never copied). `image`
    /// then carries only the size, with no pixels.
    pub(crate) external: Option<ExternalBuffer>,
    pub mapped: bool,
    pub committed: bool,
    pub chrome: ClientChrome,
    pub hints: SizeHints,
    /// The surface's size in logical units, as last committed.
    pub(crate) logical: Size,
    /// The client's buffer is shown pixel for pixel although it was not
    /// rendered at the display scale (a dma-buf, which is never resampled):
    /// for this window one logical unit is one panel pixel.
    pub(crate) unscaled: bool,
    /// The logical size the client committed at each display scale it has
    /// been shown at (scale in 120ths, size), while it had the room it
    /// wanted. A terminal's logical size differs a little from scale to
    /// scale (its cells are whole pixels), so the way back to the size a
    /// window had at a scale is to remember it.
    pub(crate) sizes_by_scale: Vec<(u32, Size)>,
    /// A display scale at which the window did not fit the panel made it
    /// smaller. The next scale change sizes it from `sizes_by_scale`, so
    /// going up and down the scales loses nothing. Both are forgotten when
    /// a person resizes the window.
    pub(crate) shrunk: bool,
}
struct Frame {
    window: u64,
    geometry: Rect,
    mapped: bool,
    paint: Option<DecorationSurface>,
    generation: u64,
    layout: DecorationLayout,
}
struct Shell {
    geometry: Rect,
    mapped: bool,
    above: bool,
    image: DecorationBuffer,
    generation: u64,
    partial: Option<Vec<DRect>>,
}

/// More partial rectangles than this and the whole image is damaged instead.
const MAX_PARTIAL_RECTS: usize = 32;

/// A client dma-buf the renderer imported (`Renderer::import_dmabuf`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExternalBuffer {
    pub id: u64,
    pub width: u32,
    pub height: u32,
}

/// One entry of the display list, borrowed from the scene.
pub(crate) enum DrawOp<'a> {
    /// A client dma-buf, drawn like an image at `pos`.
    External { pos: Point, buffer: ExternalBuffer },
    Background { color: [u8; 3], image: Option<&'a DecorationBuffer> },
    Image { pos: Point, image: &'a DecorationBuffer },
    Decoration { pos: Point, surface: &'a DecorationSurface },
}
/// Everything an image at `pos` can touch, including tiny-skia's placement
/// of images with a negative origin (see `render::drawn_image_rect`).
fn image_rect(pos: Point, image: &DecorationBuffer) -> DRect {
    DRect::from_xywh(pos.x, pos.y, image.width, image.height)
        .union(crate::render::drawn_image_rect(pos, image.width, image.height))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopHit {
    Root,
    Shell(u64, Point),
    Frame(u64, Point),
    Client(u64, Point),
}

pub struct CpuBackend {
    pub windows: HashMap<u64, Window>,
    frames: HashMap<u64, Frame>,
    shells: HashMap<u64, Shell>,
    stack: Vec<u64>, // window IDs; each is drawn with its owning frame
    shell_stack: Vec<u64>,
    monitors: Vec<MonitorInfo>,
    next_id: u64,
    background: [u8; 3],
    background_image: Option<DecorationBuffer>,
    pub dirty: bool,
    pub(crate) input_enabled: bool,
    pub(crate) pointer: Point,
    cursor: Option<(Point, DecorationBuffer)>,
    cursor_generation: u64,
    background_generation: u64,
    next_generation: u64,
    pub(crate) replay: bool,
    pub(crate) grabbed_keys: std::collections::HashSet<KeyCombo>,
    pointer_grab: Option<DragHandle>,
    popup_grab: Option<wm_theme_api::PopupGrab>,
    pub(crate) keyboard_grabbed: bool,
    shell_motion: std::collections::VecDeque<(u64, Point)>,
    shell_clicks: std::collections::VecDeque<(u64, Point, MouseButton, bool)>,
    pub(crate) input_fault: Option<String>,
    pub(crate) keyboard_config_verified:bool,
    /// The display scale (see `crate::scale`). The scene itself is in panel
    /// pixels at every scale; this is what a logical unit is worth.
    pub(crate) scale: crate::scale::Scale,
    /// A person is dragging a window's edge right now (set by the input path).
    pub(crate) interactive: bool,
    /// Windows are being given their size at a new display scale: the policy
    /// sets sizes, and the scale change itself decides which clients to ask.
    pub(crate) rescaling: bool,
}
impl CpuBackend {
    pub fn new(size: Size) -> Result<Self, String> {
        crate::pixels::image_len(size.w, size.h)?;
        Ok(Self {
            windows: HashMap::new(),
            frames: HashMap::new(),
            shells: HashMap::new(),
            stack: Vec::new(),
            shell_stack: Vec::new(),
            next_id: 1,
            background: [56, 72, 80],
            background_image: None,
            dirty: true,
            input_enabled: false, pointer: Point::new(0,0), cursor: None,
            cursor_generation: 0, background_generation: 0, next_generation: 1,
            replay: false, grabbed_keys: Default::default(), pointer_grab: None,
            popup_grab: None, keyboard_grabbed: false,
            shell_motion: Default::default(), shell_clicks: Default::default(), input_fault: None,
            keyboard_config_verified:false,
            scale: crate::scale::Scale::ONE,
            interactive: false,
            rescaling: false,
            monitors: vec![MonitorInfo {
                geometry: Rect::new(Point::new(0, 0), size),
                name: "SFGOP-0".into(),
                identity: None,
                primary: true,
            }],
        })
    }
    /// Panel pixels per logical unit of this window's surface.
    pub fn density(&self, id: u64) -> f64 {
        match self.windows.get(&id) {
            Some(w) if w.unscaled => 1.0,
            _ => self.scale.factor(),
        }
    }
    /// A size on the panel in this window's logical units.
    pub fn window_logical(&self, id: u64, physical: Size) -> Size {
        match self.windows.get(&id) {
            Some(w) if w.unscaled => physical,
            _ => self.scale.size_to_logical(physical),
        }
    }
    fn alloc(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    /// Fresh version number for changed pixels. Never reused, so an item that
    /// is destroyed and recreated under the same key still reads as changed.
    fn generation(&mut self) -> u64 {
        let g = self.next_generation;
        self.next_generation += 1;
        g
    }
    pub(crate) fn cursor(&self) -> Option<&(Point, DecorationBuffer)> {
        self.cursor.as_ref()
    }
    pub(crate) fn set_cursor(&mut self, cursor: Option<(Point, DecorationBuffer)>) {
        self.cursor = cursor;
        self.cursor_generation = self.generation();
        self.dirty = true;
    }
    pub(crate) fn set_cursor_hotspot(&mut self, hotspot: Point) {
        if let Some((old, _)) = &mut self.cursor {
            *old = hotspot;
        }
        self.dirty = true;
    }
    /// Replace a window's pixels completely (or drop them).
    pub fn set_window_image(&mut self, id: u64, image: Option<DecorationBuffer>) {
        let generation = self.generation();
        if let Some(w) = self.windows.get_mut(&id) {
            w.image = image;
            w.external = None;
            w.generation = generation;
            w.partial = None;
        }
        self.dirty = true;
    }
    /// Show a client dma-buf in a window. `damage` (buffer coordinates) bounds
    /// what changed when the size is unchanged; empty means everything.
    pub fn set_window_external(&mut self, id: u64, buffer: ExternalBuffer, damage: &[DRect]) {
        let generation = self.generation();
        if let Some(w) = self.windows.get_mut(&id) {
            let same_size = w.external.is_some_and(|e| (e.width, e.height) == (buffer.width, buffer.height));
            w.image = Some(DecorationBuffer { width: buffer.width, height: buffer.height, pixels: Vec::new() });
            w.external = Some(buffer);
            w.generation = generation;
            match &mut w.partial {
                Some(partial) if same_size && !damage.is_empty() && partial.len() + damage.len() <= MAX_PARTIAL_RECTS => {
                    partial.extend_from_slice(damage)
                }
                partial => *partial = None,
            }
        }
        self.dirty = true;
    }
    /// For in-place edits; report them with `window_image_damaged`. `None`
    /// for a window showing a dma-buf: there are no pixels to edit.
    pub fn window_pixels_mut(&mut self, id: u64) -> Option<&mut DecorationBuffer> {
        let w = self.windows.get_mut(&id)?;
        if w.external.is_some() {
            return None;
        }
        w.image.as_mut()
    }
    /// For in-place edits; report them with `window_image_damaged`.
    pub fn window_image_mut(&mut self, id: u64) -> Option<&mut DecorationBuffer> {
        self.windows.get_mut(&id)?.image.as_mut()
    }
    /// Image-space rectangles of a window image that were edited in place.
    pub fn window_image_damaged(&mut self, id: u64, rects: &[DRect]) {
        let generation = self.generation();
        if let Some(w) = self.windows.get_mut(&id) {
            w.generation = generation;
            if let Some(partial) = &mut w.partial {
                partial.extend_from_slice(rects);
                if partial.len() > MAX_PARTIAL_RECTS {
                    w.partial = None;
                }
            }
        }
        self.dirty = true;
    }
    /// Replace a shell surface's pixels by move (no clone). Used by the
    /// benchmark's internal test surface; `paint_shell_surface` clones.
    pub fn replace_shell_image(&mut self, id: u64, image: DecorationBuffer) {
        let generation = self.generation();
        if let Some(s) = self.shells.get_mut(&id) {
            s.image = image;
            s.generation = generation;
            s.partial = None;
        }
        self.dirty = true;
    }
    pub fn shell_image_mut(&mut self, id: u64) -> Option<&mut DecorationBuffer> {
        self.shells.get_mut(&id).map(|s| &mut s.image)
    }
    pub fn shell_image_damaged(&mut self, id: u64, rects: &[DRect]) {
        let generation = self.generation();
        if let Some(s) = self.shells.get_mut(&id) {
            s.generation = generation;
            if let Some(partial) = &mut s.partial {
                partial.extend_from_slice(rects);
                if partial.len() > MAX_PARTIAL_RECTS {
                    s.partial = None;
                }
            }
        }
        self.dirty = true;
    }
    /// The frame as an ordered list, bottom to top. This mirrors `render()`
    /// item for item; `render::tests` holds the two to identical pixels.
    pub(crate) fn display_list<'a>(&'a self) -> Vec<(ItemSig, DrawOp<'a>)> {
        let size = self.screen_size();
        let mut list = Vec::with_capacity(2 + self.shell_stack.len() + 2 * self.stack.len());
        list.push((
            ItemSig {
                key: ItemKey::Background,
                rect: DRect::from_xywh(0, 0, size.w, size.h),
                generation: self.background_generation,
            },
            DrawOp::Background { color: self.background, image: self.background_image.as_ref() },
        ));
        let shells = |list: &mut Vec<(ItemSig, DrawOp<'a>)>, above: bool| {
            for id in &self.shell_stack {
                if let Some(s) = self.shells.get(id).filter(|s| s.mapped && s.above == above) {
                    list.push((
                        ItemSig {
                            key: ItemKey::Shell(*id),
                            rect: image_rect(s.geometry.pos, &s.image),
                            generation: s.generation,
                        },
                        DrawOp::Image { pos: s.geometry.pos, image: &s.image },
                    ));
                }
            }
        };
        shells(&mut list, false);
        for id in &self.stack {
            let frame = self.frames.values().find(|f| f.window == *id);
            if !self.visible(*id) && !frame.is_some_and(|f| f.mapped) {
                continue;
            }
            let w = &self.windows[id];
            if let Some(image) = w.image.as_ref().filter(|_| w.mapped) {
                list.push((
                    ItemSig {
                        key: ItemKey::Window(*id),
                        rect: image_rect(w.content.pos, image),
                        generation: w.generation,
                    },
                    match w.external {
                        Some(buffer) => DrawOp::External { pos: w.content.pos, buffer },
                        None => DrawOp::Image { pos: w.content.pos, image },
                    },
                ));
            }
            if let Some((frame, surface)) = frame.and_then(|f| Some((f, f.paint.as_ref()?))) {
                let pos = frame.geometry.pos;
                let mut rect = DRect::EMPTY;
                for solid in &surface.solids {
                    rect = rect.union(DRect::from_rect(solid.rect).translate(pos.x, pos.y));
                }
                for part in &surface.parts {
                    let at = Point::new(pos.x + part.offset.x, pos.y + part.offset.y);
                    rect = rect.union(image_rect(at, &part.buffer));
                }
                list.push((
                    ItemSig { key: ItemKey::Decoration(*id), rect, generation: frame.generation },
                    DrawOp::Decoration { pos, surface },
                ));
            }
        }
        shells(&mut list, true);
        if let Some((hotspot, image)) = &self.cursor {
            let pos = Point::new(self.pointer.x - hotspot.x, self.pointer.y - hotspot.y);
            list.push((
                ItemSig {
                    key: ItemKey::Cursor,
                    rect: image_rect(pos, image),
                    generation: self.cursor_generation,
                },
                DrawOp::Image { pos, image },
            ));
        }
        list
    }
    /// Output-space bound of an item's content change since `ack_damage`.
    pub(crate) fn partial_damage(&self, key: ItemKey) -> Option<Vec<DRect>> {
        let (pos, partial) = match key {
            ItemKey::Window(id) => {
                let w = self.windows.get(&id)?;
                (w.content.pos, w.partial.as_ref()?)
            }
            ItemKey::Shell(id) => {
                let s = self.shells.get(&id)?;
                (s.geometry.pos, s.partial.as_ref()?)
            }
            _ => return None,
        };
        // An image with a negative origin also shows its last column/row one
        // pixel further out (see `render::drawn_image_rect`).
        let (right, down) = (i32::from(pos.x < 0), i32::from(pos.y < 0));
        Some(
            partial
                .iter()
                .map(|r| DRect::new(r.x0, r.y0, r.x1 + right, r.y1 + down).translate(pos.x, pos.y))
                .collect(),
        )
    }
    /// Image-space bound of an item's content change since `ack_damage`, for
    /// renderers that keep their own copy of the pixels (texture uploads).
    pub(crate) fn partial_image_damage(&self, key: ItemKey) -> Option<&[DRect]> {
        match key {
            ItemKey::Window(id) => self.windows.get(&id)?.partial.as_deref(),
            ItemKey::Shell(id) => self.shells.get(&id)?.partial.as_deref(),
            _ => None,
        }
    }
    /// The renderer has folded every change so far into its own damage.
    pub(crate) fn ack_damage(&mut self) {
        for w in self.windows.values_mut() {
            w.partial = Some(Vec::new());
        }
        for s in self.shells.values_mut() {
            s.partial = Some(Vec::new());
        }
    }
    pub fn popup_active(&self) -> bool { self.popup_grab.is_some() }
    pub fn popup_can_open(&self) -> bool { self.input_enabled && self.pointer_grab.is_none() && self.popup_grab.is_none() && !self.keyboard_grabbed }
    pub fn desktop_hit(&self, point: Point) -> DesktopHit {
        let hit_shell = |above: bool| self.shell_stack.iter().rev().find_map(|id| {
            let s=self.shells.get(id)?;
            let x=i64::from(point.x)-i64::from(s.geometry.pos.x);
            let y=i64::from(point.y)-i64::from(s.geometry.pos.y);
            (s.mapped && !s.image.pixels.is_empty() && s.above==above && x>=0 && y>=0
                && x<i64::from(s.geometry.size.w) && y<i64::from(s.geometry.size.h))
                .then(||DesktopHit::Shell(*id,Point::new(x as i32,y as i32)))
        });
        if let Some(hit)=hit_shell(true){return hit;}
        if let Some((surface,local))=self.hit_at(point){return match surface {
            SurfaceRef::Client(id)=>DesktopHit::Client(id,local),
            SurfaceRef::Frame(id)=>DesktopHit::Frame(id,local),
        };}
        hit_shell(false).unwrap_or(DesktopHit::Root)
    }
    /// `point` relative to a mapped shell surface's origin (it may lie outside
    /// the surface: a press that started on it is followed wherever it goes).
    pub fn shell_local(&self, id: u64, point: Point) -> Option<Point> {
        let s = self.shells.get(&id).filter(|s| s.mapped)?;
        Some(Point::new(point.x - s.geometry.pos.x, point.y - s.geometry.pos.y))
    }
    /// Mapped shell surfaces with pixels, as (id, rectangle on the output).
    pub fn mapped_shell_rects(&self) -> Vec<(u64, Rect)> {
        self.shell_stack
            .iter()
            .filter_map(|id| {
                let s = self.shells.get(id).filter(|s| s.mapped && !s.image.pixels.is_empty())?;
                Some((*id, Rect::new(s.geometry.pos, Size::new(s.image.width, s.image.height))))
            })
            .collect()
    }
    pub fn queue_shell_motion(&mut self,id:u64,local:Point){
        if !self.shells.get(&id).is_some_and(|s|s.mapped){return;}
        if self.shell_motion.len()>=256 {self.input_fault=Some("shell motion queue overflow".into());return;}
        self.shell_motion.push_back((id,local));
    }
    pub fn queue_shell_click(&mut self,id:u64,local:Point,button:MouseButton,pressed:bool){
        if !self.shells.get(&id).is_some_and(|s|s.mapped){return;}
        if self.shell_clicks.len()>=256 {self.input_fault=Some("shell click queue overflow".into());return;}
        self.shell_clicks.push_back((id,local,button,pressed));
    }
    pub fn add(&mut self, surface: ToplevelSurface) -> u64 {
        let id = self.alloc();
        self.windows.insert(
            id,
            Window {
                surface,
                title: None,
                app_id: None,
                content: Rect::new(Point::new(0, 0), Size::new(1, 1)),
                image: None,
                generation: 0,
                partial: None,
                external: None,
                mapped: false,
                committed: false,
                chrome: ClientChrome::Edges,
                hints: SizeHints::default(),
                logical: Size::new(1, 1),
                unscaled: false,
                sizes_by_scale: Vec::new(),
                shrunk: false,
            },
        );
        self.stack.push(id);
        id
    }
    pub fn remove(&mut self, id: u64) {
        self.windows.remove(&id);
        self.frames.retain(|_, f| f.window != id);
        self.stack.retain(|v| *v != id);
        self.dirty = true;
    }
    pub fn window_for_surface(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<u64> {
        self.windows
            .iter()
            .find(|(_, w)| w.surface.wl_surface() == surface)
            .map(|(id, _)| *id)
    }
    fn restack_window(&mut self, id: u64) {
        self.stack.retain(|v| *v != id);
        self.stack.push(id);
        self.dirty = true;
    }
    pub fn visible(&self, id: u64) -> bool {
        self.windows
            .get(&id)
            .is_some_and(|w| w.mapped && w.image.is_some())
            && self
                .frames
                .values()
                .find(|f| f.window == id)
                .is_none_or(|f| f.mapped)
    }
    pub(crate) fn hit_at(&self,point:Point)->Option<(SurfaceRef<u64,u64>,Point)> {
        use smithay::wayland::compositor::{with_states,SurfaceAttributes};
        fn inside(p:Point,r:Rect)->bool {
            i64::from(p.x)>=i64::from(r.pos.x) && i64::from(p.y)>=i64::from(r.pos.y) &&
            i64::from(p.x)<i64::from(r.pos.x)+i64::from(r.size.w) && i64::from(p.y)<i64::from(r.pos.y)+i64::from(r.size.h)
        }
        for id in self.stack.iter().rev() {
            let w=&self.windows[id];
            let frame=self.frames.iter().find(|(_,f)|f.window==*id);
            if frame.is_some_and(|(_,f)|!f.mapped){continue;}
            if w.mapped && w.image.is_some() && inside(point,w.content){
                let local=Point::new(point.x-w.content.pos.x,point.y-w.content.pos.y);
                // The input region is in the surface's logical units.
                let density=if w.unscaled{1.0}else{self.scale.factor()};
                let logical=((f64::from(local.x)/density).floor() as i32,(f64::from(local.y)/density).floor() as i32);
                let accepts=with_states(w.surface.wl_surface(),|s|s.cached_state.get::<SurfaceAttributes>().current().input_region.as_ref().is_none_or(|r|r.contains(logical)));
                if accepts{return Some((SurfaceRef::Client(*id),local));}
                continue;
            }
            if let Some((id,f))=frame {
                let local=Point::new(point.x-f.geometry.pos.x,point.y-f.geometry.pos.y);
                if inside(point,f.geometry) && !f.layout.input_exclusion.is_some_and(|r|inside(local,r)) {
                    return Some((SurfaceRef::Frame(*id),local));
                }
            }
        }
        None
    }
    pub fn render(&self) -> Result<tiny_skia::Pixmap, String> {
        let size = self.screen_size();
        let mut pix =
            tiny_skia::Pixmap::new(size.w, size.h).ok_or("CPU output allocation failed")?;
        pix.fill(tiny_skia::Color::from_rgba8(
            self.background[0],
            self.background[1],
            self.background[2],
            255,
        ));
        let draw = |pix: &mut tiny_skia::Pixmap, pos: Point, image: &DecorationBuffer| {
            if let Some(src) =
                tiny_skia::PixmapRef::from_bytes(&image.pixels, image.width, image.height)
            {
                pix.draw_pixmap(
                    pos.x,
                    pos.y,
                    src,
                    &tiny_skia::PixmapPaint::default(),
                    tiny_skia::Transform::identity(),
                    None,
                );
            }
        };
        if let Some(ref image) = self.background_image {
            draw(&mut pix, Point::new(0, 0), image);
        }
        let shells = |pix: &mut tiny_skia::Pixmap, above: bool| {
            for id in &self.shell_stack {
                if let Some(s) = self.shells.get(id).filter(|s| s.mapped && s.above == above) {
                    draw(pix, s.geometry.pos, &s.image);
                }
            }
        };
        shells(&mut pix, false);
        for id in &self.stack {
            let frame=self.frames.values().find(|f|f.window==*id);
            if !self.visible(*id) && !frame.is_some_and(|f|f.mapped) {
                continue;
            }
            let w = &self.windows[id];
            if let Some(image) = w.image.as_ref().filter(|_|w.mapped) {
                draw(&mut pix, w.content.pos, image);
            }
            if let Some(frame) = self.frames.values().find(|f| f.window == *id) {
                if let Some(surface) = &frame.paint {
                    for solid in &surface.solids {
                        let mut paint = tiny_skia::Paint::default();
                        paint.set_color_rgba8(solid.rgb[0], solid.rgb[1], solid.rgb[2], 255);
                        if let Some(rect) = tiny_skia::Rect::from_xywh(
                            (frame.geometry.pos.x + solid.rect.pos.x) as f32,
                            (frame.geometry.pos.y + solid.rect.pos.y) as f32,
                            solid.rect.size.w as f32,
                            solid.rect.size.h as f32,
                        ) {
                            pix.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
                        }
                    }
                    for part in &surface.parts {
                        draw(
                            &mut pix,
                            Point::new(
                                frame.geometry.pos.x + part.offset.x,
                                frame.geometry.pos.y + part.offset.y,
                            ),
                            &part.buffer,
                        );
                    }
                }
            }
        }
        shells(&mut pix, true);
        if let Some((hotspot,image))=&self.cursor {
            draw(&mut pix,Point::new(self.pointer.x-hotspot.x,self.pointer.y-hotspot.y),image);
        }
        Ok(pix)
    }
}
impl Backend for CpuBackend {
    type WindowId = u64;
    type FrameId = u64;
    type ShellId = u64;
    fn scan_existing_windows(&mut self) -> Vec<u64> {
        self.windows.keys().copied().collect()
    }
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.clone()
    }
    fn monitors_ref(&self) -> &[MonitorInfo] {
        &self.monitors
    }
    fn screen_size(&self) -> Size {
        self.monitors[0].geometry.size
    }
    fn poll_event(&mut self) -> Option<BackendEvent<u64, u64>> {
        None
    } // frontend dispatches synchronously
    fn window_title(&self, id: u64) -> Option<String> {
        self.windows.get(&id)?.title.clone()
    }
    fn window_class(&self, id: u64) -> Option<WmClass> {
        let s = self.windows.get(&id)?.app_id.clone()?;
        Some(WmClass {
            instance: s.clone(),
            class: s,
        })
    }
    fn window_pid(&self, _: u64) -> Option<u32> {
        None
    } // Darwin peer credentials are not qualified
    fn size_hints(&self, id: u64) -> SizeHints {
        self.windows[&id].hints
    }
    fn window_parent(&self, id: u64) -> Option<u64> {
        self.window_for_surface(&self.windows.get(&id)?.surface.parent()?)
    }
    fn supports_protocol(&self, _: u64, p: WmProtocol) -> bool {
        p == WmProtocol::DeleteWindow
    }
    fn window_geometry(&self, id: u64) -> Rect {
        self.windows[&id].content
    }
    fn client_chrome(&self, id: u64) -> ClientChrome {
        self.windows[&id].chrome
    }
    fn capture_window_image(&self, id: u64, _: Size) -> Option<DecorationBuffer> {
        let w = self.windows.get(&id)?;
        // A dma-buf window has no CPU-side pixels to hand out.
        if w.external.is_some() {
            return None;
        }
        w.image.clone()
    }
    fn create_decoration(&mut self, id: u64, l: &DecorationLayout) -> u64 {
        let frame = self.alloc();
        let generation = self.generation();
        if let Some(w) = self.windows.get_mut(&id) {
            w.content.pos = l.client_offset;
            w.mapped = true;
        }
        self.frames.insert(
            frame,
            Frame {
                window: id,
                geometry: Rect::new(Point::new(0, 0), l.frame_size),
                mapped: false,
                paint: None,
                generation,
                layout: l.clone(),
            },
        );
        self.dirty = true;
        frame
    }
    fn destroy_decoration(&mut self, id: u64) {
        self.frames.remove(&id);
        self.dirty = true;
    }
    /// One scale for the one output: the theme engine lays out and paints
    /// every frame at it.
    fn decoration_scale(&self, _: Rect) -> f32 {
        self.scale.factor() as f32
    }
    fn paint_decoration(&mut self, id: u64, p: &DecorationSurface) {
        let generation = self.generation();
        if let Some(f) = self.frames.get_mut(&id) {
            f.paint = Some(p.clone());
            f.generation = generation;
        }
        self.dirty = true;
    }
    fn set_frame_cursor(&mut self, _: u64, _: Option<ResizeEdge>) {} // no advertised pointer capability
    fn set_frame_geometry(&mut self, id: u64, g: Rect) {
        if let Some(f) = self.frames.get_mut(&id) {
            if let Some(w) = self.windows.get_mut(&f.window) {
                w.content.pos.x += g.pos.x - f.geometry.pos.x;
                w.content.pos.y += g.pos.y - f.geometry.pos.y;
            }
            f.geometry = g;
        }
        self.dirty = true;
    }
    fn set_decoration_layout(&mut self, id: u64, l: &DecorationLayout) {
        let generation = self.generation();
        if let Some(f)=self.frames.get_mut(&id){f.layout=l.clone();}
        if l.titlebar_height == 0 && l.client_offset == Point::new(0, 0) {
            if let Some(f) = self.frames.get_mut(&id) {
                f.paint = None;
                f.generation = generation;
            }
        }
        self.dirty = true;
    }
    fn resize_client(&mut self, id: u64, size: Size) {
        // The policy speaks panel pixels; the client is asked in its own
        // logical units.
        let logical = self.window_logical(id, size);
        let (interactive, rescaling) = (self.interactive, self.rescaling);
        if let Some(w) = self.windows.get_mut(&id) {
            // A size chosen by hand replaces the one remembered from before
            // a scale change shrank the window.
            if interactive && logical != w.logical {
                w.sizes_by_scale.clear();
                w.shrunk = false;
            }
            w.content.size = size;
            // A client that was left to choose its own size, and whose
            // window is being confirmed at the size it chose, is not then
            // told that size: it stays free to choose (a terminal keeps
            // its columns and rows when the display scale changes).
            let unasked = w.surface.with_pending_state(|s| s.size.is_none());
            if rescaling || (unasked && w.committed && logical == w.logical) {
                self.dirty = true;
                return;
            }
            w.surface
                .with_pending_state(|s| s.size = Some((logical.w as i32, logical.h as i32).into()));
            w.surface.send_pending_configure();
        }
        self.dirty = true;
    }
    fn accept_client_size(&mut self, id: u64, size: Size) {
        if let Some(w) = self.windows.get_mut(&id) {
            w.content.size = size;
        }
        self.dirty = true;
    }
    fn configure_unmanaged(&mut self, id: u64, g: Rect) {
        if let Some(w) = self.windows.get_mut(&id) {
            w.content = g;
        }
        self.resize_client(id, g.size);
    }
    fn map_frame(&mut self, id: u64) {
        if let Some(f) = self.frames.get_mut(&id) {
            f.mapped = true;
        }
        self.dirty = true;
    }
    fn unmap_frame(&mut self, id: u64) {
        if let Some(f) = self.frames.get_mut(&id) {
            f.mapped = false;
        }
        self.dirty = true;
    }
    fn set_client_mapped(&mut self, id: u64, mapped: bool) {
        if let Some(w) = self.windows.get_mut(&id) {
            w.mapped = mapped;
        }
        self.dirty = true;
    }
    fn map_frameless(&mut self, id: u64) {
        self.set_client_mapped(id, true);
    }
    fn unmap_frameless(&mut self, id: u64) {
        self.set_client_mapped(id, false);
    }
    fn position_client(&mut self, id: u64, p: Point) {
        let f = self
            .frames
            .values()
            .find(|f| f.window == id)
            .map(|f| f.geometry.pos)
            .unwrap_or(Point::new(0, 0));
        if let Some(w) = self.windows.get_mut(&id) {
            w.content.pos = Point::new(f.x + p.x, f.y + p.y);
        }
        self.dirty = true;
    }
    fn raise(&mut self, id: u64) {
        if let Some(f) = self.frames.get(&id) {
            self.restack_window(f.window);
        }
    }
    fn raise_frameless(&mut self, id: u64) {
        self.restack_window(id);
    }
    fn restack(&mut self, ids: &[u64]) {
        for id in ids {
            self.raise(*id);
        }
    }
    fn set_input_focus(&mut self, id: u64) {
        for (key, w) in &self.windows {
            w.surface.with_pending_state(|s| {
                if *key == id {
                    s.states.set(xdg_toplevel::State::Activated);
                } else {
                    s.states.unset(xdg_toplevel::State::Activated);
                }
            });
            w.surface.send_pending_configure();
        }
        self.dirty = true;
    }
    fn send_close(&mut self, id: u64) {
        if let Some(w) = self.windows.get(&id) {
            w.surface.send_close();
        }
    }
    fn grab_pointer_for_drag(&mut self) -> DragHandle {
        assert!(self.input_enabled,"cannot grab without an input source");
        assert!(self.pointer_grab.is_none(),"nested pointer grab");
        assert!(self.popup_grab.is_none(),"drag while popup owns pointer");
        let handle=DragHandle(self.alloc());self.pointer_grab=Some(handle);handle
    }
    fn ungrab_pointer(&mut self, handle: DragHandle) {
        assert_eq!(self.pointer_grab,Some(handle));self.pointer_grab=None;
    }
    fn grab_keyboard(&mut self){assert!(!self.keyboard_grabbed,"nested keyboard capture");self.keyboard_grabbed=true;}
    fn ungrab_keyboard(&mut self){assert!(self.keyboard_grabbed,"keyboard capture not owned");self.keyboard_grabbed=false;}
    fn take_shell_motion(&mut self)->Option<(u64,Point)>{self.shell_motion.pop_front()}
    fn take_shell_click(&mut self)->Option<(u64,Point,MouseButton,bool)>{self.shell_clicks.pop_front()}
    fn grab_key(&mut self, combo: KeyCombo) {
        self.grabbed_keys.insert(combo);
    }
    fn set_keyboard_config(&mut self,config:KeyboardConfig){
        // This frontend creates this exact keymap and repeat policy in
        // enable_sfhid_seat_caps. No runtime reconfiguration is advertised.
        let valid=self.input_enabled && config.rules.as_deref()==Some("evdev")
            && config.model.as_deref()==Some("pc105") && config.layout.as_deref()==Some("us")
            && config.variant.as_deref()==Some("") && config.options.is_none()
            && config.repeat_rate==Some(25) && config.repeat_delay==Some(500)
            && config.numlock_by_default.is_none();
        if valid{self.keyboard_config_verified=true;}
        else{self.input_fault=Some("unsupported keyboard configuration".into());}
    }
    fn ungrab_key(&mut self, combo: KeyCombo) {
        self.grabbed_keys.remove(&combo);
    }
    fn pointer_position(&self)->Option<Point>{self.input_enabled.then_some(self.pointer)}
    // All pointer events already arrive at the compositor; passive X11 grabs
    // are unnecessary. replay_pointer decides whether the triggering click
    // is delivered to the Wayland client after wm-core policy runs.
    fn grab_button_passive(&mut self, _: u64, _: MouseButton) {}
    fn ungrab_button_passive(&mut self, _: u64, _: MouseButton) {}
    fn replay_pointer(&mut self) {self.replay=true;}
    fn create_shell_surface(&mut self, g: Rect, bg: (u8, u8, u8), above: bool) -> Option<u64> {
        let len = crate::pixels::image_len(g.size.w, g.size.h).ok()?;
        let mut pixels = vec![0; len];
        for p in pixels.as_chunks_mut::<4>().0.iter_mut() {
            p.copy_from_slice(&[bg.0, bg.1, bg.2, 255]);
        }
        let id = self.alloc();
        let generation = self.generation();
        self.shells.insert(
            id,
            Shell {
                geometry: g,
                mapped: false,
                above,
                generation,
                partial: None,
                image: DecorationBuffer {
                    width: g.size.w,
                    height: g.size.h,
                    pixels,
                },
            },
        );
        self.shell_stack.push(id);
        Some(id)
    }
    fn map_shell_surface(&mut self, id: u64) {
        if let Some(s) = self.shells.get_mut(&id) {
            s.mapped = true;
        }
        self.dirty = true;
    }
    fn unmap_shell_surface(&mut self, id: u64) {
        if let Some(s) = self.shells.get_mut(&id) {
            s.mapped = false;
        }
        self.dirty = true;
    }
    fn destroy_shell_surface(&mut self, id: u64) {
        self.shells.remove(&id);
        self.shell_stack.retain(|v| *v != id);
        self.shell_motion.retain(|(v,_)|*v!=id);
        self.shell_clicks.retain(|(v,_,_,_)|*v!=id);
        self.dirty = true;
    }
    fn raise_shell_surface(&mut self, id: u64) {
        self.shell_stack.retain(|v| *v != id);
        self.shell_stack.push(id);
        self.dirty = true;
    }
    fn configure_shell_surface(&mut self, id: u64, g: Rect) {
        if let Some(s) = self.shells.get_mut(&id) {
            s.geometry = g;
        }
        self.dirty = true;
    }
    fn paint_shell_surface(&mut self, id: u64, p: &DecorationBuffer) {
        let generation = self.generation();
        if let Some(s) = self.shells.get_mut(&id) {
            s.image = p.clone();
            s.generation = generation;
            s.partial = None;
        }
        self.dirty = true;
    }
    fn release_shell_buffer(&mut self, id: u64) {
        self.unmap_shell_surface(id);
        let generation = self.generation();
        if let Some(s) = self.shells.get_mut(&id) {
            s.image.pixels.clear();
            s.generation = generation;
            s.partial = None;
        }
    }
    fn paint_root_color(&mut self, rgb: (u8, u8, u8)) {
        self.background = [rgb.0, rgb.1, rgb.2];
        self.background_image = None;
        self.background_generation = self.generation();
        self.dirty = true;
    }
    fn paint_root_image(&mut self, p: &DecorationBuffer) {
        self.background_image = Some(p.clone());
        self.background_generation = self.generation();
        self.dirty = true;
    }
}

impl wm_theme_api::PopupHost for CpuBackend {
    type PopupId=u64;
    fn create_popup(&mut self,geometry:Rect,background:(u8,u8,u8))->Option<u64>{
        if self.pointer_grab.is_some(){return None;}
        let id=self.create_shell_surface(geometry,background,true)?;
        self.map_shell_surface(id);self.raise_shell_surface(id);Some(id)
    }
    fn destroy_popup(&mut self,id:u64){self.destroy_shell_surface(id);}
    fn paint_popup(&mut self,id:u64,buffer:&DecorationBuffer){
        let valid=self.shells.get(&id).is_some_and(|s|s.geometry.size==Size::new(buffer.width,buffer.height))
            && crate::pixels::image_len(buffer.width,buffer.height).is_ok_and(|n|n==buffer.pixels.len());
        if !valid{self.input_fault=Some("invalid popup paint".into());return;}
        self.paint_shell_surface(id,buffer);
    }
    fn grab_pointer(&mut self)->wm_theme_api::PopupGrab{
        assert!(self.input_enabled && self.pointer_grab.is_none() && self.popup_grab.is_none(),"popup ownership conflict");
        let token=wm_theme_api::PopupGrab(self.alloc());self.popup_grab=Some(token);token
    }
    fn ungrab_pointer(&mut self,token:wm_theme_api::PopupGrab){assert_eq!(self.popup_grab,Some(token));self.popup_grab=None;}
    fn grab_keyboard(&mut self){Backend::grab_keyboard(self);}
    fn ungrab_keyboard(&mut self){Backend::ungrab_keyboard(self);}
}
