//! Reports playback to the account's watch history the way YouTube's own
//! players do: a playback ping at start, then watchtime pings as it goes. The
//! account is the browser session when connected, otherwise the TV sign-in.
use super::{ORIGIN, http};
use crate::api::history::Account;
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

/// The web player's `signatureTimestamp`, for player requests through a browser session.
pub(in crate::api) fn sts() -> Option<u64> {
    static STS: Mutex<Option<(u64, Instant)>> = Mutex::new(None);
    if let Some((v, at)) = *STS.lock() {
        if at.elapsed() < Duration::from_secs(6 * 3600) {
            return Some(v);
        }
    }
    let api =
        String::from_utf8(http::get_bytes("https://www.youtube.com/iframe_api", &[]).ok()?).ok()?;
    let id: String = api
        .split("/s/player/")
        .nth(1)
        .or_else(|| api.split(r"player\/").nth(1))?
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    let js = String::from_utf8(
        http::get_bytes(
            &format!("https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js"),
            &[],
        )
        .ok()?,
    )
    .ok()?;
    let v: u64 = ["signatureTimestamp:", "sts:"].iter().find_map(|k| {
        js.split(k).nth(1).and_then(|t| {
            t.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
    })?;
    *STS.lock() = Some((v, Instant::now()));
    Some(v)
}

fn stats_url(base: &str, fields: Vec<(&str, String)>, final_web_report: bool) -> String {
    let (path, query) = base.split_once('?').unwrap_or((base, ""));
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
            // Safari's final watch-time report has no scheduled follow-up.
            if final_web_report && matches!(key, "rtn" | "rti") {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect();
    for (key, value) in fields {
        if final_web_report && matches!(key, "rtn" | "rti") {
            continue;
        }
        match pairs.iter_mut().find(|(current, _)| current == key) {
            Some(pair) => pair.1 = value,
            None => pairs.push((key.to_string(), value)),
        }
    }
    format!("{path}?{}", pairs.iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>().join("&"))
}

fn watch_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("STREAMTUBE_WATCH_HISTORY_DIAGNOSTICS").is_some_and(|v| v == "1"))
}

// Log only the allowed fields in the request as sent, with their values validated.
fn watch_report_summary(url: &str, status: Option<u16>) -> String {
    use std::fmt::Write;

    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let phase = if path.ends_with("/watchtime") { "watchtime" } else { "playback" };
    let mut summary = match status {
        Some(code) => format!("{phase} http={code}"),
        None => format!("{phase} http=unavailable"),
    };
    for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        let allowed = match key {
            "cmt" | "st" | "et" | "len" | "rt" | "rtn" | "rti" => value.parse::<f64>().is_ok_and(f64::is_finite),
            "state" => matches!(value, "playing" | "paused"),
            "final" => value == "1",
            _ => false,
        };
        if allowed {
            let _ = write!(summary, " {key}={value}");
        }
    }
    summary
}

struct Watch {
    account: Account,
    recording_revision: u64,
    id: String,
    playback: String,
    watchtime: String,
    cpn: String,
    len: f64,
    started: Instant,
    last_ping: Instant,
    last_cmt: f64,
    pings: u32,
    playing: bool,
}

impl Watch {
    fn begin(start: &Start) -> Result<Self> {
        let Start {
            account,
            id,
            len,
            position,
            ..
        } = start;
        anyhow::ensure!(
            enabled(start.recording_revision),
            "history reporting is inactive"
        );
        let r = account.player_response(id)?;
        let url = |k: &str| {
            r.pointer(&format!("/playbackTracking/{k}/baseUrl"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let playback =
            url("videostatsPlaybackUrl").ok_or_else(|| anyhow!("no playback tracking"))?;
        let watchtime =
            url("videostatsWatchtimeUrl").ok_or_else(|| anyhow!("no watchtime tracking"))?;
        let len = r
            .pointer("/videoDetails/lengthSeconds")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .filter(|l: &f64| *l > 0.0)
            .unwrap_or(*len);
        let cpn: String = {
            use rand::Rng;
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut rng = rand::rng();
            (0..16)
                .map(|_| A[rng.random_range(0..A.len())] as char)
                .collect()
        };
        let now = Instant::now();
        let w = Watch {
            account: account.clone(),
            recording_revision: start.recording_revision,
            id: id.clone(),
            playback,
            watchtime,
            cpn,
            len,
            started: now,
            last_ping: now,
            last_cmt: *position,
            pings: 0,
            playing: true,
        };
        w.get(
            &w.playback,
            &[
                ("cmt", format!("{position:.3}")),
                ("rtn", "0".into()),
                ("state", "playing".into()),
            ],
        )?;
        Ok(w)
    }

    /// Sends a stats ping: the base URL from the player response with the fields
    /// youtube.com's own player adds. Fields already in the base URL are replaced,
    /// never repeated.
    fn get(&self, base: &str, fields: &[(&str, String)]) -> Result<()> {
        // the ping carries the sign-in cookies: only ever to YouTube itself
        if !is_youtube_https(base) {
            return Err(anyhow!("unexpected stats host"));
        }
        let rt = self.started.elapsed().as_secs_f64();
        let (client, version, platform) = self.account.ping_client();
        let mut add: Vec<(&str, String)> = vec![
            ("ver", "2".into()),
            ("cpn", self.cpn.clone()),
            ("c", client.into()),
            ("cver", version.into()),
            ("cplayer", "UNIPLAYER".into()),
            ("cplatform", platform.into()),
        ];
        if client == "WEB" {
            add.extend([
                ("cbr", "Chrome".into()),
                ("cbrver", "140.0.0.0".into()),
                ("cos", "Windows".into()),
                ("cosver", "10.0".into()),
            ]);
        } else {
            add.extend([("cos", "Tizen".into()), ("cosver", "2.4.0".into())]);
        }
        add.extend([
            ("fs", "0".into()),
            ("volume", "100".into()),
            ("muted", "0".into()),
            ("lact", "1200".into()),
            ("len", format!("{:.3}", self.len)),
            ("rt", format!("{rt:.3}")),
        ]);
        add.extend(fields.iter().cloned());
        let final_web_report = client == "WEB" && fields.iter().any(|(key, value)| *key == "final" && value == "1");
        let url = stats_url(base, add, final_web_report);
        let referer = format!("https://www.youtube.com/watch?v={}", self.id);
        let account_headers = self.account.ping_headers()?;
        let mut headers: Vec<(&str, &str)> = account_headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        headers.push(("Referer", referer.as_str()));
        headers.push(("Origin", ORIGIN));
        let send = || {
            anyhow::ensure!(
                self.account.if_current(|| ()).is_some() && enabled(self.recording_revision),
                "history reporting is inactive"
            );
            Ok(http::tracking_status(&url, &headers))
        };
        let response = match send()? {
            Ok(500..=599) | Err(_) => send()?,
            response => response,
        };
        if watch_diagnostics_enabled() {
            eprintln!("Watch history: {}", watch_report_summary(&url, response.as_ref().ok().copied()));
        }
        match response {
            Ok(200..=299) => Ok(()),
            Ok(status) => Err(anyhow!("HTTP {status}")),
            Err(_) => Err(anyhow!("network request failed")),
        }
    }

    /// Reports the stretch watched since the last report, ending at `cmt`.
    fn report(&mut self, cmt: f64, playing: bool, last: bool) -> Result<()> {
        let st = if cmt >= self.last_cmt && cmt - self.last_cmt < 120.0 {
            self.last_cmt
        } else {
            cmt
        };
        let rtn = self.started.elapsed().as_secs_f64();
        let mut fields = vec![
            ("cmt", format!("{cmt:.3}")),
            ("st", format!("{st:.3}")),
            ("et", format!("{cmt:.3}")),
            (
                "state",
                if playing { "playing" } else { "paused" }.to_string(),
            ),
            ("rtn", format!("{:.0}", rtn.max(1.0))),
            ("rti", format!("{:.0}", rtn.max(1.0))),
            ("idpj", "-1".into()),
            ("ldpj", "-1".into()),
        ];
        if last {
            fields.push(("final", "1".into()));
        }
        self.last_ping = Instant::now();
        self.playing = playing;
        self.get(&self.watchtime, &fields)?;
        self.last_cmt = cmt;
        self.pings += 1;
        Ok(())
    }

    /// youtube.com reports at 10, 20 and 30 seconds, then every 40 seconds.
    fn due(&self) -> bool {
        let every = if self.pings < 3 { 10 } else { 40 };
        self.last_ping.elapsed() >= Duration::from_secs(every)
    }
}

struct Start {
    account: Account,
    recording_revision: u64,
    id: String,
    serial: u64,
    len: f64,
    position: f64,
}

enum Event {
    Start(Start),
    Progress {
        serial: u64,
        position: f64,
        playing: bool,
        force: bool,
    },
    Seek {
        serial: u64,
        position: f64,
    },
    Stop {
        serial: u64,
        position: f64,
    },
    /// Forget the playback without a final report.
    Abandon {
        serial: u64,
    },
    Flush(mpsc::Sender<()>),
    FlushOnExit(oneshot::Sender<()>),
}

struct ActivePlayback {
    start: Start,
    watch: Option<Watch>,
    last_start_attempt: Instant,
}

static REPORTER: OnceLock<mpsc::Sender<Event>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_REPORTER: std::cell::RefCell<Option<Option<mpsc::Sender<Event>>>> = const { std::cell::RefCell::new(None) };
}

fn enabled(revision: u64) -> bool {
    crate::store::history_recording_revision() == Some(revision)
}

fn report(watch: &mut Watch, position: f64, playing: bool, final_report: bool) {
    if watch.account.if_current(|| ()).is_some() && enabled(watch.recording_revision) {
        if let Err(error) = watch.report(position, playing, final_report) {
            eprintln!("streamtube: YouTube watch progress failed: {error}");
        }
    }
}

fn worker(rx: mpsc::Receiver<Event>) {
    let mut active: Option<ActivePlayback> = None;
    while let Ok(event) = rx.recv() {
        match event {
            Event::Start(start) => {
                let watch = begin(&start);
                active = Some(ActivePlayback {
                    start,
                    watch,
                    last_start_attempt: Instant::now(),
                });
            }
            Event::Progress {
                serial,
                position,
                playing,
                force,
            } => {
                let Some(playback) = active.as_mut().filter(|p| p.start.serial == serial) else {
                    continue;
                };
                if playback.watch.is_none()
                    && playback.last_start_attempt.elapsed() >= Duration::from_secs(10)
                {
                    playback.watch = begin(&playback.start);
                    playback.last_start_attempt = Instant::now();
                }
                if let Some(watch) = &mut playback.watch {
                    if force || watch.due() || playing != watch.playing {
                        report(watch, position, playing, false);
                    }
                }
            }
            Event::Seek { serial, position } => {
                if let Some(playback) = active.as_mut().filter(|p| p.start.serial == serial) {
                    playback.start.position = position;
                    if let Some(watch) = &mut playback.watch {
                        watch.last_cmt = position;
                    }
                }
            }
            Event::Stop { serial, position } => {
                if active.as_ref().is_some_and(|p| p.start.serial == serial) {
                    if let Some(mut watch) = active.take().and_then(|p| p.watch) {
                        report(&mut watch, position, false, true);
                    }
                }
            }
            Event::Abandon { serial } => {
                if active.as_ref().is_some_and(|p| p.start.serial == serial) {
                    active = None;
                }
            }
            Event::Flush(done) => {
                let _ = done.send(());
            }
            Event::FlushOnExit(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn begin(start: &Start) -> Option<Watch> {
    if !enabled(start.recording_revision) || start.account.if_current(|| ()).is_none() {
        return None;
    }
    match Watch::begin(start) {
        Ok(watch) => Some(watch),
        // The error can contain details from an authenticated request.
        Err(_) => {
            eprintln!("streamtube: could not start YouTube watch reporting");
            None
        }
    }
}

fn reporter() -> &'static mpsc::Sender<Event> {
    REPORTER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("watch-report".into())
            .spawn(move || worker(rx));
        tx
    })
}

pub fn pending_reports() -> Option<mpsc::Receiver<()>> {
    let (done, receipt) = mpsc::channel();
    send_report(Event::Flush(done), false).then_some(receipt)
}

pub fn flush_reports_on_exit() -> Option<oneshot::Receiver<()>> {
    let (done, receipt) = oneshot::channel();
    send_report(Event::FlushOnExit(done), false).then_some(receipt)
}

fn send_report(event: Event, start_worker: bool) -> bool {
    let send = |sender: Option<&mpsc::Sender<Event>>| sender.is_some_and(|sender| sender.send(event).is_ok());
    #[cfg(test)]
    if let Some(sender) = TEST_REPORTER.with(|slot| slot.borrow().clone()) {
        return send(sender.as_ref());
    }
    send(if start_worker { Some(reporter()) } else { REPORTER.get() })
}

/// A single playback, tied to the account selected when it was opened.
pub struct PlaybackReport {
    account: Account,
    recording_revision: u64,
    video_id: String,
    serial: u64,
    started: bool,
}

impl PlaybackReport {
    pub fn new(video_id: String, serial: u64) -> Option<Self> {
        Some(Self {
            account: Account::current()?,
            recording_revision: crate::store::history_recording_revision()?,
            video_id,
            serial,
            started: false,
        })
    }

    pub fn progress(&self, position: f64, playing: bool, force: bool) {
        if self.started
            && enabled(self.recording_revision)
            && position.is_finite()
            && position >= 0.0
        {
            send_report(Event::Progress {
                serial: self.serial,
                position,
                playing,
                force,
            }, false);
        }
    }

    pub fn seek(&self, position: f64) {
        if self.started
            && enabled(self.recording_revision)
            && position.is_finite()
            && position >= 0.0
        {
            send_report(Event::Seek {
                serial: self.serial,
                position,
            }, false);
        }
    }

    pub fn start(&mut self, position: f64, len: f64) {
        if self.started
            || !enabled(self.recording_revision)
            || !position.is_finite()
            || position <= 0.0
        {
            return;
        }
        self.started = send_report(Event::Start(Start {
                account: self.account.clone(),
                recording_revision: self.recording_revision,
                id: self.video_id.clone(),
                serial: self.serial,
                len,
                position,
            }), true);
    }

    /// Drops the report without a final ping, when the account already has a
    /// newer position from another device than this one would report.
    pub fn abandon(self) {
        if self.started {
            send_report(Event::Abandon { serial: self.serial }, false);
        }
    }

    pub fn finish(self, position: f64) {
        if self.started
            && enabled(self.recording_revision)
            && position.is_finite()
            && position >= 0.0
        {
            send_report(Event::Stop {
                serial: self.serial,
                position,
            }, false);
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Reports a short watch of VID with the profile in STREAMTUBE_DATA_DIR, then
    /// checks the account's history page.
    #[test]
    #[ignore = "needs a signed-in profile in STREAMTUBE_DATA_DIR and network"]
    fn reports_to_the_account() {
        let id = std::env::var("VID").unwrap_or_else(|_| "aqz-KE-bpKQ".into());
        let account = Account::current().expect("a signed-in account");
        let start = Start { account: account.clone(), recording_revision: crate::store::history_recording_revision().unwrap_or(0), id: id.clone(), serial: 1, len: 600.0, position: 3.0 };
        let mut w = Watch::begin(&start).expect("playback ping");
        std::thread::sleep(Duration::from_secs(2));
        w.report(9.0, true, false).expect("watchtime ping");
        w.report(12.0, false, true).expect("final ping");
        let page = account.page(None).expect("history page");
        let first: Vec<_> = page.entries.iter().take(3).map(|e| (e.video.id.clone(), e.progress.map(|p| p.seconds))).collect();
        println!("history head: {first:?}");
        assert!(page.entries.iter().take(3).any(|e| e.video.id == id), "watched video not at the top of history");
    }
}

/// `https://youtube.com/...` or `https://<anything>.youtube.com/...`, nothing else.
fn is_youtube_https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // no user info or port tricks ("youtube.com@evil.example")
    !host.contains(['@', ':']) && (host == "youtube.com" || host.ends_with(".youtube.com"))
}

#[cfg(test)]
mod host_tests {
    use super::is_youtube_https;

    #[::core::prelude::v1::test]
    fn only_youtube() {
        assert!(is_youtube_https(
            "https://s.youtube.com/api/stats/playback?x=1"
        ));
        assert!(is_youtube_https(
            "https://www.youtube.com/api/stats/watchtime"
        ));
        assert!(!is_youtube_https("http://s.youtube.com/api"));
        assert!(!is_youtube_https("https://youtube.com.evil.example/api"));
        assert!(!is_youtube_https(
            "https://www.youtube.com@evil.example/api"
        ));
        assert!(!is_youtube_https("https://evilyoutube.com/api"));
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use crate::PlatformQuitRequested;
    use crate::api::types::VideoItem;
    use crate::player::controller;
    use crate::ui::{Pal, app::UiState, pip};
    use gpui::{Empty, TestAppContext, WindowHandle};
    use std::sync::mpsc::{Receiver, TryRecvError};

    #[::core::prelude::v1::test]
    fn watch_report_summary_uses_only_allowlisted_fields_and_status() {
        let request = "https://www.youtube.com/api/stats/watchtime?docid=video&cpn=session&ei=token&cmt=82.125&st=80&et=82.125&rt=83.200&len=464&state=paused&final=1";
        assert_eq!(
            watch_report_summary(request, Some(204)),
            "watchtime http=204 cmt=82.125 st=80 et=82.125 rt=83.200 len=464 state=paused final=1",
        );
        let invalid = "https://www.youtube.com/api/stats/playback?cmt=%53ecret&len=NaN&rti=inf&state=unknown&final=token&rtn=10";
        assert_eq!(watch_report_summary(invalid, None), "playback http=unavailable rtn=10");
    }

    #[::core::prelude::v1::test]
    fn final_web_report_omits_the_follow_up_schedule() {
        let base = "https://www.youtube.com/api/stats/watchtime?cmt=80&rtn=10&rti=10";
        let fields = vec![
            ("cmt", "85.375".into()),
            ("rtn", "3".into()),
            ("rti", "3".into()),
            ("final", "1".into()),
        ];
        let final_url = stats_url(base, fields.clone(), true);
        assert!(final_url.ends_with("?cmt=85.375&final=1"));
        let scheduled_url = stats_url(base, fields, false);
        assert!(scheduled_url.ends_with("?cmt=85.375&rtn=3&rti=3&final=1"));
    }

    #[gpui::test]
    fn closing_player_only_enqueues_the_final_position(cx: &mut TestAppContext) {
        let (_, reports) = open_main_window_with_watch_history(cx, "test-watch-history-close");
        cx.update(|cx| {
            controller::player(cx).update(cx, |player, cx| {
                player.set_paused(true, cx);
                player.stop(cx);
            });
        });
        assert!(matches!(reports.try_recv(), Ok(Event::Progress {
            serial: TEST_SERIAL, position: SHUTDOWN_TEST_POSITION, playing: false, ..
        })));
        assert_final_position(&reports);
        assert!(reports.try_recv().is_err(), "closing the player alone must not flush");
    }

    #[gpui::test]
    fn app_quit_enqueues_one_final_position_before_flushing(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-quit";
        let (_, reports) = open_main_window_with_watch_history(cx, video_id);
        cx.update(|cx| {
            crate::request_quit(cx);
            crate::request_quit(cx);
        });
        cx.run_until_parked();
        assert_final_report_before_flush(cx, &reports, video_id);
    }

    #[gpui::test]
    fn closing_last_main_window_enqueues_one_final_position_before_flushing(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-last-main-window";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        let other = cx.add_window(|_, _| Empty);
        // GPUI's native close callback also goes through Window::remove_window.
        other.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert!(matches!(reports.try_recv(), Err(TryRecvError::Empty)),
            "closing a non-final window must leave reporting active");

        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert!(cx.windows().is_empty());
        assert_final_report_before_flush(cx, &reports, video_id);
    }

    #[gpui::test]
    fn closing_last_main_window_with_pip_open_flushes_the_final_position(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-last-main-window-with-pip";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        cx.update(pip::open);
        let panel = cx.read(pip::handle).expect("PiP window opened");
        assert!(cx.windows().contains(&panel));
        assert!(cx.read(|cx| cx.try_global::<crate::QuitRequested>().is_none()));

        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert!(main.root(cx).is_err(), "the main window must be closed before the flush");
        assert_final_report_before_flush(cx, &reports, video_id);
    }

    #[gpui::test]
    fn closing_pip_alone_keeps_the_main_window_and_playback_active(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-close-pip-only";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        cx.update(pip::open);
        let panel = cx.read(pip::handle).expect("PiP window opened");

        panel.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        assert_eq!(cx.windows(), [main.into()]);
        assert!(cx.read(pip::handle).is_none());
        cx.read(|cx| {
            assert!(cx.try_global::<crate::QuitRequested>().is_none());
            let player = controller::player(cx);
            let player = player.read(cx);
            assert_eq!(
                player.video.as_ref().map(|video| video.id.as_str()),
                Some(video_id)
            );
            assert!(!player.paused);
            assert_eq!(player.time, SHUTDOWN_TEST_POSITION);
        });
        assert!(
            reports.try_iter().all(|event| !matches!(event, Event::Stop { .. } | Event::FlushOnExit(_))),
            "closing PiP alone must leave reporting active"
        );
    }

    #[gpui::test]
    fn shutdown_waits_for_watch_history_flush_acknowledgement(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-shutdown-ack";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        let flushed = take_final_report_and_flush(&reports, video_id);
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()));

        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must wait while the flush is pending");

        flushed.send(()).unwrap();
        cx.run_until_parked();
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "the flush acknowledgement must release platform exit before the deadline");
    }

    #[gpui::test]
    fn shutdown_requests_exit_when_watch_history_reporter_disconnects(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-shutdown-disconnect";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        let flushed = take_final_report_and_flush(&reports, video_id);
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must wait while the flush is pending");

        drop(reports);
        drop(flushed);
        // Process the channel closure without advancing the shutdown deadline.
        cx.run_until_parked();
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "closing the flush acknowledgement channel must release platform exit immediately");
    }

    #[gpui::test]
    fn shutdown_requests_exit_at_three_seconds_if_watch_history_flush_stalls(cx: &mut TestAppContext) {
        let video_id = "test-watch-history-shutdown-timeout";
        let (main, reports) = open_main_window_with_watch_history(cx, video_id);
        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        cx.run_until_parked();
        let flushed = take_final_report_and_flush(&reports, video_id);

        cx.executor().advance_clock(Duration::from_millis(2_999));
        cx.run_until_parked();
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must wait until the full three seconds have elapsed");

        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "platform exit must proceed at the deadline with no acknowledgement");
        assert!(flushed.is_canceled(), "the timeout must stop waiting for the reporter");
    }

    #[gpui::test]
    fn closing_last_window_without_reporter_requests_immediate_exit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            controller::init(cx);
            crate::register_window_close_handler(cx);
        });
        let main = cx.add_window(|_, _| Empty);
        install_test_reporter(cx, None);
        assert!(!cx.read(|cx| cx.has_global::<PlatformQuitRequested>()));

        main.update(cx, |_, window, _| window.remove_window()).unwrap();

        assert!(cx.windows().is_empty());
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()),
            "closing the last window without a reporter must request platform exit synchronously");

        // Let any spawned work register its timers without advancing the test clock.
        cx.run_until_parked();
        assert!(!cx.dispatcher.scheduler().has_pending_tasks(),
            "closing without a reporter must leave no shutdown timer pending");
    }

    #[gpui::test]
    fn closing_last_window_with_disconnected_report_queue_requests_immediate_exit(cx: &mut TestAppContext) {
        let (main, reports) = open_main_window_with_watch_history(cx, "test-watch-history-disconnected-queue");
        drop(reports);
        main.update(cx, |_, window, _| window.remove_window()).unwrap();
        assert!(cx.read(|cx| cx.has_global::<PlatformQuitRequested>()));
        cx.run_until_parked();
        assert!(!cx.dispatcher.scheduler().has_pending_tasks());
    }

    const SHUTDOWN_TEST_POSITION: f64 = 85.375;
    const SHUTDOWN_TEST_DURATION: f64 = 464.0;
    const TEST_SERIAL: u64 = 17;

    fn install_test_reporter(cx: &mut TestAppContext, sender: Option<mpsc::Sender<Event>>) {
        TEST_REPORTER.with(|slot| slot.replace(Some(sender)));
        cx.on_quit(|| {
            TEST_REPORTER.with(|slot| slot.take());
        });
    }

    fn open_main_window_with_watch_history(
        cx: &mut TestAppContext,
        video_id: &str,
    ) -> (WindowHandle<Empty>, Receiver<Event>) {
        let (sender, reports) = mpsc::channel();
        install_test_reporter(cx, Some(sender));
        let video = VideoItem { id: video_id.into(), ..Default::default() };
        crate::store::history_add(&video);
        crate::store::history_progress(video_id, 80.0, SHUTDOWN_TEST_DURATION);
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(Pal::new(true, 0x3ea6ff));
            cx.set_global(UiState::default());
            controller::init(cx);
            controller::player(cx).update(cx, |player, _| {
                let mut report = PlaybackReport {
                    account: Account::for_test(),
                    recording_revision: crate::store::history_recording_revision().unwrap(),
                    video_id: video.id.clone(),
                    serial: TEST_SERIAL,
                    started: false,
                };
                report.start(SHUTDOWN_TEST_POSITION, SHUTDOWN_TEST_DURATION);
                player.set_test_history_report(report);
                player.video = Some(video);
                player.time = SHUTDOWN_TEST_POSITION;
                player.duration = SHUTDOWN_TEST_DURATION;
                player.paused = false;
            });
            crate::register_window_close_handler(cx);
        });
        assert!(matches!(reports.try_recv(), Ok(Event::Start(Start {
            id, serial: TEST_SERIAL, ..
        })) if id == video_id));
        (cx.add_window(|_, _| Empty), reports)
    }

    fn assert_final_report_before_flush(cx: &TestAppContext, reports: &Receiver<Event>, video_id: &str) {
        take_final_report_and_flush(reports, video_id).send(()).unwrap();
        cx.run_until_parked();
        assert!(matches!(reports.try_recv(), Err(TryRecvError::Empty)),
            "expected exactly one final report followed by one flush");
    }

    fn assert_final_position(reports: &Receiver<Event>) {
        assert!(matches!(reports.try_recv(), Ok(Event::Stop {
            serial: TEST_SERIAL,
            position: SHUTDOWN_TEST_POSITION,
        })));
    }

    fn take_final_report_and_flush(reports: &Receiver<Event>, video_id: &str) -> oneshot::Sender<()> {
        let saved = crate::store::progress_map()[video_id];
        assert_eq!(saved, (SHUTDOWN_TEST_POSITION, SHUTDOWN_TEST_DURATION));
        assert_final_position(reports);
        let Ok(Event::FlushOnExit(flushed)) = reports.try_recv() else {
            panic!("closing the last main window must flush after the final position");
        };
        flushed
    }
}
