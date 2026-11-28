use crate::{
    entities::{Channel, Video},
    features::{
        account,
        library::Library,
        player,
        services::{self, Extras},
        youtube::{self, Category, Client, Cursor, Interests, Page},
    },
    shared::i18n::{self, Language},
};
use slint::{ComponentHandle, Model, SharedPixelBuffer, VecModel};
use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

slint::include_modules!();

type State = Rc<RefCell<AppState>>;

struct AppState {
    library: Library,
    device_language: Language,
    storage_writable: bool,
    request: Arc<AtomicU64>,
    query: String,
    channel: Option<Channel>,
    return_from_channel: Screen,
    playlist: Option<String>,
    next_page: Arc<Mutex<Option<FeedCursor>>>,
    playback_number: i32,
    playback: Option<Playback>,
    account_request: i32,
    account_channels: Vec<Channel>,
}

struct Playback {
    number: i32,
    feed_generation: Option<u64>,
    shorts: bool,
    videos: Vec<Video>,
}

enum Feed {
    Home(Category),
    ForYou(Interests),
    Shorts,
    Following(Vec<Channel>),
    Channel(Channel),
    Search(String),
    Playlist(String),
    Local(Vec<Video>),
    Next(FeedCursor),
}

#[derive(Clone)]
enum FeedCursor {
    YouTube(Cursor),
    Following(Vec<Channel>),
}

struct FeedPage {
    videos: Vec<Video>,
    next: Option<FeedCursor>,
}

impl From<Page> for FeedPage {
    fn from(page: Page) -> Self {
        Self {
            videos: page.videos,
            next: page.next.map(FeedCursor::YouTube),
        }
    }
}

pub fn run() -> Result<(), slint::PlatformError> {
    let window = StreamTube::new()?;
    let device_language = sys_locale::get_locale()
        .as_deref()
        .map_or(Language::English, Language::from_locale);
    let stored = Library::load();
    let storage_writable = stored.is_ok();
    let library = stored.unwrap_or_default();
    window
        .global::<UiText>()
        .on_lookup(|language, source| i18n::translate(&language, &source).into());
    window.set_languages(
        Rc::new(VecModel::from(
            Language::OPTIONS
                .iter()
                .map(|locale| LanguageRow {
                    code: locale.code().into(),
                    name: locale.name().into(),
                    credit: locale.credit().into(),
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    select_language(&window, library.language, device_language);
    window.set_thumbnails(library.thumbnails);
    apply_extras(&window, library.extras);
    window.set_account_available(account::available());
    let reconnect = library.connected_account;
    window.set_videos(Rc::new(VecModel::<VideoRow>::default()).into());
    if !storage_writable {
        window.set_notice("Local library is unavailable".into());
    }

    let state = Rc::new(RefCell::new(AppState {
        library,
        device_language,
        storage_writable,
        request: Arc::new(AtomicU64::new(0)),
        query: String::new(),
        channel: None,
        return_from_channel: Screen::Home,
        playlist: None,
        next_page: Arc::default(),
        playback_number: 0,
        playback: None,
        account_request: 0,
        account_channels: Vec::new(),
    }));
    bind_navigation(&window, &state);
    bind_videos(&window, &state);
    bind_player(&window, &state);
    bind_settings(&window, &state);
    bind_account(&window, &state);
    reload(&window, &state);
    if reconnect && window.get_account_available() {
        authorize(&window, &state, false, false);
    }
    window.run()
}

fn bind_navigation(window: &StreamTube, state: &State) {
    let (target, app) = (window.as_weak(), state.clone());
    window.on_navigate(move |screen| {
        if let Some(ui) = target.upgrade() {
            if matches!(
                screen,
                Screen::Home | Screen::Shorts | Screen::Following | Screen::Library
            ) {
                ui.set_tab(screen);
            }
            ui.set_screen(screen);
            reload(&ui, &app);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_back(move || {
        if let Some(ui) = target.upgrade() {
            let screen = match ui.get_screen() {
                Screen::Channel => app.borrow().return_from_channel,
                Screen::Playlist => Screen::Search,
                Screen::Language => Screen::Settings,
                _ => ui.get_tab(),
            };
            ui.set_screen(screen);
            reload(&ui, &app);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_reload(move || {
        if let Some(ui) = target.upgrade() {
            reload(&ui, &app);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_submit(move |input| {
        let Some(ui) = target.upgrade() else { return };
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        if let Some(id) = youtube::video_id(input) {
            let language = current_language(&ui);
            let title = i18n::translate(language.code(), "YouTube video");
            open_player(
                &ui,
                &app,
                vec![Video {
                    id,
                    title: title.into(),
                    ..Video::default()
                }],
                0,
                false,
                false,
            );
            return;
        }
        if let Some(playlist) = youtube::playlist_id(input) {
            app.borrow_mut().playlist = Some(playlist);
            ui.set_screen(Screen::Playlist);
            reload(&ui, &app);
            return;
        }
        if input.contains("://") || input.starts_with("www.") {
            ui.set_notice("Enter a YouTube video link".into());
            return;
        }
        ui.set_notice("".into());
        let query: String = input.chars().take(200).collect();
        app.borrow_mut().query = query.clone();
        edit_library(&ui, &app, |library| library.searched(query));
        reload(&ui, &app);
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_open_channel(move |id, name| {
        let Some(ui) = target.upgrade() else { return };
        if !youtube::is_channel_id(&id) {
            return;
        }
        {
            let mut state = app.borrow_mut();
            if ui.get_screen() != Screen::Channel {
                state.return_from_channel = ui.get_screen();
            }
            state.channel = Some(Channel {
                id: id.to_string(),
                name: name.to_string(),
            });
        }
        ui.set_screen(Screen::Channel);
        reload(&ui, &app);
    });
}

fn bind_videos(window: &StreamTube, state: &State) {
    let (target, app) = (window.as_weak(), state.clone());
    window.on_watch(move |id| {
        let Some(ui) = target.upgrade() else { return };
        let videos = feed_videos(&ui);
        if let Some(first) = videos.iter().position(|video| video.id == id.as_str()) {
            let shorts = videos[first].short;
            open_player(&ui, &app, videos, first, shorts, true);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_open_shorts(move || {
        let Some(ui) = target.upgrade() else { return };
        let videos = feed_videos(&ui);
        if ui.get_screen() == Screen::Shorts && !videos.is_empty() {
            open_player(&ui, &app, videos, 0, true, true);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_save(move |id| {
        let Some(ui) = target.upgrade() else { return };
        let video = find_video(&ui, &id);
        let Some(video) = video else { return };
        save_video(&ui, &app, video);
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_follow(move || {
        let Some(ui) = target.upgrade() else { return };
        let channel = app.borrow().channel.clone();
        let Some(channel) = channel else { return };
        if app
            .borrow()
            .account_channels
            .iter()
            .any(|remote| remote.id == channel.id)
        {
            return;
        }
        let id = channel.id.clone();
        edit_library(&ui, &app, |library| library.toggle_following(channel));
        ui.set_channel_followed(app.borrow().library.is_following(&id));
    });
}

fn bind_player(window: &StreamTube, state: &State) {
    let target = window.as_weak();
    player::listen(move |number, kind, id| {
        let _ = target.upgrade_in_event_loop(move |ui| {
            ui.invoke_player_event(number, kind, id.into());
        });
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_player_event(move |number, kind, id| {
        let Some(ui) = target.upgrade() else { return };
        let (video, shorts) = {
            let mut state = app.borrow_mut();
            let Some(playback) = state
                .playback
                .as_ref()
                .filter(|player| player.number == number)
            else {
                return;
            };
            if kind == 3 {
                state.playback = None;
                return;
            }
            (
                playback
                    .videos
                    .iter()
                    .find(|video| video.id == id.as_str())
                    .cloned(),
                playback.shorts,
            )
        };
        if kind == 4 {
            if !load_more(&ui, &app) && !ui.get_loading_more() {
                player::append(number, "[]", false, false);
            }
            return;
        }
        let Some(video) = video else { return };
        match kind {
            0 => {
                edit_library(&ui, &app, |library| library.watched(video));
                load_playback_extras(&ui, number, id.to_string(), app.borrow().library.extras);
                if !shorts {
                    load_video_description(&ui, number, id.to_string());
                }
            }
            1 => {
                let saved = save_video(&ui, &app, video);
                player::update_saved(number, &id, saved);
            }
            2 => ui.invoke_open_channel(video.channel.id.into(), video.channel.name.into()),
            _ => {}
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_load_more(move || {
        if let Some(ui) = target.upgrade() {
            load_more(&ui, &app);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_page_appended(move |failed| {
        let Some(ui) = target.upgrade() else { return };
        let mut state = app.borrow_mut();
        let generation = state.request.load(Ordering::Relaxed);
        let state = &mut *state;
        let Some(playback) = state
            .playback
            .as_mut()
            .filter(|player| player.feed_generation == Some(generation))
        else {
            return;
        };
        let ids: HashSet<_> = playback
            .videos
            .iter()
            .map(|video| video.id.as_str())
            .collect();
        let extra: Vec<_> = feed_videos(&ui)
            .into_iter()
            .filter(|video| !ids.contains(video.id.as_str()))
            .collect();
        let tracks = player::tracks(&extra, |id| state.library.is_saved(id));
        playback.videos.extend(extra);
        player::append(playback.number, &tracks, ui.get_has_more(), failed);
    });
}

fn bind_settings(window: &StreamTube, state: &State) {
    let (target, app) = (window.as_weak(), state.clone());
    window.on_choose_language(move |choice| {
        let Some(ui) = target.upgrade() else { return };
        let Some(language) = Language::OPTIONS.get(choice as usize).copied() else {
            return;
        };
        edit_library(&ui, &app, |library| library.language = language);
        let state = app.borrow();
        select_language(&ui, state.library.language, state.device_language);
        ui.set_screen(Screen::Settings);
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_toggle_thumbnails(move || {
        let Some(ui) = target.upgrade() else { return };
        edit_library(&ui, &app, |library| {
            library.thumbnails = !library.thumbnails
        });
        let enabled = app.borrow().library.thumbnails;
        ui.set_thumbnails(enabled);
    });

    let target = window.as_weak();
    window.on_open_source(move || {
        if let Some(ui) = target.upgrade() {
            if rustferry::system::open_url(
                "https://github.com/ShiroKSH/Stream-Tube-Desktop/tree/android/mobile/android",
            )
            .is_err()
            {
                ui.set_notice("Could not open the source".into());
            }
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_toggle_extra(move |choice| {
        let Some(ui) = target.upgrade() else { return };
        edit_library(&ui, &app, |library| match choice {
            0 => library.extras.sponsorblock = !library.extras.sponsorblock,
            1 => library.extras.dearrow_titles = !library.extras.dearrow_titles,
            2 => library.extras.dearrow_thumbnails = !library.extras.dearrow_thumbnails,
            3 => library.extras.dislike = !library.extras.dislike,
            _ => {}
        });
        apply_extras(&ui, app.borrow().library.extras);
    });
}

fn apply_extras(ui: &StreamTube, settings: Extras) {
    ui.set_sponsorblock(settings.sponsorblock);
    ui.set_dearrow_titles(settings.dearrow_titles);
    ui.set_dearrow_thumbnails(settings.dearrow_thumbnails);
    ui.set_dislike_count(settings.dislike);
}

fn bind_account(window: &StreamTube, state: &State) {
    let target = window.as_weak();
    account::listen(move |request, status, name, channels| {
        let _ = target.upgrade_in_event_loop(move |ui| {
            ui.invoke_account_result(request, status, name.into(), channels.into());
        });
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_authorize_account(move || {
        if let Some(ui) = target.upgrade() {
            authorize(&ui, &app, true, true);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_refresh_account(move || {
        if let Some(ui) = target.upgrade() {
            authorize(&ui, &app, true, false);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_disconnect_account(move || {
        let Some(ui) = target.upgrade() else { return };
        {
            let mut state = app.borrow_mut();
            state.account_request = state.account_request.wrapping_add(1);
            state.account_channels.clear();
        }
        account::cancel();
        edit_library(&ui, &app, |library| library.connected_account = false);
        ui.set_account_busy(false);
        ui.set_account_connected(false);
        ui.set_account_name("".into());
        ui.set_account_message("".into());
        if ui.get_screen() == Screen::Following {
            reload(&ui, &app);
        }
    });

    let (target, app) = (window.as_weak(), state.clone());
    window.on_account_result(move |request, status, name, channels| {
        let Some(ui) = target.upgrade() else { return };
        if request != app.borrow().account_request {
            return;
        }
        ui.set_account_busy(false);
        if status == 0 {
            let Ok(channels) = serde_json::from_str::<Vec<Channel>>(&channels) else {
                ui.set_account_message("Could not read subscriptions".into());
                return;
            };
            let mut seen = HashSet::new();
            app.borrow_mut().account_channels = channels
                .into_iter()
                .filter(|channel| youtube::is_channel_id(&channel.id))
                .filter(|channel| seen.insert(channel.id.clone()))
                .take(500)
                .collect();
            edit_library(&ui, &app, |library| library.connected_account = true);
            ui.set_account_name(name);
            ui.set_account_connected(true);
            ui.set_account_message("".into());
            if matches!(ui.get_screen(), Screen::Home | Screen::Following) {
                reload(&ui, &app);
            }
            return;
        }
        if status == 5 && ui.get_account_connected() {
            app.borrow_mut().account_channels.clear();
            account::cancel();
            edit_library(&ui, &app, |library| library.connected_account = false);
            ui.set_account_connected(false);
            ui.set_account_name("".into());
            if ui.get_screen() == Screen::Following {
                reload(&ui, &app);
            }
        }
        let message = match status {
            1 => "Sign-in cancelled",
            3 => "This build needs an OAuth registration in Google Cloud",
            4 => "Could not reach Google",
            5 => "Reconnect your YouTube account",
            6 => "This account does not have a YouTube channel yet",
            7 => "YouTube limited the requests. Try again later",
            _ => "Google sign-in is unavailable",
        };
        ui.set_account_message(message.into());
    });
}

fn authorize(ui: &StreamTube, state: &State, interactive: bool, choose_account: bool) {
    if ui.get_account_busy() {
        return;
    }
    let request = {
        let mut state = state.borrow_mut();
        state.account_request = state.account_request.wrapping_add(1);
        state.account_request
    };
    ui.set_account_busy(true);
    ui.set_account_message("".into());
    if !account::start(request, interactive, choose_account) {
        ui.set_account_busy(false);
        ui.set_account_message("Google sign-in is unavailable".into());
    }
}

fn following_channels(state: &AppState) -> Vec<Channel> {
    let mut ids = std::collections::HashSet::new();
    state
        .library
        .following
        .iter()
        .chain(&state.account_channels)
        .filter(|channel| ids.insert(channel.id.as_str()))
        .cloned()
        .collect()
}

fn reload(ui: &StreamTube, state: &State) {
    let screen = ui.get_screen();
    let (feed, empty) = {
        let state = state.borrow();
        state.request.fetch_add(1, Ordering::Relaxed);
        *state.next_page.lock().expect("feed cursor is available") = None;
        let feed = match screen {
            Screen::Home if ui.get_category() == 0 => {
                let interests = Interests::new(
                    &state.library.history,
                    &state.library.recent_searches,
                    following_channels(&state),
                );
                ui.set_personal_home(interests.available());
                Some(if interests.available() {
                    Feed::ForYou(interests)
                } else {
                    Feed::Home(Category::All)
                })
            }
            Screen::Home => Some(Feed::Home(match ui.get_category() {
                1 => Category::Music,
                2 => Category::Games,
                3 => Category::Tech,
                4 => Category::Live,
                _ => Category::All,
            })),
            Screen::Shorts => Some(Feed::Shorts),
            Screen::Following if !ui.get_channels_mode() => {
                let channels = following_channels(&state);
                (!channels.is_empty()).then_some(Feed::Following(channels))
            }
            Screen::Library => Some(Feed::Local(if ui.get_history_mode() {
                state.library.history.clone()
            } else {
                state.library.saved.clone()
            })),
            Screen::Search if !state.query.is_empty() => Some(Feed::Search(state.query.clone())),
            Screen::Channel => state.channel.clone().map(Feed::Channel),
            Screen::Playlist => state.playlist.clone().map(Feed::Playlist),
            _ => None,
        };
        if screen == Screen::Following {
            let channels = following_channels(&state)
                .into_iter()
                .map(|channel| ChannelRow {
                    id: channel.id.clone().into(),
                    name: channel.name.clone().into(),
                })
                .collect::<Vec<_>>();
            ui.set_channels(Rc::new(VecModel::from(channels)).into());
        }
        let empty = match screen {
            Screen::Following => "Open a channel under a video to follow it",
            Screen::Library if ui.get_history_mode() => "Videos you open will appear here",
            Screen::Library => "Save a video from any feed",
            Screen::Search if state.query.is_empty() => "Search by video or channel name",
            _ => "No videos found",
        };
        if let Some(Feed::Channel(channel)) = &feed {
            ui.set_channel_name(channel.name.clone().into());
            ui.set_channel_followed(state.library.is_following(&channel.id));
            ui.set_channel_remote(
                state
                    .account_channels
                    .iter()
                    .any(|remote| remote.id == channel.id),
            );
        }
        (feed, empty)
    };

    replace_rows(ui, Vec::new());
    ui.set_failed(false);
    ui.set_busy(false);
    ui.set_has_more(false);
    ui.set_loading_more(false);
    ui.set_load_more_failed(false);
    ui.set_message(if screen == Screen::Settings {
        "".into()
    } else {
        empty.into()
    });
    let Some(feed) = feed else { return };
    ui.set_busy(true);
    ui.set_message("Loading…".into());

    if !spawn_feed(ui, state, feed, empty) {
        feed_error(ui);
    }
}

fn load_more(ui: &StreamTube, state: &State) -> bool {
    if ui.get_loading_more() {
        return false;
    }
    let cursor = state
        .borrow()
        .next_page
        .lock()
        .expect("feed cursor is available")
        .clone();
    let Some(cursor) = cursor else { return false };
    ui.set_loading_more(true);
    ui.set_load_more_failed(false);
    if !spawn_feed(ui, state, Feed::Next(cursor), "") {
        ui.set_loading_more(false);
        ui.set_load_more_failed(true);
        ui.invoke_page_appended(true);
    }
    true
}

fn spawn_feed(ui: &StreamTube, state: &State, feed: Feed, empty: &'static str) -> bool {
    let state = state.borrow();
    let transaction = FeedTransaction {
        target: ui.as_weak(),
        request: state.request.clone(),
        generation: state.request.load(Ordering::Relaxed),
        next_page: state.next_page.clone(),
        thumbnails: state.library.thumbnails && ui.get_screen() != Screen::Shorts,
        extras: state.library.extras,
        saved: state
            .library
            .saved
            .iter()
            .map(|video| video.id.clone())
            .collect(),
        language: current_language(ui),
        empty,
    };
    let result = std::thread::Builder::new()
        .name("stream-tube-feed".into())
        .spawn(move || {
            fetch_feed(feed, transaction);
        });
    result.is_ok()
}

struct FeedTransaction {
    target: slint::Weak<StreamTube>,
    request: Arc<AtomicU64>,
    generation: u64,
    next_page: Arc<Mutex<Option<FeedCursor>>>,
    thumbnails: bool,
    extras: Extras,
    saved: HashSet<String>,
    language: Language,
    empty: &'static str,
}

fn fetch_feed(feed: Feed, transaction: FeedTransaction) {
    let FeedTransaction {
        target,
        request,
        generation,
        next_page,
        thumbnails,
        extras,
        saved,
        language,
        empty,
    } = transaction;
    let client = Client::new(language);
    let append = matches!(feed, Feed::Next(_));
    let shorts = matches!(feed, Feed::Shorts);
    let page = match feed {
        Feed::Home(category) => client.home(category).map(FeedPage::from),
        Feed::ForYou(interests) => interests.load(language).map(FeedPage::from),
        Feed::Shorts => client.shorts().map(FeedPage::from),
        Feed::Following(channels) => follow_feed(language, &channels, &request, generation),
        Feed::Channel(channel) => client.channel(&channel).map(FeedPage::from),
        Feed::Search(query) => client.search(&query, false).map(FeedPage::from),
        Feed::Playlist(id) => client.playlist(&id).map(FeedPage::from),
        Feed::Local(videos) => Some(FeedPage { videos, next: None }),
        Feed::Next(FeedCursor::YouTube(cursor)) => client.next_page(cursor).map(FeedPage::from),
        Feed::Next(FeedCursor::Following(channels)) => {
            follow_feed(language, &channels, &request, generation)
        }
    };
    let ids = page
        .iter()
        .flat_map(|page| &page.videos)
        .map(|video| video.id.clone())
        .collect::<Vec<_>>();
    let check = request.clone();
    if target
        .upgrade_in_event_loop(move |ui| {
            if check.load(Ordering::Relaxed) != generation {
                return;
            }
            let Some(page) = page else {
                if append {
                    ui.set_loading_more(false);
                    ui.set_load_more_failed(true);
                    ui.invoke_page_appended(true);
                } else {
                    feed_error(&ui);
                }
                return;
            };
            let more = page.next.is_some();
            *next_page.lock().expect("feed cursor is available") = page.next;
            ui.set_has_more(more);
            if append {
                let appended = append_rows(&ui, page.videos, &saved);
                let missed = appended == 0 && more;
                ui.set_load_more_failed(missed);
                ui.set_loading_more(false);
                ui.invoke_page_appended(missed);
            } else {
                let available = !page.videos.is_empty();
                ui.set_message(if available { "".into() } else { empty.into() });
                replace_rows(
                    &ui,
                    page.videos
                        .iter()
                        .map(|video| video_row(video, saved.contains(&video.id)))
                        .collect(),
                );
                ui.set_busy(false);
                if available && shorts {
                    ui.invoke_open_shorts();
                }
            }
        })
        .is_err()
        || shorts
        || (!thumbnails && !extras.dearrow_titles)
    {
        return;
    }

    let http = youtube::thumbnail_agent();
    for id in ids {
        if request.load(Ordering::Relaxed) != generation {
            break;
        }
        let branding = if extras.dearrow_titles || (thumbnails && extras.dearrow_thumbnails) {
            services::branding(&id).unwrap_or_default()
        } else {
            services::Branding::default()
        };
        let alternate = branding
            .thumbnail_time
            .filter(|_| extras.dearrow_thumbnails)
            .and_then(|time| services::branded_thumbnail(&id, time));
        let rgba = thumbnails
            .then(|| youtube::thumbnail(&http, &id, alternate))
            .flatten();
        let title = branding.title.filter(|_| extras.dearrow_titles);
        if rgba.is_none() && title.is_none() {
            continue;
        }
        let check = request.clone();
        let _ = target.upgrade_in_event_loop(move |ui| {
            if check.load(Ordering::Relaxed) != generation {
                return;
            }
            let model = ui.get_videos();
            if let Some((index, mut row)) = model.iter().enumerate().find(|(_, row)| row.id == id) {
                if let Some(rgba) = rgba {
                    let pixels = SharedPixelBuffer::clone_from_slice(
                        rgba.as_raw(),
                        rgba.width(),
                        rgba.height(),
                    );
                    row.thumbnail = slint::Image::from_rgba8(pixels);
                    row.has_thumbnail = true;
                }
                if let Some(title) = title {
                    row.title = title.into();
                }
                model.set_row_data(index, row);
            }
        });
    }
}

fn follow_feed(
    language: Language,
    channels: &[Channel],
    request: &AtomicU64,
    generation: u64,
) -> Option<FeedPage> {
    let (channels, remaining) = channels.split_at(channels.len().min(12));
    let mut result = Vec::new();
    let mut received = false;
    for batch in channels.chunks(6) {
        if request.load(Ordering::Relaxed) != generation {
            return None;
        }
        std::thread::scope(|scope| {
            let feeds = batch
                .iter()
                .map(|channel| scope.spawn(move || Client::new(language).channel(channel)))
                .collect::<Vec<_>>();
            let mut recent = Vec::new();
            for feed in feeds {
                if let Ok(Some(page)) = feed.join() {
                    received = true;
                    recent.push(page.videos.into_iter());
                }
            }
            for _ in 0..4 {
                result.extend(recent.iter_mut().filter_map(Iterator::next));
            }
        });
    }
    received.then(|| FeedPage {
        videos: result,
        next: (!remaining.is_empty()).then(|| FeedCursor::Following(remaining.to_vec())),
    })
}

fn replace_rows(ui: &StreamTube, rows: Vec<VideoRow>) {
    let model = ui.get_videos();
    if let Some(model) = model.as_any().downcast_ref::<VecModel<VideoRow>>() {
        model.set_vec(rows);
    }
}

fn append_rows(ui: &StreamTube, videos: Vec<Video>, saved: &HashSet<String>) -> usize {
    let model = ui.get_videos();
    let Some(rows) = model.as_any().downcast_ref::<VecModel<VideoRow>>() else {
        return 0;
    };
    let mut ids: HashSet<_> = model.iter().map(|row| row.id).collect();
    let new: Vec<_> = videos
        .into_iter()
        .filter_map(|video| {
            let row = video_row(&video, saved.contains(&video.id));
            ids.insert(row.id.clone()).then_some(row)
        })
        .collect();
    let count = new.len();
    rows.extend(new);
    count
}

fn video_row(video: &Video, saved: bool) -> VideoRow {
    VideoRow {
        id: video.id.clone().into(),
        title: video.title.clone().into(),
        channel: video.channel.name.clone().into(),
        channel_id: video.channel.id.clone().into(),
        duration: video.duration.clone().into(),
        views: video.views.clone().into(),
        published: video.published.clone().into(),
        saved,
        short: video.short,
        ..Default::default()
    }
}

fn find_video(ui: &StreamTube, id: &str) -> Option<Video> {
    ui.get_videos()
        .iter()
        .find(|row| row.id == id)
        .map(video_from_row)
}

fn feed_videos(ui: &StreamTube) -> Vec<Video> {
    ui.get_videos().iter().map(video_from_row).collect()
}

fn video_from_row(row: VideoRow) -> Video {
    Video {
        id: row.id.to_string(),
        title: row.title.to_string(),
        channel: Channel {
            id: row.channel_id.to_string(),
            name: row.channel.to_string(),
        },
        duration: row.duration.to_string(),
        views: row.views.to_string(),
        published: row.published.to_string(),
        short: row.short,
    }
}

fn open_player(
    ui: &StreamTube,
    state: &State,
    videos: Vec<Video>,
    first: usize,
    shorts: bool,
    feed: bool,
) {
    let has_more = feed && ui.get_has_more();
    let (number, tracks) = {
        let mut state = state.borrow_mut();
        state.playback_number = state.playback_number.wrapping_add(1);
        let number = state.playback_number;
        let tracks = player::tracks(&videos, |id| state.library.is_saved(id));
        state.playback = Some(Playback {
            number,
            videos,
            shorts,
            feed_generation: feed.then(|| state.request.load(Ordering::Relaxed)),
        });
        (number, tracks)
    };
    if player::open(
        number,
        &tracks,
        first,
        current_language(ui),
        shorts,
        has_more,
    )
    .is_err()
    {
        state.borrow_mut().playback = None;
        ui.set_notice("Could not open the player".into());
    }
}

fn load_video_description(ui: &StreamTube, number: i32, id: String) {
    let language = current_language(ui);
    let target = ui.as_weak();
    let _ = std::thread::Builder::new()
        .name("stream-tube-video-details".into())
        .spawn(move || {
            if let Some(description) = Client::new(language).description(&id) {
                let _ = target.upgrade_in_event_loop(move |_| {
                    player::describe(number, &id, &description);
                });
            }
        });
}

fn load_playback_extras(ui: &StreamTube, number: i32, id: String, settings: Extras) {
    if !(settings.sponsorblock || settings.dearrow_titles || settings.dislike) {
        return;
    }
    let target = ui.as_weak();
    let _ = std::thread::Builder::new()
        .name("stream-tube-playback-extras".into())
        .spawn(move || {
            let extras = services::playback(&id, settings);
            if let Ok(packet) = serde_json::to_string(&extras) {
                let _ = target.upgrade_in_event_loop(move |_| {
                    player::set_extras(number, &id, &packet);
                });
            }
        });
}

fn save_video(ui: &StreamTube, state: &State, video: Video) -> bool {
    let id = video.id.clone();
    edit_library(ui, state, |library| library.toggle_saved(video));
    let saved = state.borrow().library.is_saved(&id);
    let model = ui.get_videos();
    if let Some((index, mut row)) = model.iter().enumerate().find(|(_, row)| row.id == id) {
        if !saved && ui.get_screen() == Screen::Library && !ui.get_history_mode() {
            if let Some(rows) = model.as_any().downcast_ref::<VecModel<VideoRow>>() {
                rows.remove(index);
            }
            if model.row_count() == 0 {
                ui.set_message("Save a video from any feed".into());
            }
        } else {
            row.saved = saved;
            model.set_row_data(index, row);
        }
    }
    saved
}

fn edit_library(ui: &StreamTube, state: &State, edit: impl FnOnce(&mut Library)) {
    let mut state = state.borrow_mut();
    let mut updated = state.library.clone();
    edit(&mut updated);
    if !state.storage_writable || updated.write().is_err() {
        ui.set_notice("Could not save changes".into());
        return;
    }
    state.library = updated;
}

fn feed_error(ui: &StreamTube) {
    ui.set_busy(false);
    ui.set_failed(true);
    ui.set_message("Could not load videos".into());
}

fn current_language(ui: &StreamTube) -> Language {
    Language::from_locale(&ui.global::<UiText>().get_language())
}

fn select_language(ui: &StreamTube, language: Language, device: Language) {
    let choice = Language::OPTIONS
        .iter()
        .position(|option| *option == language)
        .unwrap_or_default();
    ui.set_language_choice(choice as i32);
    ui.set_language_label(language.name().into());
    ui.global::<UiText>()
        .set_language(language.effective(device).code().into());
}
