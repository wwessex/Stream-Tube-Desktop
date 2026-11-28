//! macOS zero-copy video path: mpv renders with its OpenGL backend into an
//! offscreen CGL context, and a small GL pass writes each frame into an
//! IOSurface-backed `420YpCbCr8BiPlanarFullRange` CVPixelBuffer, the only
//! format GPUI's `surface()` element draws. The CPU never touches pixels:
//! VideoToolbox decodes into IOSurfaces, mpv maps them as GL textures, and
//! GPUI's Metal renderer samples the result through a CVMetalTextureCache.
//!
//! GPUI converts the two planes with a fixed matrix (BT.601 full range, see
//! `ycbcrToRGBTransform` in gpui-pre-apple's shaders.metal); the pass here
//! applies the exact inverse of that matrix so GPUI reproduces mpv's RGB.
#![allow(non_upper_case_globals, non_snake_case, clippy::upper_case_acronyms)]

use anyhow::{Result, anyhow, bail};
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr::NonNull;

// ── OpenGL / CGL ────────────────────────────────────────────────────────────

type GLenum = u32;
type GLuint = u32;
type GLint = i32;
type GLsizei = i32;
type CGLContextObj = *mut c_void;
type CGLPixelFormatObj = *mut c_void;
type CGLError = c_int;

const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_TEXTURE_RECTANGLE: GLenum = 0x84F5;
const GL_RGBA8: GLenum = 0x8058;
const GL_RGBA16F: GLenum = 0x881A;
const GL_RGBA: GLenum = 0x1908;
const GL_R8: GLenum = 0x8229;
const GL_RED: GLenum = 0x1903;
const GL_RG8: GLenum = 0x822B;
const GL_RG: GLenum = 0x8227;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_FRAMEBUFFER: GLenum = 0x8D40;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
const GL_NEAREST: GLint = 0x2600;
const GL_CLAMP_TO_EDGE: GLint = 0x812F;
const GL_VERTEX_SHADER: GLenum = 0x8B31;
const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
const GL_COMPILE_STATUS: GLenum = 0x8B81;
const GL_LINK_STATUS: GLenum = 0x8B82;
const GL_TRIANGLES: GLenum = 0x0004;
const GL_TEXTURE0: GLenum = 0x84C0;
const GL_BLEND: GLenum = 0x0BE2;
const GL_SCISSOR_TEST: GLenum = 0x0C11;
const GL_DEPTH_TEST: GLenum = 0x0B71;
const GL_CULL_FACE: GLenum = 0x0B44;
const GL_VERSION: GLenum = 0x1F02;
const GL_RENDERER: GLenum = 0x1F01;

const kCGLPFAAccelerated: c_int = 73;
const kCGLPFAAllowOfflineRenderers: c_int = 96;
const kCGLPFAOpenGLProfile: c_int = 99;
const kCGLPFASupportsAutomaticGraphicsSwitching: c_int = 101;
const kCGLOGLPVersion_Legacy: c_int = 0x1000;
const kCGLOGLPVersion_3_2_Core: c_int = 0x3200;
const kCGLOGLPVersion_GL4_Core: c_int = 0x4100;

#[link(name = "OpenGL", kind = "framework")]
unsafe extern "C" {
    fn CGLChoosePixelFormat(attribs: *const c_int, pix: *mut CGLPixelFormatObj, npix: *mut GLint) -> CGLError;
    fn CGLDestroyPixelFormat(pix: CGLPixelFormatObj) -> CGLError;
    fn CGLCreateContext(pix: CGLPixelFormatObj, share: CGLContextObj, ctx: *mut CGLContextObj) -> CGLError;
    fn CGLDestroyContext(ctx: CGLContextObj) -> CGLError;
    fn CGLSetCurrentContext(ctx: CGLContextObj) -> CGLError;
    fn CGLTexImageIOSurface2D(
        ctx: CGLContextObj,
        target: GLenum,
        internal_format: GLenum,
        width: GLsizei,
        height: GLsizei,
        format: GLenum,
        ty: GLenum,
        surface: *mut c_void,
        plane: GLuint,
    ) -> CGLError;

    fn glGetString(name: GLenum) -> *const c_char;
    fn glGetError() -> GLenum;
    fn glGenTextures(n: GLsizei, out: *mut GLuint);
    fn glDeleteTextures(n: GLsizei, t: *const GLuint);
    fn glBindTexture(target: GLenum, t: GLuint);
    fn glTexParameteri(target: GLenum, pname: GLenum, param: GLint);
    fn glTexImage2D(
        target: GLenum,
        level: GLint,
        internal: GLint,
        w: GLsizei,
        h: GLsizei,
        border: GLint,
        format: GLenum,
        ty: GLenum,
        data: *const c_void,
    );
    fn glGenFramebuffers(n: GLsizei, out: *mut GLuint);
    fn glDeleteFramebuffers(n: GLsizei, f: *const GLuint);
    fn glBindFramebuffer(target: GLenum, f: GLuint);
    fn glFramebufferTexture2D(target: GLenum, attachment: GLenum, textarget: GLenum, texture: GLuint, level: GLint);
    fn glCheckFramebufferStatus(target: GLenum) -> GLenum;
    fn glViewport(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
    fn glDisable(cap: GLenum);
    fn glFlush();
    fn glCreateShader(ty: GLenum) -> GLuint;
    fn glShaderSource(s: GLuint, count: GLsizei, src: *const *const c_char, len: *const GLint);
    fn glCompileShader(s: GLuint);
    fn glGetShaderiv(s: GLuint, pname: GLenum, out: *mut GLint);
    fn glGetShaderInfoLog(s: GLuint, max: GLsizei, len: *mut GLsizei, log: *mut c_char);
    fn glDeleteShader(s: GLuint);
    fn glCreateProgram() -> GLuint;
    fn glAttachShader(p: GLuint, s: GLuint);
    fn glLinkProgram(p: GLuint);
    fn glGetProgramiv(p: GLuint, pname: GLenum, out: *mut GLint);
    fn glGetProgramInfoLog(p: GLuint, max: GLsizei, len: *mut GLsizei, log: *mut c_char);
    fn glDeleteProgram(p: GLuint);
    fn glUseProgram(p: GLuint);
    fn glGetUniformLocation(p: GLuint, name: *const c_char) -> GLint;
    fn glUniform1i(loc: GLint, v: GLint);
    fn glUniform4f(loc: GLint, a: f32, b: f32, c: f32, d: f32);
    fn glGenVertexArrays(n: GLsizei, out: *mut GLuint);
    fn glDeleteVertexArrays(n: GLsizei, v: *const GLuint);
    fn glBindVertexArray(v: GLuint);
    fn glActiveTexture(t: GLenum);
    fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);
}

// ── CoreFoundation / CoreVideo ──────────────────────────────────────────────

type CFTypeRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CVReturn = i32;

#[repr(C)]
struct CFDictionaryCallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryCallBacks;
    static kCFBooleanTrue: CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: isize,
        kcb: *const CFDictionaryCallBacks,
        vcb: *const CFDictionaryCallBacks,
    ) -> CFDictionaryRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFGetRetainCount(cf: CFTypeRef) -> isize;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferIOSurfacePropertiesKey: CFTypeRef;
    static kCVPixelBufferMetalCompatibilityKey: CFTypeRef;
    static kCVPixelBufferOpenGLCompatibilityKey: CFTypeRef;
    fn CVPixelBufferCreate(
        allocator: CFTypeRef,
        w: usize,
        h: usize,
        format: u32,
        attrs: CFDictionaryRef,
        out: *mut *mut c_void,
    ) -> CVReturn;
    fn CVPixelBufferGetIOSurface(pb: *const c_void) -> *mut c_void;
    fn CVPixelBufferLockBaseAddress(pb: *const c_void, flags: u64) -> CVReturn;
    fn CVPixelBufferUnlockBaseAddress(pb: *const c_void, flags: u64) -> CVReturn;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: *const c_void, plane: usize) -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: *const c_void, plane: usize) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pb: *const c_void, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pb: *const c_void, plane: usize) -> usize;
    fn CVPixelBufferGetPixelFormatType(pb: *const c_void) -> u32;
}

/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` ('420f')
pub const PIXEL_FORMAT_420F: u32 = u32::from_be_bytes(*b"420f");
const kCVPixelBufferLock_ReadOnly: u64 = 1;

/// A finished frame: an IOSurface-backed 420f CVPixelBuffer GPUI can draw with `surface()`.
#[derive(Clone)]
pub struct SurfaceFrame(pub CFRetained<CVPixelBuffer>);
// CoreVideo buffers are reference counted thread-safely; the pixels are only
// written by the GL context before the frame is handed over.
unsafe impl Send for SurfaceFrame {}
unsafe impl Sync for SurfaceFrame {}

impl PartialEq for SurfaceFrame {
    fn eq(&self, other: &Self) -> bool {
        CFRetained::as_ptr(&self.0) == CFRetained::as_ptr(&other.0)
    }
}

impl SurfaceFrame {
    fn raw(&self) -> *const c_void {
        CFRetained::as_ptr(&self.0).as_ptr() as *const c_void
    }

    /// RGB (0..=255) at `nx * ny` grid points spread over the frame, converted with
    /// GPUI's matrix. Reads a few hundred bytes of the IOSurface: cheap enough for
    /// the ambient glow every few frames.
    pub fn sample_grid(&self, nx: usize, ny: usize) -> Option<Vec<[u8; 3]>> {
        if nx < 2 || ny < 2 {
            return None;
        }
        self.with_pixels(|w, h, rgb| {
            let mut out = Vec::with_capacity(nx * ny);
            for gy in 0..ny {
                for gx in 0..nx {
                    out.push(rgb((w - 1) * gx / (nx - 1), (h - 1) * gy / (ny - 1)));
                }
            }
            out
        })
        .filter(|out| !out.is_empty())
    }

    /// Size of the frame in pixels (its luma plane).
    pub fn size(&self) -> (u32, u32) {
        let pb = self.raw();
        unsafe { (CVPixelBufferGetWidthOfPlane(pb, 0) as u32, CVPixelBufferGetHeightOfPlane(pb, 0) as u32) }
    }

    /// Black bars of the frame, as [`crate::player::aspect::detect_bars`] finds them
    /// in a software frame. Reads only the pixels the detector looks at.
    pub fn detect_bars(&self) -> Option<(f32, f32)> {
        self.with_pixels(|w, h, rgb| crate::player::aspect::detect_bars_by(w as u32, h as u32, |x, y| crate::player::aspect::is_dark(rgb(x as usize, y as usize))))
            .flatten()
    }

    /// Calls `f(width, height, rgb)` with the frame locked for reading, where
    /// `rgb(x, y)` is a pixel (0..=255) converted with GPUI's matrix. None when the
    /// buffer isn't 420f or can't be read.
    fn with_pixels<R>(&self, f: impl FnOnce(usize, usize, &dyn Fn(usize, usize) -> [u8; 3]) -> R) -> Option<R> {
        let pb = self.raw();
        unsafe {
            if CVPixelBufferGetPixelFormatType(pb) != PIXEL_FORMAT_420F {
                return None;
            }
            if CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly) != 0 {
                return None;
            }
            let (w, h) = (CVPixelBufferGetWidthOfPlane(pb, 0), CVPixelBufferGetHeightOfPlane(pb, 0));
            let (cw, ch) = (CVPixelBufferGetWidthOfPlane(pb, 1), CVPixelBufferGetHeightOfPlane(pb, 1));
            let (ys, cs) = (CVPixelBufferGetBytesPerRowOfPlane(pb, 0), CVPixelBufferGetBytesPerRowOfPlane(pb, 1));
            let yp = CVPixelBufferGetBaseAddressOfPlane(pb, 0);
            let cp = CVPixelBufferGetBaseAddressOfPlane(pb, 1);
            let out = if !yp.is_null() && !cp.is_null() && w >= 2 && h >= 2 && cw >= 1 && ch >= 1 {
                let rgb = |x: usize, y: usize| -> [u8; 3] {
                    let (x, y) = (x.min(w - 1), y.min(h - 1));
                    let (cx, cy) = ((x / 2).min(cw - 1), (y / 2).min(ch - 1));
                    // x < w, y < h and cx < cw, cy < ch: inside the locked planes
                    let (l, cb, cr) = (*yp.add(y * ys + x), *cp.add(cy * cs + cx * 2), *cp.add(cy * cs + cx * 2 + 1));
                    let rgb = gpui_ycbcr_to_rgb([l as f32 / 255.0, cb as f32 / 255.0, cr as f32 / 255.0]);
                    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                    [q(rgb[0]), q(rgb[1]), q(rgb[2])]
                };
                Some(f(w, h, &rgb))
            } else {
                None
            };
            CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
            out
        }
    }
}

// ── colour matrix ───────────────────────────────────────────────────────────

/// GPUI's `ycbcrToRGBTransform` (shaders.metal), as rows: RGB = M · (Y, Cb, Cr) + T.
/// (Metal's float4x4 constructor takes columns; these are the transposed rows.)
const GPUI_M: [[f64; 3]; 3] = [[1.0, 0.0, 1.4020], [1.0, -0.3441, -0.7141], [1.0, 1.7720, 0.0]];
const GPUI_T: [f64; 3] = [-0.7010, 0.5291, -0.8860];

fn gpui_ycbcr_to_rgb(v: [f32; 3]) -> [f32; 3] {
    let mut o = [0f32; 3];
    for (i, row) in GPUI_M.iter().enumerate() {
        o[i] = (row[0] * v[0] as f64 + row[1] * v[1] as f64 + row[2] * v[2] as f64 + GPUI_T[i]) as f32;
    }
    o
}

/// Exact inverse of GPUI's transform: (Y, Cb, Cr) = A · RGB + b, rows as (a0, a1, a2, b).
fn rgb_to_gpui_ycbcr() -> [[f32; 4]; 3] {
    let m = GPUI_M;
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let inv = [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) / det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) / det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) / det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) / det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) / det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) / det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) / det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) / det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) / det,
        ],
    ];
    let mut out = [[0f32; 4]; 3];
    for i in 0..3 {
        let b = -(inv[i][0] * GPUI_T[0] + inv[i][1] * GPUI_T[1] + inv[i][2] * GPUI_T[2]);
        out[i] = [inv[i][0] as f32, inv[i][1] as f32, inv[i][2] as f32, b as f32];
    }
    out
}

// ── GL renderer ─────────────────────────────────────────────────────────────

/// For mpv's `get_proc_address`: every GL entry point lives in OpenGL.framework.
pub unsafe extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    static LIB: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let h = *LIB.get_or_init(|| unsafe {
        libc::dlopen(c"/System/Library/Frameworks/OpenGL.framework/OpenGL".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL)
            as usize
    });
    if h == 0 || name.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { libc::dlsym(h as *mut c_void, name) }
}

const VS: &str = "#version 150
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}";

const FS_LUMA: &str = "#version 150
uniform sampler2D src;
uniform vec4 ky;
out vec4 o;
void main() {
    vec3 c = texelFetch(src, ivec2(gl_FragCoord.xy), 0).rgb;
    o = vec4(dot(c, ky.xyz) + ky.w, 0.0, 0.0, 1.0);
}";

const FS_CHROMA: &str = "#version 150
uniform sampler2D src;
uniform vec4 kcb;
uniform vec4 kcr;
out vec4 o;
void main() {
    ivec2 p = ivec2(gl_FragCoord.xy) * 2;
    ivec2 m = textureSize(src, 0) - 1;
    vec3 c = texelFetch(src, p, 0).rgb
           + texelFetch(src, min(p + ivec2(1, 0), m), 0).rgb
           + texelFetch(src, min(p + ivec2(0, 1), m), 0).rgb
           + texelFetch(src, min(p + ivec2(1, 1), m), 0).rgb;
    c *= 0.25;
    o = vec4(dot(c, kcb.xyz) + kcb.w, dot(c, kcr.xyz) + kcr.w, 0.0, 1.0);
}";

struct Slot {
    frame: SurfaceFrame,
    tex: [GLuint; 2],
    fbo: [GLuint; 2],
}

/// Most slots kept per size. A slot is only rewritten once nobody else retains its
/// pixel buffer: the player keeps the frame on screen, GPUI retains it (through its
/// CVMetalTextures) until the GPU finished the frame that drew it, and one more may
/// wait in the hand-over. Normally 3-4 slots are in use; the rest is headroom.
const MAX_SLOTS: usize = 8;

pub struct GlRenderer {
    ctx: CGLContextObj,
    pub description: String,
    luma: GLuint,
    chroma: GLuint,
    vao: GLuint,
    /// mpv's render target: an RGB texture the conversion pass reads
    rgb_tex: GLuint,
    rgb_fbo: GLuint,
    rgb_format: GLenum,
    size: (u32, u32),
    slots: Vec<Slot>,
    next: usize,
    warned_full: bool,
}

// Created, used and dropped on the render thread only.
unsafe impl Send for GlRenderer {}

impl GlRenderer {
    /// Creates an offscreen CGL context (4.1 core, else 3.2 core) and makes it current
    /// on this thread.
    pub fn new() -> Result<Self> {
        unsafe {
            let mut ctx: CGLContextObj = std::ptr::null_mut();
            let mut chosen = 0;
            for profile in [kCGLOGLPVersion_GL4_Core, kCGLOGLPVersion_3_2_Core, kCGLOGLPVersion_Legacy] {
                // offline renderers + automatic switching: don't force the discrete GPU on
                // dual-GPU Macs just because a GL context exists
                let attrs = [
                    kCGLPFAOpenGLProfile,
                    profile,
                    kCGLPFAAccelerated,
                    kCGLPFAAllowOfflineRenderers,
                    kCGLPFASupportsAutomaticGraphicsSwitching,
                    0,
                ];
                let mut pix: CGLPixelFormatObj = std::ptr::null_mut();
                let mut n: GLint = 0;
                if CGLChoosePixelFormat(attrs.as_ptr(), &mut pix, &mut n) != 0 || pix.is_null() {
                    continue;
                }
                let err = CGLCreateContext(pix, std::ptr::null_mut(), &mut ctx);
                CGLDestroyPixelFormat(pix);
                if err == 0 && !ctx.is_null() {
                    chosen = profile;
                    break;
                }
            }
            if ctx.is_null() {
                bail!("no CGL context");
            }
            if CGLSetCurrentContext(ctx) != 0 {
                CGLDestroyContext(ctx);
                bail!("CGLSetCurrentContext failed");
            }
            let s = |n| {
                let p = glGetString(n);
                if p.is_null() { String::new() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
            };
            let description = format!("{} / {} (profile {chosen:#x})", s(GL_RENDERER), s(GL_VERSION));
            let mut me = GlRenderer {
                ctx,
                description,
                luma: 0,
                chroma: 0,
                vao: 0,
                rgb_tex: 0,
                rgb_fbo: 0,
                rgb_format: GL_RGBA16F,
                size: (0, 0),
                slots: Vec::new(),
                next: 0,
                warned_full: false,
            };
            if chosen == kCGLOGLPVersion_Legacy {
                bail!("only a legacy OpenGL context is available ({})", me.description);
            }
            me.luma = program(FS_LUMA)?;
            me.chroma = program(FS_CHROMA)?;
            glGenVertexArrays(1, &mut me.vao);
            let k = rgb_to_gpui_ycbcr();
            let loc = |p: GLuint, n: &str| glGetUniformLocation(p, CString::new(n).unwrap().as_ptr());
            glUseProgram(me.luma);
            glUniform1i(loc(me.luma, "src"), 0);
            glUniform4f(loc(me.luma, "ky"), k[0][0], k[0][1], k[0][2], k[0][3]);
            glUseProgram(me.chroma);
            glUniform1i(loc(me.chroma, "src"), 0);
            glUniform4f(loc(me.chroma, "kcb"), k[1][0], k[1][1], k[1][2], k[1][3]);
            glUniform4f(loc(me.chroma, "kcr"), k[2][0], k[2][1], k[2][2], k[2][3]);
            glUseProgram(0);
            let e = glGetError();
            if e != 0 {
                bail!("GL setup error {e:#x}");
            }
            Ok(me)
        }
    }

    /// The FBO mpv renders into, (re)created for `w`×`h` (both even).
    /// Returns (fbo, internal format).
    pub fn target(&mut self, w: u32, h: u32) -> Result<(GLuint, GLenum)> {
        if self.size == (w, h) && self.rgb_fbo != 0 {
            return Ok((self.rgb_fbo, self.rgb_format));
        }
        unsafe {
            self.free_targets();
            glGenTextures(1, &mut self.rgb_tex);
            glBindTexture(GL_TEXTURE_2D, self.rgb_tex);
            tex_params(GL_TEXTURE_2D);
            glGenFramebuffers(1, &mut self.rgb_fbo);
            // half floats keep mpv's output unquantized until the YCbCr pass;
            // plain RGBA8 if the driver won't render to them
            for fmt in [GL_RGBA16F, GL_RGBA8] {
                let ty = if fmt == GL_RGBA16F { 0x140B } else { GL_UNSIGNED_BYTE };
                glTexImage2D(GL_TEXTURE_2D, 0, fmt as GLint, w as GLsizei, h as GLsizei, 0, GL_RGBA, ty, std::ptr::null());
                glBindFramebuffer(GL_FRAMEBUFFER, self.rgb_fbo);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, self.rgb_tex, 0);
                if glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE {
                    self.rgb_format = fmt;
                    break;
                }
                self.rgb_format = 0;
            }
            glBindTexture(GL_TEXTURE_2D, 0);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            if self.rgb_format == 0 {
                self.free_targets();
                bail!("no renderable RGB texture format");
            }
            self.size = (w, h);
        }
        Ok((self.rgb_fbo, self.rgb_format))
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            if self.rgb_fbo != 0 {
                glDeleteFramebuffers(1, &self.rgb_fbo);
                self.rgb_fbo = 0;
            }
            if self.rgb_tex != 0 {
                glDeleteTextures(1, &self.rgb_tex);
                self.rgb_tex = 0;
            }
            for s in self.slots.drain(..) {
                glDeleteFramebuffers(2, s.fbo.as_ptr());
                glDeleteTextures(2, s.tex.as_ptr());
            }
            self.next = 0;
            self.size = (0, 0);
        }
    }

    /// A slot nobody else holds (GPUI drops its reference once the frame is off
    /// screen and the GPU is done with it), a new one, or at worst the oldest.
    fn slot(&mut self) -> Result<usize> {
        let n = self.slots.len();
        for i in 0..n {
            let k = (self.next + i) % n;
            let rc = unsafe { CFGetRetainCount(self.slots[k].frame.raw()) };
            if rc <= 1 {
                self.next = (k + 1) % n.max(1);
                return Ok(k);
            }
        }
        if n < MAX_SLOTS {
            let s = unsafe { self.new_slot()? };
            self.slots.push(s);
            self.next = 0;
            return Ok(n);
        }
        // Every buffer is still retained somewhere, which shouldn't happen: rewrite the
        // oldest rather than stall playback, and say so once (it could show as tearing)
        let k = self.next % n;
        self.next = (k + 1) % n;
        if !self.warned_full {
            self.warned_full = true;
            eprintln!("[mpv-gl] all {n} surfaces still referenced, reusing the oldest");
        }
        Ok(k)
    }

    unsafe fn new_slot(&mut self) -> Result<Slot> {
        let (w, h) = self.size;
        unsafe {
            let empty = CFDictionaryCreate(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let keys = [
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
                kCVPixelBufferOpenGLCompatibilityKey,
            ];
            let vals = [empty as CFTypeRef, kCFBooleanTrue, kCFBooleanTrue];
            let attrs = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                vals.as_ptr(),
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            CFRelease(empty as CFTypeRef);
            let mut pb: *mut c_void = std::ptr::null_mut();
            let rc = CVPixelBufferCreate(std::ptr::null(), w as usize, h as usize, PIXEL_FORMAT_420F, attrs, &mut pb);
            CFRelease(attrs as CFTypeRef);
            let pb = NonNull::new(pb as *mut CVPixelBuffer).filter(|_| rc == 0).ok_or_else(|| anyhow!("CVPixelBufferCreate: {rc}"))?;
            let frame = SurfaceFrame(CFRetained::from_raw(pb));
            let surface = CVPixelBufferGetIOSurface(frame.raw());
            if surface.is_null() {
                bail!("pixel buffer has no IOSurface");
            }
            let mut tex = [0; 2];
            let mut fbo = [0; 2];
            glGenTextures(2, tex.as_mut_ptr());
            glGenFramebuffers(2, fbo.as_mut_ptr());
            let planes = [(GL_R8, GL_RED, w, h), (GL_RG8, GL_RG, w / 2, h / 2)];
            let mut result = Ok(());
            for (i, (internal, format, pw, ph)) in planes.into_iter().enumerate() {
                glBindTexture(GL_TEXTURE_RECTANGLE, tex[i]);
                tex_params(GL_TEXTURE_RECTANGLE);
                let e = CGLTexImageIOSurface2D(
                    self.ctx,
                    GL_TEXTURE_RECTANGLE,
                    internal,
                    pw as GLsizei,
                    ph as GLsizei,
                    format,
                    GL_UNSIGNED_BYTE,
                    surface,
                    i as GLuint,
                );
                if e != 0 {
                    result = Err(anyhow!("CGLTexImageIOSurface2D plane {i}: {e}"));
                    break;
                }
                glBindFramebuffer(GL_FRAMEBUFFER, fbo[i]);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_RECTANGLE, tex[i], 0);
                let st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
                if st != GL_FRAMEBUFFER_COMPLETE {
                    result = Err(anyhow!("plane {i} FBO incomplete: {st:#x}"));
                    break;
                }
            }
            glBindTexture(GL_TEXTURE_RECTANGLE, 0);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            if let Err(e) = result {
                glDeleteFramebuffers(2, fbo.as_ptr());
                glDeleteTextures(2, tex.as_ptr());
                return Err(e);
            }
            Ok(Slot { frame, tex, fbo })
        }
    }

    /// Converts what mpv just rendered into a free 420f surface and submits the GL work.
    pub fn convert(&mut self) -> Result<SurfaceFrame> {
        let k = self.slot()?;
        let (w, h) = self.size;
        let slot = &self.slots[k];
        unsafe {
            glDisable(GL_BLEND);
            glDisable(GL_SCISSOR_TEST);
            glDisable(GL_DEPTH_TEST);
            glDisable(GL_CULL_FACE);
            glBindVertexArray(self.vao);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D, self.rgb_tex);
            for (i, (prog, pw, ph)) in [(self.luma, w, h), (self.chroma, w / 2, h / 2)].into_iter().enumerate() {
                glBindFramebuffer(GL_FRAMEBUFFER, slot.fbo[i]);
                glViewport(0, 0, pw as GLsizei, ph as GLsizei);
                glUseProgram(prog);
                glDrawArrays(GL_TRIANGLES, 0, 3);
            }
            glUseProgram(0);
            glBindTexture(GL_TEXTURE_2D, 0);
            glBindVertexArray(0);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            // submit now; the driver orders GPU access to the IOSurface, so
            // Metal's read (in a later GPUI frame) sees the finished planes
            glFlush();
        }
        Ok(slot.frame.clone())
    }
}

impl Drop for GlRenderer {
    fn drop(&mut self) {
        unsafe {
            CGLSetCurrentContext(self.ctx);
            self.free_targets();
            for p in [self.luma, self.chroma] {
                if p != 0 {
                    glDeleteProgram(p);
                }
            }
            if self.vao != 0 {
                glDeleteVertexArrays(1, &self.vao);
            }
            CGLSetCurrentContext(std::ptr::null_mut());
            CGLDestroyContext(self.ctx);
        }
    }
}

unsafe fn tex_params(target: GLenum) {
    unsafe {
        glTexParameteri(target, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(target, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glTexParameteri(target, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(target, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    }
}

unsafe fn shader(ty: GLenum, src: &str) -> Result<GLuint> {
    unsafe {
        let s = glCreateShader(ty);
        let c = CString::new(src)?;
        let p = c.as_ptr();
        glShaderSource(s, 1, &p, std::ptr::null());
        glCompileShader(s);
        let mut ok = 0;
        glGetShaderiv(s, GL_COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = vec![0 as c_char; 1024];
            glGetShaderInfoLog(s, log.len() as GLsizei, std::ptr::null_mut(), log.as_mut_ptr());
            glDeleteShader(s);
            bail!("shader: {}", std::ffi::CStr::from_ptr(log.as_ptr()).to_string_lossy());
        }
        Ok(s)
    }
}

unsafe fn program(fs: &str) -> Result<GLuint> {
    unsafe {
        let v = shader(GL_VERTEX_SHADER, VS)?;
        let f = shader(GL_FRAGMENT_SHADER, fs)?;
        let p = glCreateProgram();
        glAttachShader(p, v);
        glAttachShader(p, f);
        glLinkProgram(p);
        glDeleteShader(v);
        glDeleteShader(f);
        let mut ok = 0;
        glGetProgramiv(p, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            let mut log = vec![0 as c_char; 1024];
            glGetProgramInfoLog(p, log.len() as GLsizei, std::ptr::null_mut(), log.as_mut_ptr());
            glDeleteProgram(p);
            bail!("program: {}", std::ffi::CStr::from_ptr(log.as_ptr()).to_string_lossy());
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_matrix_round_trips_gpui_transform() {
        let k = rgb_to_gpui_ycbcr();
        for rgb in [[0.0f32, 0.0, 0.0], [1.0, 1.0, 1.0], [1.0, 0.0, 0.0], [0.2, 0.7, 0.4], [0.0, 0.0, 1.0]] {
            let ycc: Vec<f32> = (0..3).map(|i| k[i][0] * rgb[0] + k[i][1] * rgb[1] + k[i][2] * rgb[2] + k[i][3]).collect();
            for v in &ycc {
                assert!((-1e-4..=1.0001).contains(v), "{ycc:?} out of range for {rgb:?}");
            }
            let back = gpui_ycbcr_to_rgb([ycc[0], ycc[1], ycc[2]]);
            for i in 0..3 {
                assert!((back[i] - rgb[i]).abs() < 1e-5, "{rgb:?} -> {back:?}");
            }
        }
    }
}
