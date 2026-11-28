//! Player controller: owns the mpv instance, stream selection, SponsorBlock, queue and progress.
use super::mpv::{Mpv, MpvEvent, PropValue};
use crate::i18n::{t as translate, tf};
use crate::api::services::{self, Segment};
use crate::api::streams;
use crate::api::types::*;
use crate::store;
use crate::ui::{blocking, toast};
use gpui_kit::*;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct FrameProgress {
    presented: u64,
    position: f64,
    since: Instant,
    min_advance: f64,
}

impl FrameProgress {
    fn new(presented: u64, position: f64) -> Self {
        Self { presented, position, since: Instant::now(), min_advance: 3.0 }
    }
}

/// The picture on screen: BGRA bytes GPUI uploads as a texture (software path), or
/// on macOS's GL path an IOSurface-backed pixel buffer GPUI's `surface()` samples
/// directly, without the CPU touching pixels.
#[derive(Clone)]
pub enum VideoImage {
    Image(Arc<RenderImage>),
    #[cfg(target_os = "macos")]
    Surface(super::mpv::gl_macos::SurfaceFrame),
}

impl VideoImage {
    /// The frame's size in pixels, for laying it out by the aspect mode.
    pub fn size(&self) -> (f32, f32) {
        match self {
            VideoImage::Image(f) => {
                let d = f.size(0);
                (i32::from(d.width) as f32, i32::from(d.height) as f32)
            }
            #[cfg(target_os = "macos")]
            VideoImage::Surface(s) => {
                let (w, h) = s.size();
                (w as f32, h as f32)
            }
        }
    }
}

/// mpv's `hwdec` value. The GL path keeps decoded frames on the GPU; the software
/// path needs them copied back to memory.
fn hwdec_option(hwdec: bool, gl: bool) -> &'static str {
    match (hwdec, gl) {
        (false, _) => "no",
        (true, true) => "videotoolbox",
        (true, false) => "auto-copy",
    }
}

pub enum PlayerEvent {
    Ended,
    Loaded,
    PlaybackReadyChanged,
    /// average colour of the picture changed (ambient mode)
    Ambient,
    QueueChanged,
}

#[derive(Clone, Copy)]
pub enum KeyboardFeedback {
    Volume(f64),
    Seek(f64),
}

const SEEK_DISK_CACHE_LIMIT: u64 = 128 * 1024 * 1024;
const BACK_CACHE_SIZE: &str = "12MiB";

pub struct Player {
    /// speed to go back to while a press-and-hold plays at 2x
    pub boost: Option<f64>,
    mpv: Option<Arc<Mpv>>,
    engine_generation: u64,
    foreground: bool,
    in_pip: bool,
    pub init_error: Option<String>,
    pub video: Option<VideoItem>,
    pub streams: Option<Arc<StreamInfo>>,
    pub loading: bool,
    pub playback_ready: bool,
    pub error: Option<String>,
    pub quality: u32,
    pub audio_only: bool,
    pub time: f64,
    pub duration: f64,
    pub paused: bool,
    pub buffering: bool,
    pub seeking: bool,
    cache_time: f64,
    cache_ranges: Option<Vec<(f64, f64)>>,
    // End of contiguous data from the last seek, and the farthest buffered end to reconnect.
    seek_cache_target: Option<(f64, f64)>,
    seek_disk_bytes: u64,
    preloaded_until: f64,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    keyboard_feedback: Option<(KeyboardFeedback, Task<()>)>,
    pub eof: bool,
    pub frame: Option<VideoImage>,
    old_frames: Vec<Arc<RenderImage>>,
    pub video_size: (f64, f64),
    pub segments: Vec<Segment>,
    skipped: HashSet<String>,
    pub sb_prompt: Option<Segment>,
    muted_by_sb: bool,
    pub caption: Option<usize>,
    pub queue: Vec<VideoItem>,
    pub queue_pos: Option<usize>,
    pub(crate) autoplay_next: Option<VideoItem>,
    /// Videos played before the current one in this session, newest last,
    /// for "previous" when there is no queue to step back through.
    played_before: Vec<VideoItem>,
    /// The video "previous" is going back to, whose `play` isn't remembered.
    going_back: Option<String>,
    queue_serial: u64,
    queue_shuffle: bool,
    queue_scroll_y: Option<(u64, Pixels)>,
    pub chapters: Vec<Chapter>,
    pub heatmap: Vec<(f64, f64, f32)>,
    /// archived annotations of the current video
    pub annotations: Arc<Vec<crate::api::annotations::Annotation>>,
    /// average colour of recent frames, for the ambient glow
    pub ambient: Option<Rgba>,
    ambient_tick: u32,
    /// black bars found in recent frames, for the "Auto" aspect mode
    pub auto_crop: super::aspect::AutoCrop,
    pub loop_video: bool,
    load_serial: u64,
    last_progress_save: f64,
    history_report: Option<crate::api::web::PlaybackReport>,
    pub storyboard: Option<Storyboard>,
    last_prop_notify: std::time::Instant,
    fallback_tried: bool,
    /// times a live stream reached its end and was joined again (#264)
    live_rejoins: u32,
    frame_progress: Option<FrameProgress>,
    hold_picture: Option<u64>,
    playback_timing: Option<(&'static str, Instant)>,
    recovering_picture: bool,
    pub channel_id: Option<String>,
    pub last_activity: std::time::Instant,
    pub audio_track: Option<String>,
    /// "Stats for nerds" overlay
    pub show_stats: bool,
}

impl EventEmitter<PlayerEvent> for Player {}

pub struct PlayerHandle {
    active: Entity<Player>,
    tabs: Vec<WeakEntity<Player>>,
}
impl Global for PlayerHandle {}

pub fn player(cx: &App) -> Entity<Player> {
    cx.global::<PlayerHandle>().active.clone()
}

pub fn init(cx: &mut App) {
    let active = cx.new(Player::new);
    cx.set_global(PlayerHandle { tabs: vec![active.downgrade()], active });
}

pub(crate) fn new_tab_player(cx: &mut App) -> Entity<Player> {
    let player = cx.new(Player::new);
    let registry = cx.global_mut::<PlayerHandle>();
    registry.tabs.retain(|tab| tab.upgrade().is_some());
    registry.tabs.push(player.downgrade());
    player
}

pub(crate) fn set_active_player(player: Entity<Player>, cx: &mut App) {
    cx.global_mut::<PlayerHandle>().active = player;
}

pub(crate) fn tab_players(cx: &App) -> Vec<Entity<Player>> {
    cx.global::<PlayerHandle>().tabs.iter().filter_map(WeakEntity::upgrade).collect()
}

/// How many videos back "previous" can go.
const PLAYED_BEFORE_LIMIT: usize = 50;

pub fn enqueue(v: VideoItem, cx: &mut App) {
    player(cx).update(cx, |p, cx| {
        if p.video.is_none() {
            p.queue.push(v.clone());
            p.queue_pos = Some(p.queue.len() - 1);
            p.play(v, None, cx);
        } else {
            if p.queue.is_empty() {
                if let Some(cur) = p.video.clone() {
                    p.queue.push(cur);
                    p.queue_pos = Some(0);
                }
            }
            p.queue.push(v);
        }
        cx.notify();
    });
}

pub const EQ_PRESETS: &[(&str, &str, &str)] = &[
    ("flat", "Flat", ""),
    ("bass", "Bass boost", "equalizer=f=60:width_type=o:w=1.5:g=6,equalizer=f=150:width_type=o:w=1.5:g=4"),
    ("voice", "Voice", "equalizer=f=300:width_type=o:w=1.5:g=-3,equalizer=f=1500:width_type=o:w=2:g=4,equalizer=f=3500:width_type=o:w=1.5:g=3"),
    ("treble", "Treble boost", "equalizer=f=4000:width_type=o:w=1.5:g=4,equalizer=f=10000:width_type=o:w=1.5:g=6"),
    ("warm", "Warm", "equalizer=f=100:width_type=o:w=1.5:g=3,equalizer=f=8000:width_type=o:w=1.5:g=-3"),
];

pub fn eq_label(id: &str) -> &'static str {
    EQ_PRESETS.iter().find(|&&(k, _, _)| k == id).map(|&(_, l, _)| l).unwrap_or("Flat")
}

fn eq_filter(id: &str) -> &'static str {
    EQ_PRESETS.iter().find(|&&(k, _, _)| k == id).map(|&(_, _, f)| f).unwrap_or("")
}

impl Player {
    fn new(cx: &mut Context<Self>) -> Self {
        let s = store::settings_snapshot();
        Self {
            mpv: None,
            engine_generation: 0,
            foreground: true,
            in_pip: false,
            init_error: None,
            video: None,
            streams: None,
            loading: false,
            playback_ready: false,
            error: None,
            quality: s.default_quality,
            audio_only: s.audio_only,
            time: 0.0,
            duration: 0.0,
            paused: true,
            buffering: false,
            seeking: false,
            cache_time: 0.0,
            cache_ranges: None,
            seek_cache_target: None,
            seek_disk_bytes: 0,
            preloaded_until: 0.0,
            volume: s.volume,
            muted: false,
            speed: s.default_speed,
            boost: None,
            keyboard_feedback: None,
            eof: false,
            frame: None,
            old_frames: Vec::new(),
            video_size: (16.0, 9.0),
            segments: Vec::new(),
            skipped: HashSet::new(),
            sb_prompt: None,
            muted_by_sb: false,
            caption: None,
            queue: Vec::new(),
            queue_pos: None,
            autoplay_next: None,
            played_before: Vec::new(),
            going_back: None,
            queue_serial: 0,
            queue_shuffle: false,
            queue_scroll_y: None,
            chapters: Vec::new(),
            heatmap: Vec::new(),
            annotations: Arc::new(Vec::new()),
            ambient: None,
            ambient_tick: 0,
            auto_crop: Default::default(),
            loop_video: s.loop_video,
            load_serial: 0,
            history_report: None,
            last_progress_save: 0.0,
            storyboard: None,
            last_prop_notify: std::time::Instant::now(),
            fallback_tried: false,
            live_rejoins: 0,
            frame_progress: None,
            hold_picture: None,
            playback_timing: None,
            recovering_picture: false,
            channel_id: None,
            last_activity: std::time::Instant::now(),
            audio_track: None,
            show_stats: false,
        }
        .with_cx(cx)
    }

    fn with_cx(self, _cx: &mut Context<Self>) -> Self {
        self
    }

    /// True while mpv is actually progressing through a video.
    fn should_inhibit_idle(&self) -> bool {
        self.mpv.is_some()
            && self.video.is_some()
            && !self.loading
            && !self.paused
            && !self.eof
            && self.error.is_none()
    }

    fn sync_idle(&self, cx: &mut App) {
        if self.in_pip || self.foreground && crate::ui::pip::handle(cx).is_none() {
            crate::idle_inhibit::set_playing(cx, self.should_inhibit_idle());
        }
    }

    pub(crate) fn set_foreground(&mut self, active: bool, cx: &mut Context<Self>) {
        self.foreground = active;
        self.update_video_output(cx);
    }

    pub(crate) fn set_in_pip(&mut self, in_pip: bool, cx: &mut Context<Self>) {
        self.in_pip = in_pip;
        self.update_video_output(cx);
    }

    pub(crate) fn refresh_idle_guard(&self, cx: &mut App) {
        self.sync_idle(cx);
    }

    pub(crate) fn has_engine(&self) -> bool {
        self.mpv.is_some()
    }

    fn update_video_output(&mut self, cx: &mut Context<Self>) {
        let visible = self.foreground || self.in_pip;
        if let Some(mpv) = &self.mpv {
            mpv.set("vid", if visible { "auto" } else { "no" });
        }
        if !visible {
            self.clear_frame(cx);
            for frame in self.old_frames.drain(..) {
                cx.drop_image(frame, None);
            }
            self.frame_progress = None;
        }
        self.sync_idle(cx);
        cx.notify();
    }

    pub(crate) fn release_tab_engine(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.engine_generation += 1;
        self.mpv = None;
    }

    fn ensure_mpv(&mut self, cx: &mut Context<Self>) -> Option<Arc<Mpv>> {
        if let Some(m) = &self.mpv {
            return Some(m.clone());
        }
        let s = store::settings_snapshot();
        let (tx, rx) = futures::channel::mpsc::unbounded::<MpvEvent>();
        let vol = format!("{}", s.volume.round());
        let ua = crate::api::http::BROWSER_UA;
        // macOS with hardware decoding: GPU decode -> GPU render -> GPUI surface, no
        // pixel copies (STREAMTUBE_NO_GL=1 forces the software path, e.g. to compare)
        let use_gl = cfg!(target_os = "macos") && s.hwdec && std::env::var_os("STREAMTUBE_NO_GL").is_none();
        let hw = hwdec_option(s.hwdec, false);
        let opts: Vec<(&str, &str)> = vec![
            ("vo", "libmpv"),
            ("vid", if self.foreground || self.in_pip { "auto" } else { "no" }),
            ("hwdec", hw),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("ytdl", "no"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            ("osc", "no"),
            ("cache", "yes"),
            ("demuxer-max-bytes", "48MiB"),
            ("demuxer-max-back-bytes", BACK_CACHE_SIZE),
            ("demuxer-readahead-secs", "40"),
            ("network-timeout", "20"),
            ("user-agent", ua),
            ("volume", &vol),
            ("volume-max", "150"),
            ("hr-seek", "yes"),
            ("force-seekable", "yes"),
            ("audio-client-name", "Stream Tube"),
            ("af", eq_filter(&s.eq_preset)),
            ("sub-font-size", "42"),
            ("sub-border-size", "2.5"),
            ("sub-auto", "no"),
            ("osd-level", "0"),
            // software render path: convert colors only, never scale on the CPU
            ("zimg-fast", "yes"),
            ("zimg-scaler", "bilinear"),
            ("zimg-dither", "no"),
            ("zimg-threads", "auto"),
            ("sws-scaler", "fast-bilinear"),
            // libswscale's integer SIMD path converts yuv420p to bgr0 in about a quarter
            // of the CPU time zimg's float pipeline needs (10.7 -> 2.9 ms a frame at
            // 1080p60 on an M-series Mac), with the same colours
            ("sws-allow-zimg", "no"),
            ("sws-fast", "yes"),
            ("video-sync", "audio"),
            ("framedrop", "vo"),
        ];
        let opts: Vec<(&str, &str)> = opts.into_iter().collect();
        let proxy = s.proxy.clone();
        let mut opts_owned: Vec<(String, String)> = opts.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        // Intel Iris Plus hangs inside VideoToolbox's VP9 decoder on frame-threaded
        // YouTube VP9 streams; keep VideoToolbox for H.264/HEVC and decode VP9/AV1 on CPU
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        opts_owned.push(("hwdec-codecs".into(), "h264,hevc".into()));
        // Embedded libmpv can have no default cache directory.
        let cache_dir = store::data_dir().join("playback-cache");
        let cache_dir = match std::fs::create_dir_all(&cache_dir) {
            Ok(()) => cache_dir,
            Err(e) => {
                eprintln!("[player] Could not create playback cache directory: {e}");
                std::env::temp_dir()
            }
        };
        opts_owned.push(("demuxer-cache-dir".into(), cache_dir.to_string_lossy().into_owned()));
        // normal videos reach mpv through the app's local stream proxy, which already
        // uses the user's proxy; mpv itself only fetches live streams directly, and can
        // only use HTTP proxies (127.0.0.1 is excluded through no_proxy, see main)
        if crate::store::mpv_http_proxy(&proxy) {
            opts_owned.push(("http-proxy".into(), proxy));
        }
        if let Ok(f) = std::env::var("STREAMTUBE_MPV_LOG") {
            opts_owned.push(("log-file".into(), f));
        }
        let refs: Vec<(&str, &str)> = opts_owned.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let sink = |tx: futures::channel::mpsc::UnboundedSender<MpvEvent>| {
            move |ev| {
                let _ = tx.unbounded_send(ev);
            }
        };
        #[cfg(target_os = "macos")]
        let gl_player = if use_gl {
            // gpu-dumb-mode: one shader pass straight to the target; mpv's full GL
            // pipeline costs about 1 ms more CPU a frame through Apple's GL driver
            let mut gl_opts: Vec<(&str, &str)> = refs.iter().filter(|(k, _)| *k != "hwdec").copied().collect();
            gl_opts.push(("hwdec", hwdec_option(true, true)));
            gl_opts.push(("gpu-dumb-mode", "yes"));
            match Mpv::new_gl(&gl_opts, sink(tx.clone())) {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("[player] GPU video path unavailable ({e}), using the software path");
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(target_os = "macos"))]
        let gl_player: Option<Arc<Mpv>> = {
            let _ = use_gl;
            None
        };
        let created = match gl_player {
            Some(m) => Ok(m),
            None => Mpv::new(&refs, sink(tx)),
        };
        match created {
            Ok(m) => {
                self.mpv = Some(m.clone());
                self.engine_generation += 1;
                let generation = self.engine_generation;
                cx.spawn(async move |this, cx| {
                    use futures::StreamExt;
                    let mut rx = rx;
                    while let Some(ev) = rx.next().await {
                        // coalesce queued events
                        let mut batch = vec![ev];
                        while let Ok(e) = rx.try_recv() {
                            batch.push(e);
                        }
                        let ok = this
                            .update(cx, |p, cx| {
                                if p.engine_generation != generation { return false; }
                                for e in batch {
                                    p.handle_event(e, cx);
                                }
                                true
                            })
                            .unwrap_or(false);
                        if !ok {
                            break;
                        }
                    }
                })
                .detach();
                Some(m)
            }
            Err(e) => {
                let error = e.to_string();
                self.init_error = Some(match error.as_str() {
                    #[cfg(windows)]
                    super::mpv::MISSING_WINDOWS_MPV => translate(super::mpv::MISSING_WINDOWS_MPV).to_owned(),
                    _ => tf!("Could not start the video engine (libmpv): {0}", error),
                });
                None
            }
        }
    }

    /// Drops the picture on screen (and its texture on the software path).
    fn clear_frame(&mut self, cx: &mut Context<Self>) {
        if let Some(VideoImage::Image(old)) = self.frame.take() {
            cx.drop_image(old, None);
        }
    }

    fn set_ambient(&mut self, c: Rgba, cx: &mut Context<Self>) {
        let changed = self.ambient.map(|o| (o.r - c.r).abs() + (o.g - c.g).abs() + (o.b - c.b).abs() > 0.04).unwrap_or(true);
        if changed {
            self.ambient = Some(c);
            cx.emit(PlayerEvent::Ambient);
        }
    }

    pub fn mpv(&self) -> Option<Arc<Mpv>> {
        self.mpv.clone()
    }

    fn handle_event(&mut self, ev: MpvEvent, cx: &mut Context<Self>) {
        let tracked = matches!(
            &ev,
            MpvEvent::FileLoaded
                | MpvEvent::PlaybackRestart
                | MpvEvent::EndFile { .. }
                | MpvEvent::Prop(..)
                | MpvEvent::Shutdown
        );
        match ev {
            MpvEvent::Frame => {
                if !self.foreground && !self.in_pip {
                    if let Some(mpv) = &self.mpv { let _ = mpv.take_frame(); }
                    return;
                }
                let t0 = std::time::Instant::now();
                crate::perf::add(&crate::perf::UI_FRAMES, 1);
                let _g = scopeguard(move || crate::perf::add(&crate::perf::UI_HANDLE_US, t0.elapsed().as_micros() as u64));
                if let Some(m) = self.mpv.clone() {
                    if let Some(f) = m.take_frame() {
                        // A seek or reload can redraw an empty surface before
                        // the first decoded picture at the new position arrives.
                        if self.hold_picture == Some(f.picture_serial) {
                            return;
                        }
                        self.hold_picture = None;
                        self.ambient_tick += 1;
                        let ambient = self.ambient_tick % 12 == 1 && store::settings_snapshot().ambient_mode;
                        // a few looks a second are enough to follow the bars
                        let find_bars = self.ambient_tick % 10 == 0 && store::settings_snapshot().aspect_mode == "auto";
                        #[cfg(target_os = "macos")]
                        if let Some(s) = f.surface {
                            // GL path: GPUI draws the pixel buffer itself; the ambient colour
                            // and the black bars are read straight from the IOSurface
                            if ambient {
                                if let Some(c) = s.sample_grid(16, 12).and_then(|px| average_rgb(&px)) {
                                    self.set_ambient(c, cx);
                                }
                            }
                            if find_bars {
                                self.auto_crop.feed(s.detect_bars());
                            }
                            if let Some(VideoImage::Image(old)) = self.frame.replace(VideoImage::Surface(s)) {
                                cx.drop_image(old, None);
                            }
                            cx.notify();
                            return;
                        }
                        if ambient {
                            if let Some(c) = average_color(&f.data, f.width, f.height) {
                                self.set_ambient(c, cx);
                            }
                        }
                        if find_bars {
                            self.auto_crop.feed(super::aspect::detect_bars(&f.data, f.width, f.height));
                        }
                        if let Some(buf) = image::RgbaImage::from_raw(f.width, f.height, f.data) {
                            let img = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(buf)]));
                            if let Some(VideoImage::Image(old)) = self.frame.replace(VideoImage::Image(img)) {
                                self.old_frames.push(old);
                            }
                            // drop textures of frames that are no longer displayed
                            while self.old_frames.len() > 1 {
                                let o = self.old_frames.remove(0);
                                cx.drop_image(o, None);
                            }
                            cx.notify();
                        }
                    }
                }
            }
            MpvEvent::FileLoaded => {
                if self.video.as_ref().is_some_and(|v| v.is_short) {
                    self.cache_time = 0.0;
                }
                self.loading = false;
                self.eof = false;
                self.apply_caption();
                cx.emit(PlayerEvent::Loaded);
                cx.notify();
            }
            MpvEvent::PlaybackRestart => {
                self.frame_progress = None;
                if !self.loading && !self.buffering && self.error.is_none() && self.streams.is_some() {
                    if let Some((action, started)) = self.playback_timing.take() {
                        eprintln!("[player] {action} to playback restart: {} ms", started.elapsed().as_millis());
                    }
                    self.set_playback_ready(true, cx);
                }
                self.seeking = false;
                cx.notify();
            }
            MpvEvent::EndFile { eof, error } => {
                if let Some(e) = error {
                    if self.video.is_some() {
                        if self.loading && !self.fallback_tried {
                            self.fallback_to_ytdlp(cx);
                        } else {
                            self.loading = false;
                            self.error = Some(tf!("Playback error: {0}", e));
                        }
                    }
                }
                if eof {
                    self.on_ended(cx);
                }
                cx.notify();
            }
            MpvEvent::Prop(name, val) => {
                // While frames are flowing, the frame event already redraws the player;
                // only redraw for clock/cache updates when no video frames arrive.
                let quiet = matches!(name, "time-pos" | "demuxer-cache-time" | "demuxer-cache-state")
                    && !self.paused
                    && !self.audio_only
                    && self.frame.is_some()
                    && self.last_prop_notify.elapsed() < std::time::Duration::from_millis(500);
                match (name, val) {
                    ("time-pos", PropValue::Double(t)) => {
                        self.time = t;
                        if t > 0. && self.error.is_none() && !self.loading && !self.buffering && !self.paused && !self.playback_ready {
                            self.set_playback_ready(true, cx);
                        }
                        if !self.recover_stalled_picture(cx) {
                            self.on_time(cx);
                        }
                    }
                    ("duration", PropValue::Double(d)) => self.duration = d,
                    ("pause", PropValue::Flag(b)) => {
                        self.update_paused(b);
                        self.frame_progress = None;
                    }
                    ("eof-reached", PropValue::Flag(b)) => {
                        if b {
                            self.on_ended(cx);
                        } else {
                            self.eof = false;
                        }
                    }
                    ("paused-for-cache", PropValue::Flag(b)) => {
                        self.buffering = b;
                        if b {
                            self.frame_progress = None;
                            self.set_playback_ready(false, cx);
                        }
                    }
                    ("seeking", PropValue::Flag(b)) => {
                        self.seeking = b;
                        self.frame_progress = None;
                    }
                    ("demuxer-cache-time", PropValue::Double(c)) => self.cache_time = c,
                    ("demuxer-cache-state", PropValue::Cache { ranges, disk_bytes }) if !self.loading && self.streams.is_some() => {
                        self.seek_disk_bytes = self.seek_disk_bytes.max(disk_bytes);
                        if let Some((from, end)) = self.seek_cache_target {
                            // Follow only the range that actually covers our last seek or
                            // refill point. A separate range ahead may itself be shrinking.
                            let filled = ranges.iter().filter(|&&(a, b)| a <= from && from <= b)
                                .map(|&(_, b)| b).fold(from, f64::max);
                            let target = (filled < end && self.seek_disk_bytes < SEEK_DISK_CACHE_LIMIT)
                                .then_some((filled, end));
                            self.set_seek_cache_target(target);
                        }
                        self.cache_ranges = Some(ranges);
                    }
                    ("speed", PropValue::Double(s)) => self.speed = s,
                    ("volume", PropValue::Double(v)) => self.volume = v,
                    ("mute", PropValue::Flag(m)) => self.muted = m,
                    ("dwidth", PropValue::Double(w)) => self.video_size.0 = w,
                    ("dheight", PropValue::Double(h)) => self.video_size.1 = h,
                    _ => {}
                }
                if !quiet {
                    self.last_prop_notify = std::time::Instant::now();
                    cx.notify();
                }
            }
            MpvEvent::Shutdown => {}
        }
        if tracked {
            self.sync_idle(cx);
        }
    }

    fn on_ended(&mut self, cx: &mut Context<Self>) {
        let Some(video) = &self.video else { return };
        if self.loading || self.error.is_some() || self.eof {
            return;
        }
        let live = video.is_live || self.streams.as_ref().is_some_and(|info| info.is_live);
        let position = self.mpv.as_ref().and_then(|mpv| mpv.get_f64("time-pos")).unwrap_or(self.time);
        if live && self.live_rejoins < 5 && !self.loop_video {
            // A live HLS playlist can stop refreshing mid-stream (the URL
            // expired, a reload failed) and mpv reports the end. Ask YouTube
            // again: still live means join it again, ended means play on.
            self.live_rejoins += 1;
            self.rejoin_live(cx);
            return;
        }
        if !live && premature_eof(position, self.duration) {
            self.time = position;
            self.error = Some("Playback stopped before the end of the video.".into());
            self.save_progress(true);
            return;
        }
        self.eof = true;
        self.time = position;
        if self.loop_video {
            self.eof = false;
            self.save_progress(true);
            if let Some(report) = &self.history_report {
                report.progress(self.time, true, true);
            }
            self.seek_to(0.0);
            self.set_paused(false, cx);
            return;
        }
        self.finish_playback_history();
        if let Some(pos) = self.queue_pos {
            if pos + 1 < self.queue.len() {
                self.play_queue_index(pos + 1, cx);
                return;
            }
        }
        cx.emit(PlayerEvent::Ended);
        self.sync_idle(cx);
    }

    pub fn play_queue_index(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(v) = self.queue.get(idx).cloned() {
            self.queue_pos = Some(idx);
            let id = v.id.clone();
            self.play(v, None, cx);
            if self.foreground {
                let source = cx.entity_id();
                cx.defer(move |cx| {
                    if player(cx).entity_id() == source {
                        crate::ui::app::sync_watch_route(id, cx);
                    }
                });
            }
        }
    }

    /// Replaces the queue with playlist items and keeps draining any remaining
    /// continuation pages in the background so playback starts immediately.
    pub fn set_playlist_queue(
        &mut self,
        videos: Vec<VideoItem>,
        start_idx: usize,
        continuation: Option<String>,
        authed: bool,
        shuffle: bool,
        cx: &mut Context<Self>,
    ) {
        self.queue_serial += 1;
        let serial = self.queue_serial;
        self.queue = videos;
        self.queue_pos = Some(start_idx);
        self.queue_shuffle = shuffle;
        self.queue_scroll_y = None;
        cx.emit(PlayerEvent::QueueChanged);
        cx.notify();
        let Some(first) = continuation.filter(|t| !t.is_empty()) else { return };
        cx.spawn(async move |this, cx| {
            let mut pager = crate::api::innertube::PlaylistPager::new(Some(first), crate::api::innertube::MAX_PLAYLIST_PAGES);
            while let Some(tok) = pager.take_next() {
                let Ok((vids, next)) = blocking(move || crate::api::innertube::playlist_continue(&tok, authed)).await else { break };
                pager.advance(next);
                let keep_going = this
                    .update(cx, |p, cx| {
                        if p.queue_serial != serial {
                            return false;
                        }
                        if extend_queue(&mut p.queue, p.queue_pos, vids, p.queue_shuffle, &mut rand::rng()) > 0 {
                            cx.emit(PlayerEvent::QueueChanged);
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Remembers the video being left for `next`, unless "previous" is going back.
    fn remember_played(&mut self, next: &VideoItem) {
        let going_back = self.going_back.take().is_some_and(|id| id == next.id);
        let Some(current) = self.video.clone() else { return };
        // Shorts are swiped through, not watched one after another
        if going_back || current.id == next.id || current.is_short || next.is_short {
            return;
        }
        self.played_before.retain(|v| v.id != current.id);
        self.played_before.push(current);
        let excess = self.played_before.len().saturating_sub(PLAYED_BEFORE_LIMIT);
        self.played_before.drain(..excess);
    }

    /// The video "previous" goes back to; the `play` that follows isn't remembered.
    pub(crate) fn take_played_before(&mut self) -> Option<VideoItem> {
        let v = self.played_before.pop()?;
        self.going_back = Some(v.id.clone());
        Some(v)
    }

    pub fn replace_queue(&mut self, queue: Vec<VideoItem>, pos: Option<usize>) {
        self.queue_serial += 1;
        self.queue = queue;
        self.queue_pos = pos;
        self.queue_shuffle = false;
        self.queue_scroll_y = None;
    }

    pub fn reset_queue(&mut self) {
        self.replace_queue(Vec::new(), None);
    }

    pub fn on_queue_replaced(&mut self) {
        self.queue_serial += 1;
        self.queue_shuffle = false;
        self.queue_scroll_y = None;
    }

    pub fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.reset_queue();
        cx.emit(PlayerEvent::QueueChanged);
        cx.notify();
    }

    pub fn shuffle_queue(&mut self, cx: &mut Context<Self>) {
        use rand::seq::SliceRandom;
        let cur = self.queue_pos.and_then(|i| self.queue.get(i).cloned());
        self.queue.shuffle(&mut rand::rng());
        if let Some(c) = cur {
            self.queue_pos = self.queue.iter().position(|v| v.id == c.id);
        }
        self.queue_shuffle = true;
        self.queue_scroll_y = None;
        cx.emit(PlayerEvent::QueueChanged);
        cx.notify();
    }

    pub fn save_queue_scroll(&mut self, y: Pixels) {
        self.queue_scroll_y = Some((self.queue_serial, y));
    }

    pub fn take_queue_scroll(&mut self) -> Option<Pixels> {
        let (serial, y) = self.queue_scroll_y.take()?;
        (serial == self.queue_serial).then_some(y)
    }

    /// A failed video decoder can leave mpv playing the audio indefinitely
    /// without an EndFile event. Compare rendered pictures with the media clock.
    fn recover_stalled_picture(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(mpv), Some(info)) = (&self.mpv, &self.streams) else { return false };
        if !(self.foreground || self.in_pip) || self.audio_only || self.loading || self.paused || self.buffering || self.seeking || self.eof || self.error.is_some()
            || info.is_live || (!info.hls_only && info.video.is_empty() && info.muxed.is_empty())
            || (self.duration > 0.0 && self.duration - self.time < 2.0)
        {
            self.frame_progress = None;
            return false;
        }
        let presented = mpv.presented_frames();
        let progress = self.frame_progress.get_or_insert_with(|| FrameProgress::new(presented, self.time));
        if progress.presented != presented || self.time < progress.position {
            *progress = FrameProgress::new(presented, self.time);
            return false;
        }
        let advanced = self.time - progress.position;
        if progress.since.elapsed() < Duration::from_secs(5) || advanced < progress.min_advance {
            return false;
        }
        // Still-image tracks and very low frame rates have legitimate gaps.
        let fps = mpv.get_f64("container-fps").unwrap_or(0.0);
        if fps > 0.0 && advanced < 2.0 / fps {
            progress.min_advance = 2.0 / fps;
            return false;
        }
        if ["current-tracks/video/image", "current-tracks/video/albumart"].iter().any(|p| mpv.get_string(p).as_deref() == Some("yes")) {
            self.frame_progress = None;
            return false;
        }
        self.frame_progress = None;
        if self.recovering_picture {
            self.set_paused(true, cx);
            self.set_playback_ready(false, cx);
            self.error = Some(translate("The video decoder stopped producing frames.").into());
            cx.notify();
        } else {
            self.recovering_picture = true;
            eprintln!("[player] Video frames stalled while playback advanced. Reopening at {:.1}s with software decoding.", self.time);
            self.load_current(Some(self.time), true, cx);
        }
        true
    }

    fn on_time(&mut self, cx: &mut Context<Self>) {
        let t = self.time;
        if !self.loading && !self.paused && !self.buffering && !self.eof {
            if let Some(report) = &mut self.history_report {
                report.start(t, self.duration);
            }
        }
        // progress save
        if !self.loading && !self.eof && (t - self.last_progress_save).abs() >= 5.0 {
            self.save_progress(false);
            if let Some(report) = &self.history_report {
                report.progress(t, !self.paused, false);
            }
            let id = self.video.as_ref().map(|v| v.id.clone());
            if let Some(id) = id {
                cx.default_global::<crate::ui::cards::Watched>().0.insert(id, (t, self.duration));
            }
        }
        // SponsorBlock
        let s = store::settings_snapshot();
        if !s.sponsorblock || self.segments.is_empty() {
            return;
        }
        let mut prompt = None;
        let mut in_mute = false;
        for seg in self.segments.clone() {
            let action = s.sb_actions.get(&seg.category).map(String::as_str).unwrap_or("off");
            if action == "off" || seg.action == "poi" || seg.action == "full" {
                continue;
            }
            let inside = t >= seg.start && t < seg.end - 0.3;
            if !inside {
                continue;
            }
            if seg.action == "mute" {
                in_mute = true;
                continue;
            }
            if seg.end - seg.start < s.sb_min_duration {
                continue;
            }
            match action {
                "skip" if !self.skipped.contains(&seg.uuid) => {
                    self.skipped.insert(seg.uuid.clone());
                    let back_to = seg.start;
                    self.seek_to(seg.end);
                    let server = s.sponsorblock_server.clone();
                    let uuid = seg.uuid.clone();
                    std::thread::spawn(move || services::sponsor_viewed(&server, &uuid));
                    if s.sb_notify {
                        let weak = cx.entity().downgrade();
                        let mut actions: Vec<(SharedString, Arc<dyn Fn(&mut Window, &mut App)>)> = vec![(
                            translate("Unskip").into(),
                            Arc::new(move |_, cx: &mut App| {
                                if let Some(p) = weak.upgrade() {
                                    p.update(cx, |p, _| p.seek_to(back_to));
                                }
                            }),
                        )];
                        if !s.sb_user_id.is_empty() {
                            for (label, up) in [("Upvote", true), ("Downvote", false)] {
                                let (server, uuid, user) = (s.sponsorblock_server.clone(), seg.uuid.clone(), s.sb_user_id.clone());
                                actions.push((
                                    translate(label).into(),
                                    Arc::new(move |_, cx: &mut App| {
                                        let (server, uuid, user) = (server.clone(), uuid.clone(), user.clone());
                                        cx.spawn(async move |cx| {
                                            let res = crate::ui::blocking(move || services::sponsor_vote(&server, &uuid, &user, up)).await;
                                            let _ = cx.update(|cx| match res {
                                                Ok(()) => toast(cx, translate("Thanks for voting")),
                                                Err(e) => toast(cx, tf!("Vote failed: {0}", e)),
                                            });
                                        })
                                        .detach();
                                    }),
                                ));
                            }
                        }
                        crate::ui::toast_with_actions(cx, tf!("Skipped {0}", translate(services::sponsor_category_name(&seg.category))), actions);
                    }
                }
                "ask" => prompt = Some(seg.clone()),
                _ => {}
            }
        }
        if in_mute != self.muted_by_sb {
            self.muted_by_sb = in_mute;
            if let Some(m) = &self.mpv {
                m.set_flag("mute", in_mute || self.muted);
            }
        }
        self.sb_prompt = prompt;
    }

    pub(crate) fn save_progress(&mut self, force: bool) {
        if let Some(v) = &self.video {
            if self.time > 0.0 || (force && (self.playback_ready || self.last_progress_save > 0.0 || self.eof)) {
                // the periodic save goes to a background thread; a forced one (switching
                // videos, the end, quitting) is written before going on
                store::history_progress_queued(&v.id, self.time, self.duration, force);
                self.last_progress_save = self.time;
            }
        }
    }

    pub fn finish_playback_history(&mut self) {
        let report = self.history_report.take();
        if self.updated_position_while_paused().is_some() {
            // another device moved on while this one was paused: keep its position
            if let Some(report) = report {
                report.abandon();
            }
            return;
        }
        if !self.loading {
            self.save_progress(true);
        }
        if let Some(report) = report {
            report.finish(self.time);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_history_report(&mut self, report: crate::api::web::PlaybackReport) {
        self.history_report = Some(report);
    }

    /// Starts playing a video. Fetches streams, SponsorBlock segments and resumes progress.
    fn set_playback_ready(&mut self, ready: bool, cx: &mut Context<Self>) {
        if self.playback_ready != ready {
            self.playback_ready = ready;
            if !ready && self.foreground {
                super::shorts_preload::suspend();
            }
            cx.emit(PlayerEvent::PlaybackReadyChanged);
        }
    }

    pub fn play(&mut self, video: VideoItem, start: Option<f64>, cx: &mut Context<Self>) {
        // a video picked while the app is quitting would start a report no one waits for
        if cx.try_global::<crate::QuitRequested>().is_some() {
            return;
        }
        self.remember_played(&video);
        self.autoplay_next = None;
        self.playback_timing = std::env::var_os("STREAMTUBE_PLAYBACK_TIMING").map(|_| ("open", Instant::now()));
        self.keyboard_feedback = None;
        self.preloaded_until = 0.0;
        self.set_playback_ready(false, cx);
        self.save_progress(true);
        self.finish_playback_history();
        let Some(mpv) = self.ensure_mpv(cx) else {
            self.video = Some(video);
            self.sync_idle(cx);
            cx.notify();
            return;
        };
        self.load_serial += 1;
        self.fallback_tried = false;
        self.live_rejoins = 0;
        self.recovering_picture = false;
        self.frame_progress = None;
        let serial = self.load_serial;
        self.video = Some(video.clone());
        self.loading = true;
        self.error = None;
        self.time = 0.0;
        self.last_progress_save = 0.0;
        self.duration = video.duration_secs.unwrap_or(0) as f64;
        self.eof = false;
        self.segments.clear();
        self.skipped.clear();
        self.sb_prompt = None;
        self.streams = None;
        self.storyboard = None;
        self.chapters.clear();
        self.heatmap.clear();
        self.annotations = Arc::new(Vec::new());
        self.ambient = None;
        self.auto_crop = Default::default();
        self.channel_id = video.channel_id.clone();
        self.audio_track = None;
        self.clear_frame(cx);
        let _ = mpv.command(&["stop"]);
        store::history_add(&video);
        self.history_report = crate::api::web::PlaybackReport::new(video.id.clone(), serial);
        self.sync_idle(cx);
        let s = store::settings_snapshot();
        let id = video.id.clone();
        let refresh = if s.resume_playback && start.is_none() && !video.is_short {
            crate::ui::history::refresh_for_playback(cx)
        } else {
            None
        };
        cx.notify();
        if s.annotations {
            self.load_annotations(cx);
        }
        if s.sponsorblock {
            let segment_id = id.clone();
            cx.spawn(async move |this, cx| {
                let segments = blocking(move || {
                    let categories: Vec<String> = s
                        .sb_actions
                        .iter()
                        .filter(|(_, action)| action.as_str() != "off")
                        .map(|(category, _)| category.clone())
                        .collect();
                    services::sponsor_segments(&segment_id, &s.sponsorblock_server, &categories)
                        .unwrap_or_default()
                })
                .await;
                let _ = this.update(cx, |p, cx| {
                    if p.load_serial == serial {
                        p.segments = segments;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.spawn(async move |this, cx| {
            let streams_id = id.clone();
            let res = blocking(move || streams::get_streams(&streams_id)).await;
            if res.is_ok() {
                if let Some(refresh) = refresh {
                    // Use fresher cross-device progress if it arrives shortly.
                    // A slow history endpoint must not hold up playback.
                    let limit = cx.background_executor().timer(std::time::Duration::from_millis(1200));
                    let _ = futures::future::select(refresh, Box::pin(limit)).await;
                }
            }
            let _ = this.update(cx, |p, cx| {
                if p.load_serial != serial {
                    return;
                }
                match res {
                    Ok(info) => {
                        let info = Arc::new(info);
                        if p.duration <= 0.0 {
                            p.duration = info.length_secs as f64;
                        }
                        if let Some(v) = p.video.as_mut() {
                            if v.channel_id.is_none() && !info.channel_id.is_empty() {
                                v.channel_id = Some(info.channel_id.clone());
                            }
                            if v.title.is_empty() {
                                v.title = info.title.clone();
                            }
                            if v.channel_name.is_empty() {
                                v.channel_name = info.author.clone();
                            }
                        }
                        if p.channel_id.is_none() && !info.channel_id.is_empty() {
                            p.channel_id = Some(info.channel_id.clone());
                        }
                        p.storyboard = info.storyboard.clone();
                        p.streams = Some(info);
                        // default caption
                        let s = store::settings_snapshot();
                        let resume = start.or_else(|| s.resume_playback.then(|| store::resume_position(&id)).flatten());
                        p.caption = if s.captions_enabled {
                            p.streams.as_ref().and_then(|i| pick_caption(i, &s.captions_lang))
                        } else {
                            None
                        };
                        // speed per channel
                        let speed = p
                            .channel_id
                            .as_deref()
                            .filter(|_| s.remember_speed_per_channel)
                            .and_then(store::channel_speed)
                            .unwrap_or(s.default_speed);
                        p.speed = speed;
                        p.load_current(resume, false, cx);
                    }
                    Err(e) => {
                        p.loading = false;
                        p.error = Some(e.to_string());
                    }
                }
                p.sync_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches the current video's archived annotations (most videos have none).
    pub fn load_annotations(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.video.as_ref().map(|v| v.id.clone()) else { return };
        let serial = self.load_serial;
        cx.spawn(async move |this, cx| {
            let list = blocking(move || crate::api::annotations::fetch(&id).unwrap_or_default()).await;
            let _ = this.update(cx, |p, cx| {
                if p.load_serial == serial && !list.is_empty() {
                    p.annotations = Arc::new(list);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A live stream stopped: resolve it again and keep playing if it is still on air.
    fn rejoin_live(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.video.as_ref().map(|v| v.id.clone()) else { return };
        streams::invalidate(&id);
        let serial = self.load_serial;
        self.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = blocking(move || streams::get_streams(&id)).await;
            let _ = this.update(cx, |p, cx| {
                if p.load_serial != serial {
                    return;
                }
                match res {
                    Ok(info) if info.is_live && info.hls_url.is_some() => {
                        p.streams = Some(Arc::new(info));
                        p.load_current(None, false, cx);
                    }
                    _ => {
                        // the broadcast ended: carry on as a finished video
                        p.loading = false;
                        p.live_rejoins = u32::MAX;
                        p.on_ended(cx);
                    }
                }
                p.sync_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The built-in streams failed to open: fetch them again through yt-dlp.
    fn fallback_to_ytdlp(&mut self, cx: &mut Context<Self>) {
        self.preloaded_until = 0.0;
        self.fallback_tried = true;
        let Some(id) = self.video.as_ref().map(|v| v.id.clone()) else { return };
        streams::invalidate(&id);
        let serial = self.load_serial;
        let resume = (self.time > 1.0).then_some(self.time);
        cx.spawn(async move |this, cx| {
            let res = blocking(move || streams::ytdlp_streams(&id)).await;
            let _ = this.update(cx, |p, cx| {
                if p.load_serial != serial {
                    return;
                }
                match res {
                    Ok(info) => {
                        if p.storyboard.is_none() {
                            p.storyboard = info.storyboard.clone();
                        }
                        p.streams = Some(Arc::new(info));
                        p.load_current(resume, false, cx);
                    }
                    Err(e) => {
                        p.loading = false;
                        p.error = Some(tf!("Could not play this video: {0}", e));
                    }
                }
                p.sync_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// (Re)loads current streams into mpv with current quality/audio settings.
    fn load_current(&mut self, start: Option<f64>, keep_pause: bool, cx: &mut Context<Self>) {
        // mpv restores file-local options when the previous file closes.
        self.seek_cache_target = None;
        self.seek_disk_bytes = 0;
        self.cache_ranges = None;
        self.cache_time = 0.0;
        self.preloaded_until = 0.0;
        self.frame_progress = None;
        self.set_playback_ready(false, cx);
        let (Some(mpv), Some(info)) = (self.mpv.clone(), self.streams.clone()) else { return };
        if let Some(("open", started)) = self.playback_timing {
            eprintln!("[player] open to stream selection: {} ms ({})", started.elapsed().as_millis(), info.source);
        }
        self.hold_picture = Some(mpv.presented_frames());
        let s = store::settings_snapshot();
        let is_short = self.video.as_ref().is_some_and(|v| v.is_short) && !info.is_live;
        let selection = streams::playback_stream(
            &info,
            self.quality,
            &s.codec_pref,
            self.audio_track.as_deref(),
            self.audio_only,
            self.recovering_picture,
        );
        let mut opening = 0.0;
        let src = match selection {
            Some(streams::PlaybackStream::Hls(url)) if info.hls_only || is_short => {
                let audio = streams::hls_audio_choice(&info, self.audio_track.as_deref());
                let source = super::proxy::playback_hls(url, self.quality, audio.as_deref(), self.recovering_picture);
                if is_short && !self.recovering_picture && source != url && start.unwrap_or(0.0) == 0.0 {
                    opening = super::shorts_preload::hls_opening_seconds(
                        url,
                        self.quality,
                        audio.as_deref(),
                    )
                    .unwrap_or(0.0);
                }
                Some(source)
            }
            Some(streams::PlaybackStream::Hls(url)) => {
                // libmpv only speaks HTTP proxies: with a SOCKS proxy, live HLS goes through the local proxy
                if crate::store::proxy_setting().is_some_and(|p| !crate::store::mpv_http_proxy(&p)) {
                    let audio = streams::hls_audio_choice(&info, self.audio_track.as_deref());
                    Some(super::proxy::local_hls(url, self.quality, audio.as_deref()))
                } else {
                    Some(url.to_owned())
                }
            }
            Some(streams::PlaybackStream::Muxed(m)) if is_short => {
                Some(super::proxy::local_url(&m.url, m.content_length, &m.mime))
            }
            Some(streams::PlaybackStream::Muxed(m)) => Some(m.url.clone()),
            Some(streams::PlaybackStream::Adaptive(v, a)) => {
                streams::mpv_source(v, a, info.length_secs)
            }
            None => None,
        };
        let Some(src) = src else {
            self.error = Some(translate("No playable stream found").into());
            self.loading = false;
            self.sync_idle(cx);
            return;
        };
        let was_paused = self.paused;
        let mut opts = vec![format!("speed={}", self.speed)];
        if self.recovering_picture {
            opts.push("hwdec=no".into());
        } else if info.hls_only && !info.is_live {
            // The proxy already selects one HLS rendition. Limit how much of its
            // opening segments libavformat reads before it can start or resume.
            opts.push("demuxer-lavf-analyzeduration=0.8".into());
        }
        // Bound network readahead so previously buffered ranges survive a seek.
        if !info.is_live {
            opts.push("cache-secs=40".into());
        }
        if let Some(t) = start.filter(|t| *t > 0.0) {
            opts.push(format!("start={t}"));
        }
        if keep_pause && was_paused {
            opts.push("pause=yes".into());
        } else {
            opts.push("pause=no".into());
        }
        if let Some(v) = &self.video {
            opts.push(mpv_media_title_option(&v.title));
        }
        mpv.set("vid", if self.audio_only { "no" } else { "auto" });
        self.loading = true;
        // mpv 0.38 added an index argument; older libmpv (Ubuntu 22.04's 0.34) takes options third
        let joined = opts.join(",");
        let res = mpv.command(&["loadfile", &src, "replace", "-1", &joined]).or_else(|_| mpv.command(&["loadfile", &src, "replace", &joined]));
        if let Err(e) = res {
            self.loading = false;
            self.error = Some(e.to_string());
        } else {
            self.preloaded_until = opening;
        }
        cx.notify();
    }

    pub fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        let mut ranges = Vec::new();
        if !self.loading {
            if let Some(cached) = &self.cache_ranges {
                ranges.extend_from_slice(cached);
            } else if self.cache_time > self.time {
                ranges.push((self.time, self.cache_time));
            }
        }
        if self.preloaded_until > 0.0 {
            ranges.push((0.0, self.preloaded_until));
        }
        ranges
    }

    /// Applies caption styling settings to mpv (see [`caption_font`] and [`mpv_color`]).
    fn apply_caption_style(&self) {
        let Some(mpv) = &self.mpv else { return };
        let s = store::settings_snapshot();
        mpv.set("sub-font-size", &s.caption_size.to_string());
        mpv.set("sub-font", caption_font(&s.caption_font));
        mpv.set("sub-bold", "no");
        mpv.set("sub-margin-y", "40");
        mpv.set("sub-color", &mpv_color(s.caption_color, s.caption_opacity));
        if s.caption_background {
            mpv.set("sub-border-style", "background-box");
            mpv.set("sub-back-color", &mpv_color(s.caption_bg_color, s.caption_bg_opacity));
            mpv.set("sub-border-size", "2");
            mpv.set("sub-shadow-offset", "0");
        } else {
            mpv.set("sub-border-style", "outline-and-shadow");
            mpv.set("sub-border-color", "#FF000000");
            mpv.set("sub-shadow-color", "#80000000");
            let (border, shadow) = match s.caption_edge.as_str() {
                "outline" => ("2.5", "0"),
                "shadow" => ("0", "2"),
                "none" => ("0", "0"),
                _ => ("2.5", "1"),
            };
            mpv.set("sub-border-size", border);
            mpv.set("sub-shadow-offset", shadow);
        }
    }

    /// Re-applies settings that affect the current playback.
    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let s = store::settings_snapshot();
        self.loop_video = s.loop_video;
        if let Some(m) = &self.mpv {
            m.set("hwdec", hwdec_option(s.hwdec, m.gl_renderer().is_some()));
        }
        self.apply_caption_style();
        if self.video.is_some() {
            if s.audio_only != self.audio_only {
                self.set_audio_only(s.audio_only, cx);
            }
            let want_caps = s.captions_enabled;
            if want_caps {
                let desired = self.streams.as_ref().and_then(|i| pick_caption(i, &s.captions_lang));
                if desired != self.caption {
                    self.caption = desired;
                    self.apply_caption();
                }
            } else if self.caption.is_some() {
                self.caption = None;
                self.apply_caption();
            }
        }
        cx.notify();
    }

    fn apply_caption(&mut self) {
        self.apply_caption_style();
        let (Some(mpv), Some(info)) = (&self.mpv, &self.streams) else { return };
        let _ = mpv.command(&["sub-remove"]);
        if let Some(c) = self.caption.and_then(|i| info.captions.get(i)) {
            let url = if !info.is_live {
                crate::player::proxy::local_caption(&c.url)
            } else if crate::store::proxy_setting().is_some() {
                crate::player::proxy::local_pass(&c.url)
            } else {
                c.url.clone()
            };
            let _ = mpv.command(&["sub-add", &url, "select", &c.name, &c.lang]);
        } else {
            mpv.set("sid", "no");
        }
    }

    pub fn set_caption(&mut self, idx: Option<usize>, cx: &mut Context<Self>) {
        self.caption = idx;
        self.apply_caption();
        if let Some(i) = idx {
            if let Some(c) = self.streams.as_ref().and_then(|s| s.captions.get(i)) {
                let lang = strip_orig_suffix(&c.lang).split('-').next().unwrap_or("en").to_string();
                store::update_settings(|s| {
                    s.captions_enabled = true;
                    s.captions_lang = lang;
                });
            }
        } else {
            store::update_settings(|s| s.captions_enabled = false);
        }
        cx.notify();
    }

    /// Toggles captions on/off (for the `C` shortcut) without overwriting `captions_lang`
    /// so `"original"` stays intact.
    pub fn toggle_captions(&mut self, cx: &mut Context<Self>) {
        if self.caption.is_some() {
            self.caption = None;
            self.apply_caption();
            store::update_settings(|s| s.captions_enabled = false);
        } else {
            let lang = store::settings_snapshot().captions_lang.clone();
            let next = self
                .streams
                .as_ref()
                .and_then(|i| pick_caption(i, &lang).or_else(|| (!i.captions.is_empty()).then_some(0)));
            self.caption = next;
            self.apply_caption();
            if next.is_some() {
                store::update_settings(|s| s.captions_enabled = true);
            }
        }
        cx.notify();
    }

    /// Where "Jump ahead" goes: the next part of the video most viewers skip to.
    pub fn jump_ahead_target(&self) -> Option<f64> {
        let from = self.time + 5.0;
        let later: Vec<&(f64, f64, f32)> = self.heatmap.iter().filter(|m| m.0 > from).collect();
        let max = later.iter().map(|m| m.2).fold(0.0f32, f32::max);
        if max <= 0.0 {
            return None;
        }
        // first local peak that is close to the strongest one ahead
        for (i, m) in later.iter().enumerate() {
            let next = later.get(i + 1).map(|n| n.2).unwrap_or(0.0);
            if m.2 >= max * 0.8 && m.2 >= next {
                return Some(m.0);
            }
        }
        None
    }

    pub fn set_quality(&mut self, q: u32, cx: &mut Context<Self>) {
        if q > 0 && store::settings_snapshot().remember_quality {
            store::update_settings(|s| s.default_quality = q);
        }
        self.quality = q;
        let t = self.time;
        self.load_current(Some(t), true, cx);
    }

    pub fn set_audio_track(&mut self, lang: Option<String>, cx: &mut Context<Self>) {
        self.audio_track = lang;
        let t = self.time;
        self.load_current(Some(t), true, cx);
    }

    pub fn set_audio_only(&mut self, on: bool, cx: &mut Context<Self>) {
        self.audio_only = on;
        store::update_settings(|s| s.audio_only = on);
        self.clear_frame(cx);
        let t = self.time;
        self.load_current(Some(t), true, cx);
    }

    pub fn available_qualities(&self) -> Vec<u32> {
        let mut q: Vec<u32> = self
            .streams
            .as_ref()
            .map(|s| s.video.iter().filter(|f| f.height > 0).map(streams::fmt_class).collect())
            .unwrap_or_default();
        q.sort_unstable_by(|a, b| b.cmp(a));
        q.dedup();
        q
    }

    pub fn audio_tracks(&self) -> Vec<String> {
        let mut t: Vec<String> = self
            .streams
            .as_ref()
            .map(|s| s.audio.iter().filter_map(|a| a.audio_language.clone()).collect())
            .unwrap_or_default();
        t.sort();
        t.dedup();
        t
    }

    pub fn current_height(&self) -> u32 {
        if self.video_size.1 <= 0.0 {
            return 0;
        }
        streams::quality_class(self.video_size.0 as u32, self.video_size.1 as u32)
    }

    /// Playback details for the "Stats for nerds" overlay, read from mpv.
    pub fn stats(&self) -> Vec<(&'static str, String)> {
        let Some(m) = &self.mpv else { return Vec::new() };
        let f = |k: &str| m.get_f64(k);
        let st = |k: &str| m.get_string(k).filter(|s| !s.is_empty());
        let mut out: Vec<(&'static str, String)> = Vec::new();
        if let Some(v) = &self.video {
            let source = self.streams.as_ref().map(|s| s.source).unwrap_or("");
            out.push(("Video ID", if source.is_empty() { v.id.clone() } else { tf!("{0}, via {1}", v.id, source) }));
            if v.is_short {
                let bytes = super::shorts_preload::served_bytes(&v.id);
                out.push((
                    "Read from Shorts preload",
                    tf!("{0} MB", format_args!("{:.2}", bytes as f64 / 1048576.0)),
                ));
            }
        }
        if !self.audio_only {
            let (w, h) = (f("video-params/w").unwrap_or(0.0), f("video-params/h").unwrap_or(0.0));
            let fps = f("container-fps").or_else(|| f("estimated-vf-fps")).unwrap_or(0.0);
            if w > 0.0 {
                out.push(("Resolution", format!("{w:.0}x{h:.0}@{fps:.0}")));
            }
            let dropped = f("frame-drop-count").unwrap_or(0.0) + f("decoder-frame-drop-count").unwrap_or(0.0);
            let shown = f("estimated-frame-number").unwrap_or(0.0);
            out.push(("Frames", tf!("{0} dropped of {1}", format_args!("{:.0}", dropped), format_args!("{:.0}", shown))));
            if let Some(c) = st("video-codec") {
                let pf = st("video-params/hw-pixelformat").or_else(|| st("video-params/pixelformat")).unwrap_or_default();
                out.push(("Video codec", if pf.is_empty() { c } else { format!("{c}, {pf}") }));
            }
            let hw = st("hwdec-current").filter(|s| s != "no").unwrap_or_else(|| translate("off (software)").into());
            out.push(("Hardware decoding", hw));
            out.push(("Rendering", translate(if m.gl_renderer().is_some() { "GPU, zero-copy (OpenGL → IOSurface)" } else { "CPU (software)" }).into()));
            let color = [st("video-params/colormatrix"), st("video-params/primaries"), st("video-params/gamma")].into_iter().flatten().collect::<Vec<_>>();
            if !color.is_empty() {
                out.push(("Color", color.join(" / ")));
            }
        }
        if let Some(c) = st("audio-codec-name").or_else(|| st("audio-codec")) {
            let rate = f("audio-params/samplerate").map(|r| tf!(", {0} kHz", format_args!("{:.1}", r / 1000.0))).unwrap_or_default();
            let ch = f("audio-params/channel-count").map(|c| tf!(", {0} ch", format_args!("{:.0}", c))).unwrap_or_default();
            out.push(("Audio codec", format!("{c}{rate}{ch}")));
        }
        let kbps = |k: &str| f(k).filter(|b| *b > 0.0).map(|b| tf!("{0} kbps", format_args!("{:.0}", b / 1000.0)));
        let rates = [kbps("video-bitrate").map(|v| tf!("video {0}", v)), kbps("audio-bitrate").map(|a| tf!("audio {0}", a))].into_iter().flatten().collect::<Vec<_>>();
        if !rates.is_empty() {
            out.push(("Bitrate", rates.join(", ")));
        }
        if let Some(b) = f("demuxer-cache-duration") {
            out.push(("Buffer health", tf!("{0} s", format_args!("{:.2}", b))));
        }
        if let Some(sp) = f("cache-speed") {
            out.push(("Connection speed", tf!("{0} Kbps", format_args!("{:.0}", sp * 8.0 / 1000.0))));
        }
        if let Some(mb) = m.get_f64("demuxer-cache-state/fw-bytes") {
            out.push(("Buffered data", tf!("{0} MB", format_args!("{:.1}", mb / 1048576.0))));
        }
        out.push(("Volume", format!("{:.0}%{}", self.volume, if self.muted { translate(" (muted)") } else { "" })));
        out.push(("Speed", format!("{:.2}x", self.speed)));
        if let Some(a) = f("avsync") {
            out.push(("A/V sync", tf!("{0} ms", format_args!("{:+.1}", a * 1000.0))));
        }
        out.push(("Position", tf!("{0} / {1} s", format_args!("{:.1}", self.time), format_args!("{:.1}", self.duration))));
        out
    }

    // ───── transport controls ─────

    pub fn toggle_pause(&mut self, cx: &mut App) {
        if self.eof {
            self.eof = false;
            if let Some(video) = &self.video {
                self.history_report = crate::api::web::PlaybackReport::new(video.id.clone(), self.load_serial);
            }
            self.seek_to(0.0);
            self.set_paused(false, cx);
            return;
        }
        let p = !self.paused;
        self.set_paused(p, cx);
    }

    pub fn set_paused(&mut self, p: bool, cx: &mut App) {
        if !p && store::settings_snapshot().resume_playback {
            if let Some(position) = self.updated_position_while_paused() {
                self.seek_to(position);
            }
        }
        if let Some(m) = &self.mpv {
            m.set_flag("pause", p);
        }
        self.update_paused(p);
        self.sync_idle(cx);
    }

    fn updated_position_while_paused(&self) -> Option<f64> {
        if !self.paused || !self.playback_ready || self.loading || self.eof || self.seeking
            || (self.time - self.last_progress_save).abs() >= 0.5
        {
            return None;
        }
        let (position, _) = store::playback_position(&self.video.as_ref()?.id)?;
        (position.is_finite() && (position - self.last_progress_save).abs() >= 1.0).then_some(position)
    }

    fn update_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if !self.loading {
            if let Some(report) = &self.history_report {
                report.progress(self.time, !paused, true);
            }
        }
        if paused && !self.loading {
            self.save_progress(true);
        }
    }

    pub fn seek_to(&mut self, t: f64) {
        self.seek(t, "absolute+exact");
    }

    pub fn seek_interactive(&mut self, t: f64) {
        // Landing on a keyframe avoids fetching and decoding an extra GOP
        // before showing the picture after a large jump through an HLS VOD.
        let buffered = t >= self.time && t <= self.cache_time;
        let remote_jump = (t - self.time).abs() >= 8.0 && !buffered
            && self.streams.as_ref().is_some_and(|s| s.hls_only && !s.is_live);
        self.seek(t, if remote_jump { "absolute+keyframes" } else { "absolute+exact" });
    }

    fn seek(&mut self, t: f64, mode: &str) {
        self.playback_timing = std::env::var_os("STREAMTUBE_PLAYBACK_TIMING").map(|_| ("seek", Instant::now()));
        self.frame_progress = None;
        self.preloaded_until = 0.0;
        let t = clamp_seek(t, self.duration);
        if (t < self.time || self.seek_cache_target.is_some()) && !self.loading
            && self.seek_disk_bytes < SEEK_DISK_CACHE_LIMIT && self.streams.as_ref().is_some_and(|s| !s.is_live)
        {
            let ranges = self.cache_ranges.as_deref().unwrap_or_default();
            let end = ranges.iter().filter(|&&(start, _)| start > t).map(|&(_, end)| end)
                .chain(self.seek_cache_target.map(|(_, end)| end)).fold(t, f64::max);
            self.set_seek_cache_target((t < end).then_some((t, end)));
        }
        self.time = t;
        if let Some(report) = &self.history_report {
            report.seek(t);
        }
        if let Some(m) = &self.mpv {
            self.hold_picture = Some(m.presented_frames());
            let _ = m.command(&["seek", &format!("{t}"), mode]);
        }
    }

    fn set_seek_cache_target(&mut self, target: Option<(f64, f64)>) {
        if self.seek_cache_target.is_some() != target.is_some() {
            if let Some(mpv) = &self.mpv {
                // Write incoming packets to a temporary file while closing the gap.
                // Their headers still use RAM even when the payload is on disk.
                mpv.set("file-local-options/demuxer-max-back-bytes", if target.is_some() { "20MiB" } else { BACK_CACHE_SIZE });
                mpv.set("file-local-options/cache-on-disk", if target.is_some() { "yes" } else { "no" });
            }
        }
        self.seek_cache_target = target;
    }

    pub fn seek_by(&mut self, d: f64) {
        let t = self.time + d;
        self.seek_interactive(t);
    }

    pub fn keyboard_feedback(&self) -> Option<KeyboardFeedback> {
        self.keyboard_feedback.as_ref().map(|(feedback, _)| *feedback)
    }

    pub fn show_keyboard_feedback(&mut self, feedback: KeyboardFeedback, cx: &mut Context<Self>) {
        let hide = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(900)).await;
            let _ = this.update(cx, |player, cx| {
                player.keyboard_feedback = None;
                cx.notify();
            });
        });
        self.keyboard_feedback = Some((feedback, hide));
    }

    pub fn set_volume(&mut self, v: f64) {
        self.set_volume_live(v);
        self.save_volume();
    }

    pub(super) fn set_volume_live(&mut self, v: f64) {
        let v = v.clamp(0.0, 150.0);
        self.volume = v;
        if let Some(m) = &self.mpv {
            m.set_f64("volume", v);
            if self.muted && v > 0.0 {
                m.set_flag("mute", false);
            }
        }
    }

    pub(super) fn save_volume(&self) {
        let value = self.volume.min(100.0);
        if store::settings_snapshot().volume != value {
            store::update_settings(|s| s.volume = value);
        }
    }

    pub fn set_eq_preset(&mut self, preset: &str) {
        let filter = eq_filter(preset);
        if let Some(m) = &self.mpv {
            m.set("af", filter);
        }
        let p = if EQ_PRESETS.iter().any(|&(k, _, _)| k == preset) { preset } else { "flat" };
        store::update_settings(|s| s.eq_preset = p.to_string());
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(m) = &self.mpv {
            m.set_flag("mute", self.muted);
        }
    }

    /// Press-and-hold: plays at 2x until `end_boost`, without touching the saved speed.
    pub fn start_boost(&mut self, cx: &mut App) {
        if self.boost.is_some() || self.video.is_none() {
            return;
        }
        self.boost = Some(self.speed);
        if self.paused {
            self.set_paused(false, cx);
        }
        if let Some(m) = &self.mpv {
            m.set_f64("speed", (self.speed * 2.0).clamp(2.0, 4.0));
        }
    }

    pub fn end_boost(&mut self) {
        if let Some(prev) = self.boost.take() {
            if let Some(m) = &self.mpv {
                m.set_f64("speed", prev);
            }
        }
    }

    pub fn set_speed(&mut self, s: f64) {
        let s = (s * 100.0).round() / 100.0;
        let s = s.clamp(0.1, 5.0);
        self.boost = None;
        self.speed = s;
        if let Some(m) = &self.mpv {
            m.set_f64("speed", s);
        }
        if store::settings_snapshot().remember_speed_per_channel {
            if let Some(c) = &self.channel_id {
                store::set_channel_speed(c, s);
            }
        }
    }

    pub fn step_speed(&mut self, increase: bool) {
        let steps = self.speed / 0.25;
        let next = if increase { steps.floor() + 1.0 } else { steps.ceil() - 1.0 };
        self.set_speed(next * 0.25);
    }

    pub fn set_loop(&mut self, l: bool) {
        self.loop_video = l;
    }

    pub fn frame_step(&mut self, forward: bool) {
        if let Some(m) = &self.mpv {
            let _ = m.command(&[if forward { "frame-step" } else { "frame-back-step" }]);
        }
    }

    pub fn screenshot(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = &self.mpv {
            let dir = dirs::picture_dir().unwrap_or_else(std::env::temp_dir).join("Stream Tube");
            let _ = std::fs::create_dir_all(&dir);
            let name = format!(
                "{}-{}.png",
                self.video.as_ref().map(|v| v.id.as_str()).unwrap_or("frame"),
                self.time as u64
            );
            let path = dir.join(name);
            let _ = m.command(&["screenshot-to-file", &path.to_string_lossy(), "video"]);
            toast(cx, tf!("Screenshot saved to {0}", path.display()));
        }
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.playback_timing = None;
        self.keyboard_feedback = None;
        self.preloaded_until = 0.0;
        self.set_playback_ready(false, cx);
        self.save_progress(true);
        self.finish_playback_history();
        self.load_serial += 1;
        if let Some(m) = &self.mpv {
            let _ = m.command(&["stop"]);
        }
        self.video = None;
        self.streams = None;
        self.loading = false;
        self.autoplay_next = None;
        self.played_before.clear();
        self.reset_queue();
        self.clear_frame(cx);
        self.sync_idle(cx);
        cx.notify();
    }

    /// Jumps to the start of the next (`dir` > 0) or previous chapter, as Ctrl+Right
    /// and Ctrl+Left do on YouTube. False when there is no chapter that way.
    pub fn seek_chapter(&mut self, dir: i32) -> bool {
        let starts: Vec<f64> = self.chapters.iter().map(|c| c.start).collect();
        match chapter_target(&starts, self.time, dir) {
            Some(t) => {
                self.seek_to(t);
                true
            }
            None => false,
        }
    }

    pub fn current_chapter(&self) -> Option<&Chapter> {
        self.chapters.iter().rev().find(|c| c.start <= self.time + 0.01)
    }

    pub fn highlight(&self) -> Option<&Segment> {
        self.segments.iter().find(|s| s.action == "poi" || s.category == "poi_highlight")
    }
}

/// Applies changed settings to the running player.
pub fn apply_settings(cx: &mut App) {
    for player in tab_players(cx) {
        player.update(cx, |p, cx| p.apply_settings(cx));
    }
}

struct Guard<F: FnMut()>(F);
impl<F: FnMut()> Drop for Guard<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
fn scopeguard<F: FnMut()>(f: F) -> Guard<F> {
    Guard(f)
}

/// Average colour of a BGRA frame from a sparse sample grid, slightly brightened
/// so dark scenes still give a visible glow.
fn average_color(data: &[u8], w: u32, h: u32) -> Option<Rgba> {
    if w < 4 || h < 4 || data.len() < (w * h * 4) as usize {
        return None;
    }
    let mut px = Vec::with_capacity(16 * 12);
    for gy in 0..12 {
        for gx in 0..16 {
            let x = (w - 1) * gx / 15;
            let y = (h - 1) * gy / 11;
            let i = ((y * w + x) * 4) as usize;
            px.push([data[i + 2], data[i + 1], data[i]]);
        }
    }
    average_rgb(&px)
}

/// Average of RGB samples, slightly brightened (see `average_color`).
fn average_rgb(px: &[[u8; 3]]) -> Option<Rgba> {
    if px.is_empty() {
        return None;
    }
    let mut sum = [0u64; 3];
    for p in px {
        for c in 0..3 {
            sum[c] += p[c] as u64;
        }
    }
    let f = |v: u64| ((v as f32 / px.len() as f32) / 255.0 * 1.25).min(1.0);
    Some(Rgba { r: f(sum[0]), g: f(sum[1]), b: f(sum[2]), a: 1.0 })
}

fn strip_orig_suffix(code: &str) -> &str {
    code.strip_suffix("-orig").unwrap_or(code)
}

fn lang_matches(track: &str, target: &str) -> bool {
    let track = strip_orig_suffix(track);
    let target = strip_orig_suffix(target);
    if target.is_empty() {
        return false;
    }
    if track.eq_ignore_ascii_case(target) {
        return true;
    }
    if track.len() > target.len()
        && track.get(..target.len()).map(|p| p.eq_ignore_ascii_case(target)).unwrap_or(false)
        && track.as_bytes().get(target.len()) == Some(&b'-')
    {
        return true;
    }
    if let Some((t_base, _)) = target.split_once('-') {
        if track.eq_ignore_ascii_case(t_base) {
            return true;
        }
        if track.len() > t_base.len()
            && track.get(..t_base.len()).map(|p| p.eq_ignore_ascii_case(t_base)).unwrap_or(false)
            && track.as_bytes().get(t_base.len()) == Some(&b'-')
        {
            return true;
        }
    }
    false
}

fn pick_for_lang(captions: &[CaptionTrack], target: &str) -> Option<usize> {
    let target_clean = strip_orig_suffix(target);
    captions
        .iter()
        .position(|c| !c.auto && strip_orig_suffix(&c.lang).eq_ignore_ascii_case(target_clean))
        .or_else(|| captions.iter().position(|c| !c.auto && lang_matches(&c.lang, target)))
        .or_else(|| captions.iter().position(|c| c.auto && strip_orig_suffix(&c.lang).eq_ignore_ascii_case(target_clean)))
        .or_else(|| captions.iter().position(|c| c.auto && lang_matches(&c.lang, target)))
}

fn base_lang(code: &str) -> String {
    strip_orig_suffix(code)
        .split('-')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Strips URLs and handles from text so English links in descriptions don't bias language scoring.
fn clean_for_lang_detect(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(1024));
    for token in text.split_whitespace().take(180) {
        let low = token.to_ascii_lowercase();
        if low.starts_with("http://")
            || low.starts_with("https://")
            || low.starts_with("www.")
            || low.contains(".com")
            || low.contains(".org")
            || low.contains(".net")
            || low.contains("youtu.be")
            || token.starts_with('@')
        {
            continue;
        }
        out.push_str(token);
        out.push(' ');
    }
    out
}

/// Scores how strongly `text` matches a given base ISO language code.
fn score_text_for_lang(text: &str, base: &str) -> i32 {
    let mut score = 0i32;

    let mut hangul = 0i32;
    let mut kana = 0i32;
    let mut cjk = 0i32;
    let mut cyrillic = 0i32;
    let mut uk_chars = 0i32;
    let mut arabic = 0i32;
    let mut persian_chars = 0i32;
    let mut urdu_chars = 0i32;
    let mut greek = 0i32;
    let mut hebrew = 0i32;
    let mut devanagari = 0i32;
    let mut thai = 0i32;
    let mut tr_strong = 0i32;
    let mut tr_soft = 0i32;
    let mut az_chars = 0i32;
    let mut es_chars = 0i32;
    let mut de_chars = 0i32;
    let mut pt_chars = 0i32;
    let mut fr_chars = 0i32;
    let mut pl_chars = 0i32;
    let mut vi_chars = 0i32;

    for ch in text.chars() {
        match ch {
            '\u{AC00}'..='\u{D7AF}' | '\u{1100}'..='\u{11FF}' => hangul += 1,
            '\u{3040}'..='\u{30FF}' => kana += 1,
            '\u{4E00}'..='\u{9FFF}' => cjk += 1,
            '\u{0400}'..='\u{04FF}' => {
                cyrillic += 1;
                if matches!(ch, 'ї' | 'Ї' | 'є' | 'Є' | 'і' | 'І' | 'ґ' | 'Ґ') {
                    uk_chars += 1;
                }
            }
            '\u{0600}'..='\u{06FF}' => {
                arabic += 1;
                if matches!(ch, 'پ' | 'چ' | 'ژ' | 'گ' | 'ی' | 'ک') {
                    persian_chars += 1;
                }
                if matches!(ch, 'ٹ' | 'ڈ' | 'ڑ' | 'ں' | 'ے') {
                    urdu_chars += 1;
                }
            }
            '\u{0370}'..='\u{03FF}' => greek += 1,
            '\u{0590}'..='\u{05FF}' => hebrew += 1,
            '\u{0900}'..='\u{097F}' => devanagari += 1,
            '\u{0E00}'..='\u{0E7F}' => thai += 1,
            'ğ' | 'Ğ' | 'ş' | 'Ş' | 'ı' | 'İ' => tr_strong += 1,
            'ç' | 'Ç' | 'ö' | 'Ö' | 'ü' | 'Ü' => tr_soft += 1,
            'ə' | 'Ə' => az_chars += 1,
            'ñ' | 'Ñ' | '¿' | '¡' => es_chars += 1,
            'ß' => de_chars += 2,
            'ä' | 'Ä' => de_chars += 1,
            'ã' | 'Ã' | 'õ' | 'Õ' => pt_chars += 1,
            'é' | 'è' | 'ê' | 'ë' | 'à' | 'ù' | 'ô' | 'î' | 'œ' => fr_chars += 1,
            'ł' | 'Ł' | 'ś' | 'Ś' | 'ź' | 'Ź' | 'ż' | 'Ż' | 'ć' | 'Ć' | 'ń' | 'Ń' | 'ą' | 'ę' => pl_chars += 1,
            'ă' | 'đ' | 'ơ' | 'ư' | 'ệ' | 'ộ' | 'ữ' | 'ả' | 'ẻ' | 'ổ' | 'ớ' | 'ờ' | 'ứ' | 'ừ' => vi_chars += 1,
            _ => {}
        }
    }

    match base {
        "ko" => return hangul * 4,
        "ja" => return kana * 5 + if kana > 0 { cjk } else { 0 },
        "zh" => return if kana == 0 && hangul == 0 { cjk * 3 } else { 0 },
        "ru" => return if uk_chars == 0 { cyrillic * 3 } else { cyrillic },
        "uk" => return uk_chars * 6 + if uk_chars > 0 { cyrillic } else { 0 },
        "bg" | "sr" | "be" | "mk" | "kk" | "ky" | "mn" => return cyrillic * 2,
        "ar" => return if persian_chars == 0 && urdu_chars == 0 { arabic * 3 } else { arabic },
        "fa" => return persian_chars * 5 + if persian_chars > 0 { arabic } else { 0 },
        "ur" => return urdu_chars * 5 + if urdu_chars > 0 { arabic } else { 0 },
        "el" => return greek * 4,
        "iw" | "he" => return hebrew * 4,
        "hi" | "mr" | "ne" => return devanagari * 4,
        "th" => return thai * 4,
        "az" => score += az_chars * 8 + tr_strong * 2,
        "tr" => score += tr_strong * 5 + tr_soft,
        "es" => score += es_chars * 5,
        "de" => score += de_chars * 4 + tr_soft,
        "pt" => score += pt_chars * 5,
        "fr" => score += fr_chars * 2,
        "pl" => score += pl_chars * 5,
        "vi" => score += vi_chars * 5,
        _ => {}
    }

    let stopwords: &[&str] = match base {
        "tr" => &[
            "ve", "bir", "bu", "için", "icin", "ile", "ama", "değil", "degil", "nasıl", "nasil",
            "nedir", "neden", "gibi", "daha", "çok", "cok", "var", "yok", "ben", "sen", "biz",
            "siz", "olan", "olarak", "çünkü", "cunku", "kadar", "şimdi", "simdi", "yeni", "bölüm",
            "bolum", "izle", "kanal", "her", "şey", "sey", "son", "böyle", "boyle", "şu", "su",
            "benim", "senin", "bizim", "oldu", "olur", "olacak", "yapmak", "yaptım", "yaptim",
        ],
        "en" => &[
            "the", "and", "you", "that", "this", "with", "for", "are", "was", "have", "from",
            "your", "what", "how", "why", "when", "where", "about", "into", "just", "like",
            "more", "will", "can", "all", "not", "but", "they", "their", "our", "out", "who",
            "here", "there", "been", "were", "would", "could", "should", "every",
        ],
        "es" => &[
            "que", "los", "las", "por", "para", "con", "una", "uno", "del", "como", "pero",
            "más", "mas", "muy", "este", "esta", "todo", "porque", "cuando", "sobre", "desde",
            "hasta", "donde", "sin", "sus", "ese", "esa", "esto", "qué", "cómo",
        ],
        "fr" => &[
            "les", "des", "une", "pour", "dans", "avec", "pas", "que", "qui", "sur", "est",
            "sont", "plus", "tout", "mais", "comme", "vous", "nous", "cette", "aussi", "bien",
            "faire", "aux", "être", "avoir", "comment", "pourquoi",
        ],
        "de" => &[
            "der", "die", "das", "und", "ist", "ein", "eine", "mit", "für", "fur", "auf",
            "nicht", "von", "den", "dem", "sich", "des", "wie", "auch", "aber", "oder", "wenn",
            "noch", "nur", "über", "uber", "nach", "wir", "ihr", "warum",
        ],
        "pt" => &[
            "que", "não", "nao", "para", "com", "uma", "por", "mais", "como", "mas", "foi",
            "são", "sao", "isso", "este", "esta", "você", "voce", "dos", "das", "tem", "muito",
            "tudo", "pelo", "pela", "porque", "quando",
        ],
        "it" => &[
            "che", "non", "per", "con", "una", "sono", "della", "delle", "degli", "questo",
            "questa", "come", "più", "piu", "anche", "tutto", "nella", "essere", "stato",
            "molto", "perché", "perche", "dove",
        ],
        "pl" => &[
            "nie", "się", "sie", "jest", "jak", "tak", "ale", "czy", "dla", "przez", "tylko",
            "bardzo", "ten", "tego", "tej", "wszystko", "może", "moze", "dlaczego",
        ],
        "nl" => &[
            "het", "een", "van", "voor", "met", "niet", "dat", "zijn", "naar", "ook", "maar",
            "nog", "wel", "deze", "over", "worden", "mijn", "wat", "waarom",
        ],
        "id" | "ms" => &[
            "yang", "dan", "ini", "untuk", "dengan", "tidak", "dari", "pada", "akan", "bisa",
            "saya", "kamu", "ada", "juga", "dalam", "atau", "tapi", "kenapa", "bagaimana",
        ],
        _ => &[],
    };

    if !stopwords.is_empty() {
        for raw_word in text.split(|c: char| !c.is_alphabetic()) {
            if raw_word.len() < 2 {
                continue;
            }
            let w = raw_word.to_lowercase();
            if stopwords.contains(&w.as_str()) {
                score += 3;
            }
        }
    }
    score
}

/// Infers which of the available caption languages best matches the video's title and description.
/// Only returns a language when the match is clear and unambiguous.
fn infer_caption_lang_from_text(info: &StreamInfo) -> Option<String> {
    let title = clean_for_lang_detect(&info.title);
    let desc = clean_for_lang_detect(&info.description);
    if title.is_empty() && desc.is_empty() {
        return None;
    }

    let mut bases: Vec<String> = Vec::new();
    for c in &info.captions {
        let b = base_lang(&c.lang);
        if !b.is_empty() && !bases.contains(&b) {
            bases.push(b);
        }
    }
    if bases.len() < 2 {
        return None;
    }

    let mut best: Option<(String, i32)> = None;
    let mut second_score = 0i32;

    for b in bases {
        // Weight title higher than description because descriptions often contain boilerplate/credits
        let s = score_text_for_lang(&title, &b) * 2 + score_text_for_lang(&desc, &b);
        match &best {
            None => best = Some((b, s)),
            Some((_, bs)) if s > *bs => {
                second_score = *bs;
                best = Some((b, s));
            }
            Some(_) => {
                if s > second_score {
                    second_score = s;
                }
            }
        }
    }

    let (lang, best_score) = best?;
    if best_score >= 6 && (best_score >= second_score * 2 || best_score >= second_score + 6) {
        Some(lang)
    } else {
        None
    }
}

/// Picks the best caption track index for `pref` (either a language code like `"en"`
/// or `crate::locale::ORIGINAL_CAPTION_LANG` for the video's original language).
pub fn pick_caption(info: &StreamInfo, pref: &str) -> Option<usize> {
    if info.captions.is_empty() {
        return None;
    }
    if pref.eq_ignore_ascii_case(crate::locale::ORIGINAL_CAPTION_LANG) {
        let text_lang = infer_caption_lang_from_text(info);

        // 1. Explicit original audio language from stream metadata
        if let Some(orig) = info.original_language.as_deref() {
            if let Some(i) = pick_for_lang(&info.captions, orig) {
                // Guard against background-music misdetection: if `orig` only matched an auto (ASR)
                // track while the video's title/description clearly matches a MANUAL caption track
                // in a different language, prefer the manual track.
                if info.captions[i].auto {
                    if let Some(tl) = text_lang.as_deref() {
                        if !lang_matches(orig, tl) {
                            if let Some(manual_i) = info
                                .captions
                                .iter()
                                .position(|c| !c.auto && lang_matches(&c.lang, tl))
                            {
                                return Some(manual_i);
                            }
                        }
                    }
                }
                return Some(i);
            }
        }

        // 2. When stream metadata doesn't specify an original language (most single-audio videos),
        // use title/description language matching BEFORE blindly trusting auto-generated ASR
        // (which YouTube often misdetects from background music).
        if let Some(tl) = text_lang.as_deref() {
            if let Some(i) = pick_for_lang(&info.captions, tl) {
                return Some(i);
            }
        }

        // 3. Uploader / YouTube default caption track for the original audio track
        if let Some(i) = info.default_caption_index.filter(|&i| i < info.captions.len()) {
            return Some(i);
        }

        // 4. Auto-generated original ASR track (preferring a manual caption of the same language)
        if let Some(asr) = info
            .captions
            .iter()
            .find(|c| c.auto && c.lang.ends_with("-orig"))
            .or_else(|| info.captions.iter().find(|c| c.auto))
        {
            if let Some(i) = pick_for_lang(&info.captions, &asr.lang) {
                return Some(i);
            }
        }

        return Some(0);
    }
    pick_for_lang(&info.captions, pref)
}

/// The font for a caption font choice. libass looks fonts up by name, and finds
/// the nearest match when one isn't installed.
pub fn caption_font(key: &str) -> &'static str {
    if cfg!(windows) {
        match key {
            "serif" => "Georgia",
            "mono" => "Consolas",
            "casual" => "Comic Sans MS",
            _ => "Segoe UI",
        }
    } else {
        match key {
            "serif" => "DejaVu Serif",
            "mono" => "DejaVu Sans Mono",
            "casual" => "Comic Neue",
            _ => "DejaVu Sans",
        }
    }
}

/// 0xRRGGBB plus an opacity in percent, as mpv's "#AARRGGBB".
pub fn mpv_color(rgb: u32, opacity: u32) -> String {
    let a = (opacity.min(100) * 255 + 50) / 100;
    format!("#{a:02X}{:06X}", rgb & 0xFFFFFF)
}

fn mpv_media_title_option(title: &str) -> String {
    // mpv's %length% quoting takes a byte count and keeps punctuation literal.
    format!("force-media-title=%{}%{title}", title.len())
}

fn premature_eof(position: f64, duration: f64) -> bool {
    // libmpv can report EOF for truncated streams. Allow for the last frame's timestamp.
    position.is_finite()
        && position >= 0.0
        && duration.is_finite()
        && duration > 0.0
        && duration - position > (duration * 0.02).clamp(1.0, 2.0)
}

#[cfg(test)]
mod playback_end_tests {
    #[::core::prelude::v1::test]
    fn distinguishes_truncated_playback_from_a_normal_end() {
        assert!(super::premature_eof(3.76, 12.0));
        assert!(!super::premature_eof(11.96, 12.0));
        assert!(!super::premature_eof(0.0, 0.0));
        assert!(!super::premature_eof(0.0, 0.3));
    }
}

#[cfg(test)]
mod media_title_tests {
    #[::core::prelude::v1::test]
    fn quotes_title_using_utf8_byte_length() {
        assert_eq!(
            super::mpv_media_title_option("[É], 日本語"),
            "force-media-title=%15%[É], 日本語"
        );
    }
}

#[cfg(test)]
mod caption_tests {
    use crate::api::types::{CaptionTrack, StreamInfo};

    #[::core::prelude::v1::test]
    fn colors() {
        assert_eq!(super::mpv_color(0xFFFF00, 100), "#FFFFFF00");
        assert_eq!(super::mpv_color(0x000000, 75), "#BF000000");
        assert_eq!(super::mpv_color(0x123456, 0), "#00123456");
    }

    #[::core::prelude::v1::test]
    fn original_uses_stream_language_and_prefers_manual() {
        let info = StreamInfo {
            original_language: Some("ja".into()),
            captions: vec![
                CaptionTrack { lang: "en".into(), name: "English".into(), url: String::new(), json3_url: None, auto: false },
                CaptionTrack { lang: "ja".into(), name: "Japanese (auto-generated)".into(), url: String::new(), json3_url: None, auto: true },
                CaptionTrack { lang: "ja".into(), name: "Japanese".into(), url: String::new(), json3_url: None, auto: false },
            ],
            ..Default::default()
        };
        assert_eq!(super::pick_caption(&info, "original"), Some(2));
    }

    #[::core::prelude::v1::test]
    fn original_prefers_spoken_over_music_asr() {
        // Turkish video where background English song caused English ASR
        let info = StreamInfo {
            title: "Ev Genci Nedir? Gençler Neden Çalışmıyor?".into(),
            description: "Bu bölümde yeni neslin sorunlarını konuşuyoruz. https://instagram.com/test".into(),
            captions: vec![
                CaptionTrack { lang: "en".into(), name: "English (auto-generated)".into(), url: String::new(), json3_url: None, auto: true },
                CaptionTrack { lang: "tr".into(), name: "Turkish".into(), url: String::new(), json3_url: None, auto: false },
            ],
            ..Default::default()
        };
        assert_eq!(super::pick_caption(&info, "original"), Some(1));

        // English video with Turkish fan translation still picks English ASR
        let en_info = StreamInfo {
            title: "How Rockets Work and Why They Don't Fall".into(),
            description: "In this video we explain how orbital mechanics work for beginners.".into(),
            captions: vec![
                CaptionTrack { lang: "en".into(), name: "English (auto-generated)".into(), url: String::new(), json3_url: None, auto: true },
                CaptionTrack { lang: "tr".into(), name: "Turkish".into(), url: String::new(), json3_url: None, auto: false },
            ],
            ..Default::default()
        };
        assert_eq!(super::pick_caption(&en_info, "original"), Some(0));
    }
}

fn clamp_seek(t: f64, duration: f64) -> f64 {
    if duration > 0.0 {
        t.clamp(0.0, (duration - 0.1).max(0.0))
    } else {
        t.max(0.0)
    }
}

/// Where a chapter jump from `now` lands: the next chapter's start (`dir` > 0), or
/// the previous chapter's start (in the first chapter, its own start).
fn chapter_target(starts: &[f64], now: f64, dir: i32) -> Option<f64> {
    let now = now + 0.01;
    if dir > 0 {
        return starts.iter().copied().find(|&s| s > now);
    }
    let cur = starts.iter().rposition(|&s| s <= now)?;
    Some(starts[cur.saturating_sub(1)])
}

#[cfg(test)]
mod chapter_tests {
    use super::chapter_target;

    #[::core::prelude::v1::test]
    fn jumps() {
        let c = [0.0, 60.0, 120.0];
        assert_eq!(chapter_target(&c, 30.0, 1), Some(60.0));
        assert_eq!(chapter_target(&c, 60.0, 1), Some(120.0));
        assert_eq!(chapter_target(&c, 130.0, 1), None);
        assert_eq!(chapter_target(&c, 90.0, -1), Some(0.0));
        assert_eq!(chapter_target(&c, 60.0, -1), Some(0.0));
        assert_eq!(chapter_target(&c, 10.0, -1), Some(0.0));
        assert_eq!(chapter_target(&[], 10.0, 1), None);
    }
}

#[cfg(test)]
mod seek_tests {
    use super::clamp_seek;

    #[::core::prelude::v1::test]
    fn stays_inside_the_video_and_does_not_panic_when_it_is_very_short() {
        assert_eq!(clamp_seek(30.0, 120.0), 30.0);
        assert_eq!(clamp_seek(120.0, 120.0), 119.9);
        assert_eq!(clamp_seek(-1.0, 120.0), 0.0);
        assert_eq!(clamp_seek(5.0, 0.0), 5.0);
        assert_eq!(clamp_seek(0.05, 0.05), 0.0);
        assert_eq!(clamp_seek(0.1, 0.1), 0.0);
    }
}

/// Appends unseen playlist videos to the queue. When `shuffle` is set, the
/// upcoming tail after the playing video is shuffled so newly loaded pages are
/// mixed into playback without moving the current or already-played tracks.
fn extend_queue(
    queue: &mut Vec<VideoItem>,
    pos: Option<usize>,
    incoming: Vec<VideoItem>,
    shuffle: bool,
    rng: &mut impl rand::Rng,
) -> usize {
    let added = crate::api::innertube::append_unique_videos(queue, incoming);
    if added > 0 && shuffle {
        use rand::seq::SliceRandom;
        let after = pos.map_or(0, |i| i + 1).min(queue.len());
        queue[after..].shuffle(rng);
    }
    added
}

#[cfg(test)]
mod queue_extend_tests {
    use super::*;

    fn vids(range: std::ops::Range<u32>) -> Vec<VideoItem> {
        range.map(|i| VideoItem { id: format!("v{i}"), ..Default::default() }).collect()
    }

    #[::core::prelude::v1::test]
    fn extends_in_order_past_200_and_deduplicates() {
        let mut q = vids(0..100);
        let mut rng = rand::rng();
        assert_eq!(extend_queue(&mut q, Some(0), vids(90..200), false, &mut rng), 100);
        assert_eq!(extend_queue(&mut q, Some(50), vids(200..320), false, &mut rng), 120);
        assert_eq!(q.len(), 320);
        assert_eq!(q[199].id, "v199");
        assert_eq!(q[200].id, "v200");
        assert_eq!(q[319].id, "v319");
    }

    #[::core::prelude::v1::test]
    fn shuffled_extension_keeps_played_prefix_and_includes_all_new_items() {
        let mut q = vids(0..100);
        let mut rng = rand::rng();
        let added = extend_queue(&mut q, Some(2), vids(100..250), true, &mut rng);
        assert_eq!(added, 150);
        assert_eq!(q.len(), 250);
        // current track and already-played prefix stay fixed
        assert_eq!((q[0].id.as_str(), q[1].id.as_str(), q[2].id.as_str()), ("v0", "v1", "v2"));
        let unique: std::collections::HashSet<&str> = q.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(unique.len(), 250);
    }
}
