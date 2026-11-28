//! Minimal dynamic binding to libmpv (client API + software render API, and on
//! macOS the OpenGL render API for the GPU video path in `gl_macos`).
//! The platform's libmpv is loaded at runtime.
#![allow(non_camel_case_types)]
#[cfg(target_os = "macos")]
#[path = "gl_macos.rs"]
pub mod gl_macos;
use anyhow::{Result, anyhow};
use libloading::Library;
use parking_lot::{Condvar, Mutex};
use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_int, c_ulong, c_void};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

type mpv_handle = c_void;
type mpv_render_context = c_void;

#[repr(C)]
struct mpv_render_param {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct mpv_event {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct mpv_event_property {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct mpv_event_end_file {
    reason: c_int,
    error: c_int,
}

const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;
const MPV_FORMAT_NODE: c_int = 6;
const MPV_FORMAT_NODE_ARRAY: c_int = 7;
const MPV_FORMAT_NODE_MAP: c_int = 8;

#[repr(C)]
struct Node {
    value: NodeValue,
    format: c_int,
}

#[repr(C)]
union NodeValue {
    number: f64,
    integer: i64,
    list: *const NodeList,
}

#[repr(C)]
struct NodeList {
    num: c_int,
    values: *const Node,
    keys: *const *const c_char,
}

impl Node {
    // All nodes and strings in an observed property belong to the current
    // mpv event and are consumed before the next call to mpv_wait_event.
    unsafe fn entries(&self, format: c_int) -> Option<(&NodeList, &[Node])> {
        if self.format != format { return None }
        let list = unsafe { self.value.list.as_ref()? };
        let values = if list.num <= 0 { &[] } else {
            unsafe { std::slice::from_raw_parts(list.values, list.num as usize) }
        };
        Some((list, values))
    }

    unsafe fn field(&self, key: &[u8]) -> Option<&Node> {
        let (list, values) = unsafe { self.entries(MPV_FORMAT_NODE_MAP)? };
        values.iter().enumerate().find_map(|(i, value)| {
            let name = unsafe { CStr::from_ptr(*list.keys.add(i)) };
            (name.to_bytes() == key).then_some(value)
        })
    }

    fn number(&self) -> Option<f64> {
        (self.format == MPV_FORMAT_DOUBLE).then(|| unsafe { self.value.number })
    }

    unsafe fn cache_state(&self) -> Option<PropValue> {
        let ranges = unsafe { self.field(b"seekable-ranges")? };
        let (_, ranges) = unsafe { ranges.entries(MPV_FORMAT_NODE_ARRAY)? };
        let ranges = ranges.iter().filter_map(|entry| {
            let start = unsafe { entry.field(b"start")? }.number()?;
            let end = unsafe { entry.field(b"end")? }.number()?;
            (start.is_finite() && end.is_finite() && end > start).then_some((start, end))
        }).collect();
        let disk_bytes = unsafe { self.field(b"file-cache-bytes") }
            .filter(|n| n.format == MPV_FORMAT_INT64)
            .and_then(|n| u64::try_from(unsafe { n.value.integer }).ok())
            .unwrap_or(0);
        Some(PropValue::Cache { ranges, disk_bytes })
    }
}

const EV_SHUTDOWN: c_int = 1;
const EV_END_FILE: c_int = 7;
const EV_FILE_LOADED: c_int = 8;
const EV_PROPERTY_CHANGE: c_int = 22;
const EV_PLAYBACK_RESTART: c_int = 21;

const RP_API_TYPE: c_int = 1;
#[cfg(target_os = "macos")]
const RP_OPENGL_INIT_PARAMS: c_int = 2;
#[cfg(target_os = "macos")]
const RP_OPENGL_FBO: c_int = 3;
#[cfg(target_os = "macos")]
const RP_FLIP_Y: c_int = 4;
const RP_NEXT_FRAME_INFO: c_int = 11;
const RP_BLOCK_FOR_TARGET_TIME: c_int = 12;
const RP_SW_SIZE: c_int = 17;
const RP_SW_FORMAT: c_int = 18;
const RP_SW_STRIDE: c_int = 19;
const RP_SW_POINTER: c_int = 20;
const RENDER_UPDATE_FRAME: u64 = 1;
const FRAME_PRESENT: u64 = 1;
const FRAME_REDRAW: u64 = 2;
const FRAME_REPEAT: u64 = 4;

#[repr(C)]
#[derive(Default)]
struct RenderFrameInfo {
    flags: u64,
    target_time: i64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct mpv_opengl_init_params {
    get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    get_proc_address_ctx: *mut c_void,
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct mpv_opengl_fbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

struct Fns {
    create: unsafe extern "C" fn() -> *mut mpv_handle,
    initialize: unsafe extern "C" fn(*mut mpv_handle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(*mut mpv_handle),
    set_option_string: unsafe extern "C" fn(*mut mpv_handle, *const c_char, *const c_char) -> c_int,
    set_property_async: unsafe extern "C" fn(*mut mpv_handle, u64, *const c_char, c_int, *mut c_void) -> c_int,
    get_property: unsafe extern "C" fn(*mut mpv_handle, *const c_char, c_int, *mut c_void) -> c_int,
    command: unsafe extern "C" fn(*mut mpv_handle, *mut *const c_char) -> c_int,
    command_async: unsafe extern "C" fn(*mut mpv_handle, u64, *mut *const c_char) -> c_int,
    observe_property: unsafe extern "C" fn(*mut mpv_handle, u64, *const c_char, c_int) -> c_int,
    wait_event: unsafe extern "C" fn(*mut mpv_handle, f64) -> *mut mpv_event,
    wakeup: unsafe extern "C" fn(*mut mpv_handle),
    free: unsafe extern "C" fn(*mut c_void),
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    rc_create: unsafe extern "C" fn(*mut *mut mpv_render_context, *mut mpv_handle, *mut mpv_render_param) -> c_int,
    rc_set_update_callback:
        unsafe extern "C" fn(*mut mpv_render_context, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    rc_update: unsafe extern "C" fn(*mut mpv_render_context) -> u64,
    rc_get_info: unsafe extern "C" fn(*mut mpv_render_context, mpv_render_param) -> c_int,
    rc_render: unsafe extern "C" fn(*mut mpv_render_context, *mut mpv_render_param) -> c_int,
    rc_free: unsafe extern "C" fn(*mut mpv_render_context),
    client_api_version: unsafe extern "C" fn() -> c_ulong,
}

pub const MACOS_LIBMPV_INSTALL_HINT: &str =
    "Install libmpv with brew install mpv, then restart Stream Tube";

#[cfg(windows)]
const LIBRARY_NAMES: &[&str] = &["libmpv-2.dll", "mpv-2.dll"];
#[cfg(target_os = "macos")]
const LIBRARY_NAMES: &[&str] = &["libmpv.2.dylib"];
#[cfg(not(any(windows, target_os = "macos")))]
const LIBRARY_NAMES: &[&str] = &["libmpv.so.2", "libmpv.so.1", "libmpv.so"];

#[cfg(windows)]
pub(super) const MISSING_WINDOWS_MPV: &str =
    "libmpv-2.dll is missing. Extract the entire Windows ZIP before opening streamtube.exe, or use the Windows installer.";

/// Linux lookup order for libmpv.
///
/// A distro's own `libmpv.so.2` (mpv ≥ 0.35) is tried before a bundled
/// `libmpv.so.1` (mpv 0.34, FFmpeg 4.4): seeking past the demuxer cache of
/// an HLS fMP4 stream corrupts VP9 packets there and playback freezes
/// forever (issue #178). No system libmpv.so.2 is worse than that, and
/// current distro builds (FFmpeg 9-era) seek these streams correctly — as
/// does the stack the Linux workflow now bundles. A bundled `libmpv.so.2`
/// still comes first, though: it ships with this build and is the one it
/// was tested against. The bundled `.so.1` is only a fallback, for systems
/// where no libmpv is installed at all.
#[cfg(target_os = "linux")]
fn linux_candidates(exe_dir: Option<&std::path::Path>) -> Vec<PathBuf> {
    // the AppImage carries its own libmpv in usr/lib, next to usr/bin
    let bundled = |name: &str| {
        exe_dir
            .map(|d| d.join("../lib").join(name))
            .filter(|p| p.is_file())
    };
    let [so2, so1, bare] = LIBRARY_NAMES else {
        unreachable!()
    };
    let mut c = Vec::new();
    if let Some(p) = bundled(so2) {
        c.push(p);
    }
    c.push(PathBuf::from(so2));
    if let Some(p) = bundled(so1) {
        c.push(p);
    }
    c.push(PathBuf::from(so1));
    c.push(PathBuf::from(bare));
    c
}

fn lib() -> Result<&'static (Library, Fns)> {
    static LIB: std::sync::OnceLock<Result<(Library, Fns), String>> = std::sync::OnceLock::new();
    LIB.get_or_init(|| {
        #[cfg(windows)]
        unsafe {
            preload_vulkan_loader();
        }
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()));
        // Linux fixes the whole order up front, plain names included.
        #[cfg(target_os = "linux")]
        let candidates = linux_candidates(exe_dir.as_deref());
        #[cfg(not(target_os = "linux"))]
        let mut candidates = Vec::new();
        #[cfg(windows)]
        if let Some(d) = &exe_dir {
            candidates.extend(LIBRARY_NAMES.iter().map(|name| d.join(name)));
        }
        #[cfg(windows)]
        candidates.push(PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/third_party/mpv/libmpv-2.dll")));
        #[cfg(target_os = "macos")]
        {
            if let Some(d) = &exe_dir {
                candidates.push(d.join("../Frameworks/libmpv.2.dylib"));
            }
            let brew = crate::macos::homebrew_prefix();
            candidates.push(brew.join("opt/mpv/lib/libmpv.2.dylib"));
            candidates.push(brew.join("lib/libmpv.2.dylib"));
        }
        #[cfg(not(target_os = "linux"))]
        candidates.extend(LIBRARY_NAMES.iter().map(PathBuf::from));
        open_library(candidates)
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

fn open_library(candidates: impl IntoIterator<Item = PathBuf>) -> Result<(Library, Fns), String> {
    unsafe {
        let mut last_err = String::from("libmpv not found");
        // the error from a copy that is there but won't load says more than
        // "not found" from the names tried after it
        let mut first_real_err: Option<String> = None;
        for c in candidates {
            match Library::new(&c) {
                Ok(l) => {
                    macro_rules! sym {
                        ($n:literal) => {
                            *l.get(concat!($n, "\0").as_bytes()).map_err(|e| e.to_string())?
                        };
                    }
                    let fns = Fns {
                        create: sym!("mpv_create"),
                        initialize: sym!("mpv_initialize"),
                        terminate_destroy: sym!("mpv_terminate_destroy"),
                        set_option_string: sym!("mpv_set_option_string"),
                        set_property_async: sym!("mpv_set_property_async"),
                        get_property: sym!("mpv_get_property"),
                        command: sym!("mpv_command"),
                        command_async: sym!("mpv_command_async"),
                        observe_property: sym!("mpv_observe_property"),
                        wait_event: sym!("mpv_wait_event"),
                        wakeup: sym!("mpv_wakeup"),
                        free: sym!("mpv_free"),
                        error_string: sym!("mpv_error_string"),
                        rc_create: sym!("mpv_render_context_create"),
                        rc_set_update_callback: sym!("mpv_render_context_set_update_callback"),
                        rc_update: sym!("mpv_render_context_update"),
                        rc_get_info: sym!("mpv_render_context_get_info"),
                        rc_render: sym!("mpv_render_context_render"),
                        rc_free: sym!("mpv_render_context_free"),
                        client_api_version: sym!("mpv_client_api_version"),
                    };
                    if crate::perf::on() {
                        let v = (fns.client_api_version)();
                        eprintln!("[mpv] loaded {} (client API {}.{})", c.display(), v >> 16, v & 0xffff);
                    }
                    return Ok((l, fns));
                }
                Err(e) => {
                    last_err = match e.source() {
                        Some(source) => format!("{} ({e}, {source})", c.display()),
                        None => format!("{} ({e})", c.display()),
                    };
                    if first_real_err.is_none() && c.is_absolute() && c.is_file() {
                        first_real_err = Some(last_err.clone());
                    }
                }
            }
        }
        #[cfg(windows)]
        if first_real_err.is_none() {
            return Err(MISSING_WINDOWS_MPV.into());
        }
        #[cfg(target_os = "macos")]
        if first_real_err.is_none() {
            return Err(MACOS_LIBMPV_INSTALL_HINT.into());
        }
        Err(first_real_err.unwrap_or(last_err))
    }
}

#[cfg(all(test, windows))]
mod windows_loader_tests {
    use super::{MISSING_WINDOWS_MPV, open_library};

    #[test]
    fn missing_dll_explains_how_to_open_the_portable_app() {
        let path = crate::store::data_dir().join("missing-test-engine").join("libmpv-2.dll");
        let error = open_library([path]).err().expect("missing playback library");
        assert_eq!(error, MISSING_WINDOWS_MPV);
    }

    #[test]
    fn existing_dll_keeps_the_windows_loader_error() {
        let path = crate::store::data_dir().join("invalid-test-engine.dll");
        std::fs::write(&path, b"invalid DLL").unwrap();
        let error = open_library([path.clone()]).err().expect("invalid playback library");
        assert!(error.contains(&*path.to_string_lossy()));
        assert!(error.contains("193"), "{error}");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_loader_tests {
    use super::linux_candidates;
    use std::path::{Path, PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("streamtube-mpv-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        // "bin" must exist: is_file() follows bin/../lib, and .. needs the real parent
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::create_dir_all(d.join("lib")).unwrap();
        d
    }

    fn bundled(dir: &Path, name: &str) {
        std::fs::write(dir.join("lib").join(name), b"").unwrap();
    }

    fn pos(candidates: &[PathBuf], name: &str) -> usize {
        candidates.iter().position(|p| p.file_name().is_some_and(|n| n.to_str() == Some(name)))
            .unwrap_or_else(|| panic!("{name} missing: {candidates:?}"))
    }

    /// The 0.7.x AppImage bundles mpv 0.34 / FFmpeg 4.4, whose HLS demuxer
    /// freezes playback on fMP4 seeks (#178): the system's libmpv.so.2 must
    /// be tried before that bundled .so.1.
    #[test]
    fn system_libmpv2_beats_a_bundled_libmpv1() {
        let d = scratch("bundled1");
        bundled(&d, "libmpv.so.1");
        let c = linux_candidates(Some(&d.join("bin")));
        assert!(pos(&c, "libmpv.so.2") < pos(&c, "libmpv.so.1"), "{c:?}");
        // the .so.1 left in the list is the bundled copy, not the system's
        assert!(c.iter().any(|p| p.ends_with("lib/libmpv.so.1")), "{c:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bundled_libmpv2_still_wins_over_the_system_one() {
        let d = scratch("bundled2");
        bundled(&d, "libmpv.so.2");
        bundled(&d, "libmpv.so.1");
        let c = linux_candidates(Some(&d.join("bin")));
        assert!(c[0].ends_with("lib/libmpv.so.2"), "{c:?}");
        assert!(pos(&c, "libmpv.so.2") < pos(&c, "libmpv.so.1"), "{c:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn without_a_bundled_library_the_plain_names_keep_their_order() {
        assert_eq!(
            linux_candidates(None),
            ["libmpv.so.2", "libmpv.so.1", "libmpv.so"].iter().map(PathBuf::from).collect::<Vec<_>>()
        );
    }
}

/// libmpv-2.dll links against vulkan-1.dll, which Windows only has when the GPU
/// driver installed it (not with older Intel graphics such as HD 4000, or in
/// some VMs). Without it libmpv fails to load at all, even though playback here
/// never uses Vulkan. The app ships the Khronos loader in a sub-folder and loads
/// it only when the system has none; once a module named vulkan-1.dll is loaded,
/// Windows uses it to satisfy libmpv's import.
#[cfg(windows)]
unsafe fn preload_vulkan_loader() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    // system copy (System32), never one from the current directory
    let sys = std::env::var_os("SystemRoot")
        .map(|r| std::path::PathBuf::from(r).join("System32").join("vulkan-1.dll"))
        .filter(|p| p.is_file());
    if sys.is_some() {
        return;
    }
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) else { return };
    for p in [dir.join("vulkan").join("vulkan-1.dll"), std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/third_party/vulkan/vulkan-1.dll"))] {
        if p.is_file() {
            if let Ok(l) = unsafe { Library::new(&p) } {
                // stays loaded for the life of the process
                std::mem::forget(l);
                return;
            }
        }
    }
}

pub fn available() -> bool {
    lib().is_ok()
}

#[derive(Clone, Debug)]
pub enum MpvEvent {
    FileLoaded,
    PlaybackRestart,
    EndFile { eof: bool, error: Option<String> },
    Prop(&'static str, PropValue),
    Frame,
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum PropValue {
    Double(f64),
    Flag(bool),
    Str(String),
    Cache { ranges: Vec<(f64, f64)>, disk_bytes: u64 },
}

const OBSERVED: &[(&str, c_int)] = &[
    ("time-pos", MPV_FORMAT_DOUBLE),
    ("duration", MPV_FORMAT_DOUBLE),
    ("pause", MPV_FORMAT_FLAG),
    ("eof-reached", MPV_FORMAT_FLAG),
    ("paused-for-cache", MPV_FORMAT_FLAG),
    ("seeking", MPV_FORMAT_FLAG),
    ("demuxer-cache-time", MPV_FORMAT_DOUBLE),
    ("demuxer-cache-state", MPV_FORMAT_NODE),
    ("speed", MPV_FORMAT_DOUBLE),
    ("volume", MPV_FORMAT_DOUBLE),
    ("mute", MPV_FORMAT_FLAG),
    ("dwidth", MPV_FORMAT_DOUBLE),
    ("dheight", MPV_FORMAT_DOUBLE),
    ("core-idle", MPV_FORMAT_FLAG),
];

#[cfg(windows)]
fn set_playback_awake(playing: bool, video: bool) {
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    let mut flags = ES_CONTINUOUS;
    if playing {
        flags |= ES_SYSTEM_REQUIRED;
        if video {
            flags |= ES_DISPLAY_REQUIRED;
        }
    }
    unsafe { SetThreadExecutionState(flags) };
}

struct Handle(*mut mpv_handle);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

struct RenderCtx(*mut mpv_render_context);
unsafe impl Send for RenderCtx {}

/// A rendered video frame (BGRA, alpha forced to 255).
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// BGRA pixels; empty on the GL path
    pub data: Vec<u8>,
    pub picture_serial: u64,
    /// GL path: the frame as an IOSurface-backed 420f pixel buffer for GPUI's `surface()`
    #[cfg(target_os = "macos")]
    pub surface: Option<gl_macos::SurfaceFrame>,
}

struct RenderShared {
    wake: Mutex<bool>,
    cv: Condvar,
    target: Mutex<(u32, u32)>,
    frame: Mutex<Option<VideoFrame>>,
    presented_frames: AtomicU64,
    stop: AtomicBool,
    force: AtomicBool,
}

pub struct Mpv {
    h: Arc<Handle>,
    render: Arc<RenderShared>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// GL path only: the GL renderer in use
    gl: Option<String>,
}

unsafe extern "C" fn on_render_update(ctx: *mut c_void) {
    let shared = unsafe { &*(ctx as *const RenderShared) };
    let mut w = shared.wake.lock();
    *w = true;
    shared.cv.notify_one();
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Block([u8; 64]);

impl Mpv {
    /// Creates a player. `events` receives property changes and frame notifications.
    pub fn new(options: &[(&str, &str)], events: impl Fn(MpvEvent) + Send + Sync + 'static) -> Result<Arc<Self>> {
        Self::create(options, false, events)
    }

    /// Like `new`, but renders through mpv's OpenGL API in an offscreen CGL context
    /// and hands frames over as IOSurface-backed pixel buffers (`VideoFrame::surface`)
    /// instead of bytes. Pair with `hwdec=videotoolbox` so decoded frames stay on the GPU.
    /// Fails, leaving nothing running, when no usable GL or mpv render context exists.
    #[cfg(target_os = "macos")]
    pub fn new_gl(options: &[(&str, &str)], events: impl Fn(MpvEvent) + Send + Sync + 'static) -> Result<Arc<Self>> {
        Self::create(options, true, events)
    }

    /// `Some(renderer description)` when frames come from the GL path.
    pub fn gl_renderer(&self) -> Option<&str> {
        self.gl.as_deref()
    }

    fn create(options: &[(&str, &str)], gl: bool, events: impl Fn(MpvEvent) + Send + Sync + 'static) -> Result<Arc<Self>> {
        let (_, f) = lib()?;
        let events = Arc::new(events);
        unsafe {
            let h = (f.create)();
            if h.is_null() {
                return Err(anyhow!("mpv_create failed"));
            }
            for (k, v) in options {
                let k = CString::new(*k)?;
                let v = CString::new(*v)?;
                (f.set_option_string)(h, k.as_ptr(), v.as_ptr());
            }
            let rc = (f.initialize)(h);
            if rc < 0 {
                return Err(anyhow!("mpv_initialize failed: {}", err_str(rc)));
            }
            for (i, (name, fmt)) in OBSERVED.iter().enumerate() {
                let n = CString::new(*name)?;
                (f.observe_property)(h, i as u64, n.as_ptr(), *fmt);
            }
            let render = Arc::new(RenderShared {
                wake: Mutex::new(false),
                cv: Condvar::new(),
                target: Mutex::new((640, 360)),
                frame: Mutex::new(None),
                presented_frames: AtomicU64::new(0),
                stop: AtomicBool::new(false),
                force: AtomicBool::new(false),
            });
            let handle = Arc::new(Handle(h));
            // GL path: its render thread creates the GL and mpv render contexts itself
            #[cfg(target_os = "macos")]
            let gl_render = if gl {
                match spawn_gl_render(handle.clone(), render.clone(), events.clone()) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        (f.terminate_destroy)(h);
                        return Err(e);
                    }
                }
            } else {
                None
            };
            #[cfg(not(target_os = "macos"))]
            let gl_render: Option<(std::thread::JoinHandle<()>, String)> = {
                let _ = gl;
                None
            };
            // render context
            let mut rctx: *mut mpv_render_context = std::ptr::null_mut();
            if gl_render.is_none() {
                let api = CString::new("sw")?;
                let mut params = [
                    mpv_render_param { kind: RP_API_TYPE, data: api.as_ptr() as *mut c_void },
                    mpv_render_param { kind: 0, data: std::ptr::null_mut() },
                ];
                let rc = (f.rc_create)(&mut rctx, h, params.as_mut_ptr());
                if rc < 0 {
                    (f.terminate_destroy)(h);
                    return Err(anyhow!("render context: {}", err_str(rc)));
                }
                (f.rc_set_update_callback)(rctx, Some(on_render_update), Arc::as_ptr(&render) as *mut c_void);
            }

            let gl_desc = gl_render.as_ref().map(|(_, desc)| desc.clone());
            let mpv = Arc::new(Mpv { h: handle.clone(), render: render.clone(), threads: Mutex::new(Vec::new()), gl: gl_desc });

            // event thread
            let ev_handle = handle.clone();
            let ev_cb = events.clone();
            let t1 = std::thread::Builder::new().name("mpv-events".into()).spawn(move || {
                let (_, f) = lib().unwrap();
                #[cfg(windows)]
                let (mut playing, mut has_video) = (false, false);
                loop {
                    let e = (f.wait_event)(ev_handle.0, -1.0);
                    if e.is_null() {
                        continue;
                    }
                    let e = &*e;
                    match e.event_id {
                        0 => {}
                        EV_SHUTDOWN => {
                            ev_cb(MpvEvent::Shutdown);
                            break;
                        }
                        EV_FILE_LOADED => ev_cb(MpvEvent::FileLoaded),
                        EV_PLAYBACK_RESTART => ev_cb(MpvEvent::PlaybackRestart),
                        EV_END_FILE => {
                            let d = &*(e.data as *const mpv_event_end_file);
                            // reason: 0 eof, 2 stop, 3 quit, 4 error, 5 redirect
                            ev_cb(MpvEvent::EndFile {
                                eof: d.reason == 0,
                                error: (d.reason == 4).then(|| err_str(d.error)),
                            });
                        }
                        EV_PROPERTY_CHANGE => {
                            let p = &*(e.data as *const mpv_event_property);
                            let idx = e.reply_userdata as usize;
                            if idx < OBSERVED.len() {
                                let name = OBSERVED[idx].0;
                                let val = if p.data.is_null() {
                                    None
                                } else {
                                    match p.format {
                                        MPV_FORMAT_DOUBLE => Some(PropValue::Double(*(p.data as *const f64))),
                                        MPV_FORMAT_FLAG => Some(PropValue::Flag(*(p.data as *const c_int) != 0)),
                                        MPV_FORMAT_NODE => (&*p.data.cast::<Node>()).cache_state(),
                                        MPV_FORMAT_STRING => {
                                            let s = *(p.data as *const *const c_char);
                                            (!s.is_null()).then(|| PropValue::Str(CStr::from_ptr(s).to_string_lossy().into_owned()))
                                        }
                                        _ => None,
                                    }
                                };
                                #[cfg(windows)]
                                match name {
                                    "core-idle" => {
                                        playing = matches!(val.as_ref(), Some(PropValue::Flag(false)));
                                        set_playback_awake(playing, has_video);
                                    }
                                    "dwidth" => {
                                        has_video = matches!(val.as_ref(), Some(PropValue::Double(width)) if *width > 0.0);
                                        set_playback_awake(playing, has_video);
                                    }
                                    _ => {}
                                }
                                if let Some(v) = val {
                                    ev_cb(MpvEvent::Prop(name, v));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                #[cfg(windows)]
                set_playback_awake(false, false);
            })?;
            if let Some((t2, _)) = gl_render {
                // Drop expects the event thread first
                mpv.threads.lock().extend([t1, t2]);
                return Ok(mpv);
            }

            // render thread
            let r = render.clone();
            let frame_cb = events.clone();
            let rc_ptr = RenderCtx(rctx);
            let t2 = std::thread::Builder::new().name("mpv-render".into()).spawn(move || {
                let rc_ptr = rc_ptr;
                let (_, f) = lib().unwrap();
                let mut buf: Vec<Block> = Vec::new();
                let mut last_size = (0u32, 0u32);
                let fmt = CString::new("bgr0").unwrap();
                loop {
                    {
                        let mut w = r.wake.lock();
                        while !*w && !r.stop.load(Ordering::Relaxed) {
                            r.cv.wait(&mut w);
                        }
                        *w = false;
                    }
                    if r.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let flags = (f.rc_update)(rc_ptr.0);
                    let forced = r.force.swap(false, Ordering::Relaxed);
                    if flags & RENDER_UPDATE_FRAME == 0 && !forced {
                        continue;
                    }
                    let (tw, th) = *r.target.lock();
                    let w = ((tw.max(16) + 15) / 16) * 16;
                    let h = th.max(16);
                    let stride = w as usize * 4;
                    let bytes = stride * h as usize;
                    let blocks = bytes.div_ceil(64);
                    if (w, h) != last_size {
                        // a new size reinterprets the old pixels at a different stride, which
                        // shows up as streaks until mpv has a frame to draw: start from black
                        last_size = (w, h);
                        buf.clear();
                        buf.resize(blocks, Block([0; 64]));
                    }
                    let mut size = [w as c_int, h as c_int];
                    let mut stride_v: usize = stride;
                    let mut no_block: c_int = 0;
                    let mut params = [
                        mpv_render_param { kind: RP_SW_SIZE, data: size.as_mut_ptr() as *mut c_void },
                        mpv_render_param { kind: RP_SW_FORMAT, data: fmt.as_ptr() as *mut c_void },
                        mpv_render_param { kind: RP_SW_STRIDE, data: &mut stride_v as *mut usize as *mut c_void },
                        mpv_render_param { kind: RP_SW_POINTER, data: buf.as_mut_ptr() as *mut c_void },
                        mpv_render_param { kind: RP_BLOCK_FOR_TARGET_TIME, data: &mut no_block as *mut c_int as *mut c_void },
                        mpv_render_param { kind: 0, data: std::ptr::null_mut() },
                    ];
                    let t0 = std::time::Instant::now();
                    let mut info = RenderFrameInfo::default();
                    let param = mpv_render_param { kind: RP_NEXT_FRAME_INFO, data: (&mut info as *mut RenderFrameInfo).cast() };
                    let info_result = (f.rc_get_info)(rc_ptr.0, param);
                    let rc = (f.rc_render)(rc_ptr.0, params.as_mut_ptr());
                    crate::perf::add(&crate::perf::MPV_RENDER_US, t0.elapsed().as_micros() as u64);
                    crate::perf::add(&crate::perf::MPV_FRAMES, 1);
                    if rc < 0 {
                        continue;
                    }
                    // one pass: copy each pixel out with its padding byte set to opaque
                    // (the buffer is 64-byte aligned, so reading it as u32 is fine)
                    let src = std::slice::from_raw_parts(buf.as_ptr() as *const u32, bytes / 4);
                    let opaque = u32::from_ne_bytes([0, 0, 0, 0xFF]);
                    // a frame the UI never picked up gives its buffer back; otherwise a
                    // fresh one, written once (not zeroed first: ~8 MB a frame at 1080p)
                    let mut data = r.frame.lock().take().map(|f| f.data).filter(|d| d.capacity() >= bytes).unwrap_or_default();
                    data.clear();
                    data.reserve_exact(bytes);
                    for (d, s) in data.spare_capacity_mut()[..bytes].chunks_exact_mut(4).zip(src) {
                        for (o, b) in d.iter_mut().zip((s | opaque).to_ne_bytes()) {
                            o.write(b);
                        }
                    }
                    // SAFETY: the loop above wrote all `bytes` bytes (bytes is w*h*4, src has w*h pixels)
                    data.set_len(bytes);
                    // Resizing the window and redrawing subtitles can both
                    // repaint the last picture while the decoder is stuck.
                    let new_picture = if info_result >= 0 {
                        info.flags & (FRAME_PRESENT | FRAME_REDRAW | FRAME_REPEAT) == FRAME_PRESENT
                    } else {
                        flags & RENDER_UPDATE_FRAME != 0
                    };
                    let picture_serial = if new_picture {
                        r.presented_frames.fetch_add(1, Ordering::Relaxed) + 1
                    } else {
                        r.presented_frames.load(Ordering::Relaxed)
                    };
                    *r.frame.lock() = Some(VideoFrame {
                        width: w,
                        height: h,
                        data,
                        picture_serial,
                        #[cfg(target_os = "macos")]
                        surface: None,
                    });
                    frame_cb(MpvEvent::Frame);
                }
                (f.rc_free)(rc_ptr.0);
            })?;
            mpv.threads.lock().extend([t1, t2]);
            Ok(mpv)
        }
    }

    /// Sets the size frames are rendered at (physical pixels).
    pub fn set_render_size(&self, w: u32, h: u32) {
        let mut t = self.render.target.lock();
        let (w, h) = (w.clamp(16, 3840), h.clamp(16, 2160));
        if *t != (w, h) {
            *t = (w, h);
            drop(t);
            self.redraw();
        }
    }

    /// Forces a re-render of the current frame (e.g. after resize while paused).
    pub fn redraw(&self) {
        self.render.force.store(true, Ordering::Relaxed);
        let mut w = self.render.wake.lock();
        *w = true;
        self.render.cv.notify_one();
    }

    pub fn presented_frames(&self) -> u64 {
        self.render.presented_frames.load(Ordering::Relaxed)
    }

    pub fn take_frame(&self) -> Option<VideoFrame> {
        self.render.frame.lock().take()
    }

    pub fn command(&self, args: &[&str]) -> Result<()> {
        let (_, f) = lib()?;
        let cs: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap_or_default()).collect();
        let mut ptrs: Vec<*const c_char> = cs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { (f.command_async)(self.h.0, 0, ptrs.as_mut_ptr()) };
        if rc < 0 { Err(anyhow!("mpv command {:?}: {}", args.first(), err_str(rc))) } else { Ok(()) }
    }

    pub fn command_sync(&self, args: &[&str]) -> Result<()> {
        let (_, f) = lib()?;
        let cs: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap_or_default()).collect();
        let mut ptrs: Vec<*const c_char> = cs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { (f.command)(self.h.0, ptrs.as_mut_ptr()) };
        if rc < 0 { Err(anyhow!("mpv command {:?}: {}", args.first(), err_str(rc))) } else { Ok(()) }
    }

    // async, like `command`: setting properties from the UI never waits on mpv's
    // dispatch lock while the core thread is opening or tearing down a stream
    pub fn set(&self, name: &str, value: &str) {
        if let Ok((_, f)) = lib() {
            let n = CString::new(name).unwrap_or_default();
            let v = CString::new(value).unwrap_or_default();
            let mut p: *const c_char = v.as_ptr();
            unsafe {
                (f.set_property_async)(self.h.0, 0, n.as_ptr(), MPV_FORMAT_STRING, &mut p as *mut *const c_char as *mut c_void);
            }
        }
    }

    pub fn set_f64(&self, name: &str, mut value: f64) {
        if let Ok((_, f)) = lib() {
            let n = CString::new(name).unwrap_or_default();
            unsafe {
                (f.set_property_async)(self.h.0, 0, n.as_ptr(), MPV_FORMAT_DOUBLE, &mut value as *mut f64 as *mut c_void);
            }
        }
    }

    pub fn set_flag(&self, name: &str, value: bool) {
        if let Ok((_, f)) = lib() {
            let n = CString::new(name).unwrap_or_default();
            let mut v: c_int = value as c_int;
            unsafe {
                (f.set_property_async)(self.h.0, 0, n.as_ptr(), MPV_FORMAT_FLAG, &mut v as *mut c_int as *mut c_void);
            }
        }
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        let (_, f) = lib().ok()?;
        let n = CString::new(name).ok()?;
        let mut v: f64 = 0.0;
        let rc = unsafe { (f.get_property)(self.h.0, n.as_ptr(), MPV_FORMAT_DOUBLE, &mut v as *mut f64 as *mut c_void) };
        (rc >= 0).then_some(v)
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        let (_, f) = lib().ok()?;
        let n = CString::new(name).ok()?;
        let mut p: *mut c_char = std::ptr::null_mut();
        let rc = unsafe { (f.get_property)(self.h.0, n.as_ptr(), MPV_FORMAT_STRING, &mut p as *mut *mut c_char as *mut c_void) };
        if rc < 0 || p.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() };
        unsafe { (f.free)(p as *mut c_void) };
        Some(s)
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        self.render.stop.store(true, Ordering::Relaxed);
        {
            let mut w = self.render.wake.lock();
            *w = true;
            self.render.cv.notify_all();
        }
        let threads: Vec<_> = self.threads.lock().drain(..).collect();
        let mut it = threads.into_iter();
        let ev = it.next();
        // 1) join the render thread (it frees the render context)
        for t in it {
            let _ = t.join();
        }
        // 2) ask the core to quit; the event thread exits on SHUTDOWN
        let _ = self.command_sync(&["quit"]);
        if let Some(t) = ev {
            let _ = t.join();
        }
        // 3) destroy once nobody is waiting on the handle
        if let Ok((_, f)) = lib() {
            unsafe {
                (f.terminate_destroy)(self.h.0);
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{Mpv, MpvEvent};
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "requires macOS libmpv and a local video in STREAMTUBE_MPV_TEST_VIDEO"]
    fn renders_local_video_without_a_window() {
        let decoder = render_local_video("no");
        assert!(matches!(decoder.as_deref(), None | Some("no")));
    }

    #[test]
    #[ignore = "requires VideoToolbox, macOS libmpv and a compatible video in STREAMTUBE_MPV_TEST_VIDEO"]
    fn renders_with_videotoolbox_without_a_window() {
        let decoder = render_local_video("auto-copy");
        assert_eq!(decoder.as_deref(), Some("videotoolbox-copy"));
    }

    /// The GL path the player uses on macOS: frames arrive as 420f surfaces at the
    /// requested size (also tiny ones), a buffer the UI still holds is never handed
    /// out again, pausing keeps redraws working, and mpv's own screenshots still work.
    #[test]
    #[ignore = "requires VideoToolbox, macOS libmpv and a local video in STREAMTUBE_MPV_TEST_VIDEO"]
    fn gl_path_without_a_window() {
        let path = std::env::var("STREAMTUBE_MPV_TEST_VIDEO").expect("set a path to a local video");
        let (tx, rx) = std::sync::mpsc::channel();
        let player = Mpv::new_gl(
            &[
                ("vo", "libmpv"),
                ("ao", "null"),
                ("hwdec", "videotoolbox"),
                ("gpu-dumb-mode", "yes"),
                ("config", "no"),
                ("osc", "no"),
                ("keep-open", "yes"),
            ],
            move |event| {
                let _ = tx.send(event);
            },
        )
        .expect("GL path");
        eprintln!("GL renderer: {}", player.gl_renderer().unwrap_or("?"));
        // the next frame of the given size, as the UI would take it
        let next = |w: u32, h: u32| -> super::gl_macos::SurfaceFrame {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Ok(MpvEvent::Frame) = rx.recv_timeout(Duration::from_secs(5)) {
                    if let Some(f) = player.take_frame() {
                        assert!(f.data.is_empty(), "no bytes on the GL path");
                        let s = f.surface.expect("GL frames carry a surface");
                        if (f.width, f.height) == (w, h) {
                            return s;
                        }
                    }
                }
            }
            panic!("no {w}x{h} frame");
        };
        player.set_render_size(1280, 720);
        player.command_sync(&["loadfile", &path]).unwrap();
        // the UI keeps the displayed frame, GPUI may still be drawing the one before it
        let mut held: Vec<super::gl_macos::SurfaceFrame> = Vec::new();
        for _ in 0..120 {
            let s = next(1280, 720);
            assert!(!held.contains(&s), "a buffer still in use was rewritten");
            held.push(s);
            if held.len() > 2 {
                held.remove(0);
            }
        }
        assert_eq!(player.get_string("hwdec-current").as_deref(), Some("videotoolbox"));
        // a tiny size, then back
        player.set_render_size(16, 16);
        next(16, 16);
        player.set_render_size(1280, 720);
        let s = next(1280, 720);
        let px = s.sample_grid(16, 12).expect("sample the surface");
        assert!(px.iter().any(|p| p.iter().any(|&v| v > 32)), "frame is not black");
        // paused: a size change still redraws the current frame
        player.set("pause", "yes");
        std::thread::sleep(Duration::from_millis(300));
        player.set_render_size(640, 360);
        next(640, 360);
        // mpv's screenshot of the decoded video works without frame bytes
        let shot = std::env::temp_dir().join(format!("streamtube-gl-shot-{}.png", std::process::id()));
        player.command_sync(&["screenshot-to-file", &shot.to_string_lossy(), "video"]).unwrap();
        let len = std::fs::metadata(&shot).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&shot);
        assert!(len > 1000, "screenshot written");
    }

    /// The "Auto" aspect mode on the GL path: the black bars of a letterboxed video
    /// are found in the surfaces themselves. Make one with
    /// `ffmpeg -f lavfi -i testsrc2=size=1920x804:duration=6 -vf pad=1920:1080:0:138:black -pix_fmt yuv420p letterbox.mp4`.
    #[test]
    #[ignore = "requires VideoToolbox, macOS libmpv and a 1920x1080 video letterboxed to 1920x804 in STREAMTUBE_MPV_TEST_LETTERBOX"]
    fn gl_path_finds_black_bars() {
        let path = std::env::var("STREAMTUBE_MPV_TEST_LETTERBOX").expect("set a path to a letterboxed video");
        let (tx, rx) = std::sync::mpsc::channel();
        let player = Mpv::new_gl(
            &[("vo", "libmpv"), ("ao", "null"), ("hwdec", "videotoolbox"), ("gpu-dumb-mode", "yes"), ("config", "no"), ("osc", "no")],
            move |event| {
                let _ = tx.send(event);
            },
        )
        .expect("GL path");
        player.set_render_size(1920, 1080);
        player.command_sync(&["loadfile", &path]).unwrap();
        let mut crop = crate::player::aspect::AutoCrop::default();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut frames = 0;
        while Instant::now() < deadline && frames < 10 {
            if let Ok(MpvEvent::Frame) = rx.recv_timeout(Duration::from_secs(5)) {
                if let Some(s) = player.take_frame().and_then(|f| f.surface) {
                    if s.size() != (1920, 1080) {
                        continue;
                    }
                    // the first frames can be all black, which tells nothing
                    let Some(bars) = s.detect_bars() else { continue };
                    assert!(bars.0 < 0.01, "no side bars: {bars:?}");
                    assert!((bars.1 - 138.0 / 1080.0).abs() < 0.005, "top/bottom bars: {bars:?}");
                    crop.feed(Some(bars));
                    frames += 1;
                }
            }
        }
        assert_eq!(frames, 10, "frames from the GL path");
        assert!((crop.trim.1 - 138.0 / 1080.0).abs() < 0.005, "steadied crop: {:?}", crop.trim);
    }

    fn render_local_video(hwdec: &str) -> Option<String> {
        let path = std::env::var("STREAMTUBE_MPV_TEST_VIDEO").expect("set a path to a local video");
        let (tx, rx) = std::sync::mpsc::channel();
        let player = Mpv::new(
            &[("vo", "libmpv"), ("ao", "null"), ("hwdec", hwdec), ("config", "no"), ("osc", "no")],
            move |event| { let _ = tx.send(event); },
        )
        .expect("load libmpv and create the software render context");
        player.set_render_size(320, 180);
        player.command_sync(&["loadfile", &path]).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(MpvEvent::Frame) => {
                    if let Some(frame) = player.take_frame() {
                        if frame.data.chunks_exact(4).any(|pixel| pixel[..3].iter().any(|&value| value > 32)) {
                            assert_eq!((frame.width, frame.height), (320, 180));
                            assert_eq!(frame.data.len(), 320 * 180 * 4);
                            let decoder = player.get_string("hwdec-current");
                            eprintln!("hwdec={hwdec}, hwdec-current={}", decoder.as_deref().unwrap_or("unavailable"));
                            return decoder;
                        }
                    }
                }
                Ok(MpvEvent::EndFile { error: Some(error), .. }) => panic!("mpv playback failed: {error}"),
                Ok(_) => {}
                Err(_) => panic!("mpv did not render a video frame"),
            }
        }
    }
}

/// Spawns the GL render thread: it owns an offscreen CGL context and the mpv render
/// context, renders each frame into an FBO, converts it to an IOSurface-backed 420f
/// pixel buffer and hands that over. Returns once the contexts exist (or failed).
#[cfg(target_os = "macos")]
fn spawn_gl_render<E: Fn(MpvEvent) + Send + Sync + 'static>(
    handle: Arc<Handle>,
    r: Arc<RenderShared>,
    frame_cb: Arc<E>,
) -> Result<(std::thread::JoinHandle<()>, String)> {
    let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<std::result::Result<String, String>>(1);
    let t = std::thread::Builder::new().name("mpv-render-gl".into()).spawn(move || unsafe {
        let (_, f) = lib().unwrap();
        let mut gl = match gl_macos::GlRenderer::new() {
            Ok(g) => g,
            Err(e) => {
                let _ = init_tx.send(Err(format!("OpenGL: {e}")));
                return;
            }
        };
        let api = c"opengl";
        let mut init = mpv_opengl_init_params { get_proc_address: Some(gl_macos::get_proc_address), get_proc_address_ctx: std::ptr::null_mut() };
        let mut params = [
            mpv_render_param { kind: RP_API_TYPE, data: api.as_ptr() as *mut c_void },
            mpv_render_param { kind: RP_OPENGL_INIT_PARAMS, data: &mut init as *mut _ as *mut c_void },
            mpv_render_param { kind: 0, data: std::ptr::null_mut() },
        ];
        let mut rctx: *mut mpv_render_context = std::ptr::null_mut();
        let rc = (f.rc_create)(&mut rctx, handle.0, params.as_mut_ptr());
        if rc < 0 {
            let _ = init_tx.send(Err(format!("GL render context: {}", err_str(rc))));
            return;
        }
        (f.rc_set_update_callback)(rctx, Some(on_render_update), Arc::as_ptr(&r) as *mut c_void);
        let _ = init_tx.send(Ok(gl.description.clone()));
        let mut warned = false;
        loop {
            {
                let mut w = r.wake.lock();
                while !*w && !r.stop.load(Ordering::Relaxed) {
                    r.cv.wait(&mut w);
                }
                *w = false;
            }
            if r.stop.load(Ordering::Relaxed) {
                break;
            }
            let flags = (f.rc_update)(rctx);
            let forced = r.force.swap(false, Ordering::Relaxed);
            if flags & RENDER_UPDATE_FRAME == 0 && !forced {
                continue;
            }
            // 4:2:0 needs even dimensions
            let (tw, th) = *r.target.lock();
            let (w, h) = ((tw.max(16) + 1) & !1, (th.max(16) + 1) & !1);
            let t0 = std::time::Instant::now();
            let (fbo, format) = match gl.target(w, h) {
                Ok(v) => v,
                Err(e) => {
                    if !warned {
                        warned = true;
                        eprintln!("[mpv-gl] {e}");
                    }
                    continue;
                }
            };
            let mut target = mpv_opengl_fbo { fbo: fbo as c_int, w: w as c_int, h: h as c_int, internal_format: format as c_int };
            let mut flip: c_int = 0;
            let mut no_block: c_int = 0;
            let mut params = [
                mpv_render_param { kind: RP_OPENGL_FBO, data: &mut target as *mut _ as *mut c_void },
                mpv_render_param { kind: RP_FLIP_Y, data: &mut flip as *mut c_int as *mut c_void },
                mpv_render_param { kind: RP_BLOCK_FOR_TARGET_TIME, data: &mut no_block as *mut c_int as *mut c_void },
                mpv_render_param { kind: 0, data: std::ptr::null_mut() },
            ];
            let mut info = RenderFrameInfo::default();
            let param = mpv_render_param { kind: RP_NEXT_FRAME_INFO, data: (&mut info as *mut RenderFrameInfo).cast() };
            let info_result = (f.rc_get_info)(rctx, param);
            let rc = (f.rc_render)(rctx, params.as_mut_ptr());
            if rc < 0 {
                continue;
            }
            let surface = match gl.convert() {
                Ok(s) => s,
                Err(e) => {
                    if !warned {
                        warned = true;
                        eprintln!("[mpv-gl] {e}");
                    }
                    continue;
                }
            };
            crate::perf::add(&crate::perf::MPV_RENDER_US, t0.elapsed().as_micros() as u64);
            crate::perf::add(&crate::perf::MPV_FRAMES, 1);
            // as on the software path: a redraw of the same picture keeps its serial
            let new_picture = if info_result >= 0 {
                info.flags & (FRAME_PRESENT | FRAME_REDRAW | FRAME_REPEAT) == FRAME_PRESENT
            } else {
                flags & RENDER_UPDATE_FRAME != 0
            };
            let picture_serial = if new_picture {
                r.presented_frames.fetch_add(1, Ordering::Relaxed) + 1
            } else {
                r.presented_frames.load(Ordering::Relaxed)
            };
            *r.frame.lock() = Some(VideoFrame { width: w, height: h, data: Vec::new(), picture_serial, surface: Some(surface) });
            frame_cb(MpvEvent::Frame);
        }
        // the GL context is still current on this thread, as mpv requires
        (f.rc_free)(rctx);
        drop(gl);
    })?;
    match init_rx.recv() {
        Ok(Ok(desc)) => Ok((t, desc)),
        Ok(Err(e)) => {
            let _ = t.join();
            Err(anyhow!(e))
        }
        Err(_) => {
            let _ = t.join();
            Err(anyhow!("GL render thread exited"))
        }
    }
}

fn err_str(rc: c_int) -> String {
    lib()
        .ok()
        .map(|(_, f)| unsafe { CStr::from_ptr((f.error_string)(rc)).to_string_lossy().into_owned() })
        .unwrap_or_else(|| rc.to_string())
}
