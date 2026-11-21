//! GLES renderer: the scene as textured quads on the GPU.
//!
//! Every dirty frame the whole display list is drawn into the next target
//! buffer (a full-screen repaint is cheap on the GPU, and three rotating
//! scanout buffers make partial repaints not worth their bookkeeping). What is
//! damage-aware is the expensive part, getting pixels to the GPU: each image
//! is a texture that is re-uploaded only when its generation changes, and
//! only the rectangles the scene reports as changed (`glTexSubImage2D`).
//!
//! The picture is meant to equal `CpuBackend::render()`: images are sampled
//! with exact integer texel fetches over the rectangle the CPU renderer
//! covers (including tiny-skia's placement of images with a negative origin),
//! blended premultiplied source-over. Opaque pixels are therefore identical;
//! translucent ones can differ by GPU rounding of the blend.
use crate::damage::{DRect, ItemKey};
use crate::gpu::*;
use crate::kms::{Committed, Flip, FlipQueue, FlipWait, Kms, Scanout, ScanoutBuffer, Steps};
use crate::presenter::Presenter;
use crate::render::{drawn_image_rect, DmabufDesc, FrameStats, Presented, Renderer, RendererKind};
use crate::scene::{CpuBackend, DrawOp, ExternalBuffer};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::time::{Duration, Instant};
use wm_core::{Backend, Point};
use wm_theme_api::DecorationBuffer;

/// Where the GL renderer draws and how a finished frame is shown.
pub trait GlTarget {
    fn size(&self) -> (u32, u32);
    fn name(&self) -> &'static str;
    /// A frame is on its way to the screen; no new frame may be started.
    fn busy(&self) -> bool;
    /// Framebuffer object for the next frame. Row 0 is the top scanline.
    fn begin(&mut self) -> Result<GLuint, String>;
    /// Show what was drawn since `begin`.
    fn present(&mut self, libs: &Libraries) -> Result<(), String>;
    /// Completion of the frame in flight, waiting up to `timeout`.
    fn poll(&mut self, libs: &Libraries, timeout: Duration) -> Result<Option<Presented>, String>;
    /// Framebuffer object holding the most recently drawn frame.
    fn last(&self) -> Option<GLuint>;
    /// Power the display down or up. `Ok(false)`: this target has none.
    fn set_power(&mut self, _: &Libraries, _: bool) -> Result<bool, String> {
        Ok(false)
    }
}

/// An ordinary texture-backed framebuffer; nothing is displayed. Host tests,
/// the off-screen benchmark and the pixel check use it.
pub struct OffscreenTarget {
    fbo: GLuint,
    size: (u32, u32),
    drawn: bool,
}
impl OffscreenTarget {
    pub fn new(libs: &Libraries, width: u32, height: u32) -> Result<Self, String> {
        let gl = &libs.gl;
        let (mut texture, mut fbo) = (0, 0);
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            (gl.glGenTextures)(1, &mut texture);
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            (gl.glTexImage2D)(GL_TEXTURE_2D, 0, GL_RGBA8 as GLint, width as GLsizei, height as GLsizei, 0, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null());
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (gl.glGenFramebuffers)(1, &mut fbo);
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fbo);
            (gl.glFramebufferTexture2D)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
            let status = (gl.glCheckFramebufferStatus)(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                return Err(format!("off-screen framebuffer status {status:#x}"));
            }
        }
        Ok(Self { fbo, size: (width, height), drawn: false })
    }
}
impl GlTarget for OffscreenTarget {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn name(&self) -> &'static str {
        "gl-offscreen"
    }
    fn busy(&self) -> bool {
        false
    }
    fn begin(&mut self) -> Result<GLuint, String> {
        Ok(self.fbo)
    }
    fn present(&mut self, _: &Libraries) -> Result<(), String> {
        self.drawn = true;
        Ok(())
    }
    fn poll(&mut self, _: &Libraries, _: Duration) -> Result<Option<Presented>, String> {
        Ok(None)
    }
    fn last(&self) -> Option<GLuint> {
        self.drawn.then_some(self.fbo)
    }
}

/// Three scanout buffers on the display, shown by atomic page flip. The
/// flip-complete event releases the previous front buffer and is the frame
/// clock; until it arrives the target is busy.
pub struct KmsTarget {
    kms: Kms,
    buffers: Vec<ScanoutBuffer>,
    /// What is on screen and which commit the display owes an event for.
    queue: FlipQueue,
    drawing: Option<usize>,
    last: Option<usize>,
    steps: Steps,
}
/// The real display behind the flip queue, for the length of one call.
struct Live<'a> {
    kms: &'a mut Kms,
    buffers: &'a [ScanoutBuffer],
    libs: &'a Libraries,
    steps: &'a mut Steps,
}
impl Scanout for Live<'_> {
    fn flip(&mut self, index: usize) -> Result<Committed, String> {
        self.kms.commit(self.libs, &self.buffers[index], self.steps)
    }
    fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<Flip>, String> {
        self.kms.wait_flip(timeout)
    }
}
impl KmsTarget {
    pub fn open(libs: &Libraries, context: &Context, wait: FlipWait) -> Result<Self, String> {
        if !context.has_display_extension("EGL_EXT_image_dma_buf_import") || !context.has_gl_extension("GL_OES_EGL_image") {
            return Err("EGL_EXT_image_dma_buf_import / GL_OES_EGL_image not advertised".into());
        }
        let mut steps = Steps::new("chonkstep-gl:");
        let mut kms = Kms::open(libs, wait, &mut steps)?;
        let mut buffers = Vec::new();
        for index in 0..3 {
            buffers.push(kms.create_buffer(libs, context, index, &mut steps)?);
        }
        Ok(Self { kms, buffers, queue: FlipQueue::new(), drawing: None, last: None, steps })
    }
    pub fn kms(&self) -> &Kms {
        &self.kms
    }
}
impl GlTarget for KmsTarget {
    fn size(&self) -> (u32, u32) {
        (self.kms.width(), self.kms.height())
    }
    fn name(&self) -> &'static str {
        "kms"
    }
    fn busy(&self) -> bool {
        self.queue.busy()
    }
    fn begin(&mut self) -> Result<GLuint, String> {
        if self.queue.busy() {
            return Err("KMS target: a commit is still in flight".into());
        }
        let index = (0..self.buffers.len())
            .find(|i| Some(*i) != self.queue.front())
            .ok_or("KMS target: no free buffer")?;
        self.drawing = Some(index);
        Ok(self.buffers[index].fbo)
    }
    fn present(&mut self, libs: &Libraries) -> Result<(), String> {
        let index = self.drawing.take().ok_or("KMS target: present without begin")?;
        self.last = Some(index);
        if self.queue.front().is_none() {
            self.steps.step("first commit (sets the mode)");
        }
        let mut display = Live { kms: &mut self.kms, buffers: &self.buffers, libs, steps: &mut self.steps };
        self.queue.submit(&mut display, index)
    }
    fn poll(&mut self, libs: &Libraries, timeout: Duration) -> Result<Option<Presented>, String> {
        let first = self.queue.front().is_none();
        let (held, lost) = (self.queue.held_frames, self.queue.wake_timeouts);
        let mut display = Live { kms: &mut self.kms, buffers: &self.buffers, libs, steps: &mut self.steps };
        let shown = self.queue.poll(&mut display, timeout)?;
        if self.queue.held_frames != held {
            self.steps.step(&format!("a flip the display refused as busy was taken on a later try ({} so far)", self.queue.held_frames));
        }
        if self.queue.wake_timeouts != lost {
            self.steps.step("display power-up event did not arrive in time; flips resume without it");
        }
        let Some(shown) = shown else {
            return Ok(None);
        };
        if first {
            self.steps.step(&format!("first flip-complete event, sequence {}", shown.flip.sequence));
        }
        Ok(Some(Presented { sequence: shown.flip.sequence, at: shown.flip.at, latency: shown.latency }))
    }
    fn last(&self) -> Option<GLuint> {
        self.last.map(|i| self.buffers[i].fbo)
    }
    fn set_power(&mut self, libs: &Libraries, on: bool) -> Result<bool, String> {
        if self.queue.busy() {
            return Err("KMS target: display power changed while a commit is in flight".into());
        }
        if self.queue.front().is_none() {
            // Nothing has been shown yet: there is no display state to change.
            return Ok(false);
        }
        self.steps.step(if on { "display on (ACTIVE=1)" } else { "display off (ACTIVE=0)" });
        self.kms.set_active(libs, on)?;
        if on {
            // The power-up commit's own flip-complete event says when the
            // panel takes flips again; until then the target is busy.
            self.queue.powered_up();
        }
        Ok(true)
    }
}

const VERTEX_SHADER: &str = "#version 300 es
uniform vec4 u_rect;    // output pixels: x0, y0, x1, y1
uniform vec2 u_screen;
uniform vec2 u_origin;  // output position of the image's texel 0,0
out vec2 v_local;
void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    vec2 p = mix(u_rect.xy, u_rect.zw, corner);
    v_local = p - u_origin;
    // No y flip: framebuffer row 0 is the top scanline of the output.
    gl_Position = vec4(p / u_screen * 2.0 - 1.0, 0.0, 1.0);
}
";
const FRAGMENT_SHADER: &str = "#version 300 es
precision highp float;
precision highp int;
uniform sampler2D u_texture;
uniform vec4 u_color;
uniform int u_textured;
uniform ivec2 u_last;   // width - 1, height - 1
in vec2 v_local;
out vec4 color;
void main() {
    if (u_textured != 0) {
        // Exact texel, edge-clamped: the padding tiny-skia's pattern uses.
        color = texelFetch(u_texture, clamp(ivec2(floor(v_local)), ivec2(0), u_last), 0);
    } else {
        color = u_color;
    }
}
";

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TextureKey {
    item: ItemKey,
    /// Decoration part index; 0 for everything else.
    part: u32,
}
struct Texture {
    id: GLuint,
    width: u32,
    height: u32,
    generation: u64,
    seen: u64,
}

pub struct GlRenderer {
    libs: Libraries,
    _context: Context,
    target: Box<dyn GlTarget>,
    program: GLuint,
    vertex_array: GLuint,
    u_rect: GLint,
    u_screen: GLint,
    u_origin: GLint,
    u_color: GLint,
    u_textured: GLint,
    u_last: GLint,
    textures: HashMap<TextureKey, Texture>,
    frame: u64,
    /// Bytes uploaded to textures while preparing the current frame.
    uploaded: u64,
    /// Client dma-bufs: id -> (EGLImage, texture). Never uploaded, never copied.
    external: HashMap<u64, (EGLImage, GLuint)>,
    next_external: u64,
    /// Client dma-bufs can be imported (the EGL extensions are there).
    can_import: bool,
}

unsafe fn compile(gl: &Gl, kind: GLenum, source: &str) -> Result<GLuint, String> {
    // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
    unsafe {
        let shader = (gl.glCreateShader)(kind);
        let (pointer, length) = (source.as_ptr() as *const c_char, source.len() as GLint);
        (gl.glShaderSource)(shader, 1, &pointer, &length);
        (gl.glCompileShader)(shader);
        let mut ok = 0;
        (gl.glGetShaderiv)(shader, GL_COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = vec![0u8; 1024];
            let mut length = 0;
            (gl.glGetShaderInfoLog)(shader, log.len() as GLsizei, &mut length, log.as_mut_ptr() as *mut c_char);
            return Err(format!("shader compile: {}", String::from_utf8_lossy(&log[..length.max(0) as usize])));
        }
        Ok(shader)
    }
}

impl GlRenderer {
    /// GL on the display: the normal GPU path.
    pub fn on_kms(wait: FlipWait) -> Result<Self, String> {
        let libs = Libraries::load()?;
        crate::kms::nodes_present(&libs.drm)?;
        let mut steps = Steps::new("chonkstep-gl:");
        let context = Context::create(&libs, &mut |s| steps.step(s))?;
        drop(steps);
        let target = KmsTarget::open(&libs, &context, wait)?;
        // Where things are mapped: lets a kernel line that names an address
        // (a code-signing page report, a fault) be attributed to this binary
        // or to one of the libraries it opened at run time.
        eprintln!(
            "WMGL addresses main={:#x} libdrm={:#x} libEGL={:#x} libGLESv2={:#x}",
            Self::on_kms as *const () as usize,
            libs.drm.drmIoctl as usize,
            libs.egl.eglInitialize as usize,
            libs.gl.glClear as usize,
        );
        Self::with_target(libs, context, Box::new(target))
    }
    /// GL into an ordinary framebuffer object of the given size.
    pub fn offscreen(width: u32, height: u32) -> Result<Self, String> {
        let libs = Libraries::load()?;
        let context = Context::create(&libs, &mut |_| {})?;
        let target = OffscreenTarget::new(&libs, width, height)?;
        Self::with_target(libs, context, Box::new(target))
    }
    fn with_target(libs: Libraries, context: Context, target: Box<dyn GlTarget>) -> Result<Self, String> {
        let gl = &libs.gl;
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            let vertex = compile(gl, GL_VERTEX_SHADER, VERTEX_SHADER)?;
            let fragment = compile(gl, GL_FRAGMENT_SHADER, FRAGMENT_SHADER)?;
            let program = (gl.glCreateProgram)();
            (gl.glAttachShader)(program, vertex);
            (gl.glAttachShader)(program, fragment);
            (gl.glLinkProgram)(program);
            let mut ok = 0;
            (gl.glGetProgramiv)(program, GL_LINK_STATUS, &mut ok);
            if ok == 0 {
                let mut log = vec![0u8; 1024];
                let mut length = 0;
                (gl.glGetProgramInfoLog)(program, log.len() as GLsizei, &mut length, log.as_mut_ptr() as *mut c_char);
                return Err(format!("program link: {}", String::from_utf8_lossy(&log[..length.max(0) as usize])));
            }
            let uniform = |name: &std::ffi::CStr| (gl.glGetUniformLocation)(program, name.as_ptr());
            let mut vertex_array = 0;
            (gl.glGenVertexArrays)(1, &mut vertex_array);
            (gl.glUseProgram)(program);
            (gl.glUniform1i)(uniform(c"u_texture"), 0);
            (gl.glPixelStorei)(GL_UNPACK_ALIGNMENT, 4);
            let error = (gl.glGetError)();
            if error != GL_NO_ERROR {
                return Err(format!("GL renderer set-up: GL error {error:#x}"));
            }
            Ok(Self {
                u_rect: uniform(c"u_rect"),
                u_screen: uniform(c"u_screen"),
                u_origin: uniform(c"u_origin"),
                u_color: uniform(c"u_color"),
                u_textured: uniform(c"u_textured"),
                u_last: uniform(c"u_last"),
                program,
                vertex_array,
                libs,
                can_import: context.has_display_extension("EGL_EXT_image_dma_buf_import")
                    && context.has_gl_extension("GL_OES_EGL_image"),
                _context: context,
                target,
                textures: HashMap::new(),
                frame: 0,
                uploaded: 0,
                external: HashMap::new(),
                next_external: 1,
            })
        }
    }
    pub fn size(&self) -> (u32, u32) {
        self.target.size()
    }

    /// Bring the texture for `image` up to date and bind it. `partial` holds
    /// image-space rectangles when only those changed since the last upload.
    fn texture(&mut self, key: TextureKey, image: &DecorationBuffer, generation: u64, partial: Option<&[DRect]>) -> bool {
        let (width, height) = (image.width, image.height);
        if width == 0 || height == 0 || image.pixels.len() != width as usize * height as usize * 4 {
            return false; // tiny-skia refuses such a pixmap too: nothing is drawn
        }
        let gl = &self.libs.gl;
        let frame = self.frame;
        let existing = self.textures.get_mut(&key);
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            match existing {
                Some(t) if t.width == width && t.height == height => {
                    (gl.glBindTexture)(GL_TEXTURE_2D, t.id);
                    t.seen = frame;
                    if t.generation != generation {
                        let whole = [DRect::from_xywh(0, 0, width, height)];
                        let rects = partial.unwrap_or(&whole);
                        (gl.glPixelStorei)(GL_UNPACK_ROW_LENGTH, width as GLint);
                        for rect in rects {
                            let r = rect.intersect(whole[0]);
                            if r.is_empty() {
                                continue;
                            }
                            (gl.glPixelStorei)(GL_UNPACK_SKIP_PIXELS, r.x0);
                            (gl.glPixelStorei)(GL_UNPACK_SKIP_ROWS, r.y0);
                            (gl.glTexSubImage2D)(GL_TEXTURE_2D, 0, r.x0, r.y0, r.width() as GLsizei, r.height() as GLsizei, GL_RGBA, GL_UNSIGNED_BYTE, image.pixels.as_ptr() as *const c_void);
                            self.uploaded += r.area() * 4;
                        }
                        (gl.glPixelStorei)(GL_UNPACK_ROW_LENGTH, 0);
                        (gl.glPixelStorei)(GL_UNPACK_SKIP_PIXELS, 0);
                        (gl.glPixelStorei)(GL_UNPACK_SKIP_ROWS, 0);
                        t.generation = generation;
                    }
                }
                _ => {
                    if let Some(old) = self.textures.remove(&key) {
                        (gl.glDeleteTextures)(1, &old.id);
                    }
                    let mut id = 0;
                    (gl.glGenTextures)(1, &mut id);
                    (gl.glBindTexture)(GL_TEXTURE_2D, id);
                    (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
                    (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
                    (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
                    (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
                    (gl.glTexImage2D)(GL_TEXTURE_2D, 0, GL_RGBA8 as GLint, width as GLsizei, height as GLsizei, 0, GL_RGBA, GL_UNSIGNED_BYTE, image.pixels.as_ptr() as *const c_void);
                    self.uploaded += u64::from(width) * u64::from(height) * 4;
                    self.textures.insert(key, Texture { id, width, height, generation, seen: frame });
                }
            }
        }
        true
    }

    /// One image as a quad over exactly the pixels the CPU renderer covers.
    fn draw_image(&mut self, key: TextureKey, pos: Point, image: &DecorationBuffer, generation: u64, partial: Option<&[DRect]>, screen: DRect) {
        let covered = drawn_image_rect(pos, image.width, image.height).intersect(screen);
        if !self.texture(key, image, generation, partial) || covered.is_empty() {
            return;
        }
        let gl = &self.libs.gl;
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            (gl.glUniform1i)(self.u_textured, 1);
            (gl.glUniform2f)(self.u_origin, pos.x as f32, pos.y as f32);
            (gl.glUniform2i)(self.u_last, image.width as GLint - 1, image.height as GLint - 1);
            (gl.glUniform4f)(self.u_rect, covered.x0 as f32, covered.y0 as f32, covered.x1 as f32, covered.y1 as f32);
            (gl.glDrawArrays)(GL_TRIANGLE_STRIP, 0, 4);
        }
    }
    /// A client dma-buf: the same quad as an image, sampling the client's memory.
    fn draw_external(&self, pos: Point, buffer: ExternalBuffer, screen: DRect) {
        let covered = drawn_image_rect(pos, buffer.width, buffer.height).intersect(screen);
        let Some((_, texture)) = self.external.get(&buffer.id) else {
            return;
        };
        if covered.is_empty() {
            return;
        }
        let gl = &self.libs.gl;
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            (gl.glBindTexture)(GL_TEXTURE_2D, *texture);
            (gl.glUniform1i)(self.u_textured, 1);
            (gl.glUniform2f)(self.u_origin, pos.x as f32, pos.y as f32);
            (gl.glUniform2i)(self.u_last, buffer.width as GLint - 1, buffer.height as GLint - 1);
            (gl.glUniform4f)(self.u_rect, covered.x0 as f32, covered.y0 as f32, covered.x1 as f32, covered.y1 as f32);
            (gl.glDrawArrays)(GL_TRIANGLE_STRIP, 0, 4);
        }
    }
    fn draw_solid(&self, rect: DRect, rgb: [u8; 3]) {
        if rect.is_empty() {
            return;
        }
        let gl = &self.libs.gl;
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            (gl.glUniform1i)(self.u_textured, 0);
            (gl.glUniform4f)(self.u_color, f32::from(rgb[0]) / 255.0, f32::from(rgb[1]) / 255.0, f32::from(rgb[2]) / 255.0, 1.0);
            (gl.glUniform4f)(self.u_rect, rect.x0 as f32, rect.y0 as f32, rect.x1 as f32, rect.y1 as f32);
            (gl.glDrawArrays)(GL_TRIANGLE_STRIP, 0, 4);
        }
    }
}

impl Renderer for GlRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Gl
    }
    fn output_name(&self) -> &'static str {
        self.target.name()
    }
    fn busy(&self) -> bool {
        self.target.busy()
    }
    fn poll_presented(&mut self, timeout: Duration) -> Result<Option<Presented>, String> {
        self.target.poll(&self.libs, timeout)
    }
    fn invalidate(&mut self) {
        // Every frame is a complete repaint; textures stay valid.
    }
    fn set_display_power(&mut self, on: bool) -> Result<bool, String> {
        self.target.set_power(&self.libs, on)
    }
    fn render_frame(&mut self, scene: &mut CpuBackend, _: &mut dyn Presenter) -> Result<FrameStats, String> {
        let start = Instant::now();
        let (width, height) = self.target.size();
        let size = scene.screen_size();
        if (size.w, size.h) != (width, height) {
            return Err(format!("scene is {}x{} but the GL target is {width}x{height}", size.w, size.h));
        }
        let screen = DRect::from_xywh(0, 0, width, height);
        let fbo = self.target.begin()?;
        self.frame += 1;
        self.uploaded = 0;
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            let gl = &self.libs.gl;
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fbo);
            (gl.glViewport)(0, 0, width as GLsizei, height as GLsizei);
            (gl.glUseProgram)(self.program);
            (gl.glBindVertexArray)(self.vertex_array);
            (gl.glActiveTexture)(GL_TEXTURE0);
            (gl.glUniform2f)(self.u_screen, width as f32, height as f32);
            // Premultiplied source-over, as tiny-skia's default pixmap paint.
            (gl.glEnable)(GL_BLEND);
            (gl.glBlendFunc)(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
        }
        let list = scene.display_list();
        for (sig, op) in &list {
            let key = TextureKey { item: sig.key, part: 0 };
            match op {
                DrawOp::Background { color, image } => {
                    // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
                    unsafe {
                        let gl = &self.libs.gl;
                        (gl.glClearColor)(f32::from(color[0]) / 255.0, f32::from(color[1]) / 255.0, f32::from(color[2]) / 255.0, 1.0);
                        (gl.glClear)(GL_COLOR_BUFFER_BIT);
                    }
                    if let Some(image) = image {
                        self.draw_image(key, Point::new(0, 0), image, sig.generation, None, screen);
                    }
                }
                DrawOp::External { pos, buffer } => self.draw_external(*pos, *buffer, screen),
                DrawOp::Image { pos, image } => {
                    let partial = scene.partial_image_damage(sig.key);
                    self.draw_image(key, *pos, image, sig.generation, partial, screen);
                }
                DrawOp::Decoration { pos, surface } => {
                    for solid in &surface.solids {
                        let rect = DRect::from_rect(solid.rect).translate(pos.x, pos.y).intersect(screen);
                        self.draw_solid(rect, solid.rgb);
                    }
                    for (index, part) in surface.parts.iter().enumerate() {
                        let key = TextureKey { item: sig.key, part: index as u32 + 1 };
                        let at = Point::new(pos.x + part.offset.x, pos.y + part.offset.y);
                        self.draw_image(key, at, &part.buffer, sig.generation, None, screen);
                    }
                }
            }
        }
        drop(list);
        scene.ack_damage();
        // Textures of items that left the scene.
        let (gl, frame) = (&self.libs.gl, self.frame);
        self.textures.retain(|_, t| {
            if t.seen != frame {
                // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
                unsafe { (gl.glDeleteTextures)(1, &t.id) };
            }
            t.seen == frame
        });
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        let error = unsafe {
            // The display reads the buffer as soon as it is flipped; the
            // kernel has no implicit fencing here, so wait for the GPU.
            (gl.glFinish)();
            (gl.glGetError)()
        };
        if error != GL_NO_ERROR {
            return Err(format!("GL error {error:#x} while rendering"));
        }
        let rendered = Instant::now();
        self.target.present(&self.libs)?;
        Ok(FrameStats {
            composite_us: (rendered - start).as_micros() as u64,
            convert_us: 0,
            present_us: rendered.elapsed().as_micros() as u64,
            bytes_written: self.uploaded,
            damage_px: u64::from(width) * u64::from(height),
            damage_rects: 1,
            large_alloc_bytes: 0,
        })
    }
    fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        if !self.can_import {
            return Vec::new();
        }
        // Linear only: what this context is proven to import (it is how the
        // scanout buffers themselves are made).
        vec![(DRM_FORMAT_XRGB8888, 0), (DRM_FORMAT_ARGB8888, 0)]
    }
    fn import_dmabuf(&mut self, buffer: &DmabufDesc) -> Result<u64, String> {
        if !self.dmabuf_formats().contains(&(buffer.format, buffer.modifier)) {
            return Err(format!("unsupported dma-buf format {:#x} modifier {:#x}", buffer.format, buffer.modifier));
        }
        if buffer.width == 0 || buffer.height == 0 || u64::from(buffer.stride) < u64::from(buffer.width) * 4 {
            return Err("dma-buf size or stride invalid".into());
        }
        let (create_image, image_target) = self._context.image_functions(&self.libs)?;
        let attribs = [
            EGL_WIDTH, buffer.width as EGLint, EGL_HEIGHT, buffer.height as EGLint,
            EGL_LINUX_DRM_FOURCC_EXT, buffer.format as EGLint,
            EGL_DMA_BUF_PLANE0_FD_EXT, buffer.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, buffer.offset as EGLint,
            EGL_DMA_BUF_PLANE0_PITCH_EXT, buffer.stride as EGLint,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (buffer.modifier & 0xffff_ffff) as u32 as EGLint,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (buffer.modifier >> 32) as u32 as EGLint,
            EGL_NONE,
        ];
        let gl = &self.libs.gl;
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        unsafe {
            let image = create_image(self._context.display, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attribs.as_ptr());
            if image.is_null() {
                return Err(format!("eglCreateImageKHR refused the client dma-buf: EGL error {:#x}", (self.libs.egl.eglGetError)()));
            }
            let mut texture = 0;
            (gl.glGenTextures)(1, &mut texture);
            (gl.glBindTexture)(GL_TEXTURE_2D, texture);
            image_target(GL_TEXTURE_2D, image);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.glTexParameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            let error = (gl.glGetError)();
            if error != GL_NO_ERROR {
                (gl.glDeleteTextures)(1, &texture);
                if let Ok(destroy) = self._context.destroy_image_function(&self.libs) {
                    destroy(self._context.display, image);
                }
                return Err(format!("client dma-buf as texture: GL error {error:#x}"));
            }
            let id = self.next_external;
            self.next_external += 1;
            self.external.insert(id, (image, texture));
            Ok(id)
        }
    }
    fn release_dmabuf(&mut self, id: u64) {
        if let Some((image, texture)) = self.external.remove(&id) {
            // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
            unsafe {
                (self.libs.gl.glDeleteTextures)(1, &texture);
                if let Ok(destroy) = self._context.destroy_image_function(&self.libs) {
                    destroy(self._context.display, image);
                }
            }
        }
    }
    fn read_back(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(fbo) = self.target.last() else {
            return Ok(None);
        };
        let (width, height) = self.target.size();
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let gl = &self.libs.gl;
        // SAFETY: the GL entry points were resolved from this renderer's own GLES library when it was loaded, its context is current on this thread (the only one that renders), and every pointer passed is to a live local or owned buffer of the size the call is given.
        let error = unsafe {
            (gl.glBindFramebuffer)(GL_FRAMEBUFFER, fbo);
            (gl.glPixelStorei)(GL_PACK_ALIGNMENT, 4);
            (gl.glReadPixels)(0, 0, width as GLsizei, height as GLsizei, GL_RGBA, GL_UNSIGNED_BYTE, pixels.as_mut_ptr() as *mut c_void);
            (gl.glGetError)()
        };
        if error != GL_NO_ERROR {
            return Err(format!("glReadPixels: GL error {error:#x}"));
        }
        // The scanout format has no meaningful alpha; every frame is opaque.
        for pixel in pixels.as_chunks_mut::<4>().0.iter_mut() {
            pixel[3] = 255;
        }
        Ok(Some(pixels))
    }
}

/// Differences between two RGBA frames: (differing pixels, largest channel difference).
pub fn compare_frames(a: &[u8], b: &[u8]) -> (u64, u8) {
    let (mut pixels, mut worst) = (0u64, 0u8);
    for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0.iter()) {
        if p != q {
            pixels += 1;
            for (x, y) in p.iter().zip(q) {
                worst = worst.max(x.abs_diff(*y));
            }
        }
    }
    (pixels, worst)
}

/// Bounding box of the pixels that differ between two RGBA frames of `width`,
/// and how many of them lie inside `inside`.
pub fn locate_differences(a: &[u8], b: &[u8], width: u32, inside: DRect) -> (DRect, u64) {
    let (mut bbox, mut within) = (DRect::EMPTY, 0u64);
    for (i, (p, q)) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0.iter()).enumerate() {
        if p != q {
            let (x, y) = ((i as u32 % width) as i32, (i as u32 / width) as i32);
            bbox = bbox.union(DRect::new(x, y, x + 1, y + 1));
            within += u64::from(inside.contains_point(x, y));
        }
    }
    (bbox, within)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wm_core::{Rect, Size};

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u32) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as u32) % bound.max(1)
        }
        fn image(&mut self, w: u32, h: u32, opaque: bool) -> DecorationBuffer {
            let mut pixels = Vec::with_capacity((w * h * 4) as usize);
            for _ in 0..w * h {
                let a = if opaque { 255 } else { [0, 255, 128, 77][self.next(4) as usize] };
                for _ in 0..3 {
                    pixels.push((self.next(256) * u32::from(a) / 255) as u8);
                }
                pixels.push(a);
            }
            DecorationBuffer { width: w, height: h, pixels }
        }
    }
    struct Nothing;
    impl Presenter for Nothing {
        fn present(&mut self, _: &tiny_skia::Pixmap) -> Result<(), String> {
            Ok(())
        }
    }

    /// Needs a working EGL/GLES 3 on the machine running the tests; without
    /// one the test says so and passes, since there is nothing to compare.
    #[test]
    fn gl_frames_equal_the_cpu_render() {
        const W: u32 = 193;
        const H: u32 = 131;
        let mut renderer = match GlRenderer::offscreen(W, H) {
            Ok(renderer) => renderer,
            Err(error) => {
                eprintln!("SKIPPED gl_frames_equal_the_cpu_render: no GL on this host ({error})");
                return;
            }
        };
        let mut rng = Lcg(0x61);
        let mut scene = CpuBackend::new(Size::new(W, H)).unwrap();
        let mut shells: Vec<(u64, bool)> = Vec::new();
        let (mut worst, mut differing, mut exact_frames, mut frames) = (0u8, 0u64, 0, 0);
        for step in 0..400 {
            let opaque = step < 250; // first only opaque images, then translucent ones too
            match rng.next(10) {
                0 | 1 if shells.len() < 12 => {
                    let g = Rect::new(
                        Point::new(rng.next(W + 60) as i32 - 30, rng.next(H + 60) as i32 - 30),
                        Size::new(1 + rng.next(90), 1 + rng.next(70)),
                    );
                    let id = scene.create_shell_surface(g, (9, 9, 9), rng.next(2) == 0).unwrap();
                    scene.replace_shell_image(id, rng.image(g.size.w, g.size.h, opaque));
                    scene.map_shell_surface(id);
                    shells.push((id, opaque));
                }
                2 if !shells.is_empty() => {
                    let (id, _) = shells.swap_remove(rng.next(shells.len() as u32) as usize);
                    scene.destroy_shell_surface(id);
                }
                3 if !shells.is_empty() => {
                    let (id, _) = shells[rng.next(shells.len() as u32) as usize];
                    scene.raise_shell_surface(id);
                }
                4 | 5 if !shells.is_empty() => {
                    // In-place partial edit: exercises glTexSubImage2D.
                    let (id, _) = shells[rng.next(shells.len() as u32) as usize];
                    let image = scene.shell_image_mut(id).unwrap();
                    let (w, h) = (image.width, image.height);
                    let r = DRect::from_xywh(rng.next(w) as i32, rng.next(h) as i32, 1 + rng.next(w), 1 + rng.next(h))
                        .intersect(DRect::from_xywh(0, 0, w, h));
                    let v = rng.next(256) as u8;
                    for y in r.y0..r.y1 {
                        for x in r.x0..r.x1 {
                            let at = ((y as u32 * w + x as u32) * 4) as usize;
                            image.pixels[at..at + 4].copy_from_slice(&[v, v / 2, 255 - v, 255]);
                        }
                    }
                    scene.shell_image_damaged(id, &[r]);
                }
                6 => scene.paint_root_color((rng.next(256) as u8, rng.next(256) as u8, 40)),
                7 => {
                    let (w, h) = (1 + rng.next(W + 30), 1 + rng.next(H + 30));
                    let image = rng.image(w, h, true);
                    scene.paint_root_image(&image);
                }
                8 => scene.set_cursor(Some((Point::new(rng.next(4) as i32, rng.next(4) as i32), rng.image(12, 18, opaque)))),
                _ => {
                    scene.pointer = Point::new(rng.next(W) as i32, rng.next(H) as i32);
                    scene.dirty = true;
                }
            }
            renderer.render_frame(&mut scene, &mut Nothing).unwrap();
            let gl = renderer.read_back().unwrap().unwrap();
            let cpu = scene.render().unwrap();
            let (pixels, channel) = compare_frames(&gl, cpu.data());
            frames += 1;
            exact_frames += u32::from(pixels == 0);
            differing += pixels;
            worst = worst.max(channel);
            if shells.iter().all(|(_, opaque)| *opaque) && opaque {
                assert_eq!(pixels, 0, "opaque scene differs at step {step} (max channel difference {channel})");
            }
        }
        eprintln!("gl vs cpu: {frames} frames, {exact_frames} identical, {differing} differing pixels in total, max channel difference {worst}");
        // Translucent blending may round differently on a GPU, never by more than one step.
        assert!(worst <= 1, "max channel difference {worst}");
        assert!(exact_frames >= 250);
    }

    #[test]
    fn frame_comparison_counts_pixels_and_the_worst_channel() {
        let a = [1, 2, 3, 255, 10, 20, 30, 255, 0, 0, 0, 255];
        let b = [1, 2, 3, 255, 10, 25, 29, 255, 0, 0, 9, 255];
        assert_eq!(compare_frames(&a, &a), (0, 0));
        assert_eq!(compare_frames(&a, &b), (2, 9));
        // Three pixels in a row; pixels 1 and 2 differ, only pixel 2 is "inside".
        let (bbox, within) = locate_differences(&a, &b, 3, DRect::new(2, 0, 3, 1));
        assert_eq!((bbox, within), (DRect::new(1, 0, 3, 1), 1));
        assert_eq!(locate_differences(&a, &a, 3, DRect::new(0, 0, 3, 1)), (DRect::EMPTY, 0));
    }
}
