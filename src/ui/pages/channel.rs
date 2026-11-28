use crate::api::innertube;
use crate::i18n::{t, tf};
use crate::api::types::*;
use crate::store;
use crate::ui::app::{MenuItem, open_menu};
use crate::ui::cards::*;
use crate::ui::widgets::*;
use crate::ui::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};

pub struct ChannelPage {
    id: String,
    data: Load<ChannelPage0>,
    tab: usize,
    tab_items: Load<Vec<Item>>,
    /// Latest / Popular / Oldest over the grid, like youtube.com
    sort_chips: Vec<crate::api::types::SortChip>,
    continuation: Option<String>,
    loading_more: bool,
    scroll: ScrollHandle,
    search: Entity<InputState>,
    searching: Option<String>,
    serial: u64,
    /// the channel description is shown in full
    about_open: bool,
    _sub: Subscription,
}

#[derive(Clone)]
struct ChannelPage0 {
    info: ChannelItem,
    banner: Option<String>,
    tabs: Vec<ChannelTab>,
}

/// Tabs shown, by kind (see ChannelTab::kind), so channels work in every content language.
const WANTED_TABS: &[&str] = &["featured", "videos", "shorts", "streams", "podcasts", "releases", "playlists", "courses", "shows"];

impl ChannelPage {
    pub fn new(id: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search channel")));
        let sub = cx.subscribe_in(&search, window, |this: &mut Self, st, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                let q = st.read(cx).value().to_string();
                if !q.trim().is_empty() {
                    this.search_channel(q, cx);
                }
            }
        });
        let mut s = Self {
            id,
            data: Load::Loading,
            tab: 0,
            tab_items: Load::Loading,
            sort_chips: Vec::new(),
            continuation: None,
            loading_more: false,
            scroll: ScrollHandle::new(),
            search,
            searching: None,
            serial: 0,
            about_open: false,
            _sub: sub,
        };
        s.load(cx);
        s
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let id = self.id.clone();
        cx.spawn(async move |this, cx| {
            // Videos tab first (most useful), header comes with it
            let res = blocking(move || innertube::channel(&id, Some(crate::feed::VIDEOS_TAB))).await;
            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(cp) => {
                        let tabs: Vec<ChannelTab> = cp
                            .tabs
                            .into_iter()
                            .filter(|t| WANTED_TABS.contains(&t.kind.as_str()))
                            .filter(|t| !(store::settings_snapshot().hide_shorts && t.kind == "shorts"))
                            .collect();
                        this.tab = tabs.iter().position(|t| t.kind == "videos").unwrap_or(0);
                        this.tab_items = Load::Ready(cp.page.items);
                        this.continuation = cp.page.continuation;
                        this.sort_chips = cp.sort_chips;
                        if store::is_subscribed(&cp.info.id) {
                            store::subscribe(&cp.info.id, &cp.info.name, cp.info.avatar.as_deref());
                        }
                        this.data = Load::Ready(ChannelPage0 { info: cp.info, banner: cp.banner, tabs });
                    }
                    Err(e) => this.data = Load::Error(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn select_tab(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(d) = self.data.ready() else { return };
        let Some(t) = d.tabs.get(idx).cloned() else { return };
        self.tab = idx;
        self.searching = None;
        self.tab_items = Load::Loading;
        self.sort_chips.clear();
        self.continuation = None;
        self.serial += 1;
        let serial = self.serial;
        let id = self.id.clone();
        cx.spawn(async move |this, cx| {
            let res = blocking(move || innertube::channel(&id, t.params.as_deref())).await;
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                match res {
                    Ok(cp) => {
                        this.tab_items = Load::Ready(cp.page.items);
                        this.continuation = cp.page.continuation;
                        this.sort_chips = cp.sort_chips;
                    }
                    Err(e) => this.tab_items = Load::Error(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Reloads the tab sorted by the chip at `idx`.
    fn select_sort(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(token) = self.sort_chips.get(idx).filter(|c| !c.selected).map(|c| c.token.clone()) else { return };
        for (i, c) in self.sort_chips.iter_mut().enumerate() {
            c.selected = i == idx;
        }
        self.tab_items = Load::Loading;
        self.continuation = None;
        self.serial += 1;
        let serial = self.serial;
        let info = self.data.ready().map(|d| d.info.clone());
        cx.spawn(async move |this, cx| {
            let res = blocking(move || innertube::browse_continue(&token)).await;
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                match res {
                    Ok(mut p) => {
                        if let Some(info) = &info {
                            for it in p.items.iter_mut() {
                                if let Item::Video(v) = it {
                                    if v.channel_name.is_empty() {
                                        v.channel_name = info.name.clone();
                                    }
                                    v.channel_id.get_or_insert(info.id.clone());
                                    if v.channel_avatar.is_none() {
                                        v.channel_avatar = info.avatar.clone();
                                    }
                                }
                            }
                        }
                        this.tab_items = Load::Ready(p.items);
                        this.continuation = p.continuation;
                    }
                    Err(e) => this.tab_items = Load::Error(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn search_channel(&mut self, q: String, cx: &mut Context<Self>) {
        self.searching = Some(q.clone());
        self.tab_items = Load::Loading;
        self.continuation = None;
        self.serial += 1;
        let serial = self.serial;
        let id = self.id.clone();
        cx.spawn(async move |this, cx| {
            let res = blocking(move || innertube::channel_search(&id, &q)).await;
            let _ = this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                match res {
                    Ok(p) => {
                        this.tab_items = Load::Ready(p.items);
                        this.continuation = p.continuation;
                    }
                    Err(e) => this.tab_items = Load::Error(e.to_string()),
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
        let info = self.data.ready().map(|d| d.info.clone());
        cx.spawn(async move |this, cx| {
            let res = blocking(move || innertube::browse_continue(&tok)).await;
            let _ = this.update(cx, |this, cx| {
                this.loading_more = false;
                match res {
                    Ok(mut p) => {
                        if let Some(info) = &info {
                            for it in p.items.iter_mut() {
                                if let Item::Video(v) = it {
                                    if v.channel_name.is_empty() {
                                        v.channel_name = info.name.clone();
                                    }
                                    v.channel_id.get_or_insert(info.id.clone());
                                }
                            }
                        }
                        if let Some(items) = this.tab_items.ready_mut() {
                            items.extend(p.items);
                        }
                        this.continuation = p.continuation;
                    }
                    Err(_) => this.continuation = None,
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for ChannelPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        localize_placeholder(&self.search, "Search channel", window, cx);
        let p = pal(cx);
        let has_more = self.continuation.is_some();
        maybe_load_more(&self.scroll, &mut self.loading_more, has_more, cx, |this, cx| this.load_more(cx));
        let width = content_width(cx).min(px(1284.));
        let d = match &self.data {
            Load::Loading => return page_scroll("ch-scroll", &self.scroll).child(spinner(cx)).into_any_element(),
            Load::Error(e) => return page_scroll("ch-scroll", &self.scroll).child(error_state(e, cx)).into_any_element(),
            Load::Ready(d) => d.clone(),
        };
        let s = store::settings_snapshot();
        let subscribed = store::is_subscribed(&d.info.id);
        let blocked = s.blocked_channels.iter().any(|(i, _)| *i == d.info.id);
        let speed = store::channel_speed(&d.info.id);
        let info2 = d.info.clone();
        let header = div()
            .flex()
            .flex_col()
            .gap_4()
            .when_some(d.banner.clone(), |el, b| {
                el.child(div().w_full().h(width / 6.2).rounded_2xl().overflow_hidden().bg(p.surface).child(img(SharedString::from(b)).size_full().rounded_2xl().object_fit(ObjectFit::Cover)))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_6()
                    .py_2()
                    .child(avatar(d.info.avatar.as_deref(), px(160.), &d.info.name, cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .flex_1()
                            .child(selectable_text(SharedString::from(format!("ch-name-{}", d.info.id)), d.info.name.clone()).text_3xl().font_weight(FontWeight::BOLD).text_color(p.text))
                            .child(
                                selectable_text(
                                    SharedString::from(format!("ch-meta-{}", d.info.id)),
                                    [d.info.handle.clone(), d.info.subscribers_text.clone().filter(|_| !store::settings_snapshot().hide_subscriber_counts), d.info.video_count_text.clone()]
                                        .into_iter()
                                        .flatten()
                                        .collect::<Vec<_>>()
                                        .join(" • "),
                                )
                                .text_sm()
                                .text_color(p.text2),
                            )
                            .when_some(d.info.description.clone().filter(|x| !x.trim().is_empty()), |el, desc| {
                                // two lines unless opened: some channels write pages here
                                let short = clamp_text(desc.trim(), 2, 180);
                                let long = short.is_some();
                                let open = self.about_open;
                                el.child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .max_w(px(700.))
                                        .child(div().text_sm().text_color(p.text2).child(if open || !long {
                                            rich_text("ch-about", desc.trim().to_string(), cx)
                                        } else {
                                            selectable_text(SharedString::from(format!("ch-about-short-{}", d.info.id)), short.unwrap_or_default()).into_any_element()
                                        }))
                                        .when(long, |d| {
                                            d.child(
                                                div()
                                                    .id("ch-about-more")
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(p.text)
                                                    .cursor_pointer()
                                                    .child(if open { t("Show less") } else { t("Show more") })
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.about_open ^= true;
                                                        cx.notify();
                                                    })),
                                            )
                                        }),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .pt_2()
                                    .child(subscribe_button("ch-sub".into(), &d.info, subscribed, cx))
                                    .when_some(speed, |el, sp| el.child(pill("ch-speed", Some("gauge"), tf!("{0}x speed", sp), cx)))
                                    .child(pill("ch-more", Some("ellipsis"), "", cx).w_9().px_0().justify_center().on_click(move |ev, window, cx| {
                                        let info = info2.clone();
                                        let info3 = info2.clone();
                                        let id = info2.id.clone();
                                        let mut items = vec![
                                            MenuItem::new("link", t("Copy channel link"), move |_, cx| {
                                                copy_to_clipboard(format!("https://www.youtube.com/channel/{}", info.id), cx)
                                            }),
                                            MenuItem::new("globe", t("Open on YouTube"), move |_, _| {
                                                let _ = open::that(format!("https://www.youtube.com/channel/{id}"));
                                            }),
                                        ];
                                        if blocked {
                                            items.push(MenuItem::new("circle-check", t("Unblock channel"), move |_, cx| {
                                                let id = info3.id.clone();
                                                store::update_settings(|s| s.blocked_channels.retain(|(i, _)| *i != id));
                                                cx.refresh_windows();
                                            }));
                                        } else {
                                            items.push(MenuItem::new("ban", t("Block channel (hide everywhere)"), move |_, cx| {
                                                let (id, name) = (info3.id.clone(), info3.name.clone());
                                                store::update_settings(|s| s.blocked_channels.push((id, name)));
                                                cx.refresh_windows();
                                            }));
                                        }
                                        if store::speed_reset_available(&info2.id) {
                                            let id = info2.id.clone();
                                            items.push(MenuItem::new("gauge", t("Reset remembered speed"), move |_, cx| {
                                                store::clear_channel_speed(&id);
                                                cx.refresh_windows();
                                            }));
                                        }
                                        open_menu(ev.position(), items, window, cx);
                                    })),
                            ),
                    ),
            );
        let mut tabs = div().flex().items_center().gap_6().border_b_1().border_color(p.border);
        for (i, t) in d.tabs.iter().enumerate() {
            let active = i == self.tab && self.searching.is_none();
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("ct-{i}")))
                    .py_3()
                    .text_base()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .text_color(if active { p.text } else { p.text2 })
                    .when(active, |el| el.border_b_2().border_color(p.text))
                    .hover(move |s| s.text_color(p.text))
                    .child(crate::i18n::t(&t.title).to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.select_tab(i, cx))),
            );
        }
        tabs = tabs.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .w(px(220.))
                .child(icon("search").size_4().text_color(p.text2))
                .child(Input::new(&self.search).appearance(false).cleanable(true)),
        );
        let tab_kind = d.tabs.get(self.tab).map(|t| t.kind.clone()).unwrap_or_default();
        let body: AnyElement = match &self.tab_items {
            Load::Loading => spinner(cx).into_any_element(),
            Load::Error(e) => error_state(e, cx).into_any_element(),
            Load::Ready(items) => {
                let items: Vec<&Item> = items
                    .iter()
                    .filter(|i| match i {
                        Item::Video(v) => !store::is_filtered(v, &s) || store::settings_snapshot().blocked_channels.iter().any(|(c, _)| Some(c) == v.channel_id.as_ref()),
                        _ => true,
                    })
                    .collect();
                if items.is_empty() {
                    empty_state("video-off", "This channel has no content here", "", cx).into_any_element()
                } else if tab_kind == "shorts" && self.searching.is_none() {
                    let (_, w) = grid_columns(width, 200.0, window.rem_size());
                    grid(items.iter().filter_map(|i| i.as_video()).map(|v| shorts_card(v, w, cx)).collect(), width).into_any_element()
                } else {
                    let (_, w) = grid_columns(width, 280.0, window.rem_size());
                    let mut col = div().flex().flex_col().gap_6().child(grid(items.iter().map(|i| grid_item(i, w, tab_kind == "featured" || self.searching.is_some(), cx)).collect(), width));
                    if self.loading_more {
                        col = col.child(spinner(cx));
                    }
                    col.into_any_element()
                }
            }
        };
        page_scroll("ch-scroll", &self.scroll)
            .child(
                div()
                    .flex()
                    .justify_center()
                    .px_6()
                    .pt_4()
                    .pb_24()
                    .child(div().w(width).flex().flex_col().gap_4().child(header).child(tabs).when(self.searching.is_none() && !self.sort_chips.is_empty(), |d| {
                        d.child(div().flex().gap_3().children(self.sort_chips.iter().enumerate().map(|(i, c)| {
                            chip(SharedString::from(format!("ch-sort-{i}")), c.title.clone(), c.selected, cx)
                                .on_click(cx.listener(move |this, _, _, cx| this.select_sort(i, cx)))
                        })))
                    }).child(body)),
            )
            .into_any_element()
    }
}
