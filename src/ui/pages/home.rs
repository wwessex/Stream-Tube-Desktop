use crate::api::innertube::{self, EXPLORE};
use crate::i18n::{t, tf};
use crate::api::types::*;
use crate::store;
use crate::ui::cards::*;
use crate::ui::widgets::*;
use crate::ui::*;
use std::sync::Arc;

/// Marks continuations of the signed-in home feed (they need the TV client).
const ACCOUNT: &str = "acct:";
/// Continuation of the account's music picks.
const MUSIC: &str = "music:";
/// The chip of the "Music" explore page.
const MUSIC_CHIP: usize = 1;
/// Marks continuations of the home feed loaded with the browser sign-in.
const WEB: &str = "web:";

/// Videos at the top of the last home load. A refresh moves them down so it
/// actually shows something new (YouTube returns much the same first page).
static LAST_TOP: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

fn freshen(items: Vec<Item>) -> Vec<Item> {
    use rand::seq::SliceRandom;
    let last: std::collections::HashSet<String> = LAST_TOP.lock().iter().cloned().collect();
    let id = |i: &Item| if let Item::Video(v) = i { Some(v.id.clone()) } else { None };
    let (mut fresh, mut seen): (Vec<Item>, Vec<Item>) = items.into_iter().partition(|i| id(i).map(|x| !last.contains(&x)).unwrap_or(true));
    let mut rng = rand::rng();
    // keep the feed's rough order but mix it within small windows
    for chunk in fresh.chunks_mut(8) {
        chunk.shuffle(&mut rng);
    }
    seen.shuffle(&mut rng);
    fresh.append(&mut seen);
    *LAST_TOP.lock() = fresh.iter().take(24).filter_map(id).collect();
    fresh
}

/// Where the home feed comes from, so a cached feed is only shown for the same source.
fn feed_source() -> &'static str {
    if crate::api::web::connected() {
        "web"
    } else if crate::api::auth::signed_in() {
        "account"
    } else {
        "local"
    }
}

/// Bumped when the cached items change shape (a new field, a different reading of a
/// tile), so a feed cached by an older version is not shown.
const CACHE_VERSION: i64 = 2;

/// The home feed from the last session, shown at once when the app starts while
/// the new one loads. Kept for three days.
fn cached_home() -> Option<Vec<Item>> {
    let v: serde_json::Value = serde_json::from_str(&store::kv_get("home_cache")?).ok()?;
    if v.get("version").and_then(serde_json::Value::as_i64) != Some(CACHE_VERSION)
        || v.get("source")?.as_str()? != feed_source()
        || crate::api::json::now() - v.get("at")?.as_i64()? > 3 * 24 * 3600
    {
        return None;
    }
    let items: Vec<Item> = serde_json::from_value(v.get("items")?.clone()).ok()?;
    (!items.is_empty()).then_some(items)
}

fn save_home(items: &[Item]) {
    let v = serde_json::json!({
        "version": CACHE_VERSION,
        "source": feed_source(),
        "at": crate::api::json::now(),
        "items": items.iter().take(60).collect::<Vec<_>>(),
    });
    store::kv_set("home_cache", &v.to_string());
}

/// The "Recently uploaded" chip (after All and the Explore categories).
const RECENT: usize = usize::MAX;
/// How far back "Recently uploaded" reaches.
const RECENT_DAYS: i64 = 14;

/// Videos from the Home feed and the subscriptions uploaded in the last two
/// weeks, newest first, like youtube.com's "Recently uploaded" chip.
fn recently_uploaded() -> anyhow::Result<Vec<Item>> {
    let mut videos: Vec<VideoItem> = Vec::new();
    let mut add = |p: anyhow::Result<Page>| {
        if let Ok(p) = p {
            videos.extend(p.items.into_iter().filter_map(|i| if let Item::Video(v) = i { Some(v) } else { None }));
        }
    };
    // the personal Home feed, two pages of it, from whichever sign-in there is
    if crate::api::web::connected() {
        let first = crate::api::web::home();
        let next = first.as_ref().ok().and_then(|p| p.continuation.clone());
        add(first);
        if let Some(t) = next {
            add(crate::api::web::browse_continue(&t));
        }
    } else if crate::api::auth::signed_in() {
        let first = crate::api::auth::home();
        let next = first.as_ref().ok().and_then(|p| p.continuation.clone());
        add(first);
        if let Some(t) = next {
            add(crate::api::auth::browse_continue(&t));
        }
    } else if crate::recommend::available() {
        add(Ok(Page { items: crate::recommend::for_you(), continuation: None }));
    }
    // new uploads from the subscriptions
    if crate::api::auth::signed_in() {
        add(crate::api::auth::subscriptions_feed());
    }
    if !store::subscriptions().is_empty() {
        let (feed, _) = crate::feed::cached_feed(None);
        videos.extend(feed.into_iter().take(150));
    }
    let since = crate::api::json::now() - RECENT_DAYS * 86400;
    let mut seen = std::collections::HashSet::new();
    let mut recent: Vec<(i64, VideoItem)> = videos
        .into_iter()
        .filter(|v| !v.is_short && !v.is_upcoming && !v.is_live)
        .filter_map(|v| {
            let ts = v.published_ts.or_else(|| v.published_text.as_deref().and_then(crate::api::json::parse_relative_time))?;
            (ts >= since && seen.insert(v.id.clone())).then_some((ts, v))
        })
        .collect();
    if recent.is_empty() {
        anyhow::bail!("{}", tf!("No videos from the last {0} days in your feed yet. Sign in or subscribe to some channels to fill this up.", RECENT_DAYS));
    }
    recent.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(recent
        .into_iter()
        .take(120)
        .map(|(ts, mut v)| {
            // a saved feed keeps the "2 days ago" it was fetched with; say it from
            // the upload time instead, so the dates match the order
            v.published_text = Some(crate::api::json::relative_from_ts(ts));
            Item::Video(v)
        })
        .collect())
}

/// Only the first home load of a session starts from the cache.
static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct HomePage {
    chip: usize,
    items: Load<Arc<Vec<Item>>>,
    continuation: Option<String>,
    loading_more: bool,
    scroll: ListState,
    rows: Arc<Vec<FeedRow>>,
    row_size: (Pixels, Pixels),
    serial: u64,
    /// redraws the music mode button when the mode changes elsewhere
    _music: Subscription,
}

#[derive(PartialEq)]
enum FeedRow {
    Cards { indices: Vec<usize>, gap: Pixels },
    Shorts { indices: Vec<usize>, gap: Pixels },
    Loading { gap: Pixels },
}

fn feed_rows(items: &[Item], settings: &store::Settings, columns: usize, width: Pixels, rem: Pixels, cx: &App) -> Vec<FeedRow> {
    let shorts_count = ((f32::from(width + rem) / f32::from(px(200.) + rem)).ceil() as usize).clamp(1, 8);
    let row_gap = grid_row_gap();
    let section_gap = rem * 2.0;
    let mut cards = Vec::new();
    let mut shorts = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if let Item::Video(video) = item {
            if store::is_filtered(video, settings) || (settings.hide_watched_in_feed && is_watched(&video.id, cx)) {
                continue;
            }
            if video.is_short {
                if !settings.hide_shorts && shorts.len() < shorts_count {
                    shorts.push(index);
                }
                continue;
            }
        }
        cards.push(index);
    }

    let split = cards.len().min(12);
    let (first, rest) = cards.split_at(split);
    let mut rows = Vec::new();
    for (index, row) in first.chunks(columns).enumerate() {
        let gap = if index + 1 < first.len().div_ceil(columns) {
            row_gap
        } else if !shorts.is_empty() || !rest.is_empty() {
            section_gap
        } else {
            px(0.)
        };
        rows.push(FeedRow::Cards { indices: row.to_vec(), gap });
    }
    if !shorts.is_empty() {
        rows.push(FeedRow::Shorts { indices: shorts, gap: if rest.is_empty() { px(0.) } else { section_gap } });
    }
    for (index, row) in rest.chunks(columns).enumerate() {
        let gap = if index + 1 < rest.len().div_ceil(columns) { row_gap } else { px(0.) };
        rows.push(FeedRow::Cards { indices: row.to_vec(), gap });
    }
    rows
}

fn render_feed_row(row: &FeedRow, items: &[Item], card_width: Pixels, cx: &mut App) -> AnyElement {
    match row {
        FeedRow::Cards { indices, gap } => grid(
            indices.iter().map(|&index| grid_item(&items[index], card_width, true, cx)).collect(),
            card_width,
        ).flex_nowrap().pb(*gap).into_any_element(),
        FeedRow::Shorts { indices, gap } => {
            let p = pal(cx);
            div()
                .flex()
                .flex_col()
                .w_full()
                .gap_4()
                .pb(*gap)
                .child(div().flex().items_center().gap_2().child(icon("zap").text_color(p.accent)).child(section_title("Shorts", cx)))
                .child(div().flex().gap_4().overflow_hidden().children(
                    indices.iter().filter_map(|&index| items[index].as_video()).map(|video| shorts_card(video, px(200.), cx)),
                ))
                .into_any_element()
        }
        FeedRow::Loading { gap } => div().pb(*gap).child(spinner(cx)).into_any_element(),
    }
}

impl HomePage {
    pub fn new(chip: usize, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut s = Self {
            chip,
            items: Load::Loading,
            continuation: None,
            loading_more: false,
            scroll: ListState::new(0, ListAlignment::Top, px(0.)),
            rows: Arc::default(),
            row_size: (px(0.), px(0.)),
            serial: 0,
            _music: cx.observe_global::<crate::ui::music::MusicMode>(|this, cx| {
                if this.chip == MUSIC_CHIP {
                    cx.notify();
                }
            }),
        };
        s.load(cx);
        s
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.items = Load::Loading;
        self.continuation = None;
        self.loading_more = false;
        self.scroll.reset(0);
        self.rows = Arc::default();
        self.serial += 1;
        let serial = self.serial;
        let chip = self.chip;
        // at startup, show the last feed right away instead of a spinner
        let mut from_cache = false;
        if chip == 0 && !STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            if let Some(items) = cached_home() {
                self.items = Load::Ready(Arc::new(items));
                from_cache = true;
            }
        }
        let shown_at = std::time::Instant::now();
        cx.spawn(async move |this, cx| {
            let res = blocking(move || -> anyhow::Result<(Vec<Item>, Option<String>)> {
                if chip == 0 && crate::api::web::connected() {
                    // the account's home feed exactly as youtube.com builds it
                    for _ in 0..2 {
                        match crate::api::web::home() {
                            Ok(p) if !p.items.is_empty() => return Ok((freshen(p.items), p.continuation.map(|c| format!("{WEB}{c}")))),
                            _ => std::thread::sleep(std::time::Duration::from_millis(400)),
                        }
                    }
                }
                if chip == 0 && crate::api::auth::signed_in() {
                    // YouTube's own recommendations for the account (a failed request is
                    // retried once before falling back, so the feed stays personal)
                    let home = crate::api::auth::home().ok().filter(|p| !p.items.is_empty()).or_else(|| {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        crate::api::auth::home().ok()
                    });
                    if let Some(p) = home {
                        if !p.items.is_empty() {
                            // shown as soon as the first page is in; the next page loads
                            // when scrolling, instead of making the start wait for it
                            return Ok((freshen(p.items), p.continuation.map(|c| format!("{ACCOUNT}{c}"))));
                        }
                    }
                }
                if chip == 0 && crate::recommend::available() {
                    // no account: videos related to what was watched and searched here,
                    // with new uploads from local subscriptions mixed in
                    let mut picks = crate::recommend::for_you();
                    if !picks.is_empty() {
                        if !store::subscriptions().is_empty() {
                            let (feed, _) = crate::feed::cached_feed(None);
                            let mut subs = feed.into_iter().take(30).map(Item::Video);
                            let mut out = Vec::new();
                            for (i, it) in picks.drain(..).enumerate() {
                                out.push(it);
                                if i % 3 == 2 {
                                    out.extend(subs.next());
                                }
                            }
                            picks = out;
                        }
                        return Ok((freshen(picks), None));
                    }
                }
                if chip == RECENT {
                    return Ok((recently_uploaded()?, None));
                }
                if chip == 0 {
                    let subs = store::subscriptions();
                    if !subs.is_empty() {
                        let (mut feed, oldest) = crate::feed::cached_feed(None);
                        let stale = oldest.map(|o| crate::api::json::now() - o > 3600).unwrap_or(true);
                        if feed.is_empty() || stale {
                            feed = crate::feed::refresh(None, |_, _| {}).0;
                        }
                        // only the subscriptions: popular videos from unrelated channels mixed
                        // in here read as random videos in the feed
                        let items: Vec<Item> = feed.into_iter().take(80).map(Item::Video).collect();
                        return Ok((freshen(items), None));
                    }
                    // no subscriptions: popular from several categories, interleaved
                    let lists: Vec<Vec<Item>> = std::thread::scope(|s| {
                        let hs: Vec<_> = EXPLORE
                            .iter()
                            .take(4)
                            .map(|(_, id)| s.spawn(move || innertube::explore(id).map(|p| p.items).unwrap_or_default()))
                            .collect();
                        hs.into_iter().map(|h| h.join().unwrap_or_default()).collect()
                    });
                    let mut out = Vec::new();
                    let mut iters: Vec<_> = lists.into_iter().map(|l| l.into_iter().filter(|i| matches!(i, Item::Video(_)))).collect();
                    let mut seen = std::collections::HashSet::new();
                    loop {
                        let mut any = false;
                        for it in iters.iter_mut() {
                            if let Some(x) = it.next() {
                                any = true;
                                if let Item::Video(v) = &x {
                                    if seen.insert(v.id.clone()) {
                                        out.push(x);
                                    }
                                }
                            }
                        }
                        if !any {
                            break;
                        }
                    }
                    Ok((freshen(out), None))
                } else {
                    if chip == MUSIC_CHIP && crate::api::auth::signed_in() {
                        // the account's own music picks, each opening as a mix
                        if let Ok(p) = crate::api::auth::music() {
                            if !p.items.is_empty() {
                                return Ok((p.items, p.continuation.map(|c| format!("{MUSIC}{c}"))));
                            }
                        }
                    }
                    let p = innertube::explore(EXPLORE[chip - 1].1)?;
                    Ok((p.items, p.continuation))
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                crate::perf::mark("home feed fetched");
                match res {
                    Ok((items, cont)) => {
                        if chip == 0 && !items.is_empty() {
                            save_home(&items);
                        }
                        // the cached feed stays if the user is already scrolling through it,
                        // or has been looking at it for a while: a slow fetch must not swap the
                        // videos under the pointer (the new feed is saved for the next start)
                        let scrolled = this.scroll.scroll_px_offset_for_scrollbar().y < px(-40.);
                        let settled = shown_at.elapsed() > std::time::Duration::from_secs(8);
                        if !(from_cache && (scrolled || settled)) {
                            this.items = Load::Ready(Arc::new(items));
                            this.continuation = cont;
                        }
                    }
                    Err(_) if from_cache => {}
                    Err(e) => this.items = Load::Error(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some(tok) = self.continuation.clone() else {
            self.loading_more = false;
            return;
        };
        let serial = self.serial;
        cx.spawn(async move |this, cx| {
            let res = blocking(move || {
                if let Some(t) = tok.strip_prefix(WEB) {
                    return crate::api::web::browse_continue(t).map(|mut p| {
                        p.continuation = p.continuation.map(|c| format!("{WEB}{c}"));
                        p
                    });
                }
                if let Some(t) = tok.strip_prefix(MUSIC) {
                    return crate::api::auth::music_continue(t).map(|mut p| {
                        p.continuation = p.continuation.map(|c| format!("{MUSIC}{c}"));
                        p
                    });
                }
                match tok.strip_prefix(ACCOUNT) {
                    Some(t) => crate::api::auth::browse_continue(t).map(|mut p| {
                        p.continuation = p.continuation.map(|c| format!("{ACCOUNT}{c}"));
                        p
                    }),
                    None => innertube::browse_continue(&tok),
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                this.loading_more = false;
                if let Ok(p) = res {
                    if let Some(items) = this.items.ready_mut() {
                        Arc::make_mut(items).extend(p.items);
                    }
                    this.continuation = p.continuation;
                } else {
                    this.continuation = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_rows(&mut self, rows: Vec<FeedRow>, card_width: Pixels, rem: Pixels, window: &Window, cx: &Context<Self>) -> bool {
        let geometry_changed = self.row_size != (card_width, rem);
        if !geometry_changed && self.rows.as_ref() == &rows {
            return false;
        }

        let old_count = self.scroll.item_count();
        let new_count = rows.len();
        let shared = old_count.min(new_count);
        if new_count != old_count {
            self.scroll.splice(shared..old_count, new_count - shared);
        }
        if geometry_changed {
            self.scroll.remeasure();
        } else if let Some(first_changed) = self.rows.iter().zip(&rows).position(|(old, new)| old != new) {
            self.scroll.remeasure_items(first_changed..shared);
        }
        self.rows = Arc::new(rows);
        self.row_size = (card_width, rem);

        // Width changes clear the list's estimates during layout. Seed any still
        // unmeasured rows after that layout so the scrollbar can reach the end.
        let state = self.scroll.clone();
        let entity = cx.entity_id();
        let estimate = card_width * (9. / 16.) + rem * 7.0 + grid_row_gap();
        window.on_next_frame(move |_, cx| {
            state.with_uniform_item_height(estimate);
            cx.notify(entity);
        });
        true
    }
}

impl Render for HomePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let s = store::settings_snapshot();
        let rem = window.rem_size();
        let width = content_width(cx);
        let (columns, card_width) = grid_columns(width, 300.0, rem);
        let mut chips = div().flex().gap_3().py_3().px_6().bg(p.bg).child(
            chip(
                "chip-all",
                if crate::api::auth::signed_in() || crate::api::web::connected() || !store::subscriptions().is_empty() {
                    t("All")
                } else if crate::recommend::available() {
                    t("For you")
                } else {
                    t("Popular")
                },
                self.chip == 0,
                cx,
            )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.chip = 0;
                    this.load(cx);
                })),
        );
        chips = chips.child(chip("chip-recent", t("Recently uploaded"), self.chip == RECENT, cx).on_click(cx.listener(|this, _, _, cx| {
            this.chip = RECENT;
            this.load(cx);
        })));
        if !s.hide_explore {
            for (i, (name, _)) in EXPLORE.iter().enumerate() {
                chips = chips.child(chip(SharedString::from(format!("chip-{i}")), t(name), self.chip == i + 1, cx).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.chip = i + 1;
                        this.load(cx);
                    },
                )));
            }
        }
        if self.chip == MUSIC_CHIP {
            let on = crate::ui::music::on(cx);
            chips = chips.child(div().flex_1()).child(
                chip("chip-music-mode", t(if on { "Leave music mode" } else { "Music mode" }), on, cx)
                    .on_click(move |_, _, cx| crate::ui::music::set(!on, cx)),
            );
        }
        let message = |content: AnyElement| {
            div().absolute().inset_0().px_6().pt_2().pb_24().child(content).into_any_element()
        };
        let body: AnyElement = match &self.items {
            Load::Loading => message(spinner(cx).into_any_element()),
            Load::Error(e) => message(error_state(e, cx).into_any_element()),
            Load::Ready(items) => {
                let items = items.clone();
                let mut rows = feed_rows(&items, &s, columns, width, rem, cx);
                if rows.is_empty() && self.continuation.is_none() {
                    message(empty_state("tv", "Nothing here yet", "", cx).into_any_element())
                } else {
                    if self.loading_more {
                        rows.push(FeedRow::Loading { gap: rem * 6.0 });
                    } else if let Some(FeedRow::Cards { gap, .. } | FeedRow::Shorts { gap, .. }) = rows.last_mut() {
                        // Include the bottom margin in the last item's measured height
                        // so jumping to the end also reveals the last card's metadata.
                        *gap += rem * 6.0;
                    }
                    let changed = self.sync_rows(rows, card_width, rem, window, cx);
                    if !changed && !self.loading_more && self.continuation.is_some() && near_bottom(&self.scroll, 900.0) {
                        self.loading_more = true;
                        cx.defer_in_entity(|page, cx| page.load_more(cx));
                        cx.notify();
                    }
                    let rows = self.rows.clone();
                    list(self.scroll.clone(), move |index, _, cx| render_feed_row(&rows[index], &items, card_width, cx))
                        .absolute().inset_0().px_6().pt_2().into_any_element()
                }
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(chips.overflow_hidden())
            .child(scroll_viewport("home-scroll", &self.scroll, body, window, cx))
    }
}
