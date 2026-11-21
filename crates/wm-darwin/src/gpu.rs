//! Run-time bindings to libEGL, libGLESv2 and libdrm, and a surfaceless GLES 3
//! context.
//!
//! The libraries are opened with `dlopen`, never linked: a system without
//! them (or without a GPU) still starts this binary and falls back to the CPU
//! renderer, and the cross-build needs no new link input. Only the calls the
//! compositor makes are bound; none of them is variadic.
#![allow(non_snake_case, non_upper_case_globals, clippy::too_many_arguments)]
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};

/// Directory holding the Mesa bundle in the guest.
pub const DEFAULT_LIBDIR: &str = "/usr/local/lib/mesa-asahi";
/// Environment override for that directory (empty: the loader's search path).
pub const LIBDIR_ENV: &str = "CHONKSTEP_DARWIN_GL_LIBDIR";

#[cfg(target_os = "macos")]
const LIB_NAMES: [&str; 3] = ["libdrm.2.dylib", "libEGL.1.dylib", "libGLESv2.2.dylib"];
#[cfg(not(target_os = "macos"))]
const LIB_NAMES: [&str; 3] = ["libdrm.so.2", "libEGL.so.1", "libGLESv2.so.2"];

fn dl_error() -> String {
    // SAFETY: dlerror takes no arguments; it returns null or this thread's NUL-terminated message.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown dlopen error".into()
    } else {
        // SAFETY: `e` is non-null (checked above) and is dlerror's NUL-terminated message, read before any other dl call.
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}
fn open_library(dir: &str, name: &str) -> Result<*mut c_void, String> {
    let path = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
    let c = CString::new(path.clone()).map_err(|_| "library path contains NUL")?;
    // SAFETY: `c` is a NUL-terminated path that lives across the call.
    let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(format!("dlopen {path}: {}", dl_error()));
    }
    Ok(handle)
}
unsafe fn symbol(handle: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).unwrap();
    // SAFETY: `handle` came from a successful dlopen and `c` is a NUL-terminated symbol name.
    let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if p.is_null() {
        return Err(format!("missing symbol {name}"));
    }
    Ok(p)
}

macro_rules! library {
    ($(#[$meta:meta])* $name:ident { $(fn $f:ident($($a:ident: $t:ty),* $(,)?) $(-> $r:ty)?;)* }) => {
        $(#[$meta])*
        pub struct $name { $(pub $f: unsafe extern "C" fn($($a: $t),*) $(-> $r)?,)* }
        impl $name {
            unsafe fn load(handle: *mut c_void) -> Result<Self, String> {
                // SAFETY: each symbol is looked up by its exact C name and converted to the signature this table declares for it.
                Ok(Self { $($f: unsafe {
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($t),*) $(-> $r)?>(
                        symbol(handle, stringify!($f))?)
                },)* })
            }
        }
    };
}

pub type EGLDisplay = *mut c_void;
pub type EGLConfig = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLSurface = *mut c_void;
pub type EGLImage = *mut c_void;
pub type EGLint = i32;
pub type GLenum = c_uint;
pub type GLuint = c_uint;
pub type GLint = c_int;
pub type GLsizei = c_int;

library! {
    /// libEGL entry points.
    Egl {
        fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
        fn eglQueryString(dpy: EGLDisplay, name: EGLint) -> *const c_char;
        fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> c_uint;
        fn eglBindAPI(api: c_uint) -> c_uint;
        fn eglChooseConfig(dpy: EGLDisplay, attribs: *const EGLint, configs: *mut EGLConfig, size: EGLint, count: *mut EGLint) -> c_uint;
        fn eglCreateContext(dpy: EGLDisplay, config: EGLConfig, share: EGLContext, attribs: *const EGLint) -> EGLContext;
        fn eglMakeCurrent(dpy: EGLDisplay, draw: EGLSurface, read: EGLSurface, ctx: EGLContext) -> c_uint;
        fn eglGetError() -> EGLint;
    }
}
library! {
    /// libGLESv2 entry points.
    Gl {
        fn glGetString(name: GLenum) -> *const u8;
        fn glGetError() -> GLenum;
        fn glGenTextures(n: GLsizei, out: *mut GLuint);
        fn glBindTexture(target: GLenum, texture: GLuint);
        fn glTexParameteri(target: GLenum, name: GLenum, value: GLint);
        fn glTexImage2D(target: GLenum, level: GLint, internal: GLint, width: GLsizei, height: GLsizei, border: GLint, format: GLenum, kind: GLenum, pixels: *const c_void);
        fn glGenFramebuffers(n: GLsizei, out: *mut GLuint);
        fn glBindFramebuffer(target: GLenum, framebuffer: GLuint);
        fn glFramebufferTexture2D(target: GLenum, attachment: GLenum, textarget: GLenum, texture: GLuint, level: GLint);
        fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
        fn glViewport(x: GLint, y: GLint, width: GLsizei, height: GLsizei);
        fn glClearColor(r: f32, g: f32, b: f32, a: f32);
        fn glClear(mask: c_uint);
        fn glFinish();
        fn glDisable(cap: GLenum);
        fn glPixelStorei(name: GLenum, value: GLint);
        fn glReadPixels(x: GLint, y: GLint, width: GLsizei, height: GLsizei, format: GLenum, kind: GLenum, pixels: *mut c_void);
        fn glTexSubImage2D(target: GLenum, level: GLint, x: GLint, y: GLint, width: GLsizei, height: GLsizei, format: GLenum, kind: GLenum, pixels: *const c_void);
        fn glDeleteTextures(n: GLsizei, textures: *const GLuint);
        fn glActiveTexture(texture: GLenum);
        fn glEnable(cap: GLenum);
        fn glBlendFunc(source: GLenum, destination: GLenum);
        fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);
        fn glGenVertexArrays(n: GLsizei, out: *mut GLuint);
        fn glBindVertexArray(array: GLuint);
        fn glCreateShader(kind: GLenum) -> GLuint;
        fn glShaderSource(shader: GLuint, count: GLsizei, strings: *const *const c_char, lengths: *const GLint);
        fn glCompileShader(shader: GLuint);
        fn glGetShaderiv(shader: GLuint, name: GLenum, out: *mut GLint);
        fn glGetShaderInfoLog(shader: GLuint, size: GLsizei, length: *mut GLsizei, log: *mut c_char);
        fn glCreateProgram() -> GLuint;
        fn glAttachShader(program: GLuint, shader: GLuint);
        fn glLinkProgram(program: GLuint);
        fn glGetProgramiv(program: GLuint, name: GLenum, out: *mut GLint);
        fn glGetProgramInfoLog(program: GLuint, size: GLsizei, length: *mut GLsizei, log: *mut c_char);
        fn glUseProgram(program: GLuint);
        fn glGetUniformLocation(program: GLuint, name: *const c_char) -> GLint;
        fn glUniform1i(location: GLint, a: GLint);
        fn glUniform2i(location: GLint, a: GLint, b: GLint);
        fn glUniform2f(location: GLint, a: f32, b: f32);
        fn glUniform4f(location: GLint, a: f32, b: f32, c: f32, d: f32);
    }
}

// Layouts of xf86drm.h / xf86drmMode.h (libdrm 2.4.133, LP64). The sizes were
// taken from the target C compiler and are asserted below.
#[repr(C)]
pub struct DrmVersion {
    pub version_major: c_int,
    pub version_minor: c_int,
    pub version_patchlevel: c_int,
    pub name_len: c_int,
    pub name: *mut c_char,
    pub date_len: c_int,
    pub date: *mut c_char,
    pub desc_len: c_int,
    pub desc: *mut c_char,
}
#[repr(C)]
pub struct DrmModeRes {
    pub count_fbs: c_int,
    pub fbs: *mut u32,
    pub count_crtcs: c_int,
    pub crtcs: *mut u32,
    pub count_connectors: c_int,
    pub connectors: *mut u32,
    pub count_encoders: c_int,
    pub encoders: *mut u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmModeModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub kind: u32,
    pub name: [c_char; 32],
}
#[repr(C)]
pub struct DrmModeConnector {
    pub connector_id: u32,
    pub encoder_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: c_int,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: c_int,
    pub count_modes: c_int,
    pub modes: *mut DrmModeModeInfo,
    pub count_props: c_int,
    pub props: *mut u32,
    pub prop_values: *mut u64,
    pub count_encoders: c_int,
    pub encoders: *mut u32,
}
#[repr(C)]
pub struct DrmModeEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}
#[repr(C)]
pub struct DrmModePlaneRes {
    pub count_planes: u32,
    pub planes: *mut u32,
}
#[repr(C)]
pub struct DrmModePlane {
    pub count_formats: u32,
    pub formats: *mut u32,
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub crtc_x: u32,
    pub crtc_y: u32,
    pub x: u32,
    pub y: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
}
#[repr(C)]
pub struct DrmModeObjectProperties {
    pub count_props: u32,
    pub props: *mut u32,
    pub prop_values: *mut u64,
}
#[repr(C)]
pub struct DrmModePropertyRes {
    pub prop_id: u32,
    pub flags: u32,
    pub name: [c_char; 32],
    pub count_values: c_int,
    pub values: *mut u64,
    pub count_enums: c_int,
    pub enums: *mut c_void,
    pub count_blobs: c_int,
    pub blob_ids: *mut u32,
}
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<DrmVersion>() == 56 && offset_of!(DrmVersion, name) == 16);
    assert!(size_of::<DrmModeRes>() == 80 && offset_of!(DrmModeRes, crtcs) == 24);
    assert!(offset_of!(DrmModeRes, connectors) == 40 && offset_of!(DrmModeRes, encoders) == 56);
    assert!(size_of::<DrmModeModeInfo>() == 68 && offset_of!(DrmModeModeInfo, vdisplay) == 14);
    assert!(offset_of!(DrmModeModeInfo, vrefresh) == 24 && offset_of!(DrmModeModeInfo, kind) == 32);
    assert!(size_of::<DrmModeConnector>() == 88 && offset_of!(DrmModeConnector, connection) == 16);
    assert!(offset_of!(DrmModeConnector, count_modes) == 32 && offset_of!(DrmModeConnector, modes) == 40);
    assert!(offset_of!(DrmModeConnector, count_encoders) == 72 && offset_of!(DrmModeConnector, encoders) == 80);
    assert!(size_of::<DrmModeEncoder>() == 20 && offset_of!(DrmModeEncoder, possible_crtcs) == 12);
    assert!(size_of::<DrmModePlaneRes>() == 16 && offset_of!(DrmModePlaneRes, planes) == 8);
    assert!(size_of::<DrmModePlane>() == 56 && offset_of!(DrmModePlane, plane_id) == 16);
    assert!(offset_of!(DrmModePlane, possible_crtcs) == 44);
    assert!(size_of::<DrmModeObjectProperties>() == 24 && offset_of!(DrmModeObjectProperties, prop_values) == 16);
    assert!(size_of::<DrmModePropertyRes>() == 88 && offset_of!(DrmModePropertyRes, name) == 8);
};

library! {
    /// libdrm entry points.
    Drm {
        fn drmGetVersion(fd: c_int) -> *mut DrmVersion;
        fn drmFreeVersion(version: *mut DrmVersion);
        fn drmIoctl(fd: c_int, request: libc::c_ulong, arg: *mut c_void) -> c_int;
        fn drmPrimeHandleToFD(fd: c_int, handle: u32, flags: u32, prime_fd: *mut c_int) -> c_int;
        fn drmPrimeFDToHandle(fd: c_int, prime_fd: c_int, handle: *mut u32) -> c_int;
        fn drmSetClientCap(fd: c_int, capability: u64, value: u64) -> c_int;
        fn drmModeAddFB2(fd: c_int, width: u32, height: u32, format: u32, handles: *const u32, pitches: *const u32, offsets: *const u32, fb: *mut u32, flags: u32) -> c_int;
        fn drmModeGetResources(fd: c_int) -> *mut DrmModeRes;
        fn drmModeFreeResources(res: *mut DrmModeRes);
        fn drmModeGetConnector(fd: c_int, id: u32) -> *mut DrmModeConnector;
        fn drmModeFreeConnector(connector: *mut DrmModeConnector);
        fn drmModeGetEncoder(fd: c_int, id: u32) -> *mut DrmModeEncoder;
        fn drmModeFreeEncoder(encoder: *mut DrmModeEncoder);
        fn drmModeGetPlaneResources(fd: c_int) -> *mut DrmModePlaneRes;
        fn drmModeFreePlaneResources(res: *mut DrmModePlaneRes);
        fn drmModeGetPlane(fd: c_int, id: u32) -> *mut DrmModePlane;
        fn drmModeFreePlane(plane: *mut DrmModePlane);
        fn drmModeObjectGetProperties(fd: c_int, object: u32, kind: u32) -> *mut DrmModeObjectProperties;
        fn drmModeFreeObjectProperties(props: *mut DrmModeObjectProperties);
        fn drmModeGetProperty(fd: c_int, id: u32) -> *mut DrmModePropertyRes;
        fn drmModeFreeProperty(prop: *mut DrmModePropertyRes);
        fn drmModeCreatePropertyBlob(fd: c_int, data: *const c_void, size: usize, id: *mut u32) -> c_int;
        fn drmModeAtomicAlloc() -> *mut c_void;
        fn drmModeAtomicAddProperty(req: *mut c_void, object: u32, property: u32, value: u64) -> c_int;
        fn drmModeAtomicCommit(fd: c_int, req: *mut c_void, flags: u32, user_data: *mut c_void) -> c_int;
        fn drmModeAtomicFree(req: *mut c_void);
    }
}

// Constants, as the target headers define them (values checked with the
// target C compiler; the two asahi ioctl numbers use the BSD ioccom encoding,
// which for these sizes equals Linux's).
pub const DRM_IOCTL_ASAHI_GEM_CREATE: libc::c_ulong = 0xC018_6445;
pub const DRM_IOCTL_ASAHI_GEM_MMAP_OFFSET: libc::c_ulong = 0xC010_6446;
pub const DRM_CLOEXEC: u32 = 0x0100_0000;
pub const DRM_RDWR: u32 = 2;
pub const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
pub const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x1;
pub const DRM_MODE_ATOMIC_NONBLOCK: u32 = 0x200;
pub const DRM_MODE_ATOMIC_ALLOW_MODESET: u32 = 0x400;
pub const DRM_MODE_OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const DRM_MODE_OBJECT_PLANE: u32 = 0xeeee_eeee;
pub const DRM_PLANE_TYPE_PRIMARY: u64 = 1;
pub const DRM_MODE_CONNECTED: c_int = 1;
pub const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
pub const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241; // "AR24"
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258; // "XR24"
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 2;

pub const EGL_NONE: EGLint = 0x3038;
pub const EGL_EXTENSIONS: EGLint = 0x3055;
pub const EGL_HEIGHT: EGLint = 0x3056;
pub const EGL_WIDTH: EGLint = 0x3057;
pub const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
pub const EGL_OPENGL_ES3_BIT: EGLint = 0x0040;
pub const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
pub const EGL_OPENGL_ES_API: c_uint = 0x30A0;
pub const EGL_PLATFORM_SURFACELESS_MESA: c_uint = 0x31DD;
pub const EGL_LINUX_DMA_BUF_EXT: c_uint = 0x3270;
pub const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
pub const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;

pub const GL_NO_ERROR: GLenum = 0;
pub const GL_TEXTURE_2D: GLenum = 0x0DE1;
pub const GL_DITHER: GLenum = 0x0BD0;
pub const GL_PACK_ALIGNMENT: GLenum = 0x0D05;
pub const GL_UNSIGNED_BYTE: GLenum = 0x1401;
pub const GL_RGBA: GLenum = 0x1908;
pub const GL_RGBA8: GLenum = 0x8058;
pub const GL_VENDOR: GLenum = 0x1F00;
pub const GL_RENDERER: GLenum = 0x1F01;
pub const GL_VERSION: GLenum = 0x1F02;
pub const GL_EXTENSIONS: GLenum = 0x1F03;
pub const GL_NEAREST: GLint = 0x2600;
pub const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
pub const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
pub const GL_COLOR_BUFFER_BIT: c_uint = 0x4000;
pub const GL_TRIANGLE_STRIP: GLenum = 0x0005;
pub const GL_ONE: GLenum = 1;
pub const GL_ONE_MINUS_SRC_ALPHA: GLenum = 0x0303;
pub const GL_BLEND: GLenum = 0x0BE2;
pub const GL_UNPACK_ROW_LENGTH: GLenum = 0x0CF2;
pub const GL_UNPACK_SKIP_ROWS: GLenum = 0x0CF3;
pub const GL_UNPACK_SKIP_PIXELS: GLenum = 0x0CF4;
pub const GL_UNPACK_ALIGNMENT: GLenum = 0x0CF5;
pub const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
pub const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
pub const GL_CLAMP_TO_EDGE: GLint = 0x812F;
pub const GL_TEXTURE0: GLenum = 0x84C0;
pub const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
pub const GL_VERTEX_SHADER: GLenum = 0x8B31;
pub const GL_COMPILE_STATUS: GLenum = 0x8B81;
pub const GL_LINK_STATUS: GLenum = 0x8B82;
pub const GL_FRAMEBUFFER: GLenum = 0x8D40;
pub const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
pub const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;

type GetPlatformDisplay = unsafe extern "C" fn(c_uint, *mut c_void, *const EGLint) -> EGLDisplay;
pub type CreateImage =
    unsafe extern "C" fn(EGLDisplay, EGLContext, c_uint, *mut c_void, *const EGLint) -> EGLImage;
pub type ImageTargetTexture = unsafe extern "C" fn(GLenum, *mut c_void);
pub type DestroyImage = unsafe extern "C" fn(EGLDisplay, EGLImage) -> c_uint;

/// The three libraries. They stay loaded for the life of the process.
pub struct Libraries {
    pub egl: Egl,
    pub gl: Gl,
    pub drm: Drm,
}
impl Libraries {
    /// `Err` is the normal answer on a system without the Mesa bundle.
    pub fn load() -> Result<Self, String> {
        let dir = std::env::var(LIBDIR_ENV).unwrap_or_else(|_| {
            if cfg!(target_os = "macos") { DEFAULT_LIBDIR.into() } else { String::new() }
        });
        let drm = open_library(&dir, LIB_NAMES[0])?;
        let egl = open_library(&dir, LIB_NAMES[1])?;
        let gl = open_library(&dir, LIB_NAMES[2])?;
        // SAFETY: each handle came from a successful dlopen of the library whose table it fills.
        unsafe { Ok(Self { egl: Egl::load(egl)?, gl: Gl::load(gl)?, drm: Drm::load(drm)? }) }
    }
}

unsafe fn string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: `p` is non-null (checked above) and EGL documents the string as NUL-terminated and owned by the display.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// A current surfaceless GLES 3 context: the sequence of the reference
/// `agx-panel-demo.c`. All rendering goes to framebuffer objects.
pub struct Context {
    pub display: EGLDisplay,
    pub context: EGLContext,
    pub display_extensions: String,
    pub gl_extensions: String,
    pub renderer: String,
    pub version: String,
}
impl Context {
    pub fn create(libs: &Libraries, step: &mut dyn FnMut(&str)) -> Result<Self, String> {
        let egl = &libs.egl;
        // SAFETY: the EGL entry points come from the EGL library loaded with this context; the display and context handles are the ones created here, and every out-parameter is a live local.
        unsafe {
            step("eglGetPlatformDisplayEXT(EGL_PLATFORM_SURFACELESS_MESA)");
            let get = (egl.eglGetProcAddress)(c"eglGetPlatformDisplayEXT".as_ptr());
            if get.is_null() {
                return Err("eglGetPlatformDisplayEXT is not available".into());
            }
            let get: GetPlatformDisplay = std::mem::transmute(get);
            let display = get(EGL_PLATFORM_SURFACELESS_MESA, std::ptr::null_mut(), std::ptr::null());
            if display.is_null() {
                return Err(format!("no surfaceless EGL display: EGL error {:#x}", (egl.eglGetError)()));
            }
            step("eglInitialize");
            let (mut major, mut minor) = (0, 0);
            if (egl.eglInitialize)(display, &mut major, &mut minor) == 0 {
                return Err(format!("eglInitialize failed: EGL error {:#x}", (egl.eglGetError)()));
            }
            (egl.eglBindAPI)(EGL_OPENGL_ES_API);
            let attribs = [EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE];
            let (mut config, mut count): (EGLConfig, EGLint) = (std::ptr::null_mut(), 0);
            (egl.eglChooseConfig)(display, attribs.as_ptr(), &mut config, 1, &mut count);
            // No config is EGL_NO_CONFIG_KHR (null), as the reference does.
            let config = if count > 0 { config } else { std::ptr::null_mut() };
            step("eglCreateContext (GLES 3) and eglMakeCurrent (no surface)");
            let context_attribs = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
            let context = (egl.eglCreateContext)(display, config, std::ptr::null_mut(), context_attribs.as_ptr());
            if context.is_null()
                || (egl.eglMakeCurrent)(display, std::ptr::null_mut(), std::ptr::null_mut(), context) == 0
            {
                return Err(format!("GLES 3 context failed: EGL error {:#x}", (egl.eglGetError)()));
            }
            let gl = &libs.gl;
            (gl.glDisable)(GL_DITHER);
            (gl.glPixelStorei)(GL_PACK_ALIGNMENT, 4);
            Ok(Self {
                display,
                context,
                display_extensions: string((egl.eglQueryString)(display, EGL_EXTENSIONS)),
                gl_extensions: string((gl.glGetString)(GL_EXTENSIONS) as *const c_char),
                renderer: string((gl.glGetString)(GL_RENDERER) as *const c_char),
                version: string((gl.glGetString)(GL_VERSION) as *const c_char),
            })
        }
    }
    pub fn has_display_extension(&self, name: &str) -> bool {
        self.display_extensions.split(' ').any(|e| e == name)
    }
    pub fn has_gl_extension(&self, name: &str) -> bool {
        self.gl_extensions.split(' ').any(|e| e == name)
    }
    /// `eglDestroyImageKHR`.
    pub fn destroy_image_function(&self, libs: &Libraries) -> Result<DestroyImage, String> {
        // SAFETY: the EGL entry points come from the EGL library loaded with this context; the display and context handles are the ones created here, and every out-parameter is a live local.
        unsafe {
            let destroy = (libs.egl.eglGetProcAddress)(c"eglDestroyImageKHR".as_ptr());
            if destroy.is_null() {
                return Err("eglDestroyImageKHR missing".into());
            }
            Ok(std::mem::transmute::<*mut c_void, DestroyImage>(destroy))
        }
    }
    /// `eglCreateImageKHR` and `glEGLImageTargetTexture2DOES`.
    pub fn image_functions(&self, libs: &Libraries) -> Result<(CreateImage, ImageTargetTexture), String> {
        // SAFETY: the EGL entry points come from the EGL library loaded with this context; the display and context handles are the ones created here, and every out-parameter is a live local.
        unsafe {
            let create = (libs.egl.eglGetProcAddress)(c"eglCreateImageKHR".as_ptr());
            let target = (libs.egl.eglGetProcAddress)(c"glEGLImageTargetTexture2DOES".as_ptr());
            if create.is_null() || target.is_null() {
                return Err("eglCreateImageKHR / glEGLImageTargetTexture2DOES missing".into());
            }
            Ok((std::mem::transmute::<*mut c_void, CreateImage>(create), std::mem::transmute::<*mut c_void, ImageTargetTexture>(target)))
        }
    }
}
