//! Music mode: listening the way YouTube Music does it. The player becomes a bar
//! along the bottom of the window on every page (instead of the floating mini
//! player), and the watch page becomes "Now playing": the song's cover art or
//! its video, with the queue beside it. It is offered when a mix starts and can
//! be left from the bar.
use crate::player::controller::{Player, PlayerHandle, player};
use crate::ui::app::{Route, navigate};
use crate::i18n::t as translate;
use crate::ui::{icon, pal, toast_with_actions};
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

pub const BAR_HEIGHT: f32 = 72.0;

#[derive(Clone, Default)]
pub struct MusicMode {
    pub on: bool,
    /// "Now playing" shows the video instead of the cover art.
    pub video: bool,
    /// The mix the current queue came from, to reopen "Now playing" with it.
    pub playlist: Option<String>,
    /// A list's id and title, shown as "Playing from" while the next song's
    /// page is still loading.
    pub playlist_title: Option<(String, String)>,
    /// Music mode was offered (or turned on) this session, so it isn't offered again.
    offered: bool,
}

impl Global for MusicMode {}

pub fn init(cx: &mut App) {
    cx.set_global(MusicMode::default());
}

pub fn state(cx: &App) -> MusicMode {
    cx.try_global::<MusicMode>().cloned().unwrap_or_default()
}

pub fn on(cx: &App) -> bool {
    cx.try_global::<MusicMode>().is_some_and(|m| m.on)
}

pub fn set(on: bool, cx: &mut App) {
    let m = cx.global_mut::<MusicMode>();
    m.on = on;
    m.offered |= on;
}

pub fn set_video(video: bool, cx: &mut App) {
    cx.global_mut::<MusicMode>().video = video;
}

/// Remembers the YouTube list a watch page opened with, for reopening "Now playing".
pub fn note_playlist(playlist: Option<&str>, cx: &mut App) {
    if let Some(list) = playlist.filter(|l| !l.starts_with("local:")) {
        if cx.global::<MusicMode>().playlist.as_deref() != Some(list) {
            cx.global_mut::<MusicMode>().playlist = Some(list.to_string());
        }
    }
}

/// Remembers a YouTube list's title for "Playing from".
pub fn note_playlist_title(playlist: &str, title: &str, cx: &mut App) {
    let known = cx.global::<MusicMode>().playlist_title.as_ref().is_some_and(|(id, t)| id == playlist && t == title);
    if !known {
        cx.global_mut::<MusicMode>().playlist_title = Some((playlist.to_string(), title.to_string()));
    }
}

/// When a mix starts, offers music mode once per session.
pub fn offer(cx: &mut App) {
    let m = cx.global::<MusicMode>();
    if m.on || m.offered {
        return;
    }
    cx.global_mut::<MusicMode>().offered = true;
    toast_with_actions(
        cx,
        translate("Listening to music? Music mode shows the cover art and keeps the player at the bottom."),
        vec![(translate("Music mode").into(), Arc::new(|_: &mut Window, cx: &mut App| set(true, cx)) as Arc<dyn Fn(&mut Window, &mut App)>)],
    );
}

fn clock(secs: f64) -> String {
    crate::api::json::format_duration(secs.max(0.0) as u64)
}

/// The cover art for a video: the full-resolution frame, else the medium one
/// (both 16:9 without letterbox bars, cropped to the square they're shown in).
pub fn cover(id: &str, radius: Pixels) -> Img {
    let medium = SharedString::from(format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"));
    img(SharedString::from(format!("https://i.ytimg.com/vi/{id}/maxresdefault.jpg")))
        .size_full()
        .rounded(radius)
        .object_fit(ObjectFit::Cover)
        .with_fallback(move || img(medium.clone()).size_full().rounded(radius).object_fit(ObjectFit::Cover).into_any_element())
}

/// What the bar shows; it only re-renders when this changes, not on every video frame.
#[derive(Clone, PartialEq)]
struct Shown {
    on: bool,
    video: Option<(String, String, String)>,
    second: u64,
    duration: u64,
    paused: bool,
    looping: bool,
    muted: bool,
    can_next: bool,
}

impl Shown {
    fn of(p: &Player, on: bool) -> Self {
        Shown {
            on,
            video: p.video.as_ref().map(|v| (v.id.clone(), v.title.clone(), v.channel_name.clone())),
            second: p.time.max(0.0) as u64,
            duration: p.duration.max(0.0) as u64,
            paused: p.paused,
            looping: p.loop_video,
            muted: p.muted,
            can_next: p.queue_pos.is_some_and(|i| i + 1 < p.queue.len()) || p.autoplay_next.is_some(),
        }
    }
}

/// The player bar along the bottom of the window in music mode.
pub struct MusicBar {
    observing: Option<(EntityId, Subscription)>,
    shown: Option<Shown>,
    /// Where the progress strip is, to turn a click into a position.
    track: Rc<Cell<Option<Bounds<Pixels>>>>,
    _subs: Vec<Subscription>,
}

impl MusicBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.observe_global::<PlayerHandle>(|this, cx| this.follow(cx)),
            cx.observe_global::<MusicMode>(|this, cx| this.changed(cx)),
        ];
        let mut bar = Self { observing: None, shown: None, track: Rc::default(), _subs: subs };
        bar.follow(cx);
        bar
    }

    fn follow(&mut self, cx: &mut Context<Self>) {
        let active = player(cx);
        if self.observing.as_ref().map(|(id, _)| *id) != Some(active.entity_id()) {
            let sub = cx.observe(&active, |this, _, cx| this.changed(cx));
            self.observing = Some((active.entity_id(), sub));
            self.changed(cx);
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        let shown = Shown::of(player(cx).read(cx), on(cx));
        if self.shown.as_ref() != Some(&shown) {
            self.shown = Some(shown);
            cx.notify();
        }
    }
}

fn button(id: &'static str, name: &'static str, size: Pixels, color: Hsla, cx: &App) -> Stateful<Div> {
    let p = pal(cx);
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size + px(16.))
        .rounded_full()
        .cursor_pointer()
        .hover(move |s| s.bg(p.hover))
        .child(icon(name).size(size).text_color(color))
}

impl Render for MusicBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let Some(shown) = self.shown.clone().filter(|s| s.on) else { return div().into_any_element() };
        let Some((id, title, channel)) = shown.video.clone() else { return div().into_any_element() };
        let frac = if shown.duration > 0 { (shown.second as f32 / shown.duration as f32).clamp(0.0, 1.0) } else { 0.0 };
        let track = self.track.clone();
        let progress = div()
            .id("music-progress")
            .absolute()
            .top(px(-6.))
            .left_0()
            .right_0()
            .h(px(12.))
            .cursor_pointer()
            .group("music-progress")
            .child(canvas(move |b, _, _| track.set(Some(b)), |_, _, _, _| {}).absolute().size_full())
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(5.))
                    .h(px(2.))
                    .group_hover("music-progress", |s| s.top(px(4.)).h(px(4.)))
                    .bg(p.border)
                    .child(div().h_full().w(relative(frac)).bg(p.accent)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    let Some(b) = this.track.get() else { return };
                    let f = f32::from(ev.position.x - b.origin.x) / f32::from(b.size.width).max(1.0);
                    player(cx).update(cx, |p, cx| {
                        let to = p.duration * f.clamp(0.0, 1.0) as f64;
                        p.seek_interactive(to);
                        cx.notify();
                    });
                }),
            );

        let transport = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_none()
            .child(button("music-prev", "skip-back", px(20.), p.text, cx).on_click(|_, window, cx| crate::ui::app::play_previous(window, cx)))
            .child(button("music-play", if shown.paused { "play" } else { "pause" }, px(28.), p.text, cx).on_click(|_, _, cx| {
                player(cx).update(cx, |p, cx| {
                    p.toggle_pause(cx);
                    cx.notify();
                })
            }))
            .child(
                button("music-next", "skip-forward", px(20.), if shown.can_next { p.text } else { p.text3 }, cx)
                    .on_click(|_, window, cx| crate::ui::app::play_next(window, cx)),
            )
            .child(div().pl_2().text_xs().text_color(p.text2).child(format!("{} / {}", clock(shown.second as f64), clock(shown.duration as f64))));

        let open_id = id.clone();
        let expand_id = id.clone();
        let on_watch = cx
            .try_global::<crate::ui::app::AppHandle>()
            .and_then(|h| h.0.upgrade())
            .is_some_and(|a| matches!(a.read(cx).route, Route::Watch { .. }));
        let info = div()
            .id("music-now")
            .flex()
            .min_w_0()
            .items_center()
            .gap_3()
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                let playlist = state(cx).playlist;
                navigate(Route::Watch { id: open_id.clone(), playlist, start: None }, window, cx);
            })
            .child(div().size(px(48.)).flex_none().rounded_md().overflow_hidden().bg(p.surface2).child(cover(&id, px(6.))))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .max_w(px(420.))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).truncate().child(title))
                    .child(div().text_xs().text_color(p.text2).truncate().child(channel)),
            );
        // like / dislike, as on the watch page: kept here and sent to the signed-in account
        let rating = crate::store::rating(&id);
        let rate = |button_id: &'static str, name: &'static str, value: i8| {
            let id = id.clone();
            button(button_id, name, px(20.), if rating == value { p.accent } else { p.text2 }, cx).on_click(cx.listener(move |_, _, _, cx| {
                crate::ui::pages::watch::set_rating(&id, if rating == value { 0 } else { value }, cx);
                cx.notify();
            }))
        };
        let now_playing = div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .justify_center()
            .gap_2()
            .child(info)
            .child(div().flex().flex_none().child(rate("music-like", "thumbs-up", 1)).child(rate("music-dislike", "thumbs-down", -1)));

        let extras = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_none()
            .child(button("music-loop", if shown.looping { "repeat-1" } else { "repeat" }, px(18.), if shown.looping { p.accent } else { p.text2 }, cx).on_click(|_, _, cx| {
                player(cx).update(cx, |p, cx| {
                    let l = !p.loop_video;
                    p.set_loop(l);
                    cx.notify();
                })
            }))
            .child(button("music-shuffle", "shuffle", px(18.), p.text2, cx).on_click(|_, _, cx| {
                player(cx).update(cx, |p, cx| {
                    if p.queue.len() > 1 {
                        p.shuffle_queue(cx);
                    }
                })
            }))
            .child(button("music-mute", if shown.muted { "volume-x" } else { "volume-2" }, px(18.), p.text2, cx).on_click(|_, _, cx| {
                player(cx).update(cx, |p, cx| {
                    p.toggle_mute();
                    cx.notify();
                })
            }))
            // opens "Now playing", or goes back from it
            .child(button("music-expand", if on_watch { "chevron-down" } else { "chevron-up" }, px(20.), p.text, cx).on_click(move |_, window, cx| {
                if on_watch {
                    crate::ui::app::go_back(window, cx);
                } else {
                    let playlist = state(cx).playlist;
                    navigate(Route::Watch { id: expand_id.clone(), playlist, start: None }, window, cx);
                }
            }))
            .child(button("music-exit", "x", px(16.), p.text3, cx).on_click(|_, _, cx| set(false, cx)));

        div()
            .id("music-bar")
            .relative()
            .flex_none()
            .w_full()
            .h(px(BAR_HEIGHT))
            .px_4()
            .flex()
            .items_center()
            .gap_4()
            .bg(p.surface)
            .border_t_1()
            .border_color(p.border)
            .child(progress)
            .child(transport)
            .child(now_playing)
            .child(extras)
            .into_any_element()
    }
}
