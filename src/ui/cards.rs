//! Video / channel / playlist cards in YouTube desktop style.
use super::*;
use crate::i18n::{t, tf};
use crate::api::types::*;
use crate::store;
use crate::ui::app::{MenuItem, Route, navigate, open_in_new_tab, open_menu};
use crate::ui::widgets::has_text_selection;
use gpui_kit::component::tooltip::Tooltip;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ───────────────────────── watched progress cache ─────────────────────────

#[derive(Default)]
pub struct Watched(pub HashMap<String, (f64, f64)>);
impl Global for Watched {}

pub fn watched_progress(id: &str, cx: &App) -> Option<f32> {
    let (p, d) = *cx.try_global::<Watched>()?.0.get(id)?;
    if d > 0.0 {
        Some((p / d).clamp(0.0, 1.0) as f32)
    } else {
        Some(0.0)
    }
}

pub fn is_watched(id: &str, cx: &App) -> bool {
    cx.try_global::<Watched>().map(|w| w.0.contains_key(id)).unwrap_or(false)
}

// ───────────────────────── DeArrow lazy lookups ─────────────────────────

fn pending() -> &'static Mutex<HashSet<String>> {
    static P: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// Returns (title, thumbnail) possibly replaced by DeArrow.
pub fn branded(v: &VideoItem, cx: &mut App) -> (String, String) {
    let s = store::settings_snapshot();
    if !s.dearrow_titles && !s.dearrow_thumbnails {
        return (v.title.clone(), v.thumbnail.clone());
    }
    match crate::api::services::branding_cached(&v.id) {
        Some(Some(b)) => {
            let title = if s.dearrow_titles { b.title.clone().unwrap_or(v.title.clone()) } else { v.title.clone() };
            let thumb = match (s.dearrow_thumbnails, b.thumbnail_time) {
                (true, Some(t)) => crate::api::services::dearrow_thumbnail_url(&v.id, t),
                _ => v.thumbnail.clone(),
            };
            (title, thumb)
        }
        Some(None) => (v.title.clone(), v.thumbnail.clone()),
        None => {
            let id = v.id.clone();
            if pending().lock().insert(id.clone()) {
                cx.spawn(async move |cx| {
                    let id2 = id.clone();
                    blocking(move || crate::api::services::dearrow_branding(&id2)).await;
                    // cached now; dropping the mark lets it be looked up again if the cache is trimmed
                    pending().lock().remove(&id);
                    let _ = cx.update(|cx| cx.refresh_windows());
                })
                .detach();
            }
            (v.title.clone(), v.thumbnail.clone())
        }
    }
}

// ───────────────────────── shared pieces ─────────────────────────

pub fn meta_line(v: &VideoItem) -> String {
    let s = store::settings_snapshot();
    let mut parts = Vec::new();
    if !s.hide_view_counts {
        if let Some(vw) = &v.views_text {
            parts.push(vw.clone());
        }
    }
    if v.is_live {
        if parts.is_empty() {
            parts.push(t("LIVE").into());
        }
    } else if let Some(p) = &v.published_text {
        parts.push(p.clone());
    } else if let Some(ts) = v.published_ts {
        parts.push(crate::api::json::relative_from_ts(ts));
    }
    parts.join(", ")
}

fn badge(v: &VideoItem, cx: &App) -> Option<AnyElement> {
    let _ = cx;
    let (text, bg): (String, Hsla) = if v.is_live {
        (t("LIVE").into(), rgb(0xcc0000).into())
    } else if v.is_upcoming {
        (t("UPCOMING").into(), hsla(0., 0., 0., 0.8))
    } else if v.is_short && v.duration_text.is_none() {
        ("SHORTS".into(), hsla(0., 0., 0., 0.8))
    } else {
        (v.duration_text.clone()?, hsla(0., 0., 0., 0.8))
    };
    Some(
        div()
            .absolute()
            .bottom_1()
            .right_1()
            .px_1()
            .rounded_sm()
            .bg(bg)
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(white())
            .child(text)
            .into_any_element(),
    )
}

pub fn thumbnail(v: &VideoItem, url: String, w: Pixels, h: Pixels, radius: Pixels, cx: &App) -> Div {
    let p = pal(cx);
    let progress = watched_progress(&v.id, cx);
    let radius = radius * thumbnail_corner_scale();
    div()
        .relative()
        .w(w)
        .h(h)
        .flex_none()
        .rounded(radius)
        .overflow_hidden()
        .bg(p.surface)
        // the card's ratio: otherwise the image's own (4:3 for letterboxed
        // sddefault/hqdefault) wins over the size, the image ends up taller than
        // the card and shows top-aligned with the bottom cut off
        // overflow_hidden clips to a rectangle, so the image rounds its own corners
        .child(img(SharedString::from(url)).size_full().rounded(radius).aspect_ratio(w / h).object_fit(ObjectFit::Cover))
        .child(crate::player::preview::slot_marker(&v.id, cx))
        .children(badge(v, cx))
        .when_some(progress.filter(|p| *p > 0.01), |d, pr| {
            d.child(
                // a rectangle would stick out past the rounded bottom corners, so the bar
                // stops short of them
                div()
                    .absolute()
                    .bottom_0()
                    .left(radius * 0.5)
                    .right(radius * 0.5)
                    .h(px(4.))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(hsla(0., 0., 0.6, 0.6))
                    .child(div().h_full().w(relative(pr)).rounded_full().bg(p.accent)),
            )
        })
}

pub fn download_menu_items(video: &VideoItem, anchor: Point<Pixels>) -> Vec<MenuItem> {
    let mut items: Vec<_> = crate::downloads::FORMATS
        .iter()
        .map(|&(format, label)| {
            let video = video.clone();
            let icon = if format == "audio" { "music" } else { "film" };
            MenuItem::new(icon, t(label), move |_, cx| {
                crate::downloads::start(video.clone(), Some(format.into()), cx);
            })
        })
        .collect();
    for &(format, label) in crate::downloads::TRANSCRIPTS {
        let video = video.clone();
        items.push(MenuItem::new("captions", t(label), move |window, cx| {
            super::transcript_menu::open_picker(anchor, &video, format, window, cx);
        }));
    }
    items
}

pub fn video_menu_items(v: &VideoItem) -> Vec<MenuItem> {
    video_menu_items_with(v, true)
}

fn video_menu_items_with(v: &VideoItem, save_to_watch_later: bool) -> Vec<MenuItem> {
    let mut items = Vec::new();
    {
        let route = if v.is_short { Route::Shorts(Some(v.id.clone())) } else { watch_route(v) };
        items.push(MenuItem::new("app-window", t("Open in new tab"), move |window, cx| {
            crate::ui::app::open_in_new_tab(route.clone(), false, window, cx)
        }));
    }
    let v1 = v.clone();
    items.push(MenuItem::new("list-plus", t("Add to queue"), move |_, cx| {
        crate::player::controller::enqueue(v1.clone(), cx);
        toast(cx, t("Added to queue"));
    }));
    let v2 = v.clone();
    if save_to_watch_later {
        items.push(MenuItem::new("clock", t("Save to Watch later"), move |_, cx| {
            let local = store::playlists().into_iter().find(|p| p.protected && p.name == "Watch later");
            if let Some(wl) = &local {
                store::playlist_add(wl.id, &v2);
            }
            if let Some(session) = crate::api::auth::session_identity() {
                let id = v2.id.clone();
                let saved_locally = local.is_some();
                cx.spawn(async move |cx| {
                    let result = blocking(move || crate::api::auth::set_account_playlist_saved(&session, "WL", &id, true)).await;
                    let _ = cx.update(|cx| match result {
                        Ok(()) => toast(cx, t("Saved to Watch later")),
                        Err(_) if saved_locally => toast(cx, t("Saved locally. Couldn't save to YouTube Watch later")),
                        Err(_) => toast(cx, t("Couldn't save to that YouTube playlist")),
                    });
                }).detach();
            } else if local.is_some() {
                toast(cx, t("Saved to Watch later"));
            }
        }));
    }
    let v3 = v.clone();
    items.push(MenuItem::new("bookmark", t("Save to playlist…"), move |window, cx| {
        crate::ui::app::open_save_dialog(v3.clone(), window, cx);
    }));
    let v4 = v.clone();
    items.push(MenuItem::new("download", t("Download"), move |_, cx| {
        crate::downloads::start(v4.clone(), None, cx);
    }));
    let v5 = v.clone();
    items.push(MenuItem::new("external-link", t("Open in external player"), move |_, cx| {
        crate::downloads::open_external(&v5.id, cx);
    }));
    let title_copy = if store::settings_snapshot().dearrow_titles {
        crate::api::services::branding_cached(&v.id).flatten().and_then(|b| b.title).unwrap_or_else(|| v.title.clone())
    } else {
        v.title.clone()
    };
    if !title_copy.is_empty() {
        items.push(MenuItem::new("copy", t("Copy title"), move |_, cx| {
            copy_text_to_clipboard(title_copy.clone(), cx);
        }));
    }
    let id = v.id.clone();
    items.push(MenuItem::new("link", t("Copy link"), move |_, cx| {
        copy_to_clipboard(format!("https://www.youtube.com/watch?v={id}"), cx);
    }));
    let id2 = v.id.clone();
    items.push(MenuItem::new("globe", t("Open on YouTube"), move |_, _| {
        let _ = open::that(format!("https://www.youtube.com/watch?v={id2}"));
    }));
    let hid_id = v.id.clone();
    let hid_title = v.title.clone();
    items.push(MenuItem::new("eye-off", t("Not interested"), move |_, cx| {
        let (id, title) = (hid_id.clone(), hid_title.clone());
        store::update_settings(|s| {
            if !s.hidden_videos.iter().any(|(i, _)| *i == id) {
                s.hidden_videos.push((id, title));
            }
        });
        toast(cx, t("Video hidden. Manage in Settings → Content filters"));
        cx.refresh_windows();
    }));
    if let Some(cid) = v.channel_id.clone() {
        let name = v.channel_name.clone();
        items.push(MenuItem::new("ban", t("Don't recommend channel"), move |_, cx| {
            let (cid, name) = (cid.clone(), name.clone());
            store::update_settings(|s| {
                if !s.blocked_channels.iter().any(|(i, _)| *i == cid) {
                    s.blocked_channels.push((cid, name));
                }
            });
            toast(cx, t("Channel blocked. Manage in Settings → Content filters"));
            cx.refresh_windows();
        }));
    }
    if is_watched_in_history(&v.id) {
        let id3 = v.id.clone();
        items.push(MenuItem::new("eye-off", t("Remove from history"), move |_, cx| {
            store::history_remove(&id3);
            cx.default_global::<Watched>().0.remove(&id3);
            cx.refresh_windows();
        }));
    }
    items
}

fn is_watched_in_history(id: &str) -> bool {
    store::progress_map().contains_key(id)
}

fn more_button(id: SharedString, v: &VideoItem, cx: &App) -> Stateful<Div> {
    more_button_with(id, v, MenuExtra::default(), cx)
}

/// Extra items for a card's menu.
#[derive(Clone, Default)]
pub struct MenuExtra {
    pub items: Vec<MenuItem>,
    /// the card is in Watch later: the items take the place of "Save to Watch later"
    pub replaces_watch_later: bool,
}

impl From<Vec<MenuItem>> for MenuExtra {
    fn from(items: Vec<MenuItem>) -> Self {
        Self { items, replaces_watch_later: false }
    }
}

/// The video's menu; `extra` items go in after "Save to playlist…", or where
/// "Save to Watch later" would be.
fn menu_with(v: &VideoItem, extra: &MenuExtra) -> Vec<MenuItem> {
    let mut items = video_menu_items_with(v, !extra.replaces_watch_later);
    let at = items.len().min(if extra.replaces_watch_later { 2 } else { 4 });
    items.splice(at..at, extra.items.iter().cloned());
    items
}

/// A right click on a card opens the same menu as its three-dots button.
fn right_click_menu(v: &VideoItem, extra: MenuExtra) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
    let v = v.clone();
    move |ev, window, cx| {
        cx.stop_propagation();
        let selected = gpui_kit::base::TextSelection::selected_text(window, cx).trim().to_string();
        let mut items = Vec::new();
        if !selected.is_empty() {
            items.push(MenuItem::new("copy", t("Copy"), move |_, cx| {
                copy_text_to_clipboard(selected.clone(), cx);
            }));
        }
        items.extend(menu_with(&v, &extra));
        open_menu(ev.position, items, window, cx);
    }
}

/// The lighter rounded background that appears behind a card under the pointer. It
/// reaches past the card's edges so the thumbnail isn't flush with it, as on YouTube.
fn hover_backdrop(cx: &App) -> impl IntoElement {
    let p = pal(cx);
    div()
        .id("card-hover")
        .absolute()
        .top(px(-8.))
        .bottom(px(-8.))
        .left(px(-8.))
        .right(px(-8.))
        .rounded_xl()
        .bg(p.surface)
        .invisible()
        .group_hover("card", |s| s.visible())
}

/// The three-dots button, shown while the pointer is over its card; `extra` items go
/// into the menu after "Save to playlist…".
fn more_button_with(id: SharedString, v: &VideoItem, extra: MenuExtra, cx: &App) -> Stateful<Div> {
    let p = pal(cx);
    let v = v.clone();
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size_8()
        .rounded_full()
        .cursor_pointer()
        .invisible()
        .group_hover("card", |s| s.visible())
        .hover(move |s| s.bg(p.hover))
        .child(icon("ellipsis-vertical").size_4().text_color(p.text))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |ev, window, cx| {
            cx.stop_propagation();
            open_menu(ev.position(), menu_with(&v, &extra), window, cx);
        })
}

fn watch_route(v: &VideoItem) -> Route {
    match &v.playlist_id {
        // a show's own list ("OL…") plays the latest episode with the episodes queued,
        // a YouTube Mix ("RD…") the song with the radio queued after it
        Some(list) if list.starts_with("OL") || list.starts_with("RD") => {
            Route::Watch { id: v.id.clone(), playlist: Some(list.clone()), start: None }
        }
        // any other show opens its page, a list of episodes, like on YouTube
        Some(list) => Route::Playlist(list.clone()),
        None => Route::Watch { id: v.id.clone(), playlist: None, start: None },
    }
}

fn video_click(
    route: Route,
    on_click: Option<Arc<dyn Fn(&mut Window, &mut App)>>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |event, window, cx| {
        if has_text_selection(window, cx) {
            return;
        }
        if cfg!(target_os = "macos") && event.modifiers().platform {
            cx.stop_propagation();
            open_in_new_tab(route.clone(), false, window, cx);
        } else if let Some(action) = &on_click {
            action(window, cx);
        } else {
            navigate(route.clone(), window, cx);
        }
    }
}

fn video_title(id: impl Into<ElementId>, title: String) -> Stateful<Div> {
    let title = SharedString::from(title);
    let full_title = title.clone();
    div()
        .id("video-title")
        .text_ellipsis().line_clamp(2)
        .tooltip(move |window, cx| Tooltip::new(full_title.clone()).max_w(px(420.)).build(window, cx))
        .child(gpui_kit::base::SelectableText::new(id, title))
}

fn middle_click_tab(route: Route) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |event, window, cx| {
        if let ClickEvent::Mouse(click) = event {
            if click.down.button == MouseButton::Middle && click.up.button == MouseButton::Middle {
                cx.stop_propagation();
                open_in_new_tab(route.clone(), false, window, cx);
            }
        }
    }
}

// ───────────────────────── grid card (home / subscriptions / channel) ─────────────────────────

/// Starts resolving a video's streams when the pointer rests on its thumbnail,
/// so playback begins almost immediately on click.
fn prefetch_hover(id: &str) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    let id = id.to_string();
    move |hovered, _, cx| {
        if *hovered {
            crate::api::streams::prefetch_on_hover(&id);
        }
        crate::player::preview::hover(&id, *hovered, cx);
    }
}

pub fn video_card(v: &VideoItem, width: Pixels, show_channel: bool, cx: &mut App) -> AnyElement {
    if v.is_short {
        return shorts_card(v, width.min(px(240.)), cx);
    }
    let p = pal(cx);
    let (title, thumb) = branded(v, cx);
    let h = width * (9. / 16.);
    let route = watch_route(v);
    let channel_route = v.channel_id.clone().map(Route::Channel);
    let meta = meta_line(v);
    let show_avatar = show_channel && !store::settings_snapshot().hide_channel_avatars;
    div()
        .id(SharedString::from(format!("vc-{}", v.id)))
        .on_hover(prefetch_hover(&v.id))
        .relative()
        .flex()
        .flex_col()
        .w(width)
        .gap_3()
        .cursor_pointer()
        .group("card")
        .on_aux_click(middle_click_tab(route.clone()))
        .on_mouse_down(MouseButton::Right, right_click_menu(v, MenuExtra::default()))
        .on_click(video_click(route, None))
        .child(hover_backdrop(cx))
        .child(thumbnail(v, thumb, width, h, px(12.), cx))
        .child(
            div()
                .flex()
                .gap_3()
                .w_full()
                .when(show_avatar, |d| {
                    let cr = channel_route.clone();
                    d.child(
                        div()
                            .id(SharedString::from(format!("va-{}", v.id)))
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                if let Some(r) = cr.clone() {
                                    navigate(r, window, cx);
                                }
                            })
                            .child(avatar(v.channel_avatar.as_deref(), px(36.), &v.channel_name, cx)),
                    )
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            video_title(SharedString::from(format!("vct-{}", v.id)), title)
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text),
                        )
                        .when(show_channel && !v.channel_name.is_empty(), |d| {
                            let cr = channel_route.clone();
                            d.child(
                                div()
                                    .id(SharedString::from(format!("vn-{}", v.id)))
                                    .text_sm()
                                    .text_color(p.text2)
                                    .hover(move |s| s.text_color(p.text))
                                    .truncate()
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        if has_text_selection(window, cx) {
                                            return;
                                        }
                                        if let Some(r) = cr.clone() {
                                            navigate(r, window, cx);
                                        }
                                    })
                                    .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("vcn-{}", v.id)), v.channel_name.clone())),
                            )
                        })
                        .child(div().text_sm().text_color(p.text2).truncate().child(meta)),
                )
                .child(more_button(SharedString::from(format!("vm-{}", v.id)), v, cx)),
        )
        .into_any_element()
}

pub fn shorts_card(v: &VideoItem, width: Pixels, cx: &mut App) -> AnyElement {
    let p = pal(cx);
    let (title, thumb) = branded(v, cx);
    let route = if store::settings_snapshot().shorts_in_player {
        watch_route(v)
    } else {
        Route::Shorts(Some(v.id.clone()))
    };
    let h = width * (16. / 9.);
    div()
        .id(SharedString::from(format!("sc-{}", v.id)))
        .relative()
        .flex()
        .flex_col()
        .w(width)
        .gap_2()
        .cursor_pointer()
        .group("card")
        .on_aux_click(middle_click_tab(route.clone()))
        .on_mouse_down(MouseButton::Right, right_click_menu(v, MenuExtra::default()))
        .on_click(video_click(route, None))
        .child(hover_backdrop(cx))
        .child(thumbnail(v, thumb, width, h, px(12.), cx))
        .child(
            div()
                .flex()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(video_title(SharedString::from(format!("sct-{}", v.id)), title).text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text))
                        .when_some(v.views_text.clone(), |d, vt| d.child(div().text_xs().text_color(p.text2).child(vt))),
                )
                .child(more_button(SharedString::from(format!("sm-{}", v.id)), v, cx)),
        )
        .into_any_element()
}

// ───────────────────────── list row (search results) ─────────────────────────

pub fn video_row(v: &VideoItem, thumb_w: Pixels, cx: &mut App) -> AnyElement {
    let p = pal(cx);
    let (title, thumb) = branded(v, cx);
    let route = watch_route(v);
    let channel_route = v.channel_id.clone().map(Route::Channel);
    let meta = meta_line(v);
    div()
        .id(SharedString::from(format!("vr-{}", v.id)))
        .on_hover(prefetch_hover(&v.id))
        .relative()
        .flex()
        .gap_4()
        .w_full()
        .cursor_pointer()
        .group("card")
        .on_aux_click(middle_click_tab(route.clone()))
        .on_mouse_down(MouseButton::Right, right_click_menu(v, MenuExtra::default()))
        .on_click(video_click(route, None))
        .child(hover_backdrop(cx))
        .child(thumbnail(v, thumb, thumb_w, thumb_w * (9. / 16.), px(12.), cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(video_title(SharedString::from(format!("vrt-{}", v.id)), title).flex_1().text_lg().text_color(p.text))
                        .child(more_button(SharedString::from(format!("vrm-{}", v.id)), v, cx)),
                )
                .child(div().text_xs().text_color(p.text2).child(meta))
                .when(!v.channel_name.is_empty(), |d| {
                    let cr = channel_route.clone();
                    d.child(
                        div()
                            .id(SharedString::from(format!("vrc-{}", v.id)))
                            .flex()
                            .items_center()
                            .gap_2()
                            .py_2()
                            .text_xs()
                            .text_color(p.text2)
                            .hover(move |s| s.text_color(p.text))
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                if has_text_selection(window, cx) {
                                    return;
                                }
                                if let Some(r) = cr.clone() {
                                    navigate(r, window, cx);
                                }
                            })
                            .child(avatar(v.channel_avatar.as_deref(), px(24.), &v.channel_name, cx))
                            .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("vrcn-{}", v.id)), v.channel_name.clone())),
                    )
                })
                .when_some(v.description_snippet.clone(), |d, s| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(p.text2)
                            .text_ellipsis().line_clamp(2)
                            .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("vrd-{}", v.id)), s)),
                    )
                }),
        )
        .into_any_element()
}

/// Compact playlist or queue row with its own menu actions.
pub fn compact_row_with(v: &VideoItem, active: bool, on_click: Option<Arc<dyn Fn(&mut Window, &mut App)>>, extra: impl Into<MenuExtra>, cx: &mut App) -> AnyElement {
    compact_row(v, size(px(168.), px(94.)), active, on_click, extra, cx)
}

/// A compact row with its thumbnail at `thumb_size`; `extra` items go into the video's menu.
pub fn compact_row(v: &VideoItem, thumb_size: Size<Pixels>, active: bool, on_click: Option<Arc<dyn Fn(&mut Window, &mut App)>>, extra: impl Into<MenuExtra>, cx: &mut App) -> AnyElement {
    let extra = extra.into();
    let p = pal(cx);
    let (title, thumb) = branded(v, cx);
    let thumb = if thumb == v.thumbnail {
        v.compact_thumbnail.clone().unwrap_or(thumb)
    } else {
        thumb
    };
    let route = watch_route(v);
    div()
        .id(SharedString::from(format!("cr-{}", v.id)))
        .on_hover(prefetch_hover(&v.id))
        .flex()
        .gap_2()
        .w_full()
        .p_1()
        .rounded_lg()
        .cursor_pointer()
        .group("card")
        .when(active, |d| d.bg(p.hover))
        .hover(move |s| s.bg(p.surface))
        .on_aux_click(middle_click_tab(route.clone()))
        .on_mouse_down(MouseButton::Right, right_click_menu(v, extra.clone()))
        .on_click(video_click(route, on_click))
        .child(thumbnail(v, thumb, thumb_size.width, thumb_size.height, px(8.), cx))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(video_title(SharedString::from(format!("crt-{}", v.id)), title).text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text))
                .child({
                    // the channel name opens the channel, like on youtube.com; the rest of the row opens the video
                    let cid = v.channel_id.clone();
                    div()
                        .id(SharedString::from(format!("crc-{}", v.id)))
                        .text_xs()
                        .text_color(p.text2)
                        .truncate()
                        .when(cid.is_some(), |d| d.cursor_pointer().hover(move |s| s.text_color(p.text)))
                        .on_click(move |_, window, cx| {
                            let Some(id) = cid.clone() else { return };
                            cx.stop_propagation();
                            if has_text_selection(window, cx) {
                                return;
                            }
                            navigate(Route::Channel(id), window, cx);
                        })
                        .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("crn-{}", v.id)), v.channel_name.clone()))
                })
                .child(div().text_xs().text_color(p.text2).truncate().child(meta_line(v))),
        )
        .child(more_button_with(SharedString::from(format!("crm-{}", v.id)), v, extra, cx))
        .into_any_element()
}

// ───────────────────────── channel & playlist ─────────────────────────

pub fn channel_row(c: &ChannelItem, thumb_w: Pixels, cx: &mut App) -> AnyElement {
    let p = pal(cx);
    let id = c.id.clone();
    let subscribed = store::is_subscribed(&c.id);
    let c2 = c.clone();
    div()
        .id(SharedString::from(format!("ch-{}", c.id)))
        .flex()
        .items_center()
        .gap_4()
        .w_full()
        .py_2()
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            if has_text_selection(window, cx) {
                return;
            }
            navigate(Route::Channel(id.clone()), window, cx)
        })
        .child(div().w(thumb_w).flex_none().flex().justify_center().child(avatar(c.avatar.as_deref(), px(136.).min(thumb_w), &c.name, cx)))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(div().text_lg().text_color(p.text).child(gpui_kit::base::SelectableText::new(SharedString::from(format!("chr-n-{}", c.id)), c.name.clone())))
                .child(
                    div().text_xs().text_color(p.text2).child(gpui_kit::base::SelectableText::new(
                        SharedString::from(format!("chr-m-{}", c.id)),
                        [c.handle.clone(), c.subscribers_text.clone().filter(|_| !store::settings_snapshot().hide_subscriber_counts), c.video_count_text.clone()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" • "),
                    )),
                )
                .when_some(c.description.clone(), |d, s| {
                    d.child(div().text_xs().text_color(p.text2).text_ellipsis().line_clamp(2).child(gpui_kit::base::SelectableText::new(SharedString::from(format!("chr-d-{}", c.id)), s)))
                }),
        )
        .child(subscribe_button(SharedString::from(format!("chs-{}", c.id)), &c2, subscribed, cx))
        .into_any_element()
}

/// Mirrors a subscribe/unsubscribe to the signed-in YouTube account.
pub fn sync_subscription(channel_id: String, on: bool, cx: &mut App) {
    if !crate::api::auth::signed_in() || channel_id.is_empty() {
        return;
    }
    cx.spawn(async move |cx| {
        let res = blocking(move || crate::api::auth::subscribe(&channel_id, on)).await;
        if res.is_err() {
            let _ = cx.update(|cx| toast(cx, t("Couldn't update the subscription on your YouTube account")));
        }
    })
    .detach();
}

pub fn subscribe_button(id: SharedString, c: &ChannelItem, subscribed: bool, cx: &App) -> AnyElement {
    let p = pal(cx);
    let c = c.clone();
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_2()
        .h_9()
        .px_4()
        .rounded_full()
        .cursor_pointer()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .when(subscribed, |d| d.bg(p.chip).text_color(p.text).hover(move |s| s.bg(p.hover)).child(icon("bell").size_4()).child(t("Subscribed")))
        .when(!subscribed, |d| d.bg(p.chip_active_bg).text_color(p.chip_active_fg).child(t("Subscribe")))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            let on = !store::is_subscribed(&c.id);
            if on {
                store::subscribe(&c.id, &c.name, c.avatar.as_deref());
                toast(cx, tf!("Subscribed to {0}", c.name));
            } else {
                store::unsubscribe(&c.id);
                toast(cx, tf!("Unsubscribed from {0}", c.name));
            }
            sync_subscription(c.id.clone(), on, cx);
            cx.refresh_windows();
        })
        .into_any_element()
}

pub fn channel_card(c: &ChannelItem, width: Pixels, cx: &mut App) -> AnyElement {
    let p = pal(cx);
    let id = c.id.clone();
    let subscribed = store::is_subscribed(&c.id);
    div()
        .id(SharedString::from(format!("cc-{}", c.id)))
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .w(width)
        .py_4()
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            if has_text_selection(window, cx) {
                return;
            }
            navigate(Route::Channel(id.clone()), window, cx)
        })
        .child(avatar(c.avatar.as_deref(), px(104.), &c.name, cx))
        .child(div().text_base().text_color(p.text).text_center().text_ellipsis().line_clamp(1).child(gpui_kit::base::SelectableText::new(SharedString::from(format!("ccn-{}", c.id)), c.name.clone())))
        .when_some(c.subscribers_text.clone().filter(|_| !store::settings_snapshot().hide_subscriber_counts), |d, s| d.child(div().text_xs().text_color(p.text2).child(s)))
        .child(subscribe_button(SharedString::from(format!("ccs-{}", c.id)), c, subscribed, cx))
        .into_any_element()
}

pub fn playlist_card(pl: &PlaylistItem, width: Pixels, cx: &mut App) -> AnyElement {
    let p = pal(cx);
    let id = pl.id.clone();
    let h = width * (9. / 16.);
    div()
        .id(SharedString::from(format!("pc-{}", pl.id)))
        .flex()
        .flex_col()
        .w(width)
        .gap_3()
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            if has_text_selection(window, cx) {
                return;
            }
            navigate(Route::Playlist(id.clone()), window, cx)
        })
        .child(playlist_thumb(pl.thumbnail.clone(), pl.video_count_text.clone(), width, h, cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .text_ellipsis().line_clamp(2)
                        .child(gpui_kit::base::SelectableText::new(SharedString::from(format!("pct-{}", pl.id)), pl.title.clone())),
                )
                .when_some(pl.channel_name.clone(), |d, c| d.child(div().text_sm().text_color(p.text2).truncate().child(c)))
                .child(div().text_sm().text_color(p.text2).child(t("View full playlist"))),
        )
        .into_any_element()
}

pub fn playlist_thumb(thumb: Option<String>, count: Option<String>, w: Pixels, h: Pixels, cx: &App) -> Div {
    let p = pal(cx);
    let scale = thumbnail_corner_scale();
    let radius = rems(0.75 * scale);
    div()
        .relative()
        .w(w)
        .h(h)
        .pt_2()
        .child(
            div()
                .absolute()
                .top_0()
                .left(px(12.))
                .right(px(12.))
                .h_4()
                .rounded_t(rems(0.5 * scale))
                .bg(p.hover),
        )
        .child(
            div()
                .relative()
                .size_full()
                .rounded(radius)
                .overflow_hidden()
                .bg(p.surface)
                .when_some(thumb, |d, t| d.child(img(SharedString::from(t)).size_full().rounded(radius).object_fit(ObjectFit::Cover)))
                .child(
                    div()
                        .absolute()
                        .bottom_1()
                        .right_1()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_1()
                        .rounded_sm()
                        .bg(hsla(0., 0., 0., 0.8))
                        .text_xs()
                        .text_color(white())
                        .child(icon("list-video").size_3().text_color(white()))
                        .child(count.unwrap_or_default()),
                ),
        )
}

pub fn playlist_row(pl: &PlaylistItem, thumb_w: Pixels, cx: &mut App) -> AnyElement {
    let p = pal(cx);
    let id = pl.id.clone();
    div()
        .id(SharedString::from(format!("pr-{}", pl.id)))
        .flex()
        .gap_4()
        .w_full()
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            if has_text_selection(window, cx) {
                return;
            }
            navigate(Route::Playlist(id.clone()), window, cx)
        })
        .child(playlist_thumb(pl.thumbnail.clone(), pl.video_count_text.clone(), thumb_w, thumb_w * (9. / 16.), cx))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(div().text_lg().text_color(p.text).text_ellipsis().line_clamp(2).child(gpui_kit::base::SelectableText::new(SharedString::from(format!("prt-{}", pl.id)), pl.title.clone())))
                .when_some(pl.channel_name.clone(), |d, c| d.child(div().text_xs().text_color(p.text2).child(c)))
                .child(div().text_xs().text_color(p.text2).font_weight(FontWeight::MEDIUM).child(t("View full playlist"))),
        )
        .into_any_element()
}

/// Renders any item in a grid.
pub fn grid_item(item: &Item, width: Pixels, show_channel: bool, cx: &mut App) -> AnyElement {
    match item {
        Item::Video(v) => video_card(v, width, show_channel, cx),
        Item::Channel(c) => channel_card(c, width, cx),
        Item::Playlist(p) => playlist_card(p, width, cx),
    }
}

/// Grid layout: computes columns from available width.
pub fn grid(items: Vec<AnyElement>, _avail: Pixels) -> Div {
    div().flex().flex_wrap().gap_x_4().gap_y(grid_row_gap()).w_full().children(items)
}

pub fn grid_row_gap() -> Pixels {
    px(match store::settings_snapshot().density.as_str() {
        "compact" => 20.,
        "comfortable" => 48.,
        _ => 36.,
    })
}

pub fn grid_columns(avail: Pixels, min_card: f32, gap: Pixels) -> (usize, Pixels) {
    let cfg = store::settings_snapshot().grid_columns as usize;
    let gap: f32 = gap.into();
    let w: f32 = avail.into();
    let cols = if cfg > 0 { cfg } else { (((w + gap) / (min_card + gap)).floor() as usize).clamp(1, 6) };
    let card = (w - gap * (cols as f32 - 1.0)) / cols as f32;
    (cols, px(card.floor() - 1.0))
}
