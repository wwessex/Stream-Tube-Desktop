use crate::api::innertube::{self, SearchFilter};
use crate::api::types::*;
use crate::i18n::t;
use crate::store;
use crate::ui::cards::*;
use crate::ui::widgets::*;
use crate::ui::*;
use std::collections::HashSet;
use std::sync::Arc;

pub struct SearchPage {
    query: String,
    filter: SearchFilter,
    show_filters: bool,
    items: Load<Arc<Vec<Item>>>,
    continuation: Option<String>,
    loading_more: bool,
    more_failed: bool,
    last_load_more_top: usize,
    scroll: ListState,
    rows: Arc<Vec<SearchRow>>,
    rows_settings: Option<Arc<store::Settings>>,
    row_geometry: (Pixels, Pixels),
    serial: u64,
}

#[derive(PartialEq)]
enum SearchRow {
    Result(usize),
    Shorts(Vec<usize>),
    Empty,
    More,
}

fn append_results(items: &mut Vec<Item>, incoming: Vec<Item>) {
    let key = |item: &Item| {
        let id = match item {
            Item::Video(video) => &video.id,
            Item::Channel(channel) => &channel.id,
            Item::Playlist(playlist) => &playlist.id,
        };
        (std::mem::discriminant(item), id.clone())
    };
    let mut seen: HashSet<_> = items.iter().map(key).collect();
    items.extend(incoming.into_iter().filter(|item| seen.insert(key(item))));
}

impl SearchPage {
    pub fn new(query: String, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut s = Self {
            query,
            filter: SearchFilter::default(),
            show_filters: false,
            items: Load::Loading,
            continuation: None,
            loading_more: false,
            more_failed: false,
            last_load_more_top: 0,
            scroll: ListState::new(0, ListAlignment::Top, px(240.)),
            rows: Arc::default(),
            rows_settings: None,
            row_geometry: (px(0.), px(0.)),
            serial: 0,
        };
        s.load(cx);
        s
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.items = Load::Loading;
        self.continuation = None;
        self.loading_more = false;
        self.more_failed = false;
        self.last_load_more_top = 0;
        self.scroll.reset(0);
        self.rows = Arc::default();
        self.rows_settings = None;
        self.serial += 1;
        let serial = self.serial;
        let (q, f) = (self.query.clone(), self.filter);
        cx.spawn(async move |this, cx| {
            let res = blocking(move || innertube::search(&q, &f)).await;
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                match res {
                    Ok(p) => {
                        let mut items = Vec::new();
                        append_results(&mut items, p.items);
                        this.items = Load::Ready(Arc::new(items));
                        this.continuation = p.continuation;
                    }
                    Err(e) => this.items = Load::Error(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.loading_more {
            return;
        }
        let Some(tok) = self.continuation.clone() else {
            return;
        };
        self.loading_more = true;
        self.more_failed = false;
        self.last_load_more_top = self.scroll.logical_scroll_top().item_ix;
        let serial = self.serial;
        cx.spawn(async move |this, cx| {
            let res = blocking(move || innertube::search_continue(&tok)).await;
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                this.loading_more = false;
                match res {
                    Ok(p) => {
                        if let Some(items) = this.items.ready_mut() {
                            append_results(Arc::make_mut(items), p.items);
                        }
                        this.continuation = p.continuation.filter(|next| this.continuation.as_ref() != Some(next));
                        this.rows_settings = None;
                    }
                    Err(_) => this.more_failed = true,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn sync_rows(&mut self, settings: &Arc<store::Settings>) -> bool {
        let Some(items) = self.items.ready() else {
            return false;
        };
        if self.rows_settings.as_ref().is_some_and(|current| Arc::ptr_eq(current, settings)) {
            return false;
        }

        let mut rows = Vec::new();
        let mut shorts = Vec::new();
        for (index, item) in items.iter().enumerate() {
            match item {
                Item::Video(video) if store::is_filtered(video, settings) => {}
                Item::Video(video) if video.is_short => {
                    if shorts.len() < 6 {
                        shorts.push(index);
                    }
                }
                _ => rows.push(SearchRow::Result(index)),
            }
        }
        if !shorts.is_empty() {
            rows.push(SearchRow::Shorts(shorts));
        }
        if self.continuation.is_some() {
            rows.push(SearchRow::More);
        } else if rows.is_empty() {
            rows.push(SearchRow::Empty);
        }
        self.rows_settings = Some(settings.clone());
        if self.rows.as_ref() == &rows {
            return false;
        }

        let old_count = self.rows.len();
        let shared = old_count.min(rows.len());
        if old_count != rows.len() {
            self.scroll.splice(shared..old_count, rows.len() - shared);
        }
        if let Some(first_changed) = self.rows.iter().zip(&rows).position(|(old, new)| old != new) {
            self.scroll.remeasure_items(first_changed..shared);
        }
        self.rows = Arc::new(rows);
        true
    }

    fn filter_group(&self, title: &str, options: &[(&str, u8)], current: u8, set: fn(&mut SearchFilter, u8), cx: &mut Context<Self>) -> Div {
        let p = pal(cx);
        let mut col = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(150.))
            .child(div().pb_3().mb_2().border_b_1().border_color(p.border).text_xs().font_weight(FontWeight::BOLD).text_color(p.text).child(t(title).to_uppercase()));
        for (label, v) in options {
            let v = *v;
            let active = current == v && v != 0;
            col = col.child(
                div()
                    .id(SharedString::from(format!("f-{title}-{v}")))
                    .py_2()
                    .text_sm()
                    .cursor_pointer()
                    .text_color(if active { p.text } else { p.text2 })
                    .when(active, |d| d.font_weight(FontWeight::BOLD))
                    .hover(move |s| s.text_color(p.text))
                    .child(t(label).to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let cur = current;
                        set(&mut this.filter, if cur == v { 0 } else { v });
                        this.load(cx);
                    })),
            );
        }
        col
    }

    fn feature(&self, label: &str, on: bool, toggle: fn(&mut SearchFilter), cx: &mut Context<Self>) -> Stateful<Div> {
        let p = pal(cx);
        div()
            .id(SharedString::from(format!("feat-{label}")))
            .py_2()
            .text_sm()
            .cursor_pointer()
            .text_color(if on { p.text } else { p.text2 })
            .when(on, |d| d.font_weight(FontWeight::BOLD))
            .hover(move |s| s.text_color(p.text))
            .child(t(label).to_string())
            .on_click(cx.listener(move |this, _, _, cx| {
                toggle(&mut this.filter);
                this.load(cx);
            }))
    }
}

impl Render for SearchPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let available = content_width(cx);
        let width = available.min(px(1280.));
        let thumb_w = if width < px(900.) { px(240.).min(width * 0.45) } else { px(360.) };
        let rows_changed = self.sync_rows(&store::settings_snapshot());
        let geometry = (available, window.rem_size());
        let resized = self.row_geometry != geometry;
        self.row_geometry = geometry;
        // A page with no new visible rows must wait for scroll progress before fetching again.
        let scrolled = self.scroll.logical_scroll_top().item_ix > self.last_load_more_top;
        if resized {
            self.scroll.remeasure();
        }
        if rows_changed || resized {
            // Seed heights after GPUI lays out the resized list, so the scrollbar
            // can reach results that have not been mounted yet.
            let scroll = self.scroll.clone();
            let entity = cx.entity_id();
            let estimate = thumb_w * (9. / 16.) + geometry.1;
            window.on_next_frame(move |_, cx| {
                scroll.with_uniform_item_height(estimate);
                cx.notify(entity);
            });
        } else if self.continuation.is_some()
            && !self.loading_more
            && !self.more_failed
            && scrolled
            && near_bottom(&self.scroll, 900.0)
        {
            cx.defer_in_entity(Self::load_more);
        }
        let f = self.filter;
        let header = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .justify_between()
            .pb_2()
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [("All", 0u8), ("Videos", 1), ("Channels", 2), ("Playlists", 3), ("Movies", 4)].into_iter().map(|(l, k)| {
                        chip(SharedString::from(format!("t-{l}")), t(l), f.kind == k, cx).on_click(cx.listener(move |this, _, _, cx| {
                            this.filter.kind = k;
                            this.load(cx);
                        }))
                    }),
                ),
            )
            .child(
                pill("filters", Some("sliders-horizontal"), if f.is_default() { "Filters" } else { "Filters (active)" }, cx).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.show_filters ^= true;
                        cx.notify();
                    }),
                ),
            );
        let filters = self.show_filters.then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap_6()
                .p_4()
                .mb_4()
                .rounded_xl()
                .bg(p.surface)
                .child(self.filter_group("Upload date", &[("Last hour", 1), ("Today", 2), ("This week", 3), ("This month", 4), ("This year", 5)], f.date, |f, v| f.date = v, cx))
                .child(self.filter_group("Type", &[("Video", 1), ("Channel", 2), ("Playlist", 3), ("Movie", 4)], f.kind, |f, v| f.kind = v, cx))
                .child(self.filter_group("Duration", &[("Under 4 minutes", 1), ("4 - 20 minutes", 3), ("Over 20 minutes", 2)], f.duration, |f, v| f.duration = v, cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(150.))
                        .child(div().pb_3().mb_2().border_b_1().border_color(p.border).text_xs().font_weight(FontWeight::BOLD).text_color(p.text).child(t("FEATURES")))
                        .child(self.feature("Live", f.live, |f| f.live ^= true, cx))
                        .child(self.feature("4K", f.four_k, |f| f.four_k ^= true, cx))
                        .child(self.feature("HD", f.hd, |f| f.hd ^= true, cx))
                        .child(self.feature("Subtitles/CC", f.subtitles, |f| f.subtitles ^= true, cx))
                        .child(self.feature("Creative Commons", f.creative_commons, |f| f.creative_commons ^= true, cx))
                        .child(self.feature("HDR", f.hdr, |f| f.hdr ^= true, cx)),
                )
                .child(self.filter_group("Sort by", &[("Relevance", 0), ("Upload date", 2), ("View count", 3), ("Rating", 1)], f.sort, |f, v| f.sort = v, cx))
        });
        let controls = div()
            .id("search-controls")
            .flex_none()
            .max_h(relative(0.65))
            .overflow_y_scroll()
            .px_6()
            .pt_4()
            .child(div().flex().justify_center().child(div().w(width).child(header).children(filters)));
        let body = match &self.items {
            Load::Loading => spinner(cx).into_any_element(),
            Load::Error(e) => error_state(e, cx).into_any_element(),
            Load::Ready(items) => {
                let items = items.clone();
                let rows = self.rows.clone();
                let page = cx.entity().downgrade();
                let loading_more = self.loading_more;
                let more_failed = self.more_failed;
                list(self.scroll.clone(), move |index, _, cx| {
                    let element = match &rows[index] {
                        SearchRow::Result(item) => match &items[*item] {
                            Item::Video(video) => video_row(video, thumb_w, cx),
                            Item::Channel(channel) => channel_row(channel, thumb_w, cx),
                            Item::Playlist(playlist) => playlist_row(playlist, thumb_w, cx),
                        },
                        SearchRow::Shorts(indices) => div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .py_4()
                            .border_t_1()
                            .border_b_1()
                            .border_color(p.border)
                            .child(div().flex().items_center().gap_2().child(icon("zap").text_color(p.accent)).child(section_title("Shorts", cx)))
                            .child(div().flex().gap_4().overflow_hidden().children(
                                indices.iter().filter_map(|&index| items[index].as_video()).map(|video| shorts_card(video, px(190.), cx)),
                            ))
                            .into_any_element(),
                        SearchRow::Empty => empty_state("search-x", "No results found", t("Try different keywords or remove filters"), cx).into_any_element(),
                        SearchRow::More if loading_more => spinner(cx).into_any_element(),
                        SearchRow::More => {
                            let page = page.clone();
                            let label = if more_failed { "Retry" } else { "Show more" };
                            div().flex().justify_center().py_4().child(
                                pill("search-more", None, label, cx)
                                    .tab_index(0)
                                    .focus(move |style| style.bg(p.hover))
                                    .on_click(move |_, _, cx| {
                                        let _ = page.update(cx, Self::load_more);
                                    }),
                            ).into_any_element()
                        }
                    };
                    div().flex().justify_center().child(div().w(width).pb_4().child(element)).into_any_element()
                })
                .absolute()
                .inset_0()
                .px_6()
                .pb_24()
                .into_any_element()
            }
        };
        div().size_full().flex().flex_col().child(controls).child(
            div().relative().flex_1().min_h_0().child(scroll_viewport("search-scroll", &self.scroll, body, window, cx)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::append_results;
    use crate::api::types::{Item, VideoItem};

    #[test]
    fn keeps_search_rank_when_pages_repeat_results() {
        let video = |id: &str| Item::Video(VideoItem { id: id.into(), ..Default::default() });
        let mut results = vec![video("first")];

        append_results(&mut results, vec![video("first"), video("new"), video("new"), video("last")]);

        assert_eq!(results, [video("first"), video("new"), video("last")]);
    }
}
