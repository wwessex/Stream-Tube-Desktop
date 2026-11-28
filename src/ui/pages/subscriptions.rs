use crate::api::types::*;
use crate::i18n::{t, tf};
use crate::store;
use crate::ui::app::{MenuItem, Route, confirm, navigate, open_menu, prompt_text};
use crate::ui::cards::*;
use crate::ui::widgets::*;
use crate::ui::*;
use gpui_kit::component::input::{Input, InputState};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    All,
    Videos,
    Shorts,
    Live,
}

#[derive(Clone, Default)]
struct FeedSnapshot {
    videos: Arc<[VideoItem]>,
    updated_at: Option<i64>,
}

struct AccountFeed {
    session: Arc<()>,
    snapshot: FeedSnapshot,
    continuation: Option<String>,
}

#[derive(PartialEq)]
enum SubscriptionRow {
    Heading(&'static str),
    Videos(Vec<usize>),
}

pub struct FeedPage {
    feed: FeedSnapshot,
    account_feed: Option<AccountFeed>,
    profile: Option<i64>,
    kind: Kind,
    refreshing: bool,
    progress: Arc<(AtomicUsize, AtomicUsize)>,
    scroll: ListState,
    rows: Arc<Vec<SubscriptionRow>>,
    row_width: Pixels,
    loading_older: bool,
}

impl FeedPage {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut s = Self {
            feed: FeedSnapshot::default(),
            account_feed: None,
            profile: None,
            kind: Kind::All,
            refreshing: false,
            progress: Arc::new((AtomicUsize::new(0), AtomicUsize::new(0))),
            scroll: ListState::new(0, ListAlignment::Top, px(200.)),
            rows: Arc::new(Vec::new()),
            row_width: px(0.),
            loading_older: false,
        };
        s.load_cached();
        let stale = s.feed.updated_at.map(|t| crate::api::json::now() - t > 30 * 60).unwrap_or(true);
        if crate::api::auth::signed_in() || (stale && !store::subscriptions().is_empty()) {
            s.refresh(cx);
        }
        s
    }

    fn filter(&self) -> Option<Vec<String>> {
        self.profile.and_then(|id| store::profiles().into_iter().find(|p| p.id == id).map(|p| p.channels))
    }

    fn load_cached(&mut self) {
        if self.profile.is_none() {
            if let Some(account_feed) = &self.account_feed {
                self.feed = account_feed.snapshot.clone();
                return;
            }
        }
        let f = self.filter();
        let (videos, updated_at) = crate::feed::cached_feed(f.as_deref());
        self.feed = FeedSnapshot { videos: videos.into(), updated_at };
    }

    fn clear_stale_account_feed(&mut self) {
        if let Some(feed) = &self.account_feed {
            let current = crate::api::auth::session_identity();
            if current.as_ref().is_none_or(|session| !Arc::ptr_eq(session, &feed.session)) {
                self.account_feed = None;
                if self.profile.is_none() {
                    self.load_cached();
                }
            }
        }
    }

    fn select_profile(&mut self, profile: Option<i64>, cx: &mut Context<Self>) {
        if self.profile == profile {
            return;
        }
        self.clear_stale_account_feed();
        self.profile = profile;
        self.load_cached();
        if profile.is_none() && self.account_feed.is_none() && crate::api::auth::signed_in() {
            self.refresh(cx);
        }
        cx.notify();
    }

    fn receive_feed(&mut self, profile: Option<i64>, snapshot: FeedSnapshot, session: Option<Arc<()>>, continuation: Option<String>) {
        if let Some(session) = session {
            self.account_feed = Some(AccountFeed { session, snapshot: snapshot.clone(), continuation });
        }
        if profile == self.profile {
            self.feed = match (&self.account_feed, profile) {
                (Some(feed), None) => feed.snapshot.clone(),
                _ => snapshot,
            };
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing || self.loading_older {
            return;
        }
        self.refreshing = true;
        self.progress.0.store(0, Ordering::SeqCst);
        let profile = self.profile;
        let filter = self.filter();
        let session = if profile.is_none() { crate::api::auth::session_identity() } else { None };
        let account = session.is_some();
        self.progress.1.store(if account { 0 } else { store::subscriptions().len() }, Ordering::SeqCst);
        let prog = self.progress.clone();
        // UI ticker for progress
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
                let go = this
                    .update(cx, |t, cx| {
                        cx.notify();
                        t.refreshing
                    })
                    .unwrap_or(false);
                if !go {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let (videos, failed, from_account, continuation) = blocking(move || {
                // signed in: the account's own subscriptions feed, one request instead of one per channel
                if account {
                    if let Ok((mut v, continuation)) = crate::api::auth::subscriptions_videos(4) {
                        if !v.is_empty() || continuation.is_some() {
                            // it isn't in date order, and the day headings repeat
                            // ("Yesterday", "This week", "Yesterday") unless it is
                            crate::feed::sort_feed(&mut v);
                            return (v, 0, true, continuation);
                        }
                    }
                }
                let (videos, failed) = crate::feed::refresh(filter, move |d, t| {
                    prog.0.store(d, Ordering::SeqCst);
                    prog.1.store(t, Ordering::SeqCst);
                });
                (videos, failed, false, None)
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.refreshing = false;
                this.clear_stale_account_feed();
                let account_session = session.filter(|session| {
                    crate::api::auth::session_identity().is_some_and(|current| Arc::ptr_eq(session, &current))
                });
                if !from_account || account_session.is_some() {
                    let snapshot = FeedSnapshot {
                        videos: videos.into(),
                        updated_at: Some(crate::api::json::now()),
                    };
                    this.receive_feed(profile, snapshot, if from_account { account_session } else { None }, continuation);
                }
                if profile.is_some() && this.profile.is_none() && this.account_feed.is_none() && crate::api::auth::signed_in() {
                    this.refresh(cx);
                }
                if failed > 0 && profile == this.profile {
                    toast(cx, tf!("{0} channels could not be refreshed", failed));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_older(&mut self, cx: &mut Context<Self>) {
        let Some(feed) = self.account_feed.as_ref() else { return };
        let Some(token) = feed.continuation.clone() else { return };
        if self.profile.is_some() || self.refreshing || self.loading_older {
            return;
        }
        let session = feed.session.clone();
        self.loading_older = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let request = token.clone();
            let result = blocking(move || crate::api::auth::browse_continue(&request)).await;
            let _ = this.update(cx, |this, cx| {
                this.loading_older = false;
                cx.notify();
                this.clear_stale_account_feed();
                let Some(feed) = this.account_feed.as_mut().filter(|feed| {
                    Arc::ptr_eq(&feed.session, &session) && feed.continuation.as_deref() == Some(token.as_str())
                }) else { return };
                match result {
                    Ok(page) => {
                        let mut videos = feed.snapshot.videos.to_vec();
                        let mut seen: std::collections::HashSet<_> = videos.iter().map(|v| v.id.clone()).collect();
                        videos.extend(page.items.into_iter().filter_map(|item| match item {
                            Item::Video(v) if seen.insert(v.id.clone()) => Some(v),
                            _ => None,
                        }));
                        crate::feed::sort_feed(&mut videos);
                        feed.snapshot.videos = videos.into();
                        feed.continuation = page.continuation.filter(|next| next != &token);
                        if this.profile.is_none() {
                            this.feed = feed.snapshot.clone();
                        }
                    }
                    Err(error) => toast(cx, friendly_error(&error.to_string())),
                }
            });
        }).detach();
    }

    fn update_rows(&mut self, rows: Vec<SubscriptionRow>, width: Pixels, window: &mut Window, cx: &Context<Self>) {
        if self.rows.as_ref() == &rows && self.row_width == width {
            return;
        }
        let old = self.rows.len();
        let new = rows.len();
        let shared = old.min(new);
        if old != new {
            self.scroll.splice(shared..old, new - shared);
        }
        if self.row_width != width {
            self.scroll.remeasure();
        } else if let Some(first) = self.rows.iter().zip(&rows).position(|(old, new)| old != new) {
            self.scroll.remeasure_items(first..shared);
        }
        self.rows = Arc::new(rows);
        self.row_width = width;
        let estimate = width * (9. / 16.) + window.rem_size() * 7.0 + grid_row_gap();
        let state = self.scroll.clone();
        let entity = cx.entity_id();
        window.on_next_frame(move |_, cx| {
            state.with_uniform_item_height(estimate);
            cx.notify(entity);
        });
    }
}

impl Render for FeedPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.clear_stale_account_feed();
        let p = pal(cx);
        let s = store::settings_snapshot();
        let width = content_width(cx);
        let min_width = if self.kind == Kind::Shorts { 200.0 } else { 300.0 };
        let (columns, card_w) = grid_columns(width, min_width, window.rem_size());
        if store::subscriptions().is_empty() && !crate::api::auth::signed_in() {
            return div().size_full().p_6().child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(empty_state("tv", "No subscriptions yet", "", cx))
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .child(pill("go-signin", Some("circle-user"), "Sign in", cx).on_click(|_, window, cx| crate::ui::app::open_sign_in(window, cx)))
                                .child(pill("go-import", Some("upload"), "Import subscriptions", cx).on_click(|_, window, cx| navigate(Route::settings(), window, cx))),
                        ),
                )
                .into_any_element();
        }
        let profiles = store::profiles();
        let mut bar = div().flex().items_center().gap_2().flex_wrap().child(chip("pf-all", t("All channels"), self.profile.is_none(), cx).on_click(cx.listener(|t, _, _, cx| {
            t.select_profile(None, cx);
        })));
        for pr in &profiles {
            let id = pr.id;
            bar = bar.child(chip(SharedString::from(format!("pf-{id}")), pr.name.clone(), self.profile == Some(id), cx).on_click(cx.listener(move |t, _, _, cx| {
                t.select_profile(Some(id), cx);
            })));
        }
        bar = bar.child(div().w(px(1.)).h_6().bg(p.border).mx_2());
        for (k, l) in [(Kind::All, "All"), (Kind::Videos, "Videos"), (Kind::Shorts, "Shorts"), (Kind::Live, "Live")] {
            if k == Kind::Shorts && s.hide_shorts {
                continue;
            }
            bar = bar.child(chip(SharedString::from(format!("k-{l}")), t(l), self.kind == k, cx).on_click(cx.listener(move |t, _, _, cx| {
                t.kind = k;
                cx.notify();
            })));
        }
        let (done, total) = (self.progress.0.load(Ordering::SeqCst), self.progress.1.load(Ordering::SeqCst));
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .pb_4()
            .child(bar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when_some(self.feed.updated_at.filter(|_| !self.refreshing), |d, t| d.child(div().text_xs().text_color(p.text2).child(tf!("Updated {0}", crate::api::json::relative_from_ts(t)))))
                    .when(self.refreshing, |d| d.child(div().text_xs().text_color(p.text2).child(if total == 0 { t("Refreshing…").to_string() } else { tf!("Refreshing {0}/{1}…", done, total) })))
                    .child(
                        pill("hide-watched", Some(if s.hide_watched_in_feed { "eye-off" } else { "eye" }), if s.hide_watched_in_feed { "Watched hidden" } else { "Hide watched" }, cx).on_click(|_, _, cx| {
                            store::update_settings(|s| s.hide_watched_in_feed ^= true);
                            cx.refresh_windows();
                        }),
                    )
                    .child(pill("refresh", Some("refresh-cw"), "Refresh", cx).on_click(cx.listener(|t, _, _, cx| t.refresh(cx))))
                    .child(pill("manage", Some("users"), "Manage", cx).on_click(|_, window, cx| navigate(Route::Channels, window, cx))),
            );
        let kind = self.kind;
        let videos = self.feed.videos.clone();
        let filtered = videos.iter().enumerate()
            .filter(|(_, v)| !store::is_filtered(v, &s))
            .filter(|(_, v)| !(s.hide_watched_in_feed && is_watched(&v.id, cx)))
            .filter(|(_, v)| match kind {
                Kind::All => true,
                Kind::Videos => !v.is_short && !v.is_live,
                Kind::Shorts => v.is_short,
                Kind::Live => v.is_live || v.is_upcoming,
            });
        let rows = subscription_rows(filtered, columns, s.feed_group_by_day && kind != Kind::Shorts, crate::api::json::now());
        self.update_rows(rows, card_w, window, cx);

        let rows = self.rows.clone();
        let body = if rows.is_empty() {
            if self.refreshing { spinner(cx).into_any_element() } else { empty_state("inbox", "No videos", "", cx).into_any_element() }
        } else {
            list(self.scroll.clone(), move |index, _, cx| {
                match &rows[index] {
                    SubscriptionRow::Heading(label) => div().pb_4().pt_2().child(section_title(t(label), cx)).into_any_element(),
                    SubscriptionRow::Videos(indices) => {
                        let cards = indices.iter().map(|&index| {
                            if kind == Kind::Shorts {
                                shorts_card(&videos[index], card_w, cx)
                            } else {
                                video_card(&videos[index], card_w, true, cx)
                            }
                        }).collect();
                        grid(cards, width).flex_nowrap().pb(grid_row_gap()).into_any_element()
                    }
                }
            }).absolute().inset_0().px_6().into_any_element()
        };
        let can_load_older = self.profile.is_none()
            && self.account_feed.as_ref().is_some_and(|feed| feed.continuation.is_some());
        div().size_full().flex().flex_col()
            .child(div().flex_none().px_6().pt_4().child(header))
            .child(div().relative().min_h_0().flex_1().child(scroll_viewport("feed-scroll", &self.scroll, body, window, cx)))
            .when(can_load_older, |page| {
                let loading = self.loading_older || self.refreshing;
                page.child(div().flex_none().flex().justify_center().p_3().child(
                    pill("load-older-subscriptions", Some("arrow-down"), if loading { "Refreshing…" } else { "Load older videos" }, cx)
                        .when(!loading, |d| d.on_click(cx.listener(|this, _, _, cx| this.load_older(cx)))),
                ))
            })
            .into_any_element()
    }
}

fn subscription_rows<'a>(
    videos: impl Iterator<Item = (usize, &'a VideoItem)>,
    columns: usize,
    grouped: bool,
    now: i64,
) -> Vec<SubscriptionRow> {
    let mut rows = Vec::new();
    let mut last_heading = None;
    for (index, video) in videos {
        if grouped {
            let elapsed = video.published_ts.map(|ts| now.saturating_sub(ts)).unwrap_or(i64::MAX);
            let heading = match elapsed {
                d if d < 86400 => "Today",
                d if d < 2 * 86400 => "Yesterday",
                d if d < 7 * 86400 => "This week",
                d if d < 30 * 86400 => "This month",
                _ => "Older",
            };
            if last_heading != Some(heading) {
                rows.push(SubscriptionRow::Heading(heading));
                last_heading = Some(heading);
            }
        }
        match rows.last_mut() {
            Some(SubscriptionRow::Videos(indices)) if indices.len() < columns => indices.push(index),
            _ => rows.push(SubscriptionRow::Videos(vec![index])),
        }
    }
    rows
}

#[cfg(test)]
mod feed_selection_tests {
    use super::*;

    fn feed(id: &str) -> FeedSnapshot {
        FeedSnapshot {
            videos: vec![VideoItem { id: id.to_string(), ..Default::default() }].into(),
            updated_at: Some(1),
        }
    }

    #[::core::prelude::v1::test]
    fn account_feed_survives_profile_switches_and_local_fallback() {
        let mut page = FeedPage {
            feed: feed("filtered"),
            account_feed: None,
            profile: Some(1),
            kind: Kind::All,
            refreshing: false,
            progress: Arc::new((AtomicUsize::new(0), AtomicUsize::new(0))),
            scroll: ListState::new(0, ListAlignment::Top, px(200.)),
            rows: Arc::new(Vec::new()),
            row_width: px(0.),
            loading_older: false,
        };

        page.receive_feed(None, feed("account"), Some(Arc::new(())), Some("cursor".into()));
        assert_eq!(page.feed.videos[0].id, "filtered");

        page.profile = None;
        page.load_cached();
        assert_eq!(page.feed.videos[0].id, "account");

        page.receive_feed(Some(1), feed("late-filtered"), None, None);
        page.receive_feed(None, FeedSnapshot::default(), None, None);
        assert_eq!(page.feed.videos[0].id, "account");
        assert_eq!(page.account_feed.as_ref().unwrap().continuation.as_deref(), Some("cursor"));
    }

    #[::core::prelude::v1::test]
    fn older_feed_stays_virtualized_by_rows_in_both_layouts() {
        let videos: Vec<_> = (0..400).map(|i| VideoItem {
            id: format!("video-{i}"),
            published_ts: Some(-(i as i64) * 3600),
            ..Default::default()
        }).collect();
        let flat = subscription_rows(videos.iter().enumerate(), 5, false, 0);
        assert_eq!(flat.len(), 80);
        let grouped = subscription_rows(videos.iter().enumerate(), 5, true, 0);
        assert!(matches!(grouped.first(), Some(SubscriptionRow::Heading("Today"))));
        assert_eq!(grouped.iter().filter_map(|row| match row {
            SubscriptionRow::Videos(indices) => Some(indices.len()),
            SubscriptionRow::Heading(_) => None,
        }).sum::<usize>(), 400);
    }
}

// ───────────────────────── manage channels & profiles ─────────────────────────

pub struct ChannelsPage {
    filter: Entity<InputState>,
    scroll: ScrollHandle,
    profile: Option<i64>,
}

impl ChannelsPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(t("Filter channels")));
        cx.observe(&filter, |_, _, cx| cx.notify()).detach();
        Self { filter, scroll: ScrollHandle::new(), profile: None }
    }
}

impl Render for ChannelsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        localize_placeholder(&self.filter, "Filter channels", window, cx);
        let p = pal(cx);
        let q = self.filter.read(cx).value().to_lowercase();
        let subs: Vec<store::Subscription> = store::subscriptions().into_iter().filter(|s| q.is_empty() || s.name.to_lowercase().contains(&q)).collect();
        let profiles = store::profiles();
        let cur_profile = self.profile.and_then(|id| profiles.iter().find(|p| p.id == id).cloned());
        let mut pbar = div().flex().items_center().gap_2().flex_wrap().child(chip("mp-none", t("All"), self.profile.is_none(), cx).on_click(cx.listener(|t, _, _, cx| {
            t.profile = None;
            cx.notify();
        })));
        for pr in &profiles {
            let id = pr.id;
            pbar = pbar.child(
                chip(SharedString::from(format!("mp-{id}")), format!("{} ({})", pr.name, pr.channels.len()), self.profile == Some(id), cx).on_click(cx.listener(move |t, _, _, cx| {
                    t.profile = Some(id);
                    cx.notify();
                })),
            );
        }
        pbar = pbar
            .child(pill("mp-new", Some("plus"), "New profile", cx).on_click(|_, window, cx| {
                prompt_text("New profile", "Profile name (e.g. Music, Tech)", window, cx, |name, _, cx| {
                    let colors = [0x3ea6ff, 0xff4e45, 0x2ba640, 0xffa000, 0xab47bc, 0x26c6da];
                    let c = colors[store::profiles().len() % colors.len()];
                    store::create_profile(&name, c);
                    cx.refresh_windows();
                });
            }))
            .when_some(self.profile, |d, id| {
                d.child(pill("mp-del", Some("trash-2"), "Delete profile", cx).on_click(cx.listener(move |t, _, window, cx| {
                    let ent = cx.entity().downgrade();
                    confirm("Delete profile?", "Channels stay subscribed.", "Delete", window, cx, move |_, cx| {
                        store::delete_profile(id);
                        if let Some(e) = ent.upgrade() {
                            e.update(cx, |t, cx| {
                                t.profile = None;
                                cx.notify();
                            });
                        }
                    });
                    let _ = t;
                })))
            });
        let mut list = div().flex().flex_col().gap_1();
        for s in &subs {
            let cid = s.channel_id.clone();
            let cid2 = s.channel_id.clone();
            let name = s.name.clone();
            let in_profile = cur_profile.as_ref().map(|pr| pr.channels.contains(&s.channel_id));
            let pid = self.profile;
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .hover(move |st| st.bg(p.surface))
                    .child(avatar(s.avatar.as_deref(), px(48.), &s.name, cx))
                    .child(
                        div()
                            .id(SharedString::from(format!("mc-{}", s.channel_id)))
                            .flex_1()
                            .text_base()
                            .text_color(p.text)
                            .cursor_pointer()
                            .child(s.name.clone())
                            .on_click(move |_, window, cx| navigate(Route::Channel(cid.clone()), window, cx)),
                    )
                    .when_some(in_profile.zip(pid), |d, (inp, pid)| {
                        let cid3 = s.channel_id.clone();
                        d.child(
                            pill(SharedString::from(format!("mpt-{}", s.channel_id)), Some(if inp { "square-check" } else { "square" }), "In profile", cx).on_click(move |_, _, cx| {
                                store::set_profile_channel(pid, &cid3, !inp);
                                cx.refresh_windows();
                            }),
                        )
                    })
                    .child(
                        icon_button(SharedString::from(format!("mpm-{}", s.channel_id)), "ellipsis-vertical", cx).on_click(move |ev, window, cx| {
                            let mut items = Vec::new();
                            for pr in store::profiles() {
                                let member = pr.channels.contains(&cid2);
                                let c = cid2.clone();
                                items.push(MenuItem::new(if member { "square-check" } else { "square" }, tf!("Profile: {0}", pr.name), move |_, cx| {
                                    store::set_profile_channel(pr.id, &c, !member);
                                    cx.refresh_windows();
                                }));
                            }
                            let c = cid2.clone();
                            let n = name.clone();
                            items.push(MenuItem::new("user-minus", t("Unsubscribe"), move |_, cx| {
                                store::unsubscribe(&c);
                                crate::ui::cards::sync_subscription(c.clone(), false, cx);
                                toast(cx, tf!("Unsubscribed from {0}", n));
                                cx.refresh_windows();
                            }));
                            open_menu(ev.position(), items, window, cx);
                        }),
                    ),
            );
        }
        page_scroll("chans-scroll", &self.scroll).child(
            div().flex().justify_center().px_6().pt_6().pb_24().child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .w(px(900.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(section_title(tf!("Subscriptions ({0})", store::subscriptions().len()), cx))
                            .child(div().w(px(260.)).px_3().py_1().rounded_full().border_1().border_color(p.border).child(Input::new(&self.filter).appearance(false).cleanable(true))),
                    )
                    .child(pbar)
                    .child(list),
            ),
        )
    }
}
