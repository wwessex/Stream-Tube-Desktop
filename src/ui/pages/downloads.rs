use crate::downloads::{Downloads, JobStatus};
use crate::i18n::{t, tf};
use crate::store;
use crate::ui::app::{Route, navigate};
use crate::ui::widgets::*;
use crate::ui::*;

pub struct DownloadsPage {
    scroll: ScrollHandle,
}

impl DownloadsPage {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { scroll: ScrollHandle::new() }
    }
}

impl Render for DownloadsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = pal(cx);
        let jobs = cx.default_global::<Downloads>().jobs.lock().clone();
        let dir = store::settings_snapshot().download_dir.clone();
        let ytdlp = crate::api::streams::ytdlp_path().is_some();
        let radius = rems(0.5 * thumbnail_corner_scale());
        let mut list = div().flex().flex_col().gap_3();
        for j in jobs.iter().rev() {
            let id = j.id;
            let vid = j.video.id.clone();
            let (status, color): (String, Hsla) = match &j.status {
                JobStatus::Queued => (t("Queued").into(), p.text2),
                JobStatus::Running => (if j.detail.is_empty() { t("Starting…").into() } else { j.detail.clone() }, p.text2),
                JobStatus::Done(path) => (tf!("Saved to {0}", path), rgb(0x2ba640).into()),
                JobStatus::Failed(e) => (tf!("Failed: {0}", e), rgb(0xff4e45).into()),
                JobStatus::Cancelled => (t("Cancelled").into(), p.text3),
            };
            let done_path = if let JobStatus::Done(path) = &j.status { Some(path.clone()) } else { None };
            let running = matches!(j.status, JobStatus::Running | JobStatus::Queued);
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .p_3()
                    .rounded_xl()
                    .bg(p.surface)
                    .child(
                        div()
                            .id(SharedString::from(format!("dlt-{id}")))
                            .flex_none()
                            .w(px(160.))
                            .h(px(90.))
                            .overflow_hidden()
                            .cursor_pointer()
                            .on_click(move |_, window, cx| navigate(Route::Watch { id: vid.clone(), playlist: None, start: None }, window, cx))
                            .child(img(SharedString::from(j.video.thumbnail.clone())).size_full().aspect_ratio(16. / 9.).rounded(radius).object_fit(ObjectFit::Cover)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(selectable_text(SharedString::from(format!("dl-title-{id}")), j.video.title.clone()).text_base().font_weight(FontWeight::SEMIBOLD).text_color(p.text).truncate())
                            .child(selectable_text(SharedString::from(format!("dl-meta-{id}")), tf!("{0}, format {1}", j.video.channel_name, j.format)).text_xs().text_color(p.text2))
                            .when(running, |d| {
                                d.child(div().w_full().h(px(4.)).rounded_full().bg(p.hover).child(div().h_full().rounded_full().w(relative(j.progress.clamp(0.0, 1.0))).bg(p.accent)))
                            })
                            .child(selectable_text(SharedString::from(format!("dl-status-{id}")), status).text_xs().text_color(color).truncate()),
                    )
                    .when(running, |d| d.child(pill(SharedString::from(format!("dlc-{id}")), Some("x"), "Cancel", cx).on_click(move |_, _, cx| crate::downloads::cancel(id, cx))))
                    .when_some(done_path, |d, path| {
                        let p2 = path.clone();
                        d.child(pill(SharedString::from(format!("dlo-{id}")), Some("play"), "Open", cx).on_click(move |_, _, cx| crate::downloads::open_file(&path, cx)))
                            .child(icon_button(SharedString::from(format!("dlf-{id}")), "folder-open", cx).on_click(move |_, _, cx| crate::ui::app::reveal_in_folder(&p2, cx)))
                            .child(icon_button(SharedString::from(format!("dld-{id}")), "trash-2", cx).on_click(move |_, _, cx| crate::downloads::delete_file(id, cx)))
                    })
                    .when(!running, |d| d.child(icon_button(SharedString::from(format!("dlr-{id}")), "x", cx).on_click(move |_, _, cx| crate::downloads::remove(id, cx)))),
            );
        }
        if jobs.is_empty() {
            list = list.child(empty_state("download", "No downloads yet", "", cx));
        }
        let dir2 = dir.clone();
        page_scroll("dl-scroll", &self.scroll).child(
            div().flex().justify_center().px_6().pt_6().pb_24().child(
                div()
                    .w(px(900.))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(section_title(t("Downloads"), cx).text_3xl())
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .when(jobs.iter().any(|j| !matches!(j.status, JobStatus::Running | JobStatus::Queued)), |d| {
                                        d.child(pill("dl-clear", Some("list-x"), "Clear list", cx).on_click(|_, _, cx| crate::downloads::clear_history(cx)))
                                    })
                                    .child(pill("dl-change", Some("folder-cog"), "Change folder", cx).on_click(|_, _, cx| {
                                        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some(t("Save downloads here").into()) });
                                        cx.spawn(async move |cx| {
                                            if let Ok(Ok(Some(paths))) = rx.await {
                                                if let Some(d) = paths.into_iter().next() {
                                                    store::update_settings(|s| {
                                                        s.download_dir = d.to_string_lossy().into_owned();
                                                        s.download_dir_chosen = true;
                                                    });
                                                    let _ = cx.update(|cx| cx.refresh_windows());
                                                }
                                            }
                                        })
                                        .detach();
                                    }))
                                    .child(pill("dl-folder", Some("folder-open"), "Open folder", cx).on_click(move |_, _, _| {
                                        let _ = std::fs::create_dir_all(&dir2);
                                        let _ = open::that(&dir2);
                                    })),
                            ),
                    )
                    .child(selectable_text("dl-dir-info", tf!("Saving to {0}. Engine {1}", dir, if ytdlp { t("yt-dlp (all qualities, SponsorBlock chapters)") } else { t("built-in (combined 360p/720p or audio only). Install yt-dlp for more") })).text_sm().text_color(p.text2))
                    .child(list),
            ),
        )
    }
}
