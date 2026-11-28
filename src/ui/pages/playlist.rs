use crate::api::innertube;
use crate::i18n::{t, tf};
use crate::api::types::*;
use crate::player::controller::player;
use crate::store;
use crate::ui::app::{Route, confirm, navigate, prompt_text};
use crate::ui::cards::*;
use crate::ui::widgets::*;
use crate::ui::*;

enum Source {
    Remote(String),
    Local(i64),
}

pub struct PlaylistPage {
    source: Source,
    title: String,
    owner: Option<(String, Option<String>)>,
    count_text: Option<String>,
    description: Option<String>,
    videos: Load<Vec<VideoItem>>,
    continuation: Option<String>,
    loading_more: bool,
    load_serial: u64,
    load_failed: bool,
    scroll: ScrollHandle,
    protected: bool,
    info: Option<PlaylistItem>,
    /// loaded through the signed-in account (continuations go there too)
    authed: bool,
    filter: Option<Entity<gpui_kit::component::input::InputState>>,
    /// 0 playlist order, 1 title, 2 longest, 3 shortest, 4 channel
    sort: u8,
}

impl PlaylistPage {
    pub fn remote(id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut s = Self::base(Source::Remote(id.clone()));
        s.make_filter(window, cx);
        cx.spawn(async move |this, cx| {
            let res = blocking(move || {
                let own = matches!(id.as_str(), "WL" | "LL") || id.starts_with("LL");
                let signed = crate::api::auth::signed_in();
                if own && signed {
                    return crate::api::auth::playlist(&id).map(|p| (p, true));
                }
                match innertube::playlist(&id) {
                    Ok(p) if !p.videos.is_empty() || !signed => Ok((p, false)),
                    // private / unlisted: only the owner's account can see it
                    other => crate::api::auth::playlist(&id).map(|p| (p, true)).or_else(|e| other.map(|p| (p, false)).map_err(|_| e)),
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok((p, authed)) => {
                        this.authed = authed;
                        this.title = p.info.title.clone();
                        this.owner = p.info.channel_name.clone().map(|n| (n, p.info.channel_id.clone()));
                        this.count_text = p.info.video_count_text.clone();
                        this.description = p.description.clone();
                        this.videos = Load::Ready(p.videos);
                        this.continuation = p.continuation;
                        this.info = Some(p.info);
                        this.start_loading_remaining(cx);
                    }
                    Err(e) => this.videos = Load::Error(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        s.title = "Playlist".into();
        s
    }

    pub fn local(id: i64, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut s = Self::base(Source::Local(id));
        s.make_filter(window, cx);
        s.reload_local();
        s
    }

    fn make_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let f = cx.new(|cx| gpui_kit::component::input::InputState::new(window, cx).placeholder(t("Search in playlist")));
        cx.observe(&f, |_, _, cx| cx.notify()).detach();
        self.filter = Some(f);
    }

    /// Videos after the search filter and sort, with their index in the playlist.
    fn visible(&self, cx: &App) -> Vec<(usize, VideoItem)> {
        let q = self.filter.as_ref().map(|f| f.read(cx).value().trim().to_lowercase()).unwrap_or_default();
        let mut v: Vec<(usize, VideoItem)> = self
            .videos
            .ready()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter(|(_, v)| q.is_empty() || v.title.to_lowercase().contains(&q) || v.channel_name.to_lowercase().contains(&q))
            .collect();
        match self.sort {
            1 => v.sort_by(|a, b| a.1.title.to_lowercase().cmp(&b.1.title.to_lowercase())),
            2 => v.sort_by(|a, b| b.1.duration_secs.unwrap_or(0).cmp(&a.1.duration_secs.unwrap_or(0))),
            3 => v.sort_by(|a, b| a.1.duration_secs.unwrap_or(u64::MAX).cmp(&b.1.duration_secs.unwrap_or(u64::MAX))),
            4 => v.sort_by(|a, b| a.1.channel_name.to_lowercase().cmp(&b.1.channel_name.to_lowercase())),
            _ => {}
        }
        v
    }

    fn base(source: Source) -> Self {
        Self {
            source,
            title: String::new(),
            owner: None,
            count_text: None,
            description: None,
            videos: Load::Loading,
            continuation: None,
            loading_more: false,
            load_serial: 0,
            load_failed: false,
            scroll: ScrollHandle::new(),
            protected: false,
            info: None,
            authed: false,
            filter: None,
            sort: 0,
        }
    }

    fn reload_local(&mut self) {
        if let Source::Local(id) = self.source {
            let pl = store::playlists().into_iter().find(|p| p.id == id);
            self.title = pl.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "Playlist".into());
            self.protected = pl.as_ref().map(|p| p.protected).unwrap_or(false);
            let items = store::playlist_items(id);
            self.count_text = Some(tf!("{0} videos", items.len()));
            self.owner = Some(("You".into(), None));
            self.videos = Load::Ready(items);
        }
    }

    fn start_loading_remaining(&mut self, cx: &mut Context<Self>) {
        if self.loading_more || self.continuation.is_none() {
            return;
        }
        let first = self.continuation.clone();
        let authed = self.authed;
        self.load_serial += 1;
        let serial = self.load_serial;
        self.loading_more = true;
        self.load_failed = false;
        cx.spawn(async move |this, cx| {
            let mut pager = innertube::PlaylistPager::new(first, innertube::MAX_PLAYLIST_PAGES);
            while let Some(tok) = pager.take_next() {
                let res = blocking(move || innertube::playlist_continue(&tok, authed)).await;
                let keep_going = match res {
                    Ok((vids, next)) => {
                        pager.advance(next);
                        let next_tok = pager.next_token().map(str::to_string);
                        this.update(cx, |this, cx| {
                            if this.load_serial != serial {
                                return false;
                            }
                            if let Some(v) = this.videos.ready_mut() {
                                innertube::append_unique_videos(v, vids);
                            }
                            this.continuation = next_tok.clone();
                            // Pause while another page is on screen (e.g. Watch); rendering resumes it on Back.
                            let more = next_tok.is_some() && crate::ui::app::is_current_page(cx.entity_id(), cx);
                            if !more {
                                this.loading_more = false;
                            }
                            cx.notify();
                            more
                        })
                        .unwrap_or(false)
                    }
                    Err(_) => {
                        let _ = this.update(cx, |this, cx| {
                            if this.load_serial == serial {
                                this.loading_more = false;
                                this.load_failed = true;
                                cx.notify();
                            }
                        });
                        false
                    }
                };
                if !keep_going {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.load_serial == serial && this.loading_more {
                    this.loading_more = false;
                    this.continuation = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn is_watch_later(&self) -> bool {
        match &self.source {
            Source::Remote(id) => id == "WL",
            Source::Local(_) => self.protected && self.title == "Watch later",
        }
    }

    /// Like YouTube's "Remove watched videos": drops every video in the list that is in
    /// the watch history (the same "watched" as the feeds' Hide watched).
    fn remove_watched(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let authed = self.authed;
        let first = self.continuation.take();
        self.load_serial += 1;
        let serial = self.load_serial;
        self.loading_more = first.is_some();
        self.load_failed = false;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // the whole list, not only the pages loaded so far
            let mut more = Vec::new();
            let mut pager = innertube::PlaylistPager::new(first, innertube::MAX_PLAYLIST_PAGES);
            while let Some(tok) = pager.take_next() {
                match blocking(move || innertube::playlist_continue(&tok, authed)).await {
                    Ok((vids, next)) => {
                        more.extend(vids);
                        pager.advance(next);
                    }
                    Err(_) => break,
                }
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if this.load_serial != serial {
                    return;
                }
                this.loading_more = false;
                if let Some(v) = this.videos.ready_mut() {
                    innertube::append_unique_videos(v, more);
                }
                cx.notify();
                let watched: Vec<String> = this.videos.ready().map(|v| v.iter().filter(|v| is_watched(&v.id, cx)).map(|v| v.id.clone()).collect()).unwrap_or_default();
                if watched.is_empty() {
                    toast(cx, format!("No watched videos in {}", this.title));
                    return;
                }
                let n = watched.len();
                let body = format!("{n} watched {} will be removed from {}.", if n == 1 { "video" } else { "videos" }, this.title);
                let page = cx.entity().downgrade();
                confirm("Remove watched videos?", &body, "Remove", window, cx, move |_, cx| {
                    let ids = watched.clone();
                    let _ = page.update(cx, |this, cx| this.do_remove_watched(ids, cx));
                });
            });
        })
        .detach();
    }

    fn do_remove_watched(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let n = ids.len();
        let done = format!("Removed {n} watched {}", if n == 1 { "video" } else { "videos" });
        match &self.source {
            Source::Local(lid) => {
                for id in &ids {
                    store::playlist_remove(*lid, id);
                }
                self.reload_local();
                toast(cx, done);
                cx.notify();
            }
            Source::Remote(pid) => {
                let pid = pid.clone();
                cx.spawn(async move |this, cx| {
                    let ids2 = ids.clone();
                    let res = blocking(move || crate::api::auth::remove_many_from_playlist(&pid, &ids2)).await;
                    let _ = this.update(cx, |this, cx| {
                        match res {
                            Ok(()) => {
                                if let Some(v) = this.videos.ready_mut() {
                                    v.retain(|v| !ids.contains(&v.id));
                                }
                                // the whole list is loaded now, so its length is the count
                                this.count_text = None;
                                toast(cx, done);
                            }
                            Err(_) => toast(cx, t("Couldn't remove the watched videos on your YouTube account")),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    /// Takes one video out of the playlist: the local list, or the signed-in account's.
    fn remove_video(&mut self, video_id: String, cx: &mut Context<Self>) {
        match &self.source {
            Source::Local(lid) => {
                store::playlist_remove(*lid, &video_id);
                self.reload_local();
                toast(cx, t("Removed from playlist"));
                cx.notify();
            }
            Source::Remote(pid) => {
                let pid = pid.clone();
                cx.spawn(async move |this, cx| {
                    let ids = vec![video_id.clone()];
                    let res = blocking(move || crate::api::auth::remove_many_from_playlist(&pid, &ids)).await;
                    let _ = this.update(cx, |this, cx| {
                        match res {
                            Ok(()) => {
                                if let Some(v) = this.videos.ready_mut() {
                                    v.retain(|v| v.id != video_id);
                                }
                                // with the whole list loaded its length is the count
                                if this.continuation.is_none() {
                                    this.count_text = None;
                                }
                                toast(cx, t("Removed from playlist"));
                            }
                            Err(_) => toast(cx, t("Couldn't remove the video from your YouTube playlist")),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    fn playlist_param(&self) -> String {
        match &self.source {
            Source::Remote(id) => id.clone(),
            Source::Local(id) => format!("local:{id}"),
        }
    }

    fn play(&mut self, shuffle: bool, start_idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut q) = self.videos.ready().cloned() else { return };
        if q.is_empty() {
            return;
        }
        let mut idx = start_idx.min(q.len() - 1);
        if shuffle {
            use rand::seq::SliceRandom;
            q.shuffle(&mut rand::rng());
            idx = 0;
        }
        let first = q[idx].id.clone();
        let continuation = self.continuation.clone();
        let authed = self.authed;
        if continuation.is_some() {
            // Hand off remaining pages to the player so both do not fetch in parallel.
            self.load_serial += 1;
            self.loading_more = false;
        }
        player(cx).update(cx, |p, cx| {
            p.set_playlist_queue(q, idx, continuation, authed, shuffle, cx);
        });
        let list = if shuffle { None } else { Some(self.playlist_param()) };
        navigate(Route::Watch { id: first, playlist: list, start: None }, window, cx);
    }
}

impl Render for PlaylistPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(filter) = &self.filter {
            localize_placeholder(filter, "Search in playlist", window, cx);
        }
        let p = pal(cx);
        let has_more = self.continuation.is_some();
        if has_more && !self.loading_more {
            if !self.load_failed {
                cx.defer_in_entity(|this, cx| this.start_loading_remaining(cx));
            } else if near_bottom(&self.scroll, 900.0) {
                self.load_failed = false;
                cx.defer_in_entity(|this, cx| this.start_loading_remaining(cx));
            }
        }
        let videos = self.videos.ready().cloned().unwrap_or_default();
        let thumb = videos.first().map(|v| v.thumbnail.clone());
        let radius = rems(0.75 * thumbnail_corner_scale());
        let local_id = if let Source::Local(id) = self.source { Some(id) } else { None };
        let remote_info = self.info.clone();
        let saved = remote_info.as_ref().map(|i| store::is_playlist_saved(&i.id)).unwrap_or(false);
        let owner = self.owner.clone();
        let header = div()
            .flex()
            .flex_col()
            .flex_none()
            .gap_4()
            .w(px(360.))
            .p_6()
            .rounded_2xl()
            .bg(linear_gradient(180., linear_color_stop(p.hover, 0.), linear_color_stop(p.bg, 1.)))
            .child(
                div()
                    .w_full()
                    .h(px(176.))
                    .rounded(radius)
                    .overflow_hidden()
                    .bg(p.surface)
                    .when_some(thumb, |d, t| d.child(img(SharedString::from(t)).size_full().rounded(radius).object_fit(ObjectFit::Cover))),
            )
            .child(
                selectable_text(
                    "pl-title",
                    if self.protected || (local_id.is_none() && self.info.is_none()) {
                        t(&self.title).to_string()
                    } else {
                        self.title.clone()
                    },
                )
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .text_color(p.text),
            )
            .when_some(owner, |d, (name, cid)| {
                d.child(
                    div()
                        .id("pl-owner")
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .cursor_pointer()
                        .child(gpui_kit::base::SelectableText::new("pl-owner-name", if local_id.is_some() { t("You").to_string() } else { name }))
                        .on_click(move |_, window, cx| {
                            if has_text_selection(window, cx) {
                                return;
                            }
                            if let Some(c) = cid.clone() {
                                navigate(Route::Channel(c), window, cx)
                            }
                        }),
                )
            })
            .child({
                let total: u64 = videos.iter().filter_map(|v| v.duration_secs).sum();
                let count = self.count_text.clone().unwrap_or_else(|| tf!("{0} videos", videos.len()));
                let dur = if total >= 3600 { tf!("{0} h {1} min", total / 3600, (total % 3600) / 60) } else if total > 0 { tf!("{0} min", total / 60) } else { String::new() };
                selectable_text("pl-meta", if dur.is_empty() { count } else { format!("{count}, {dur}") }).text_xs().text_color(p.text2)
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(icon_button("pl-dl", "download", cx).bg(p.chip).on_click(cx.listener(|this, _, _, cx| {
                        let vids = this.videos.ready().cloned().unwrap_or_default();
                        crate::downloads::start_many(vids, cx);
                    })))
                    .child(icon_button("pl-share", "share-2", cx).bg(p.chip).on_click(cx.listener(|this, _, _, cx| {
                        if let Source::Remote(id) = &this.source {
                            copy_to_clipboard(format!("https://www.youtube.com/playlist?list={id}"), cx);
                        }
                    })))
                    .when_some(remote_info, |d, info| {
                        d.child(
                            icon_button("pl-save", if saved { "bookmark-check" } else { "bookmark-plus" }, cx).bg(p.chip).on_click(move |_, _, cx| {
                                store::save_playlist(&info, !saved);
                                toast(cx, if saved { "Removed from library" } else { "Saved to library" });
                                cx.refresh_windows();
                            }),
                        )
                    })
                    .when(self.is_watch_later(), |d| {
                        d.child(pill("pl-rm-watched", Some("list-x"), "Remove watched", cx).on_click(cx.listener(|this, _, window, cx| this.remove_watched(window, cx))))
                    })
                    .when_some(local_id.filter(|_| !self.protected), |d, id| {
                        d.child(icon_button("pl-rename", "pencil", cx).bg(p.chip).on_click(cx.listener(move |_, _, window, cx| {
                            prompt_text("Rename playlist", "Name", window, cx, move |name, _, cx| {
                                store::playlist_rename(id, &name);
                                cx.refresh_windows();
                            });
                        })))
                        .child(icon_button("pl-del", "trash-2", cx).bg(p.chip).on_click(move |_, window, cx| {
                            confirm("Delete playlist?", "This cannot be undone.", "Delete", window, cx, move |window, cx| {
                                store::playlist_delete(id);
                                navigate(Route::Library, window, cx);
                            });
                        }))
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        pill("pl-play", Some("play"), "Play all", cx)
                            .flex_1()
                            .justify_center()
                            .bg(p.chip_active_bg)
                            .text_color(p.chip_active_fg)
                            .on_click(cx.listener(|this, _, window, cx| this.play(false, 0, window, cx))),
                    )
                    .child(pill("pl-shuffle", Some("shuffle"), "Shuffle", cx).flex_1().justify_center().on_click(cx.listener(|this, _, window, cx| this.play(true, 0, window, cx)))),
            )
            .when_some(self.description.clone().filter(|d| !d.is_empty()), |d, desc| d.child(div().text_xs().text_color(p.text2).child(desc)));

        let list: AnyElement = match &self.videos {
            Load::Loading => spinner(cx).into_any_element(),
            Load::Error(e) => error_state(e, cx).into_any_element(),
            Load::Ready(v) if v.is_empty() => empty_state("list-video", "No videos in this playlist yet", "", cx).into_any_element(),
            Load::Ready(_) => {
                let shown = self.visible(cx);
                let sort = self.sort;
                let mut bar = div().flex().items_center().gap_2().pb_3().mb_1().border_b_1().border_color(p.border);
                if let Some(f) = &self.filter {
                    bar = bar.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .w(px(260.))
                            .px_3()
                            .h_8()
                            .rounded_full()
                            .bg(p.chip)
                            .child(icon("search").size_4().text_color(p.text2))
                            .child(div().flex_1().child(gpui_kit::component::input::Input::new(f).appearance(false).cleanable(true))),
                    );
                }
                for (k, label) in [(0u8, "Default"), (1, "Title"), (2, "Longest"), (3, "Shortest"), (4, "Channel")] {
                    bar = bar.child(chip(SharedString::from(format!("pl-sort-{k}")), t(label), sort == k, cx).on_click(cx.listener(move |this, _, _, cx| {
                        this.sort = k;
                        cx.notify();
                    })));
                }
                let mut col = div().flex().flex_col().gap_1().flex_1().min_w_0().child(bar);
                if shown.is_empty() {
                    col = col.child(div().py_8().text_sm().text_color(p.text2).child(t("No videos match")));
                }
                let vs = &shown;
                // a list of the user's own: local, or an account playlist the account can edit
                // (its Favorites can be public, so it may not have been loaded through the account)
                let account_favorites = matches!(&self.source, Source::Remote(id) if crate::ui::app::account_favorites_id(cx).as_ref() == Some(id));
                let editable = local_id.is_some() || self.authed || account_favorites;
                // saving to the list being shown makes no sense, so removing takes its place
                let in_watch_later = self.is_watch_later();
                for (i, v) in vs.iter().map(|(i, v)| (*i, v)) {
                    let extra = if editable {
                        let (page, rid) = (cx.entity().downgrade(), v.id.clone());
                        let label = if in_watch_later { "Remove from Watch later" } else { "Remove from playlist" };
                        MenuExtra {
                            items: vec![crate::ui::app::MenuItem::new("list-x", t(label), move |_, cx| {
                                let _ = page.update(cx, |this, cx| this.remove_video(rid.clone(), cx));
                            })],
                            replaces_watch_later: in_watch_later,
                        }
                    } else {
                        MenuExtra::default()
                    };
                    let row = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w_6().text_sm().text_color(p.text2).text_center().child((i + 1).to_string()))
                        .child(div().flex_1().min_w_0().child(compact_row_with(
                            v,
                            false,
                            Some({
                                let entity = cx.entity().downgrade();
                                Arc::new(move |window: &mut Window, cx: &mut App| {
                                    if let Some(e) = entity.upgrade() {
                                        e.update(cx, |page, cx| page.play(false, i, window, cx));
                                    }
                                })
                            }),
                            extra,
                            cx,
                        )))
                        .when_some(local_id, |d, lid| {
                            let (a, b, c) = (v.id.clone(), v.id.clone(), v.id.clone());
                            d.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(icon_button(SharedString::from(format!("up-{i}")), "chevron-up", cx).size_7().on_click(cx.listener(move |this, _, _, cx| {
                                        store::playlist_move(lid, &a, -1);
                                        this.reload_local();
                                        cx.notify();
                                    })))
                                    .child(icon_button(SharedString::from(format!("dn-{i}")), "chevron-down", cx).size_7().on_click(cx.listener(move |this, _, _, cx| {
                                        store::playlist_move(lid, &b, 1);
                                        this.reload_local();
                                        cx.notify();
                                    }))),
                            )
                            .child(icon_button(SharedString::from(format!("rm-{i}")), "trash-2", cx).on_click(cx.listener(move |this, _, _, cx| {
                                store::playlist_remove(lid, &c);
                                this.reload_local();
                                cx.notify();
                            })))
                        });
                    col = col.child(row);
                }
                if self.loading_more {
                    col = col.child(spinner(cx));
                }
                col.into_any_element()
            }
        };
        page_scroll("pl-scroll", &self.scroll).child(div().flex().items_start().gap_6().px_6().pt_6().pb_24().child(header).child(list))
    }
}
