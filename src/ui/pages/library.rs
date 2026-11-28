use crate::api::history::{Account, Entry};
use crate::i18n::{t as translate, tf};
use crate::store;
use crate::ui::app::{Route, confirm, navigate, prompt_text};
use crate::ui::cards::*;
use crate::ui::history::{AccountHistory, Snapshot, refresh_current};
use crate::ui::widgets::*;
use crate::ui::*;
use gpui_kit::component::input::{Input, InputState};
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq)]
enum HistTab {
    Account,
    Device,
    Searches,
}

pub struct HistoryPage {
    filter: Entity<InputState>,
    scroll: ScrollHandle,
    limit: usize,
    tab: HistTab,
    account: Option<Account>,
    snapshot: Option<Snapshot>,
    yt: Option<Load<Vec<Entry>>>,
    yt_cont: Option<String>,
    yt_loading_more: bool,
    yt_loaded_more: bool,
    yt_head: HashSet<String>,
    yt_revision: u64,
}

impl HistoryPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(translate("Search history")));
        cx.observe(&filter, |_, _, cx| cx.notify()).detach();
        let account = Account::current();
        let tab = if account.is_some() { HistTab::Account } else { HistTab::Device };
        let mut s = Self {
            filter,
            scroll: ScrollHandle::new(),
            limit: 100,
            tab,
            account,
            snapshot: None,
            yt: None,
            yt_cont: None,
            yt_loading_more: false,
            yt_loaded_more: false,
            yt_head: HashSet::new(),
            yt_revision: 0,
        };
        refresh_current(cx, true);
        s.update_snapshot(cx);
        cx.observe_global::<AccountHistory>(|this, cx| this.update_snapshot(cx)).detach();
        s
    }

    fn update_snapshot(&mut self, cx: &mut Context<Self>) {
        let account = Account::current();
        let key = account.as_ref().map(Account::key);
        if self.account.as_ref().map(Account::key) != key {
            self.account = account;
            self.snapshot = None;
            self.yt_cont = None;
            self.yt_loading_more = false;
            self.yt_loaded_more = false;
            self.yt_head.clear();
            self.yt_revision += 1;
            self.yt = None;
            if key.is_none() && self.tab == HistTab::Account {
                self.tab = HistTab::Device;
            }
        }
        if let Some(snapshot) = cx.try_global::<AccountHistory>()
            .filter(|state| state.account_key == key)
            .and_then(|state| state.snapshot.clone())
        {
            if self.snapshot.as_ref().is_some_and(|old| std::sync::Arc::ptr_eq(old, &snapshot)) {
                return;
            }
            match snapshot.as_ref() {
                Ok(page) => {
                    let mut entries = page.entries.clone();
                    let head = entries.iter().map(|entry| entry.video.id.clone()).collect::<HashSet<_>>();
                    // A refresh of the newest page should not discard older
                    // pages or interrupt an in-flight continuation.
                    if self.yt_loaded_more || self.yt_loading_more {
                        if let Some(Load::Ready(existing)) = &mut self.yt {
                            entries.extend(existing.drain(..).filter(|entry| {
                                !self.yt_head.contains(&entry.video.id) && !head.contains(&entry.video.id)
                            }));
                        }
                    } else {
                        self.yt_revision += 1;
                        self.yt_cont = page.continuation.clone();
                    }
                    self.yt_head = head;
                    self.yt = Some(Load::Ready(entries));
                }
                Err(error) if !matches!(self.yt, Some(Load::Ready(_))) => self.yt = Some(Load::Error(error.clone())),
                Err(_) => {}
            }
            self.snapshot = Some(snapshot);
        } else if self.yt.is_none() && key.is_some() {
            self.yt = Some(Load::Loading);
        }
        cx.notify();
    }

    fn load_more_yt(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else {
            self.yt_loading_more = false;
            return;
        };
        let Some(tok) = self.yt_cont.take() else {
            self.yt_loading_more = false;
            return;
        };
        let revision = self.yt_revision;
        cx.spawn(async move |this, cx| {
            let res = blocking(move || account.page(Some(&tok))).await;
            let _ = this.update(cx, |t, cx| {
                if t.yt_revision != revision {
                    return;
                }
                t.yt_loading_more = false;
                if let Ok(page) = res {
                    t.yt_cont = page.continuation;
                    t.yt_loaded_more = true;
                    if let Some(Load::Ready(list)) = &mut t.yt {
                        let seen: HashSet<String> = list.iter().map(|entry| entry.video.id.clone()).collect();
                        list.extend(page.entries.into_iter().filter(|entry| !seen.contains(&entry.video.id)));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn account_list(&mut self, q: &str, cx: &mut Context<Self>) -> Div {
        let mut list = div().flex().flex_col().gap_4().flex_1().min_w_0();
        let has_more = self.yt_cont.is_some();
        let shown: Vec<&Entry> = match &self.yt {
            None => return list.child(empty_state("history", "Sign in to see your YouTube watch history", "", cx)),
            Some(Load::Loading) => return list.child(loading_row(cx)),
            Some(Load::Error(e)) => {
                let e = e.clone();
                return list.child(empty_state("cloud-off", "Could not load your YouTube history", &e, cx));
            }
            Some(Load::Ready(videos)) => {
                let ql = q.to_lowercase();
                videos.iter().filter(|entry| {
                    let v = &entry.video;
                    ql.is_empty() || v.title.to_lowercase().contains(&ql) || v.channel_name.to_lowercase().contains(&ql)
                }).collect()
            }
        };
        if shown.is_empty() {
            list = list.child(empty_state("history", if q.is_empty() { "Your YouTube watch history is empty" } else { "No matches" }, "", cx));
        }
        for entry in shown {
            let v = &entry.video;
            let id = v.id.clone();
            list = list.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(video_row(v, px(246.), cx)))
                    .when(entry.removal_token.is_some(), |d| {
                        d.child(icon_button(SharedString::from(format!("yhx-{}", v.id)), "x", cx).on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.remove_account_entry(id.clone(), cx);
                        })))
                    }),
            );
        }
        if has_more {
            list = list.child(loading_row(cx));
        }
        list
    }

    /// Removes an entry in the same account that supplied its feedback token.
    fn remove_account_entry(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(account) = self.account.clone() else { return };
        let Some(Load::Ready(list)) = &mut self.yt else { return };
        let Some(pos) = list.iter().position(|entry| entry.video.id == id) else { return };
        let Some(token) = list[pos].removal_token.clone() else { return };
        let removed = list.remove(pos);
        let key = account.key();
        let revision = self.yt_revision;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let id2 = id.clone();
            let res = blocking(move || account.remove(&id2, &token)).await;
            if res.is_ok() {
                let _ = cx.update(|cx| {
                    cx.default_global::<Watched>().0.remove(&id);
                    crate::ui::app::refresh_all_pages(cx);
                });
            }
            let _ = this.update(cx, |t, cx| match res {
                Ok(()) => {
                    if t.account.as_ref().map(Account::key) == Some(key) {
                        if let Some(Load::Ready(list)) = &mut t.yt {
                            list.retain(|entry| entry.video.id != id);
                        }
                    }
                    toast(cx, translate("Removed from watch history"));
                    cx.notify();
                }
                Err(e) => {
                    if t.yt_revision == revision {
                        if let Some(Load::Ready(list)) = &mut t.yt {
                            list.insert(pos.min(list.len()), removed);
                        }
                    }
                    toast(cx, tf!("Couldn't remove it from YouTube: {0}", crate::ui::friendly_error(&e.to_string())));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn search_list(&mut self, q: &str, cx: &mut Context<Self>) -> Div {
        let p = pal(cx);
        let mut list = div().flex().flex_col().gap_1().flex_1().min_w_0();
        let ql = q.to_lowercase();
        let items: Vec<String> = store::search_history("", 500).into_iter().filter(|s| ql.is_empty() || s.to_lowercase().contains(&ql)).collect();
        if items.is_empty() {
            return list.child(empty_state("search", if q.is_empty() { "No searches yet" } else { "No matches" }, translate("Searches made in this app show here"), cx));
        }
        for (i, item) in items.into_iter().enumerate() {
            let (a, b) = (item.clone(), item.clone());
            list = list.child(
                div()
                    .id(SharedString::from(format!("sh-{i}")))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .hover(|d| d.bg(p.hover))
                    .on_click(move |_, window, cx| navigate(Route::Search(a.clone()), window, cx))
                    .child(icon("history").size_4().text_color(p.text2))
                    .child(div().flex_1().min_w_0().truncate().text_color(p.text).child(item))
                    .child(icon_button(SharedString::from(format!("shx-{i}")), "x", cx).on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        store::search_history_remove(&b);
                        cx.notify();
                    }))),
            );
        }
        list
    }
}

fn loading_row(cx: &App) -> Div {
    let p = pal(cx);
    div().py_6().flex().justify_center().text_color(p.text2).child(translate("Loading…"))
}

impl Render for HistoryPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        localize_placeholder(&self.filter, "Search history", window, cx);
        let p = pal(cx);
        let s = store::settings_snapshot();
        let q = self.filter.read(cx).value().to_string();
        if near_bottom(&self.scroll, 800.0) {
            match self.tab {
                HistTab::Device => self.limit += 100,
                HistTab::Account if !self.yt_loading_more && self.yt_cont.is_some() => {
                    self.yt_loading_more = true;
                    cx.defer_in_entity(|t, cx| t.load_more_yt(cx));
                }
                _ => {}
            }
        }
        let width = content_width(cx).min(px(1100.));
        let signed_in = self.account.is_some();
        let mut tabs = div().flex().gap_2();
        let tab_defs: Vec<(HistTab, &str)> = if signed_in {
            vec![(HistTab::Account, "YouTube history"), (HistTab::Device, "On this device"), (HistTab::Searches, "Searches")]
        } else {
            vec![(HistTab::Device, "Watched"), (HistTab::Searches, "Searches")]
        };
        for (t, label) in tab_defs {
            tabs = tabs.child(chip(SharedString::from(format!("ht-{label}")), translate(label), self.tab == t, cx).on_click(cx.listener(move |this, _, _, cx| {
                this.tab = t;
                if t == HistTab::Account {
                    refresh_current(cx, true);
                }
                cx.notify();
            })));
        }
        let entries = if self.tab == HistTab::Device { store::history_list(&q, self.limit) } else { Vec::new() };
        let mut list = div().flex().flex_col().gap_4().flex_1().min_w_0();
        let mut cur_day = String::new();
        for h in &entries {
            let day = chrono::DateTime::from_timestamp(h.watched_at, 0)
                .map(|d| d.with_timezone(&chrono::Local).format("%A, %B %-d, %Y").to_string())
                .unwrap_or_default();
            if day != cur_day {
                list = list.child(div().pt_4().text_lg().font_weight(FontWeight::BOLD).text_color(p.text).child(day.clone()));
                cur_day = day;
            }
            let id = h.video.id.clone();
            list = list.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(video_row(&h.video, px(246.), cx)))
                    .child(icon_button(SharedString::from(format!("hx-{}", h.video.id)), "x", cx).on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        store::history_remove(&id);
                        cx.default_global::<Watched>().0.remove(&id);
                        crate::ui::app::refresh_all_pages(cx);
                    }))),
            );
        }
        if self.tab == HistTab::Account {
            list = self.account_list(&q, cx);
        } else if self.tab == HistTab::Searches {
            list = self.search_list(&q, cx);
        } else if entries.is_empty() {
            list = list.child(empty_state("history", if q.is_empty() { "Your watch history is empty" } else { "No matches" }, "", cx));
        }
        let side = div()
            .flex()
            .flex_col()
            .gap_4()
            .w(px(320.))
            .flex_none()
            .child(div().flex().items_center().gap_2().border_b_1().border_color(p.border).pb_1().child(icon("search").size_4().text_color(p.text2)).child(Input::new(&self.filter).appearance(false).cleanable(true)))
            .child(
                pill("h-clear", Some("trash-2"), "Clear all watch history", cx).on_click(|_, window, cx| {
                    confirm("Clear watch history?", "All watch history on this device and resume positions will be deleted.", "Clear", window, cx, |_, cx| {
                        store::history_clear();
                        cx.default_global::<Watched>().0.clear();
                        crate::ui::app::refresh_all_pages(cx);
                    });
                }),
            )
            .child(
                pill("h-pause", Some(if s.history_enabled { "pause" } else { "play" }), if s.history_enabled { "Pause watch history" } else { "Turn on watch history" }, cx).on_click(cx.listener(|_, _, _, cx| {
                    store::update_settings(|s| s.history_enabled ^= true);
                    cx.notify();
                })),
            )
            .child(
                pill("h-search", Some("search-x"), "Clear search history", cx).on_click(cx.listener(|_, _, _, cx| {
                    store::search_history_clear();
                    toast(cx, translate("Search history cleared"));
                    cx.notify();
                })),
            );
        page_scroll("hist-scroll", &self.scroll).child(
            div().flex().justify_center().px_6().pt_6().pb_24().child(
                div()
                    .w(width)
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(section_title(translate("History"), cx).text_3xl())
                    .child(tabs)
                    .child(div().flex().gap_8().child(list).child(side)),
            ),
        )
    }
}

pub struct LibraryPage {
    scroll: ScrollHandle,
    /// the signed-in account's YouTube playlists
    yt: Option<Load<Vec<crate::api::types::PlaylistItem>>>,
}

impl LibraryPage {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut s = Self { scroll: ScrollHandle::new(), yt: None };
        if crate::api::auth::signed_in() {
            s.yt = Some(Load::Loading);
            cx.spawn(async move |this, cx| {
                let res = blocking(crate::api::auth::my_playlists).await;
                let _ = this.update(cx, |t, cx| {
                    t.yt = Some(match res {
                        Ok(l) => Load::Ready(l),
                        Err(e) => Load::Error(e.to_string()),
                    });
                    cx.notify();
                });
            })
            .detach();
        }
        s
    }
}

impl Render for LibraryPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let width = content_width(cx);
        let (_, w) = grid_columns(width, 260.0, window.rem_size());
        let pls = store::playlists();
        let mut cards: Vec<AnyElement> = pls
            .iter()
            .map(|pl| {
                let id = pl.id;
                div()
                    .id(SharedString::from(format!("lp-{id}")))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(w)
                    .cursor_pointer()
                    .on_click(move |_, window, cx| navigate(Route::LocalPlaylist(id), window, cx))
                    .child(playlist_thumb(pl.thumbnail.clone(), Some(tf!("{0} videos", pl.count)), w, w * (9. / 16.), cx))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(pl.protected, |d| d.child(icon(if pl.name == "Watch later" { "clock" } else { "heart" }).size_4().text_color(p.text2)))
                            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(if pl.protected { translate(&pl.name).to_string() } else { pl.name.clone() })),
                    )
                    .child(div().text_xs().text_color(p.text2).child(translate("Local, private")))
                    .into_any_element()
            })
            .collect();
        cards.push(
            div()
                .id("lp-new")
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .w(w)
                .h(w * (9. / 16.))
                .rounded_xl()
                .border_2()
                .border_dashed()
                .border_color(p.border)
                .cursor_pointer()
                .hover(move |s| s.bg(p.surface))
                .child(icon("plus").size_8().text_color(p.text2))
                .child(div().text_sm().text_color(p.text2).child(translate("New playlist")))
                .on_click(|_, window, cx| {
                    prompt_text("New playlist", "Playlist name", window, cx, |name, _, cx| {
                        store::playlist_create(&name);
                        cx.refresh_windows();
                    });
                })
                .into_any_element(),
        );
        let saved = store::saved_playlists();
        let history: Vec<_> = store::history_list("", 12).into_iter().map(|h| h.video).collect();
        let (_, hw) = grid_columns(width, 220.0, window.rem_size());
        page_scroll("lib-scroll", &self.scroll).child(
            div()
                .flex()
                .flex_col()
                .gap_10()
                .px_6()
                .pt_6()
                .pb_24()
                .when(!history.is_empty(), |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(section_title(translate("History"), cx))
                                    .child(pill("lib-hist", None, "View all", cx).on_click(|_, window, cx| navigate(Route::History, window, cx))),
                            )
                            .child(div().flex().gap_4().overflow_hidden().children(history.iter().take(6).map(|v| video_card(v, hw, true, cx)))),
                    )
                })
                .when_some(self.yt.as_ref().map(|l| match l {
                    Load::Loading => spinner(cx).into_any_element(),
                    Load::Error(e) => div().text_sm().text_color(p.text2).child(tf!("Could not load your YouTube playlists: {0}", e)).into_any_element(),
                    Load::Ready(l) if l.is_empty() => div().text_sm().text_color(p.text2).child(translate("No playlists on your account yet")).into_any_element(),
                    Load::Ready(l) => grid(l.iter().map(|pl| playlist_card(pl, w, cx)).collect(), width).into_any_element(),
                }), |d, body| {
                    d.child(div().flex().flex_col().gap_4().child(section_title(translate("Your YouTube playlists"), cx)).child(body))
                })
                .child(div().flex().flex_col().gap_4().child(section_title(translate("On this computer"), cx)).child(grid(cards, width)))
                .when(!saved.is_empty(), |d| {
                    d.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(section_title(translate("Saved playlists"), cx))
                            .child(grid(saved.iter().map(|pl| playlist_card(pl, w, cx)).collect(), width)),
                    )
                }),
        )
    }
}
