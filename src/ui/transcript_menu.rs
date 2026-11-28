use super::app::{MenuContent, OpenMenu, open_menu};
use super::widgets::Load;
use super::*;
use crate::api::types::{CaptionTrack, VideoItem};
use crate::downloads::{
    self,
    transcript::{self, Choice},
};
use crate::i18n::t;
use gpui_kit::component::input::{Input, InputEvent, InputState};

const SEARCH_THRESHOLD: usize = 8;

pub(super) fn open_picker(
    pos: Point<Pixels>,
    video: &VideoItem,
    format: &'static str,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| TrackMenu::new(video.clone(), format, pos, window, cx));
    cx.set_global(OpenMenu(Some((pos, MenuContent::View(picker.into())))));
    cx.refresh_windows();
}

struct TrackMenu {
    video: VideoItem,
    format: &'static str,
    anchor: Point<Pixels>,
    tracks: Load<Vec<Choice>>,
    search: Entity<InputState>,
    scroll: ScrollHandle,
    _subscription: Subscription,
}

impl TrackMenu {
    fn new(
        video: VideoItem,
        format: &'static str,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t("Search languages")));
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this: &mut Self, input, event: &InputEvent, _, cx| match event {
                InputEvent::Change => {
                    this.scroll.set_offset(Point::default());
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    let query = input.read(cx).value().to_string();
                    if let Some(tracks) = this.tracks.ready() {
                        let mut matches = tracks.iter().filter(|c| c.matches(&query));
                        if let Some(choice) = matches.next().filter(|_| matches.next().is_none()) {
                            this.download(&choice.track, cx);
                        }
                    }
                }
                _ => {}
            },
        );
        let id = video.id.clone();
        cx.spawn_in(window, async move |menu, cx| {
            let result = blocking(move || transcript::available(&id)).await;
            let _ = menu.update_in(cx, |menu, window, cx| {
                // The user may have gone back, dismissed this menu, or changed tabs while loading.
                if !matches!(&cx.global::<OpenMenu>().0, Some((_, MenuContent::View(view))) if view.entity_id() == cx.entity_id()) {
                    return;
                }
                menu.tracks = match result {
                    Ok(tracks) if tracks.is_empty() => Load::Error(t("No transcript available").into()),
                    Ok(tracks) if tracks.len() == 1 => {
                        menu.download(&tracks[0].track, cx);
                        return;
                    }
                    Ok(tracks) => {
                        if tracks.len() >= SEARCH_THRESHOLD {
                            window.focus(&menu.search.read(cx).focus_handle(cx), cx);
                        }
                        Load::Ready(tracks)
                    }
                    Err(_) => Load::Error(t("Could not load captions").into()),
                };
                cx.notify();
            });
        }).detach();
        Self {
            video,
            format,
            anchor,
            tracks: Load::Loading,
            search,
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    fn download(&self, track: &CaptionTrack, cx: &mut App) {
        cx.set_global(OpenMenu(None));
        downloads::start_with_track(
            self.video.clone(),
            Some(self.format.into()),
            Some(track.clone()),
            cx,
        );
        cx.refresh_windows();
    }
}

impl Render for TrackMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let mut panel = div().w(px(300.)).text_sm().text_color(p.text).child(
            div()
                .id("caption-back")
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .h_8()
                .cursor_pointer()
                .hover(move |s| s.bg(p.hover))
                .child(icon("arrow-left").size_4().text_color(p.text))
                .child(t("Back"))
                .on_click(cx.listener(|this, _, window, cx| {
                    open_menu(
                        this.anchor,
                        super::cards::download_menu_items(&this.video, this.anchor),
                        window,
                        cx,
                    )
                })),
        );
        let tracks = match &self.tracks {
            Load::Ready(tracks) => tracks,
            state => {
                let message = match state {
                    Load::Error(s) => s.clone(),
                    _ => t("Loading…").into(),
                };
                return panel.child(div().px_3().py_2().text_color(p.text2).child(message));
            }
        };
        if tracks.len() >= SEARCH_THRESHOLD {
            panel = panel.child(
                div()
                    .px_3()
                    .py_1()
                    .child(Input::new(&self.search).cleanable(true)),
            );
        }
        let query = self.search.read(cx).value().to_string();
        let height = (f32::from(window.viewport_size().height) - 110.).clamp(72., 320.);
        let mut list = div()
            .id("caption-languages")
            .max_h(px(height))
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        let mut found = false;
        for (i, choice) in tracks
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.matches(&query))
        {
            found = true;
            list = list.child(
                div()
                    .id(("caption-track", i))
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .hover(move |s| s.bg(p.hover))
                    .child(choice.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(choice) = this.tracks.ready().and_then(|tracks| tracks.get(i)) {
                            this.download(&choice.track, cx);
                        }
                    })),
            );
        }
        if !found {
            list = list.child(
                div()
                    .px_3()
                    .py_2()
                    .text_color(p.text2)
                    .child(t("No results found")),
            );
        }
        panel.child(list)
    }
}
