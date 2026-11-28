#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
mod api;
mod assets;
mod display;
mod downloads;
mod feed;
mod i18n;
mod idle_inhibit;
mod import;
mod lan_cast;
mod links;
mod locale;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod media_controls;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod media_session;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "macos")]
mod now_playing;
mod perf;
mod player;
mod recommend;
mod store;
#[cfg(windows)]
mod tray;
mod ui;
mod updater;

use gpui_kit::*;

struct QuitRequested;
impl Global for QuitRequested {}

#[cfg(test)]
struct PlatformQuitRequested;
#[cfg(test)]
impl Global for PlatformQuitRequested {}

fn quit_platform(cx: &mut App) {
    // The headless GPUI platform does not act on quit requests.
    #[cfg(test)]
    cx.set_global(PlatformQuitRequested);
    idle_inhibit::release(cx);
    cx.quit();
}

/// Quits the app: saves where every tab's video stopped and waits briefly for the
/// watch-history reports. The last main window closing and the macOS menu's Quit
/// both end here.
pub(crate) fn request_quit(cx: &mut App) {
    if cx.try_global::<QuitRequested>().is_some() {
        return;
    }
    cx.set_global(QuitRequested);
    for player in player::controller::tab_players(cx) {
        player.update(cx, |p, _| p.save_progress(true));
    }
    let Some(flushed) = prepare_watch_history_flush(cx)
        .then(api::web::flush_reports_on_exit)
        .flatten()
    else {
        quit_platform(cx);
        return;
    };
    // Wait before platform shutdown, which gives quit observers only a short deadline.
    cx.spawn(async move |cx| {
        let deadline = cx.background_executor().timer(std::time::Duration::from_secs(3));
        let _ = futures::future::select(flushed, deadline).await;
        let _ = cx.update(quit_platform);
    })
    .detach();
}

fn register_window_close_handler(cx: &mut App) {
    // GPUI would otherwise quit on last-window close before the reporter can flush.
    cx.set_quit_mode(QuitMode::Explicit);
    cx.on_window_closed(|cx, id| {
        ui::pip::window_closed(id, cx);
        // the picture-in-picture window alone doesn't keep the app running
        let pip = ui::pip::handle(cx);
        if cx.windows().iter().all(|w| Some(*w) == pip) {
            request_quit(cx);
        }
    })
    .detach();
}

/// Window placement from the last session (size, position, maximized), or a
/// centered default sized to the screen.
fn saved_window_bounds(cx: &App) -> WindowBounds {
    if let Some(v) =
        store::kv_get("window").and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    {
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
        let (w, h) = (f("w"), f("h"));
        if w >= 640.0 && h >= 400.0 {
            let b = Bounds::new(point(px(f("x")), px(f("y"))), size(px(w), px(h)));
            // ignore positions that are no longer on any display
            let visible = cx.displays().iter().any(|d| d.bounds().intersects(&b));
            if visible {
                return if v
                    .get("maximized")
                    .and_then(|m| m.as_bool())
                    .unwrap_or(false)
                {
                    WindowBounds::Maximized(b)
                } else {
                    WindowBounds::Windowed(b)
                };
            }
        }
    }
    let display = cx
        .primary_display()
        .map(|d| d.bounds().size)
        .unwrap_or(size(px(1600.), px(1000.)));
    let sz = size(
        (display.width * 0.82).min(px(1600.)),
        (display.height * 0.85).min(px(1000.)),
    );
    WindowBounds::Windowed(Bounds::centered(None, sz, cx))
}

/// How much bigger the window reports itself than the size it was opened with (on macOS
/// the title bar): subtracted when saving, or the window grew by that much every launch.
static FRAME_EXTRA: std::sync::Mutex<(f32, f32)> = std::sync::Mutex::new((0.0, 0.0));

fn measure_frame_extra(requested: &WindowBounds, window: &Window) {
    if let (WindowBounds::Windowed(want), WindowBounds::Windowed(got)) = (requested, window.window_bounds()) {
        let dw = f32::from(got.size.width - want.size.width);
        let dh = f32::from(got.size.height - want.size.height);
        if (0.0..=100.0).contains(&dw) && (0.0..=100.0).contains(&dh) {
            *FRAME_EXTRA.lock().unwrap() = (dw, dh);
        }
    }
}

pub fn save_window_bounds(window: &Window) {
    let (b, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(b) => (b, false),
        WindowBounds::Maximized(b) => (b, true),
        WindowBounds::Fullscreen(b) => (b, false),
    };
    let extra = *FRAME_EXTRA.lock().unwrap();
    let v = serde_json::json!({
        "x": f32::from(b.origin.x), "y": f32::from(b.origin.y),
        "w": f32::from(b.size.width) - extra.0, "h": f32::from(b.size.height) - extra.1,
        "maximized": maximized,
    });
    store::kv_set("window", &v.to_string());
}

struct WatchHistoryFlushed;
impl Global for WatchHistoryFlushed {}

pub(crate) fn flush_watch_history(cx: &mut App) {
    if !prepare_watch_history_flush(cx) {
        return;
    }
    // Direct platform quits and installer restarts cannot await a UI task here.
    if let Some(pending) = api::web::pending_reports() {
        if pending.recv_timeout(std::time::Duration::from_secs(2)).is_err() {
            eprintln!("streamtube: YouTube watch reports did not finish before exit");
        }
    }
}

fn prepare_watch_history_flush(cx: &mut App) -> bool {
    if cx.try_global::<WatchHistoryFlushed>().is_some() {
        return false;
    }
    cx.set_global(WatchHistoryFlushed);
    player::controller::player(cx).update(cx, |p, _| p.finish_playback_history());
    true
}

fn main() {
    perf::mark("main");
    // the local stream proxy must never be reached through the user's proxy
    // SAFETY: set before any other thread starts
    unsafe {
        std::env::set_var("no_proxy", "127.0.0.1,localhost,::1");
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost,::1");
    }
    #[cfg(target_os = "macos")]
    if let Some(path) = macos::path_with_homebrew() {
        // SAFETY: this still runs before any other thread starts.
        unsafe { std::env::set_var("PATH", path) };
    }
    perf::init();
    // one running app: a second launch (e.g. a clicked streamtube: link) hands over and
    // exits before it opens the database the running one is using
    let arg = std::env::args().nth(1).unwrap_or_default();
    if !display::relaunched() && links::forward_to_running(&arg) {
        return;
    }
    store::init();
    api::web::sweep_cookie_files();
    perf::mark("store");
    display::choose();
    perf::mark("display");
    let (incoming_tx, mut incoming_rx) = futures::channel::mpsc::unbounded();
    let _ = links::listen(incoming_tx.clone());
    perf::mark("links");
    let application = gpui_kit::application()
        .with_assets(assets::Assets)
        .with_http_client(ui::UreqHttp::new());
    #[cfg(target_os = "macos")]
    {
        let urls_tx = incoming_tx.clone();
        application.on_open_urls(move |urls| {
            for url in urls {
                let _ = urls_tx.unbounded_send(url);
            }
        });
        let reopen_tx = incoming_tx.clone();
        application.on_reopen(move |_| {
            let _ = reopen_tx.unbounded_send(String::new());
        });
    }
    drop(incoming_tx);
    application.run(move |cx: &mut App| {
        perf::mark("gpui running");
        gpui_kit::init(cx);
        i18n::apply_component_locale();
        perf::mark("gpui_kit init");
        player::preview::init(cx);
        perf::mark("preview init");
        cx.set_global(ui::Pal::new(true, store::settings_snapshot().accent));
        ui::apply_theme(None, cx);
        player::controller::init(cx);
        perf::mark("player init");
        ui::app::bind_keys(cx);
        let requested_bounds = saved_window_bounds(cx);
        let opts = WindowOptions {
            // Without this the Wayland/X11 window has no identity, so docks and
            // taskbars can't match it to streamtube.desktop (StartupWMClass).
            app_id: Some("streamtube".into()),
            window_bounds: Some(requested_bounds),
            titlebar: Some(TitlebarOptions {
                title: Some("Stream Tube".into()),
                appears_transparent: true,
                // macOS: the system's buttons sit in the app's 56 px top bar, left of
                // the menu button (the buttons are 12 px tall)
                traffic_light_position: cfg!(target_os = "macos").then(|| point(px(16.), px(22.))),
            }),
            window_min_size: Some(size(px(720.), px(480.))),
            // GPUI draws an inactive window at most ~30 times a second by default, so a
            // 60 fps video watched next to another app ran at half rate; frames are only
            // drawn when something changed, so an idle background window still costs nothing
            inactive_frame_interval: None,
            ..Default::default()
        };
        let (main_window, _) = gpui_kit::open_window(opts, cx, |window, cx| {
            measure_frame_extra(&requested_bounds, window);
            ui::apply_theme(Some(window), cx);
            #[cfg(windows)]
            media_controls::init(window, cx);
            #[cfg(target_os = "macos")]
            now_playing::init(cx);
            #[cfg(target_os = "linux")]
            mpris::init(cx);
            let app = cx.new(|cx| ui::app::AppView::new(window, cx));
            cx.new(|_| ui::app::Shell::new(app))
        })
        .expect("failed to open window");
        #[cfg(target_os = "macos")]
        macos::init_menu(main_window, cx);
        perf::mark("window open");
        ui::app_icon::apply(cx);
        links::register_scheme();
        perf::mark("icon");
        cx.spawn(async move |cx| {
            use futures::StreamExt;
            while let Some(arg) = incoming_rx.next().await {
                let _ = cx.update(|cx| {
                    let _ = main_window.update(cx, |_, window, cx| {
                        ui::app::restore_window(window);
                        window.activate_window();
                        if let Some(route) = ui::app::route_from_link(&arg) {
                            ui::app::navigate(route, window, cx);
                        }
                    });
                });
            }
        })
        .detach();
        register_window_close_handler(cx);
        cx.on_app_quit(|cx| {
            // Cmd+Q and the system quitting the app don't close the window first
            for player in player::controller::tab_players(cx) {
                player.update(cx, |p, _| p.save_progress(true));
            }
            flush_watch_history(cx);
            idle_inhibit::release(cx);
            std::future::ready(())
        })
        .detach();
    });
}
