//! Inline previews: when the pointer rests on a video thumbnail, a second, muted
//! player plays the video inside it (like youtube.com).
//!
//! The preview is drawn by the app above the page (see `slot::SlotFollower`), so
//! its frames never re-render the page the thumbnail belongs to.
use super::mpv::{Mpv, MpvEvent};
use crate::api::streams;
use crate::store;
use gpui_kit::*;
use gpui_kit::prelude::FluentBuilder as _;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// How long the pointer has to rest on a thumbnail before the preview starts.
const DELAY: Duration = Duration::from_millis(650);

pub struct Preview {
    mpv: Option<Arc<Mpv>>,
    /// video being previewed (or about to be)
    pub id: Option<String>,
    pub frame: Option<Arc<RenderImage>>,
    old: Vec<Arc<RenderImage>>,
    serial: u64,
    /// playback position and length of the preview, in seconds
    pub pos: f64,
    pub dur: f64,
    /// frames are shown only once the current file has started; until then mpv may
    /// still hand over frames of the previously hovered video
    ready: bool,
}

pub enum PreviewEvent {
    TargetChanged,
    Changed,
}

impl EventEmitter<PreviewEvent> for Preview {}

#[derive(Clone)]
pub struct PreviewHandle(pub Entity<Preview>);
impl Global for PreviewHandle {}

/// Where the thumbnail being previewed is on screen (written while painting it).
#[derive(Clone, Default)]
pub struct PreviewSlot(pub Rc<Cell<Option<Bounds<Pixels>>>>);
impl Global for PreviewSlot {}

pub fn init(cx: &mut App) {
    let e = cx.new(|_| Preview { mpv: None, id: None, frame: None, old: Vec::new(), serial: 0, pos: 0.0, dur: 0.0, ready: false });
    cx.set_global(PreviewHandle(e));
    cx.set_global(PreviewSlot::default());
}

pub fn preview(cx: &App) -> Entity<Preview> {
    cx.global::<PreviewHandle>().0.clone()
}

/// The id of the video currently previewed, once frames are showing.
pub fn showing(cx: &App) -> Option<String> {
    cx.try_global::<PreviewHandle>().and_then(|h| {
        let p = h.0.read(cx);
        p.frame.as_ref().and(p.id.clone())
    })
}

/// Called from thumbnails' hover handlers.
pub fn hover(id: &str, hovered: bool, cx: &mut App) {
    if cx.try_global::<PreviewHandle>().is_none() {
        return;
    }
    let p = preview(cx);
    if !hovered {
        if p.read(cx).id.as_deref() == Some(id) {
            stop(cx);
        }
        return;
    }
    if !store::settings_snapshot().hover_preview {
        return;
    }
    // no previews while a video is playing (e.g. hovering "Up next")
    {
        let pl = crate::player::controller::player(cx).read(cx);
        if pl.video.is_some() && !pl.paused && !pl.eof {
            return;
        }
    }
    let id = id.to_string();
    if p.read(cx).id.as_deref() != Some(id.as_str()) {
        cx.global::<PreviewSlot>().0.set(None);
    }
    let serial = p.update(cx, |p, cx| {
        p.serial += 1;
        if p.id.as_deref() != Some(id.as_str()) {
            p.clear(cx);
            p.id = Some(id.clone());
            cx.emit(PreviewEvent::TargetChanged);
        }
        p.serial
    });
    // wait for the pointer to rest, then resolve streams and start
    cx.spawn(async move |cx| {
        cx.background_executor().timer(DELAY).await;
        let current = cx.update(|cx| preview(cx).read(cx).serial == serial);
        if !current {
            return;
        }
        let id2 = id.clone();
        let info = crate::ui::blocking(move || streams::get_streams(&id2)).await;
        let _ = cx.update(|cx| {
            let p = preview(cx);
            if p.read(cx).serial != serial {
                return;
            }
            let Ok(info) = info else { return };
            p.update(cx, |p, cx| p.start(&info, cx));
        });
    })
    .detach();
}

/// Ends any preview (pointer left, page changed, real playback started).
pub fn stop(cx: &mut App) {
    if cx.try_global::<PreviewHandle>().is_none() {
        return;
    }
    // forget where the last thumbnail was: a preview only shows over a thumbnail
    // that's drawn on the current page
    cx.global::<PreviewSlot>().0.set(None);
    let serial = preview(cx).update(cx, |p, cx| {
        p.serial += 1;
        p.id = None;
        p.clear(cx);
        p.serial
    });
    // no preview for a while: close the preview player to give its memory back
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(20)).await;
        let _ = cx.update(|cx| {
            preview(cx).update(cx, |p, _| {
                if p.serial == serial {
                    if let Some(m) = p.mpv.take() {
                        // shutting mpv down waits for its threads; keep that off the UI thread
                        std::thread::spawn(move || drop(m));
                    }
                }
            })
        });
    })
    .detach();
}

impl Preview {
    /// Jumps to a fraction (0..1) of the video.
    pub fn seek_fraction(&mut self, f: f64, cx: &mut Context<Self>) {
        let Some(m) = &self.mpv else { return };
        if self.dur <= 0.0 {
            return;
        }
        let t = (f.clamp(0.0, 1.0) * self.dur).min(self.dur - 0.5).max(0.0);
        self.pos = t;
        let _ = m.command(&["seek", &format!("{t:.2}"), "absolute+keyframes"]);
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = &self.mpv {
            let _ = m.command(&["stop"]);
        }
        self.pos = 0.0;
        self.dur = 0.0;
        self.ready = false;
        let had = self.frame.is_some();
        if let Some(f) = self.frame.take() {
            self.old.push(f);
        }
        for o in self.old.drain(..) {
            cx.drop_image(o, None);
        }
        if had {
            cx.emit(PreviewEvent::Changed);
        }
        cx.notify();
    }

    fn ensure_mpv(&mut self, cx: &mut Context<Self>) -> Option<Arc<Mpv>> {
        if let Some(m) = &self.mpv {
            return Some(m.clone());
        }
        let (tx, rx) = futures::channel::mpsc::unbounded::<MpvEvent>();
        let ua = crate::api::http::BROWSER_UA;
        let proxy = store::settings_snapshot().proxy.clone();
        let mut opts: Vec<(&str, &str)> = vec![
            ("vo", "libmpv"),
            ("hwdec", "no"),
            ("mute", "yes"),
            ("aid", "no"),
            ("sid", "no"),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("ytdl", "no"),
            ("terminal", "no"),
            ("input-default-bindings", "no"),
            ("osc", "no"),
            ("osd-level", "0"),
            ("cache", "yes"),
            ("demuxer-max-bytes", "12MiB"),
            ("network-timeout", "15"),
            ("hls-bitrate", "min"),
            ("user-agent", ua),
            ("zimg-fast", "yes"),
            ("sws-scaler", "fast-bilinear"),
            ("framedrop", "vo"),
        ];
        if store::mpv_http_proxy(&proxy) {
            opts.push(("http-proxy", proxy.as_str()));
        }
        let m = Mpv::new(&opts, move |ev| {
            let _ = tx.unbounded_send(ev);
        })
        .ok()?;
        m.set_render_size(640, 360);
        self.mpv = Some(m.clone());
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            let mut rx = rx;
            while let Some(ev) = rx.next().await {
                let mut frame = false;
                let mut started = false;
                for e in std::iter::once(ev).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
                    match e {
                        MpvEvent::Frame => frame = true,
                        MpvEvent::PlaybackRestart => started = true,
                        MpvEvent::Shutdown => return,
                        _ => {}
                    }
                }
                let ok = this.update(cx, |p, cx| {
                    if started {
                        p.ready = p.id.is_some();
                    }
                    if frame {
                        p.on_frame(cx);
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Some(m)
    }

    fn start(&mut self, info: &crate::api::types::StreamInfo, cx: &mut Context<Self>) {
        let src = if info.hls_only {
            info.hls_url.as_deref().map(|u| crate::player::proxy::local_hls(u, 360, None))
        } else if info.is_live || (info.video.is_empty() && info.muxed.is_empty()) {
            info.hls_url.clone()
        } else {
            let codec = store::settings_snapshot().codec_pref.clone();
            let (v, _) = streams::pick_formats(info, 360, &codec, None);
            match v {
                Some(v) => streams::mpv_source(Some(v), None, info.length_secs),
                None => info.muxed.first().map(|m| m.url.clone()),
            }
        };
        let Some(src) = src else { return };
        let Some(m) = self.ensure_mpv(cx) else { return };
        self.ready = false;
        // The loadfile options moved from the third to the fourth argument in mpv 0.38.
        let _ = m.command(&["loadfile", &src, "replace", "-1", "pause=no"])
            .or_else(|_| m.command(&["loadfile", &src, "replace", "pause=no"]));
    }

    fn on_frame(&mut self, cx: &mut Context<Self>) {
        let Some(m) = &self.mpv else { return };
        let Some(f) = m.take_frame() else { return };
        if !self.ready {
            // a late frame of the previous video
            return;
        }
        self.pos = m.get_f64("time-pos").unwrap_or(self.pos);
        self.dur = m.get_f64("duration").unwrap_or(self.dur);
        if self.id.is_none() {
            return;
        }
        let Some(buf) = image::RgbaImage::from_raw(f.width, f.height, f.data) else { return };
        let img = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(buf)]));
        let first = self.frame.is_none();
        if let Some(old) = self.frame.replace(img) {
            self.old.push(old);
        }
        while self.old.len() > 1 {
            let o = self.old.remove(0);
            cx.drop_image(o, None);
        }
        if first {
            cx.emit(PreviewEvent::Changed);
        }
        cx.notify();
    }
}

/// Draws the preview frame and its seek bar; lives outside the page tree.
pub struct PreviewView {
    preview: Entity<Preview>,
    /// dragging along the seek bar
    scrubbing: bool,
    _sub: Subscription,
}

impl PreviewView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let preview = preview(cx);
        let sub = cx.observe(&preview, |_, _, cx| cx.notify());
        Self { preview, scrubbing: false, _sub: sub }
    }

    /// Fraction of the thumbnail's width at the pointer.
    fn fraction_at(x: Pixels, cx: &App) -> Option<f64> {
        let b = cx.global::<PreviewSlot>().0.get()?;
        let w = f32::from(b.size.width);
        (w > 0.0).then(|| (f32::from(x - b.origin.x) / w).clamp(0.0, 1.0) as f64)
    }

    fn seek_to(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if let Some(f) = Self::fraction_at(x, cx) {
            self.preview.update(cx, |p, cx| p.seek_fraction(f, cx));
        }
    }
}

fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

impl Render for PreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let radius = px(12. * store::settings_snapshot().thumb_radius / 12.);
        let accent = cx.try_global::<crate::ui::Pal>().map(|p| p.accent).unwrap_or(red());
        let (frame, pos, dur) = {
            let p = self.preview.read(cx);
            (p.frame.clone(), p.pos, p.dur)
        };
        let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let scrubbing = self.scrubbing;
        let fade_id = SharedString::from(format!("preview-in-{}", self.preview.read(cx).id.clone().unwrap_or_default()));
        let el = div().id("preview").group("preview").relative().size_full().rounded(radius).overflow_hidden().when_some(frame, |d, f| {
            d.bg(black())
                .child(img(ImageSource::Render(f)).size_full().rounded(radius).object_fit(ObjectFit::Cover))
                // time left, bottom right (like the duration badge it covers)
                .when(dur > 0.0, |d| {
                    d.child(
                        div()
                            .absolute()
                            .right(px(8.))
                            .bottom(px(14.))
                            .px_1()
                            .rounded_sm()
                            .bg(hsla(0., 0., 0., 0.75))
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(white())
                            .child(clock(dur - pos)),
                    )
                })
                // seek bar: click or drag to move through the video
                .when(dur > 0.0, |d| {
                    d.child(
                        div()
                            .id("preview-seek")
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(14.))
                            .flex()
                            .items_end()
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.scrubbing = true;
                                this.seek_to(e.position.x, cx);
                            }))
                            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                                if this.scrubbing {
                                    if e.pressed_button == Some(MouseButton::Left) {
                                        this.seek_to(e.position.x, cx);
                                    } else {
                                        this.scrubbing = false;
                                    }
                                }
                            }))
                            .on_mouse_up(MouseButton::Left, cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                cx.stop_propagation();
                                this.scrubbing = false;
                                cx.notify();
                            }))
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(
                                div()
                                    .w_full()
                                    .h(px(if scrubbing { 6. } else { 4. }))
                                    .group_hover("preview-seek-g", |s| s.h(px(6.)))
                                    .bg(hsla(0., 0., 1., 0.35))
                                    .child(div().h_full().w(relative(frac)).bg(accent)),
                            )
                            .group("preview-seek-g"),
                    )
                })
        });
        el.with_animation(fade_id, Animation::new(std::time::Duration::from_millis(200)).with_easing(ease_out_quint()), |d, t| d.opacity(t))
    }
}

/// Placed inside every thumbnail: records its bounds when it's the one previewed.
pub fn slot_marker(id: &str, cx: &App) -> impl IntoElement {
    let slot = cx.global::<PreviewSlot>().0.clone();
    let handle = cx.global::<PreviewHandle>().0.clone();
    let id = id.to_string();
    canvas(
        move |b, _, cx| {
            if handle.read(cx).id.as_deref() == Some(id.as_str()) {
                slot.set(Some(b));
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
