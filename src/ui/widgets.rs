//! Shared page helpers.
use super::*;
use crate::api::types::DescriptionLink;
use crate::i18n::{t, tf};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use std::rc::Rc;

pub enum Load<T> {
    Loading,
    Ready(T),
    Error(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(t) => Some(t),
            _ => None,
        }
    }
    pub fn ready_mut(&mut self) -> Option<&mut T> {
        match self {
            Load::Ready(t) => Some(t),
            _ => None,
        }
    }
}

/// Vertical scroll container used by every page, with a scrollbar that can be
/// dragged (or clicked on its track) and keyboard scrolling (see [`scroll_page_key`]).
pub fn page_scroll(id: &'static str, handle: &ScrollHandle) -> PageScroll {
    PageScroll {
        id,
        inner: div().id("page-scroll-content").absolute().inset_0().overflow_y_scroll().track_scroll(handle),
        handle: handle.clone(),
    }
}

/// A page's scroll area. The scrollbar is added after the page's content so it
/// is drawn, and takes clicks, on top of it.
#[derive(IntoElement)]
pub struct PageScroll {
    id: &'static str,
    inner: Stateful<Div>,
    handle: ScrollHandle,
}

impl PageScroll {
    pub fn flex_col(mut self) -> Self {
        self.inner = self.inner.flex().flex_col();
        self
    }
}

impl ParentElement for PageScroll {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.inner.extend(elements)
    }
}

impl RenderOnce for PageScroll {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        scroll_viewport(self.id, &self.handle, self.inner, window, cx)
    }
}

/// Give virtual lists and regular scroll areas the same keyboard, glide and scrollbar behavior.
pub fn scroll_viewport<H: PageScrollHandle + Clone + 'static, E: IntoElement>(
    id: &'static str,
    handle: &H,
    content: E,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<H, E> {
    let shared: Rc<dyn PageScrollHandle> = Rc::new(handle.clone());
    ACTIVE_PAGE.with(|a| *a.borrow_mut() = Some((id, shared.clone())));
    let mode = if crate::store::settings_snapshot().always_scrollbars { ScrollbarMode::Always } else { ScrollbarMode::Hover };
    div()
        .id(id)
        .relative()
        .size_full()
        .overflow_hidden()
        // Update the scroll position before the content reads it for this frame.
        .child(smooth_scroll(id, shared))
        .child(content)
        // Keep the scrollbar overlay in viewport coordinates as the content moves underneath it.
        .when(!window.is_inspector_picking(cx), |page| {
            page.child(div().absolute().inset_0().child(
                Scrollbar::vertical(handle).id("page-scrollbar").mode(mode).viewport_from_layout(),
            ))
        })
}

pub trait PageScrollHandle: ScrollbarHandle {
    fn jump_to_end(&self) -> bool {
        false
    }
}

impl PageScrollHandle for ScrollHandle {}

impl PageScrollHandle for ListState {
    fn jump_to_end(&self) -> bool {
        self.scroll_to_end();
        true
    }
}

type ActivePage = (&'static str, Rc<dyn PageScrollHandle>);

thread_local! {
    /// the page scroll area drawn last, which is the page on screen
    static ACTIVE_PAGE: std::cell::RefCell<Option<ActivePage>> = const { std::cell::RefCell::new(None) };
    static GLIDES: std::cell::RefCell<std::collections::HashMap<&'static str, Glide>> = Default::default();
}

struct Glide {
    /// where the page is drawn now / where it's heading (offset.y, <= 0)
    shown: f32,
    target: f32,
    last: std::time::Instant,
    /// the last wheel event came from a touchpad
    precise: bool,
    /// a mouse wheel moved the page since the last frame (anything else that
    /// moves it, like dragging the scrollbar, is followed exactly)
    wheel: Option<std::time::Instant>,
}

/// Scrolls the page on screen for PageUp/PageDown, Home/End, the arrow keys and
/// Space / Shift+Space. Returns false when the key isn't a scrolling key.
pub fn scroll_page_key(key: &str, shift: bool, window: &mut Window) -> bool {
    let Some((id, handle)) = ACTIVE_PAGE.with(|a| a.borrow().clone()) else { return false };
    if key == "end" && handle.jump_to_end() {
        GLIDES.with(|g| g.borrow_mut().remove(id));
        window.refresh();
        return true;
    }
    let view: f32 = handle.viewport_bounds().size.height.into();
    let max: f32 = scroll_limit(handle.as_ref()).into();
    let page = (view * 0.85).max(100.0);
    let delta = match (key, shift) {
        ("pagedown", _) | ("space", false) => -page,
        ("pageup", _) | ("space", true) => page,
        ("down", _) => -80.0,
        ("up", _) => 80.0,
        ("home", _) => f32::INFINITY,
        ("end", _) => f32::NEG_INFINITY,
        _ => return false,
    };
    let now: f32 = handle.offset().y.into();
    GLIDES.with(|g| {
        let mut g = g.borrow_mut();
        let st = g.entry(id).or_insert(Glide { shown: now, target: now, last: std::time::Instant::now(), precise: false, wheel: None });
        // from where it's already heading, so repeated presses add up
        let in_flight = (st.target - now).abs() > 0.5 && !st.precise;
        let from = if in_flight { st.target } else { now };
        if !in_flight {
            st.last = std::time::Instant::now();
        }
        st.precise = false;
        st.target = (from + delta).clamp(-max.max(0.0), 0.0);
        st.shown = now;
    });
    window.refresh();
    true
}

/// Mouse-wheel scrolling that glides instead of jumping a fixed step per notch.
/// gpui moves the page by the whole step; this notices the jump, keeps it as the
/// target and eases the visible position there over the next frames. Touchpad
/// scrolling is already smooth and passes through untouched.
fn smooth_scroll(id: &'static str, handle: Rc<dyn PageScrollHandle>) -> impl IntoElement {
    /// how quickly it settles: ~63% of the way every TAU
    const TAU: f32 = 0.045;
    /// mouse-wheel notches move a bit further than gpui's line step
    const WHEEL_BOOST: f32 = 1.4;
    let wheel_handle = handle.clone();
    canvas(
        move |_, window, _| {
            let now = std::time::Instant::now();
            let actual: f32 = handle.offset().y.into();
            let max: f32 = scroll_limit(handle.as_ref()).into();
            GLIDES.with(|g| {
                let mut g = g.borrow_mut();
                let st = g.entry(id).or_insert(Glide { shown: actual, target: actual, last: now, precise: false, wheel: None });
                let wheel = st.wheel.take();
                let moved = actual - st.shown;
                if moved.abs() > 0.01 {
                    // something moved the page since the last frame
                    let Some(wheel_at) = wheel.filter(|_| !st.precise && moved.abs() <= 3000.0) else {
                        // touchpad, the scrollbar, or a jump (new page, scroll to top): follow it exactly
                        *st = Glide { shown: actual, target: actual, last: now, precise: st.precise, wheel: None };
                        return;
                    };
                    if (st.target - st.shown).abs() < 0.5 {
                        st.last = wheel_at;
                    }
                    st.target += moved * WHEEL_BOOST;
                }
                st.target = st.target.clamp(-max.max(0.0), 0.0);
                let gap = st.target - st.shown;
                if gap.abs() < 0.5 {
                    st.shown = st.target;
                } else {
                    let dt = now.duration_since(st.last).as_secs_f32().min(0.05);
                    st.shown += gap * (1.0 - (-dt / TAU).exp());
                    window.request_animation_frame();
                }
                st.last = now;
                if (st.shown - actual).abs() > 0.01 {
                    handle.set_offset(point(handle.offset().x, px(st.shown)));
                }
            });
        },
        move |_, _, window, _| {
            let wheel_handle = wheel_handle.clone();
            window.on_mouse_event(move |ev: &ScrollEvent, phase, _, _| {
                if phase == DispatchPhase::Capture && wheel_handle.viewport_bounds().contains(&ev.position) {
                    GLIDES.with(|g| {
                        if let Some(st) = g.borrow_mut().get_mut(id) {
                            st.precise = ev.delta.precise();
                            st.wheel.get_or_insert_with(std::time::Instant::now);
                        }
                    });
                }
            });
        },
    )
    .absolute()
    .size_0()
}

/// A scroll area inside a page (queue, chat, chapters). gpui hands a wheel event
/// to every scroll area under the pointer, so without this the page scrolls
/// along with (or instead of) the list the pointer is over.
pub fn nested_scroll(id: impl Into<ElementId>, list: impl IntoElement) -> Stateful<Div> {
    div().id(id).on_scroll(|_, _, cx| cx.stop_propagation()).child(list)
}

/// True when the scroll position is within `margin` of the bottom, or when the
/// page is laid out but its content doesn't fill it (nothing to scroll yet).
fn scroll_limit(handle: &dyn ScrollbarHandle) -> Pixels {
    (handle.content_size().height - handle.viewport_bounds().size.height).max(px(0.))
}

pub fn near_bottom(handle: &impl ScrollbarHandle, margin: f32) -> bool {
    let off: f32 = (-handle.offset().y).into();
    let max: f32 = scroll_limit(handle).into();
    let laid_out = handle.viewport_bounds().size.height > px(0.);
    (max > 0.0 && off >= max - margin) || (laid_out && max <= 0.0)
}

pub fn content_width(cx: &App) -> Pixels {
    let w = cx.global::<crate::ui::app::UiState>().content_width;
    if w <= px(100.) { px(1000.) } else { w }
}

pub fn section_title(text: impl Into<SharedString>, cx: &App) -> Div {
    let p = pal(cx);
    div().text_xl().font_weight(FontWeight::BOLD).text_color(p.text).child(text.into())
}

/// Sets up a "load more" trigger: call from render; spawns `f` when near the bottom.
pub fn maybe_load_more<T: 'static>(
    handle: &impl ScrollbarHandle,
    loading: &mut bool,
    has_more: bool,
    cx: &mut Context<T>,
    f: impl FnOnce(&mut T, &mut Context<T>) + 'static,
) {
    if has_more && !*loading && near_bottom(handle, 900.0) {
        *loading = true;
        cx.defer_in_entity(f);
    }
}

pub trait DeferExt<T> {
    fn defer_in_entity(&mut self, f: impl FnOnce(&mut T, &mut Context<T>) + 'static);
}

impl<T: 'static> DeferExt<T> for Context<'_, T> {
    fn defer_in_entity(&mut self, f: impl FnOnce(&mut T, &mut Context<T>) + 'static) {
        let weak = self.entity().downgrade();
        self.defer(move |cx| {
            if let Some(e) = weak.upgrade() {
                e.update(cx, f);
            }
        });
    }
}

// ───────────────────────── rich text (links, timestamps, hashtags) ─────────────────────────

#[derive(Clone)]
pub(crate) enum Token {
    Url(String),
    Time(f64),
    Hashtag(String),
    Handle(String),
}

pub(crate) fn tokenize(text: &str) -> Vec<(std::ops::Range<usize>, Token)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let at_word_start = i == 0 || !text[..i].chars().last().map(|c| c.is_alphanumeric()).unwrap_or(false);
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(|c: char| c.is_whitespace() || c == ')' || c == '"').unwrap_or(rest.len());
            let url = rest[..end].trim_end_matches(['.', ',', '!', '?']).to_string();
            out.push((i..i + url.len(), Token::Url(url.clone())));
            i += url.len().max(1);
            continue;
        }
        if at_word_start && bytes[i].is_ascii_digit() {
            let end = rest.find(|c: char| !(c.is_ascii_digit() || c == ':')).unwrap_or(rest.len());
            let cand = &rest[..end];
            if cand.contains(':') && !cand.ends_with(':') && cand.split(':').count() <= 3 && cand.split(':').skip(1).all(|p| p.len() == 2) {
                if let Some(secs) = crate::api::json::parse_duration(cand) {
                    out.push((i..i + end, Token::Time(secs as f64)));
                    i += end;
                    continue;
                }
            }
        }
        if at_word_start && (bytes[i] == b'#' || bytes[i] == b'@') && rest.len() > 1 {
            let end = rest[1..]
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '.'))
                .map(|e| e + 1)
                .unwrap_or(rest.len());
            let word = rest[..end].trim_end_matches('.');
            if word.len() > 1 {
                let tok = if bytes[i] == b'#' { Token::Hashtag(word.to_string()) } else { Token::Handle(word.to_string()) };
                out.push((i..i + word.len(), tok));
                i += word.len();
                continue;
            }
        }
        i += rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
    }
    out
}

fn rich_text_tokens(text: &str, links: &[DescriptionLink]) -> Vec<(std::ops::Range<usize>, Token)> {
    let mut tokens = tokenize(text);
    if !links.is_empty() {
        for link in links {
            let range = link.range.start..link.range.end.min(text.len());
            if text.get(range.clone()).is_some_and(|label| !label.is_empty()) {
                tokens.retain(|(r, _)| r.end <= range.start || range.end <= r.start);
                tokens.push((range, Token::Url(link.url.clone())));
            }
        }
        tokens.sort_by_key(|(range, _)| range.start);
    }
    tokens
}

/// Escapes text so Markdown shows it literally.
fn md_escape(t: &str, out: &mut String) {
    for c in t.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Plain text with links, timestamps, #hashtags and @handles as Markdown links
/// (app-internal schemes for the non-URL ones), line breaks kept.
fn to_markdown(text: &str, tokens: &[(std::ops::Range<usize>, Token)]) -> String {
    let mut md = String::with_capacity(text.len() + 64);
    let mut last = 0;
    for (r, tok) in tokens {
        md_escape(&text[last..r.start], &mut md);
        let label = &text[r.clone()];
        md.push('[');
        md_escape(label, &mut md);
        md.push_str("](");
        match tok {
            Token::Url(u) => {
                md.push('<');
                md.push_str(&u.replace(['<', '>', ' '], ""));
                md.push('>');
            }
            Token::Time(t) => md.push_str(&format!("st-time:{}", *t as u64)),
            Token::Hashtag(h) => md.push_str(&format!("st-tag:{}", crate::api::http::urlencode(h))),
            Token::Handle(h) => md.push_str(&format!("st-handle:{}", crate::api::http::urlencode(h))),
        }
        md.push(')');
        last = r.end;
    }
    md_escape(&text[last..], &mut md);
    // single newlines are hard breaks, not spaces
    md.replace("\r\n", "\n").replace('\n', "  \n")
}

fn open_link(url: &str, window: &mut Window, cx: &mut App) {
    if let Some(t) = url.strip_prefix("st-time:") {
        let t: f64 = t.parse().unwrap_or(0.0);
        crate::player::controller::player(cx).update(cx, |p, cx| {
            p.seek_to(t);
            p.set_paused(false, cx);
            cx.notify();
        });
        // back up to the player, like YouTube does
        scroll_page_key("home", false, window);
    } else if let Some(h) = url.strip_prefix("st-tag:") {
        crate::ui::app::navigate(crate::ui::app::Route::Search(crate::api::http::urldecode(h)), window, cx);
    } else if let Some(h) = url.strip_prefix("st-handle:") {
        let url = format!("https://www.youtube.com/{}", crate::api::http::urldecode(h));
        cx.spawn(async move |cx| {
            if let Ok(crate::api::innertube::ResolvedUrl::Channel(id)) = blocking(move || crate::api::innertube::resolve_url(&url)).await {
                let _ = cx.update(|cx| {
                    if let Some(w) = cx.active_window() {
                        let _ = w.update(cx, |_, window, cx| crate::ui::app::navigate(crate::ui::app::Route::Channel(id), window, cx));
                    }
                });
            }
        })
        .detach();
    } else if let Some(route) = crate::ui::app::route_from_link(url).filter(|r| !matches!(r, crate::ui::app::Route::Search(_))) {
        crate::ui::app::navigate(route, window, cx);
    } else {
        crate::ui::open_web(url);
    }
}

/// True when the window currently has a non-empty text selection.
/// Clickable parents use this so finishing a drag-selection doesn't also navigate.
pub fn has_text_selection(window: &mut Window, cx: &mut App) -> bool {
    !gpui_kit::base::TextSelection::selected_text(window, cx).is_empty()
}

/// Plain text that can be dragged to select and copied with Ctrl+C.
/// Right-click opens a menu to copy the selection or the full text.
pub fn selectable_text(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Div {
    let text: SharedString = text.into();
    let copy_all = text.clone();
    div()
        .cursor_text()
        .child(gpui_kit::base::SelectableText::new(id, text))
        .on_mouse_down(MouseButton::Right, move |ev: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            let selected = gpui_kit::base::TextSelection::selected_text(window, cx).trim().to_string();
            let all = copy_all.to_string();
            let mut items = Vec::new();
            if !selected.is_empty() && selected != all {
                items.push(crate::ui::app::MenuItem::new("copy", t("Copy"), move |_, cx| {
                    crate::ui::copy_text_to_clipboard(selected.clone(), cx);
                }));
            }
            let label = if items.is_empty() { t("Copy") } else { t("Copy all text") };
            items.push(crate::ui::app::MenuItem::new("copy", label, move |_, cx| {
                crate::ui::copy_text_to_clipboard(all.clone(), cx);
            }));
            crate::ui::app::open_menu(ev.position, items, window, cx);
        })
}

/// Selectable text (drag to select, Ctrl+C) with clickable links, timestamps,
/// hashtags and handles. Right-click copies the selection or the whole text.
pub fn rich_text(id: impl Into<ElementId>, text: String, cx: &App) -> AnyElement {
    rich_text_with(id, text, &[], cx)
}

pub fn rich_text_with_links(id: impl Into<ElementId>, text: String, links: &[DescriptionLink]) -> AnyElement {
    render_rich_text(id, text, &[], links)
}

/// [`rich_text`] with channel emojis (shortcut, image URL) drawn inline as images.
pub fn rich_text_with(id: impl Into<ElementId>, text: String, emojis: &[(String, String)], _cx: &App) -> AnyElement {
    render_rich_text(id, text, emojis, &[])
}

fn render_rich_text(
    id: impl Into<ElementId>,
    text: String,
    emojis: &[(String, String)],
    links: &[DescriptionLink],
) -> AnyElement {
    let tokens = rich_text_tokens(&text, links);
    let urls: Vec<String> = tokens.iter().filter_map(|(_, t)| if let Token::Url(u) = t { Some(u.clone()) } else { None }).collect();
    let md = if emojis.is_empty() {
        to_markdown(&text, &tokens)
    } else {
        // swap each shortcut for a marker the Markdown conversion leaves alone,
        // then the marker for an inline image
        let mut marked = text.clone();
        for (i, (code, _)) in emojis.iter().enumerate() {
            marked = marked.replace(code.as_str(), &format!("\u{E000}{i}\u{E001}"));
        }
        let mut md = to_markdown(&marked, &tokenize(&marked));
        for (i, (code, url)) in emojis.iter().enumerate() {
            let alt: String = code.chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
            let src: String = url.chars().filter(|c| !matches!(c, '<' | '>' | '"' | ' ')).collect();
            md = md.replace(&format!("\u{E000}{i}\u{E001}"), &format!(r#"<img src="{src}" alt="{alt}" width="18" height="18" />"#));
        }
        md
    };
    let view = gpui_kit::base::TextView::markdown(id, md)
        .selectable(true)
        .on_link_click(|url, _, window, cx| open_link(url, window, cx));
    let copy_text = text;
    div()
        .w_full()
        .child(view)
        .on_mouse_down(MouseButton::Right, move |ev: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            let selected = gpui_kit::base::TextSelection::selected_text(window, cx).trim().to_string();
            let text = copy_text.clone();
            let mut items = Vec::new();
            if !selected.is_empty() && selected != text {
                items.push(crate::ui::app::MenuItem::new("copy", t("Copy"), move |_, cx| {
                    crate::ui::copy_text_to_clipboard(selected.clone(), cx);
                }));
            }
            items.push(crate::ui::app::MenuItem::new("copy", t("Copy all text"), move |_, cx| {
                crate::ui::copy_text_to_clipboard(text.clone(), cx);
            }));
            for u in urls.iter().take(3) {
                let u2 = u.clone();
                let label = if u.chars().count() > 40 { tf!("Copy link {0}…", u.chars().take(40).collect::<String>()) } else { tf!("Copy link {0}", u) };
                items.push(crate::ui::app::MenuItem::new("link", label, move |_, cx| crate::ui::copy_to_clipboard(u2.clone(), cx)));
            }
            crate::ui::app::open_menu(ev.position, items, window, cx);
        })
        .into_any_element()
}

thread_local! {
    /// long comments the user opened with "Read more"
    static OPEN_COMMENTS: std::cell::RefCell<std::collections::HashSet<SharedString>> = Default::default();
}

const CLAMP_LINES: usize = 5;
const CLAMP_CHARS: usize = 400;

/// The start of a long text (whole lines, then cut at a word), or None when it's short.
pub(crate) fn clamp_text(text: &str, max_lines: usize, max_chars: usize) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines.iter().take(max_lines).copied().collect::<Vec<_>>().join("
");
    let mut cut = lines.len() > max_lines;
    if out.chars().count() > max_chars {
        let end = out.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(out.len());
        let end = out[..end].rfind(char::is_whitespace).filter(|&i| i > end / 2).unwrap_or(end);
        out.truncate(end);
        cut = true;
    }
    cut.then(|| format!("{}…", out.trim_end()))
}

/// A comment's text (with channel emojis) and the GIF or sticker posted with it.
/// Long ones are cut to a few lines with "Read more".
pub fn comment_body(id: impl Into<SharedString>, c: &crate::api::types::Comment, cx: &App) -> AnyElement {
    let id: SharedString = id.into();
    let p = pal(cx);
    let open = OPEN_COMMENTS.with(|o| o.borrow().contains(&id));
    let short = clamp_text(&c.text, CLAMP_LINES, CLAMP_CHARS);
    let long = short.is_some();
    let text = if open { c.text.clone() } else { short.unwrap_or_else(|| c.text.clone()) };
    let toggle_id = id.clone();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(rich_text_with(id.clone(), text, &c.emojis, cx))
        .when(long, |d| {
            d.child(
                div()
                    .id(SharedString::from(format!("{id}-more")))
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text2)
                    .cursor_pointer()
                    .hover(move |s| s.text_color(p.text))
                    .child(if open { t("Show less") } else { t("Read more") })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        OPEN_COMMENTS.with(|o| {
                            let mut o = o.borrow_mut();
                            if !o.remove(&toggle_id) {
                                o.insert(toggle_id.clone());
                            }
                        });
                        window.refresh();
                    }),
            )
        })
        .when_some(c.image.clone(), |d, (url, ratio)| {
            // at most 200px tall and 240px wide, aspect kept
            let h = (240.0 / ratio).min(200.0);
            let w = h * ratio;
            // the id gives the image the per-element state gpui keeps the
            // animation frame in; without one a GIF only ever shows its first frame
            d.child(img(SharedUri::from(url)).id(SharedString::from(format!("{id}-img"))).w(px(w)).h(px(h)).rounded_lg().object_fit(ObjectFit::Contain))
        })
        .into_any_element()
}

#[cfg(test)]
mod clamp_tests {
    use super::{DescriptionLink, clamp_text, rich_text_tokens, to_markdown};
    #[test]
    fn clamps() {
        assert_eq!(clamp_text("short", 5, 400), None);
        assert_eq!(clamp_text("a
b
c", 2, 400).as_deref(), Some("a
b…"));
        let long = "word ".repeat(200);
        let c = clamp_text(&long, 5, 400).unwrap();
        assert!(c.chars().count() <= 401 && c.ends_with("word…"));
        // multi-byte text is cut on a char boundary
        assert!(clamp_text(&"é".repeat(500), 5, 400).is_some());
    }

    #[test]
    fn rich_text_preserves_labels_and_links_in_collapsed_descriptions() {
        let text = "Watch [this] at 1:23\r\n#Playlist";
        let playlist_start = text.find('#').unwrap();
        let links = [
            DescriptionLink { range: 6..12, url: "https://www.youtube.com/watch?v=_0do0jAoJt8".into() },
            DescriptionLink { range: playlist_start..text.len() + 10, url: "https://www.youtube.com/playlist?list=PLExample".into() },
            DescriptionLink { range: text.len() + 1..text.len() + 4, url: "https://www.youtube.com/playlist?list=PLOutside".into() },
        ];
        let md = to_markdown(text, &rich_text_tokens(text, &links));
        assert!(md.contains(r"[\[this\]](<https://www.youtube.com/watch?v=_0do0jAoJt8>)"));
        assert!(md.contains("[1\\:23](st-time:83)"));
        assert!(md.contains(r"[\#Playlist](<https://www.youtube.com/playlist?list=PLExample>)"));
        assert!(!md.contains("PLOutside"));
    }
}

/// First timestamp ("1:23", "1:02:03") mentioned in a text, in seconds.
pub fn first_timestamp(text: &str) -> Option<f64> {
    tokenize(text).into_iter().find_map(|(_, t)| if let Token::Time(s) = t { Some(s) } else { None })
}
