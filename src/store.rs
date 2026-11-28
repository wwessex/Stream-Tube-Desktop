//! Persistent storage: settings (JSON) + SQLite for subscriptions, history, playlists, caches.
use crate::api::types::VideoItem;
use crate::i18n::AppLanguage;
use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEFAULT_UP_NEXT_THUMBNAIL_WIDTH: u32 = 168;
pub const UP_NEXT_THUMBNAIL_SIZES: &[(u32, &str)] = &[(128, "Small"), (DEFAULT_UP_NEXT_THUMBNAIL_WIDTH, "Medium"), (208, "Large")];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Appearance
    pub theme: String,
    pub app_language: AppLanguage,
    pub accent: u32,
    pub start_page: String,
    pub minimize_to_tray: bool,
    pub grid_columns: u32,
    pub up_next_thumbnail_width: u32,
    // Region
    pub region: String,
    pub language: String,
    pub original_video_titles: bool,
    // Backend
    pub stream_backend: String,
    /// Linux: "auto", "x11" or "wayland" (see display.rs)
    pub display_server: String,
    pub ytdlp_path: String,
    pub proxy: String,
    // Player
    pub theater_mode: bool,
    pub theater_full_height: bool,
    pub pip_always_on_top: bool,
    pub default_quality: u32,
    pub codec_pref: String,
    pub default_speed: f64,
    pub remember_speed_per_channel: bool,
    pub autoplay_next: bool,
    pub resume_playback: bool,
    pub volume: f64,
    pub hwdec: bool,
    pub audio_only: bool,
    pub captions_enabled: bool,
    pub captions_lang: String,
    pub loop_video: bool,
    pub seek_step: f64,
    // SponsorBlock / DeArrow / RYD
    pub sponsorblock: bool,
    pub sponsorblock_server: String,
    /// category -> "skip" | "ask" | "show" | "off"
    pub sb_actions: HashMap<String, String>,
    pub sb_min_duration: f64,
    pub sb_notify: bool,
    pub dearrow_titles: bool,
    pub dearrow_thumbnails: bool,
    pub ryd: bool,
    // Distraction-free / content filtering
    pub hide_shorts: bool,
    pub hide_live: bool,
    pub hide_upcoming: bool,
    pub hide_comments: bool,
    pub hide_related: bool,
    pub hide_watched_in_feed: bool,
    pub hide_view_counts: bool,
    pub hide_subscriber_counts: bool,
    pub hide_description: bool,
    pub hide_channel_avatars: bool,
    pub hide_chapters: bool,
    pub hide_explore: bool,
    pub blocked_keywords: Vec<String>,
    /// (channel_id, name)
    pub blocked_channels: Vec<(String, String)>,
    /// (video_id, title) hidden via "Not interested"
    pub hidden_videos: Vec<(String, String)>,
    // Privacy
    pub history_enabled: bool,
    pub search_history_enabled: bool,
    /// look for a new version on GitHub at startup
    pub check_updates: bool,
    // Subscriptions
    pub feed_source: String,
    pub feed_include_shorts: bool,
    pub feed_include_live: bool,
    // Downloads / external
    pub download_dir: String,
    /// The user picked (or accepted) the download folder; until then the first download asks.
    pub download_dir_chosen: bool,
    pub download_format: String,
    /// File name for downloads, with {title}, {channel} and {id}; empty is "{title} [{id}]"
    pub download_name: String,
    pub external_player: String,
    // appearance / player customization
    pub ui_scale: f32,
    pub density: String,
    pub thumb_radius: f32,
    pub caption_size: u32,
    pub caption_background: bool,
    /// "sans", "serif", "mono" or "casual"
    pub caption_font: String,
    /// 0xRRGGBB
    pub caption_color: u32,
    /// percent
    pub caption_opacity: u32,
    pub caption_bg_color: u32,
    pub caption_bg_opacity: u32,
    /// edge around the text when there's no background:
    /// "outline-shadow", "outline", "shadow" or "none"
    pub caption_edge: String,
    pub controls_hide_ms: u64,
    pub show_storyboard: bool,
    // SponsorBlock account (private ID, used for voting)
    pub sb_user_id: String,
    // Premium-style features
    pub show_heatmap: bool,
    pub jump_ahead: bool,
    pub enhanced_bitrate: bool,
    /// Connect to YouTube over IPv4 only. Many ISPs' IPv6 ranges get the
    /// "confirm you're not a bot" wall while IPv4 from the same line doesn't.
    pub ipv4_only: bool,
    pub background_play: bool,
    pub independent_tab_playback: bool,
    // Tweaks (from YouTube patch sets like Morphe/ReVanced)
    pub ambient_mode: bool,
    pub shorts_in_player: bool,
    pub shorts_autoadvance: bool,
    pub shorts_preload: u8,
    pub shorts_yt_layout: bool,
    pub shorts_edge_nav: bool,
    pub shorts_show_subscribe: bool,
    pub shorts_show_dislike: bool,
    pub remember_quality: bool,
    pub exit_fullscreen_at_end: bool,
    /// "light" | "default" | "strong": gradient behind the player controls
    pub controls_shade: String,
    pub double_click_fullscreen: bool,
    pub original_audio: bool,
    /// play a muted preview when the pointer rests on a thumbnail
    pub hover_preview: bool,
    /// show archived annotations (notes and links YouTube removed in 2019)
    pub annotations: bool,
    /// crop or zoom of the picture, a value of `player::aspect::MODES` ("original", "auto", "21:9"...)
    pub aspect_mode: String,
    /// keep page scrollbars on screen instead of showing them on hover
    pub always_scrollbars: bool,
    /// show the video title in the upper-left corner of the player when controls are visible
    pub show_player_title: bool,
    /// group subscriptions feed videos into day buckets (Today, Yesterday, ...)
    pub feed_group_by_day: bool,
    /// audio equalizer preset ("flat", "bass", "voice", "treble", "warm")
    pub eq_preset: String,
    /// keep the tab bar visible even when only one tab is open
    pub always_show_tab_bar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let mut sb = HashMap::new();
        for (c, _, _) in crate::api::services::SB_CATEGORIES {
            let a = match *c {
                "sponsor" | "selfpromo" | "interaction" => "skip",
                "poi_highlight" | "intro" | "outro" | "preview" | "filler" | "music_offtopic" | "hook" => "show",
                _ => "off",
            };
            sb.insert(c.to_string(), a.to_string());
        }
        Self {
            theme: "dark".into(),
            app_language: AppLanguage::default(),
            accent: 0xff0033,
            start_page: "home".into(),
            minimize_to_tray: false,
            grid_columns: 0,
            up_next_thumbnail_width: DEFAULT_UP_NEXT_THUMBNAIL_WIDTH,
            region: "US".into(),
            language: "en".into(),
            original_video_titles: false,
            stream_backend: "auto".into(),
            display_server: "auto".into(),
            ytdlp_path: String::new(),
            proxy: String::new(),
            theater_mode: false,
            theater_full_height: false,
            pip_always_on_top: false,
            default_quality: 1080,
            codec_pref: "vp9".into(),
            default_speed: 1.0,
            remember_speed_per_channel: true,
            autoplay_next: true,
            resume_playback: true,
            volume: 80.0,
            hwdec: true,
            audio_only: false,
            captions_enabled: false,
            captions_lang: "en".into(),
            loop_video: false,
            seek_step: 5.0,
            sponsorblock: true,
            sponsorblock_server: "https://sponsor.ajay.app".into(),
            sb_actions: sb,
            sb_min_duration: 0.0,
            sb_notify: true,
            annotations: true,
            aspect_mode: "original".into(),
            always_scrollbars: false,
            dearrow_titles: false,
            dearrow_thumbnails: false,
            ryd: true,
            hide_shorts: false,
            hide_live: false,
            hide_upcoming: false,
            hide_comments: false,
            hide_related: false,
            hide_watched_in_feed: false,
            hide_view_counts: false,
            hide_subscriber_counts: false,
            hide_description: false,
            hide_channel_avatars: false,
            hide_chapters: false,
            hide_explore: false,
            blocked_keywords: Vec::new(),
            blocked_channels: Vec::new(),
            hidden_videos: Vec::new(),
            history_enabled: true,
            search_history_enabled: true,
            check_updates: true,
            feed_source: "full".into(),
            feed_include_shorts: false,
            feed_include_live: true,
            feed_group_by_day: true,
            show_player_title: false,
            eq_preset: "flat".into(),
            always_show_tab_bar: false,
            download_dir: dirs::download_dir()
                .map(|d| d.join("Stream Tube").to_string_lossy().into_owned())
                .unwrap_or_default(),
            download_dir_chosen: false,
            download_format: "1080".into(),
            download_name: String::new(),
            external_player: String::new(),
            ui_scale: 1.0,
            density: "default".into(),
            thumb_radius: 12.0,
            caption_size: 40,
            caption_background: true,
            caption_font: "sans".into(),
            caption_color: 0xFFFFFF,
            caption_opacity: 100,
            caption_bg_color: 0x000000,
            caption_bg_opacity: 75,
            caption_edge: "outline-shadow".into(),
            controls_hide_ms: 2500,
            show_storyboard: true,
            sb_user_id: String::new(),
            show_heatmap: true,
            jump_ahead: true,
            enhanced_bitrate: false,
            ipv4_only: true,
            background_play: true,
            independent_tab_playback: false,
            ambient_mode: true,
            shorts_in_player: false,
            shorts_autoadvance: false,
            shorts_preload: 1,
            shorts_yt_layout: false,
            shorts_edge_nav: true,
            shorts_show_subscribe: true,
            shorts_show_dislike: true,
            remember_quality: false,
            exit_fullscreen_at_end: true,
            controls_shade: "default".into(),
            double_click_fullscreen: true,
            original_audio: true,
            hover_preview: true,
        }
    }
}

fn settings_lock() -> &'static RwLock<Arc<Settings>> {
    static S: OnceLock<RwLock<Arc<Settings>>> = OnceLock::new();
    S.get_or_init(|| {
        let mut s = load_settings();
        normalize_settings(&mut s);
        RwLock::new(Arc::new(s))
    })
}

fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}

/// Settings live in settings.json (written atomically) and are mirrored into the
/// database. The file is read first, so a damaged or replaced database never
/// takes the settings with it, and a field that no longer parses (a type changed
/// between versions) only resets that one field instead of all of them.
fn load_settings() -> Settings {
    let from_file = std::fs::read_to_string(settings_file())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object());
    let raw = from_file.or_else(|| {
        load_setting_json::<serde_json::Value>("settings").map(|v| match v {
            serde_json::Value::String(t) => serde_json::from_str(&t).unwrap_or(serde_json::Value::Null),
            v => v,
        })
    });
    let Some(raw) = raw else { return Settings::default() };
    let s = settings_from_value(raw);
    // the file is the copy that survives; write it the first time
    if !settings_file().exists() {
        let _ = write_settings_file(&s);
    }
    s
}

/// Builds settings from stored JSON, keeping every field that parses.
pub(crate) fn settings_from_value(raw: serde_json::Value) -> Settings {
    if let Ok(s) = serde_json::from_value::<Settings>(raw.clone()) {
        return s;
    }
    let serde_json::Value::Object(stored) = raw else { return Settings::default() };
    let Ok(serde_json::Value::Object(mut merged)) = serde_json::to_value(Settings::default()) else {
        return Settings::default();
    };
    for (k, v) in stored {
        if !merged.contains_key(&k) {
            continue;
        }
        let prev = merged.insert(k.clone(), v);
        if serde_json::from_value::<Settings>(serde_json::Value::Object(merged.clone())).is_err() {
            // this field doesn't fit the current type: keep the default
            if let Some(p) = prev {
                merged.insert(k, p);
            }
        }
    }
    serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_default()
}

fn write_settings_file(s: &Settings) -> Result<()> {
    let path = settings_file();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn normalize_settings(s: &mut Settings) {
    if !UP_NEXT_THUMBNAIL_SIZES.iter().any(|&(width, _)| width == s.up_next_thumbnail_width) {
        s.up_next_thumbnail_width = DEFAULT_UP_NEXT_THUMBNAIL_WIDTH;
    }
    const LADDER: [u32; 7] = [144, 240, 360, 480, 720, 1080, 1440];
    if s.default_quality != 2160 && s.default_quality != 4320 && !LADDER.contains(&s.default_quality) {
        s.default_quality = LADDER.iter().copied().filter(|l| *l <= s.default_quality.max(144)).max().unwrap_or(1080);
        if s.default_quality < 720 {
            s.default_quality = 1080;
        }
    }
    s.ui_scale = s.ui_scale.clamp(0.75, 1.5);
    if s.ui_scale == 0.0 {
        s.ui_scale = 1.0;
    }
}

pub fn settings_snapshot() -> Arc<Settings> {
    settings_lock().read().clone()
}

static HISTORY_RECORDING_REVISION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn history_recording_revision() -> Option<u64> {
    let settings = settings_lock().read();
    settings.history_enabled.then(|| HISTORY_RECORDING_REVISION.load(Ordering::Relaxed))
}

pub fn update_settings(f: impl FnOnce(&mut Settings)) {
    // one writer at a time, so two quick changes from different threads
    // can't both start from the same old copy and drop one of them
    static WRITE: Mutex<()> = Mutex::new(());
    let _w = WRITE.lock();
    let mut s = (*settings_snapshot()).clone();
    f(&mut s);
    let _ = write_settings_file(&s);
    let _ = save_setting_json("settings", &s);
    let mut current = settings_lock().write();
    if current.history_enabled != s.history_enabled {
        HISTORY_RECORDING_REVISION.fetch_add(1, Ordering::Relaxed);
    }
    *current = Arc::new(s);
}

/// Whether mpv can use this proxy itself (it only speaks HTTP proxies).
pub fn mpv_http_proxy(p: &str) -> bool {
    let p = p.trim().to_ascii_lowercase();
    p.starts_with("http://") || p.starts_with("https://")
}

pub fn proxy_setting() -> Option<String> {
    let p = settings_snapshot().proxy.clone();
    (!p.trim().is_empty()).then_some(p)
}

/// The profile holds the sign-in (OAuth tokens and Google cookies), so on Linux and
/// macOS only its owner may open it: the folder is 0700 and the files already in it
/// 0600. Windows keeps %APPDATA% per user already.
#[cfg(unix)]
fn make_private(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                let _ = std::fs::set_permissions(e.path(), std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

#[cfg(not(unix))]
fn make_private(_dir: &std::path::Path) {}

pub fn data_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        // a separate profile (portable use, clean test runs)
        if let Some(d) = std::env::var_os("STREAMTUBE_DATA_DIR").filter(|d| !d.is_empty()) {
            let d = PathBuf::from(d);
            let _ = std::fs::create_dir_all(&d);
            make_private(&d);
            return d;
        }
        // tests never touch the real profile
        #[cfg(test)]
        {
            let d = std::env::temp_dir().join(format!("streamtube-test-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&d);
            return d;
        }
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        #[cfg(target_os = "macos")]
        let safari_profile = crate::api::safari::profile_name();
        #[cfg(not(target_os = "macos"))]
        let safari_profile: Option<String> = None;
        let d = base.join(safari_profile.as_deref().unwrap_or("StreamTube"));
        // Carry over data from the regular app's previous name.
        let old = base.join("Ferrotube");
        if safari_profile.is_none() && !d.exists() && old.exists() {
            let _ = std::fs::rename(&old, &d);
        }
        let _ = std::fs::create_dir_all(&d);
        make_private(&d);
        // A -wal/-shm left without its database belongs to some other copy of it;
        // SQLite would replay it onto whatever database is put there next and
        // damage it, so drop it before anything opens the database.
        if !d.join("streamtube.db").exists() {
            let _ = std::fs::remove_file(d.join("streamtube.db-wal"));
            let _ = std::fs::remove_file(d.join("streamtube.db-shm"));
        }
        if !d.join("streamtube.db").exists() && d.join("ferrotube.db").exists() {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::rename(d.join(format!("ferrotube.db{ext}")), d.join(format!("streamtube.db{ext}")));
            }
        }
        d
    })
    .clone()
}

const TABLES: &str = "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE IF NOT EXISTS subscriptions (channel_id TEXT PRIMARY KEY, name TEXT NOT NULL, avatar TEXT, added_at INTEGER NOT NULL);
     CREATE TABLE IF NOT EXISTS profiles (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, color INTEGER NOT NULL);
     CREATE TABLE IF NOT EXISTS profile_channels (profile_id INTEGER NOT NULL, channel_id TEXT NOT NULL, PRIMARY KEY(profile_id, channel_id));
     CREATE TABLE IF NOT EXISTS history (video_id TEXT PRIMARY KEY, json TEXT NOT NULL, watched_at INTEGER NOT NULL, progress REAL NOT NULL DEFAULT 0, duration REAL NOT NULL DEFAULT 0);
     CREATE TABLE IF NOT EXISTS history_sync (video_id TEXT PRIMARY KEY, account TEXT, remote_progress REAL, local_updated_at INTEGER NOT NULL DEFAULT 0, removed INTEGER NOT NULL DEFAULT 0);
     CREATE TABLE IF NOT EXISTS playlists (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, created_at INTEGER NOT NULL, protected INTEGER NOT NULL DEFAULT 0, yt_id TEXT);
     CREATE TABLE IF NOT EXISTS playlist_items (playlist_id INTEGER NOT NULL, video_id TEXT NOT NULL, json TEXT NOT NULL, added_at INTEGER NOT NULL, position INTEGER NOT NULL, PRIMARY KEY(playlist_id, video_id));
     CREATE TABLE IF NOT EXISTS search_history (query TEXT PRIMARY KEY, at INTEGER NOT NULL);
     CREATE TABLE IF NOT EXISTS feed_cache (channel_id TEXT PRIMARY KEY, json TEXT NOT NULL, fetched_at INTEGER NOT NULL);
     CREATE TABLE IF NOT EXISTS channel_speed (channel_id TEXT PRIMARY KEY, speed REAL NOT NULL);
     CREATE TABLE IF NOT EXISTS saved_playlists (playlist_id TEXT PRIMARY KEY, json TEXT NOT NULL, added_at INTEGER NOT NULL);";

/// Opens the database and checks it. A damaged one (its pages no longer agree
/// with each other, so lookups can return another row's data) is rebuilt from
/// every row that can still be read, and the damaged file is kept beside it.
fn open_checked(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).expect("open db");
    if !check_says_damaged(conn.query_row("PRAGMA quick_check(1)", [], |r| r.get::<_, String>(0))) {
        return conn;
    }
    // Another copy of the app has the database open: moving the file away under it
    // would split its writes from ours. Repair at the next start instead.
    if crate::links::other_instance_running() {
        eprintln!("streamtube: the database failed its check; it is repaired when no other copy is running");
        return conn;
    }
    eprintln!("streamtube: the database failed its check, rebuilding it");
    let fresh = path.with_extension("db.rebuild");
    let _ = std::fs::remove_file(&fresh);
    if let Err(e) = rebuild_into(&conn, &fresh) {
        eprintln!("streamtube: couldn't rebuild the database: {e}");
        let _ = std::fs::remove_file(&fresh);
        return conn;
    }
    drop(conn);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let damaged = path.with_extension(format!("db.damaged-{stamp}"));
    let side = |base: &std::path::Path, ext: &str| PathBuf::from(format!("{}{ext}", base.display()));
    let reopen = |why: &str| {
        eprintln!("streamtube: {why}; keeping the database as it is");
        Connection::open(path).expect("open db")
    };
    if std::fs::rename(path, &damaged).is_err() {
        let _ = std::fs::remove_file(&fresh);
        return reopen("couldn't move the damaged database aside");
    }
    // A -wal left in place would be replayed onto the rebuilt database and damage it.
    for ext in ["-wal", "-shm"] {
        let from = side(path, ext);
        if from.exists() && std::fs::rename(&from, side(&damaged, ext)).is_err() {
            for back in ["-wal", "-shm"] {
                let _ = std::fs::rename(side(&damaged, back), side(path, back));
            }
            let _ = std::fs::rename(&damaged, path);
            let _ = std::fs::remove_file(&fresh);
            return reopen("couldn't move the damaged database's journal aside");
        }
    }
    if std::fs::rename(&fresh, path).is_err() {
        for ext in ["-wal", "-shm"] {
            let _ = std::fs::rename(side(&damaged, ext), side(path, ext));
        }
        let _ = std::fs::rename(&damaged, path);
        let _ = std::fs::remove_file(&fresh);
        return reopen("couldn't put the rebuilt database in place");
    }
    Connection::open(path).expect("open db")
}

/// Whether the result of `PRAGMA quick_check` means the file is damaged. A check
/// that couldn't run (the database is busy or locked by another copy) says nothing.
fn check_says_damaged(result: rusqlite::Result<String>) -> bool {
    match result {
        Ok(r) => r != "ok",
        Err(rusqlite::Error::SqliteFailure(e, _)) => {
            matches!(e.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
        }
        Err(_) => false,
    }
}

fn rebuild_into(old: &Connection, fresh: &std::path::Path) -> Result<()> {
    let new = Connection::open(fresh)?;
    new.execute_batch(TABLES)?;
    let tables: Vec<String> = {
        let mut st = new.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?;
        st.query_map([], |r| r.get(0))?.filter_map(Result::ok).collect()
    };
    new.execute_batch("BEGIN")?;
    for t in tables {
        let Ok(st) = old.prepare(&format!("SELECT * FROM \"{t}\" LIMIT 0")) else { continue };
        let n = st.column_count();
        let cols: Vec<String> = st.column_names().map(|c| format!("\"{c}\"")).collect();
        drop(st);
        let marks = vec!["?"; n].join(",");
        let insert = format!("INSERT OR IGNORE INTO \"{t}\"({}) VALUES ({marks})", cols.join(","));
        // Walk the table itself in rowid order (not an index: a damaged index points
        // keys at the wrong rows). When a page can't be read, start again a little
        // past the last good row, so one bad page doesn't lose everything after it.
        let select = format!("SELECT rowid, * FROM \"{t}\" NOT INDEXED WHERE rowid > ?1 ORDER BY rowid");
        let mut after = i64::MIN;
        let mut skip = 1i64;
        let mut tries = 0;
        while tries < 64 {
            let Ok(mut st) = old.prepare(&select) else { break };
            let Ok(mut rows) = st.query([after]) else { break };
            let mut progressed = false;
            let finished = loop {
                match rows.next() {
                    Ok(Some(r)) => {
                        let Ok(id) = r.get::<_, i64>(0) else { continue };
                        after = id;
                        progressed = true;
                        let vals: Vec<rusqlite::types::Value> =
                            (1..=n).map(|i| r.get(i).unwrap_or(rusqlite::types::Value::Null)).collect();
                        let _ = new.execute(&insert, rusqlite::params_from_iter(vals));
                    }
                    Ok(None) => break true,
                    Err(_) => break false,
                }
            };
            if finished {
                break;
            }
            // jump over the unreadable part, further each time it fails again
            skip = if progressed { 1 } else { skip.saturating_mul(4) };
            after = after.saturating_add(skip);
            tries += 1;
        }
    }
    new.execute_batch("COMMIT")?;
    Ok(())
}

fn db() -> &'static Mutex<Connection> {
    static DB: OnceLock<Mutex<Connection>> = OnceLock::new();
    DB.get_or_init(|| {
        let conn = open_checked(&data_dir().join("streamtube.db"));
        // WAL mode is crash-safe with synchronous=NORMAL and avoids an fsync
        // on every 5-second playback progress save.
        conn.execute_batch(&format!("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; {TABLES}"))
        .expect("init db");
        // built-in playlists
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM playlists WHERE protected=1", [], |r| r.get(0))
            .unwrap_or(0);
        if n == 0 {
            let now = crate::api::json::now();
            let _ = conn.execute("INSERT INTO playlists(name, created_at, protected) VALUES ('Watch later', ?1, 1)", [now]);
            let _ = conn.execute("INSERT INTO playlists(name, created_at, protected) VALUES ('Favorites', ?1, 1)", [now + 1]);
        }
        Mutex::new(conn)
    })
}

pub fn init() {
    let _ = db();
    let _ = settings_snapshot();
}

fn load_setting_json<T: for<'de> Deserialize<'de>>(key: &str) -> Option<T> {
    let conn = db().lock();
    let v: Option<String> = conn
        .query_row("SELECT value FROM kv WHERE key=?1", [key], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    v.and_then(|s| serde_json::from_str(&s).ok())
}

fn save_setting_json<T: Serialize>(key: &str, v: &T) -> Result<()> {
    db().lock().execute(
        "INSERT INTO kv(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, serde_json::to_string(v)?],
    )?;
    Ok(())
}

/// Moves on every write that can change what the sidebar lists (subscriptions,
/// playlists), so readers can cache those queries.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

fn bump_generation() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn kv_get(key: &str) -> Option<String> {
    load_setting_json(key)
}

pub fn kv_set(key: &str, v: &str) {
    let _ = save_setting_json(key, &v);
}

// ───────────────────────── subscriptions & profiles ─────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub channel_id: String,
    pub name: String,
    pub avatar: Option<String>,
}

pub fn subscriptions() -> Vec<Subscription> {
    let conn = db().lock();
    let mut st = conn
        .prepare("SELECT channel_id, name, avatar FROM subscriptions ORDER BY name COLLATE NOCASE")
        .unwrap();
    st.query_map([], |r| Ok(Subscription { channel_id: r.get(0)?, name: r.get(1)?, avatar: r.get(2)? }))
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}

pub fn is_subscribed(channel_id: &str) -> bool {
    db().lock()
        .query_row("SELECT 1 FROM subscriptions WHERE channel_id=?1", [channel_id], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

pub fn subscribe(channel_id: &str, name: &str, avatar: Option<&str>) {
    let _ = db().lock().execute(
        "INSERT INTO subscriptions(channel_id, name, avatar, added_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(channel_id) DO UPDATE SET name=excluded.name, avatar=COALESCE(excluded.avatar, avatar)",
        params![channel_id, name, avatar, crate::api::json::now()],
    );
    bump_generation();
}

pub fn unsubscribe(channel_id: &str) {
    let conn = db().lock();
    let _ = conn.execute("DELETE FROM subscriptions WHERE channel_id=?1", [channel_id]);
    let _ = conn.execute("DELETE FROM profile_channels WHERE channel_id=?1", [channel_id]);
    let _ = conn.execute("DELETE FROM feed_cache WHERE channel_id=?1", [channel_id]);
    bump_generation();
}

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub color: u32,
    pub channels: Vec<String>,
}

pub fn profiles() -> Vec<Profile> {
    let conn = db().lock();
    let mut st = conn.prepare("SELECT id, name, color FROM profiles ORDER BY id").unwrap();
    let mut ps: Vec<Profile> = st
        .query_map([], |r| Ok(Profile { id: r.get(0)?, name: r.get(1)?, color: r.get::<_, i64>(2)? as u32, channels: vec![] }))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    for p in ps.iter_mut() {
        let mut st = conn.prepare("SELECT channel_id FROM profile_channels WHERE profile_id=?1").unwrap();
        p.channels = st.query_map([p.id], |r| r.get(0)).unwrap().filter_map(Result::ok).collect();
    }
    ps
}

pub fn create_profile(name: &str, color: u32) -> i64 {
    let conn = db().lock();
    let _ = conn.execute("INSERT INTO profiles(name, color) VALUES (?1, ?2)", params![name, color as i64]);
    conn.last_insert_rowid()
}

pub fn delete_profile(id: i64) {
    let conn = db().lock();
    let _ = conn.execute("DELETE FROM profiles WHERE id=?1", [id]);
    let _ = conn.execute("DELETE FROM profile_channels WHERE profile_id=?1", [id]);
}

pub fn set_profile_channel(profile_id: i64, channel_id: &str, member: bool) {
    let conn = db().lock();
    let _ = if member {
        conn.execute("INSERT OR IGNORE INTO profile_channels VALUES (?1, ?2)", params![profile_id, channel_id])
    } else {
        conn.execute("DELETE FROM profile_channels WHERE profile_id=?1 AND channel_id=?2", params![profile_id, channel_id])
    };
}

// ───────────────────────── feed cache ─────────────────────────

pub fn feed_cache_get(channel_id: &str) -> Option<(Vec<VideoItem>, i64)> {
    let conn = db().lock();
    conn.query_row(
        "SELECT json, fetched_at FROM feed_cache WHERE channel_id=?1",
        [channel_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|(j, t)| serde_json::from_str(&j).ok().map(|v| (v, t)))
}

pub fn feed_cache_put(channel_id: &str, videos: &[VideoItem]) {
    if let Ok(j) = serde_json::to_string(videos) {
        let _ = db().lock().execute(
            "INSERT INTO feed_cache VALUES (?1, ?2, ?3) ON CONFLICT(channel_id) DO UPDATE SET json=excluded.json, fetched_at=excluded.fetched_at",
            params![channel_id, j, crate::api::json::now()],
        );
    }
}

// ───────────────────────── history ─────────────────────────

#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub video: VideoItem,
    pub watched_at: i64,
    pub progress: f64,
    pub duration: f64,
}

pub fn history_add(v: &VideoItem) {
    if !settings_snapshot().history_enabled {
        return;
    }
    let result = (|| -> Result<()> {
        let j = serde_json::to_string(v)?;
        let mut conn = db().lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO history(video_id, json, watched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(video_id) DO UPDATE SET json=excluded.json, watched_at=excluded.watched_at",
            params![v.id, j, crate::api::json::now()],
        )?;
        tx.execute("UPDATE history_sync SET removed=0 WHERE video_id=?1 AND removed<>0", [&v.id])?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("streamtube: could not save watch history: {error}");
    }
}

pub fn history_progress(video_id: &str, progress: f64, duration: f64) {
    if !settings_snapshot().history_enabled || !progress.is_finite() || progress < 0.0 || !duration.is_finite() || duration < 0.0 {
        return;
    }
    let result = (|| -> Result<()> {
        let mut conn = db().lock();
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE history SET progress=?2, duration=?3
             WHERE video_id=?1 AND (progress<>?2 OR duration<>?3)",
            params![video_id, progress, duration],
        )?;
        if updated > 0 {
            tx.execute(
                "INSERT INTO history_sync(video_id, local_updated_at) VALUES (?1, ?2)
                 ON CONFLICT(video_id) DO UPDATE SET local_updated_at=excluded.local_updated_at",
                params![video_id, chrono::Utc::now().timestamp_millis()],
            )?;
        }
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("streamtube: could not save playback position: {error}");
    }
}

/// Like `history_progress`, on a background thread: the player saves progress every
/// few seconds, and a slow disk must not stall the UI thread (it draws the video).
/// Writes keep their order; with `wait`, this returns once the write (and every
/// one queued before it) is done.
pub fn history_progress_queued(video_id: &str, progress: f64, duration: f64, wait: bool) {
    type Job = (String, f64, f64, Option<std::sync::mpsc::Sender<()>>);
    static WORKER: OnceLock<Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    let tx = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let _ = std::thread::Builder::new().name("history-progress".into()).spawn(move || {
            for (id, p, d, done) in rx {
                history_progress(&id, p, d);
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
        });
        Mutex::new(tx)
    });
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    if tx.lock().send((video_id.to_string(), progress, duration, wait.then_some(done_tx))).is_err() {
        history_progress(video_id, progress, duration);
        return;
    }
    if wait {
        let _ = done_rx.recv_timeout(std::time::Duration::from_secs(5));
    }
}

pub fn history_list(query: &str, limit: usize) -> Vec<HistoryEntry> {
    let conn = db().lock();
    let mut st = conn
        .prepare("SELECT json, watched_at, progress, duration FROM history ORDER BY watched_at DESC")
        .unwrap();
    let q = query.to_lowercase();
    st.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?))
    })
    .unwrap()
    .filter_map(Result::ok)
    .filter_map(|(j, t, p, d)| {
        let v: VideoItem = serde_json::from_str(&j).ok()?;
        Some(HistoryEntry { video: v, watched_at: t, progress: p, duration: d })
    })
    .filter(|h| q.is_empty() || h.video.title.to_lowercase().contains(&q) || h.video.channel_name.to_lowercase().contains(&q))
    .take(limit)
    .collect()
}

/// video_id -> (progress, duration)
pub fn progress_map() -> HashMap<String, (f64, f64)> {
    let conn = db().lock();
    let mut st = conn.prepare("SELECT video_id, progress, duration FROM history").unwrap();
    st.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))))
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}

pub fn playback_position(video_id: &str) -> Option<(f64, f64)> {
    db().lock().query_row(
        "SELECT progress, duration FROM history WHERE video_id=?1",
        [video_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).ok()
}

pub fn resume_position(video_id: &str) -> Option<f64> {
    let (progress, duration) = playback_position(video_id)?;
    (progress > 10.0 && (duration <= 0.0 || progress < duration - 15.0)).then_some(progress)
}

pub fn history_merge_remote(
    account: u64,
    entries: &[crate::api::history::Entry],
    protected_video: Option<&str>,
    requested_at: i64,
    offset: usize,
    recording_revision: u64,
) -> Result<()> {
    if history_recording_revision() != Some(recording_revision) {
        return Ok(());
    }
    merge_remote_history(&mut db().lock(), &account.to_string(), entries, protected_video, requested_at, offset)
}

fn merge_remote_history(
    conn: &mut Connection,
    account: &str,
    entries: &[crate::api::history::Entry],
    protected_video: Option<&str>,
    requested_at: i64,
    offset: usize,
) -> Result<()> {
    const LOCAL_GRACE_MS: i64 = 30_000;
    let tx = conn.transaction()?;
    {
        let mut local = tx.prepare("SELECT progress FROM history WHERE video_id=?1")?;
        let mut state = tx.prepare(
            "SELECT account, remote_progress, local_updated_at, removed FROM history_sync WHERE video_id=?1",
        )?;
        let mut observe = tx.prepare(
            "INSERT INTO history_sync(video_id, account, remote_progress, removed) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(video_id) DO UPDATE SET account=excluded.account,
                 remote_progress=excluded.remote_progress, removed=excluded.removed",
        )?;
        let mut save = tx.prepare(
            "INSERT INTO history(video_id, json, watched_at, progress, duration) VALUES (?1, ?2, ?3, COALESCE(?4, 0), ?5)
             ON CONFLICT(video_id) DO UPDATE SET progress=COALESCE(?4, history.progress),
                 duration=CASE WHEN history.duration > 0 THEN history.duration ELSE excluded.duration END",
        )?;
        for (index, entry) in entries.iter().enumerate() {
            let id = entry.video.id.as_str();
            if Some(id) == protected_video {
                continue;
            }
            let (observed_account, previous_remote, local_at, removed): (Option<String>, Option<f64>, i64, bool) =
                state.query_row([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                    .optional()?.unwrap_or_default();
            if local_at > requested_at.saturating_sub(LOCAL_GRACE_MS) {
                continue;
            }
            let local_progress: Option<f64> = local.query_row([id], |row| row.get(0)).optional()?;
            let previous_remote = previous_remote.filter(|_| observed_account.as_deref() == Some(account));
            let remote_changed = entry.progress.zip(previous_remote)
                .is_some_and(|(remote, previous)| (remote.seconds - previous).abs() >= remote.precision);
            let still_removed = removed && !remote_changed;
            let observation = entry.progress.map(|p| p.seconds).or(previous_remote);
            observe.execute(params![id, account, observation, still_removed])?;
            if still_removed {
                continue;
            }
            let accepted = entry.progress.filter(|remote| {
                // Without a prior server observation, its age is unknown. Keep
                // meaningful local progress until the server position changes.
                local_progress.is_none_or(|position| (position == 0.0 && local_at == 0 && previous_remote.is_none()) ||
                    (remote_changed && (remote.seconds - position).abs() >= remote.precision))
            });
            let json = serde_json::to_string(&entry.video)?;
            let duration = entry.video.duration_secs.unwrap_or(0) as f64;
            // History responses are ordered but don't give a per-video date.
            let watched_at = requested_at / 1000 - (offset + index) as i64;
            save.execute(params![id, json, watched_at, accepted.map(|p| p.seconds), duration])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn history_remove(video_id: &str) {
    if let Err(error) = try_history_remove(video_id) {
        eprintln!("streamtube: could not remove watch history: {error}");
    }
}

pub(crate) fn try_history_remove(video_id: &str) -> Result<()> {
    let mut conn = db().lock();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO history_sync(video_id, local_updated_at, removed) VALUES (?1, ?2, 1)
         ON CONFLICT(video_id) DO UPDATE SET removed=1, local_updated_at=excluded.local_updated_at",
        params![video_id, chrono::Utc::now().timestamp_millis()],
    )?;
    tx.execute("DELETE FROM history WHERE video_id=?1", [video_id])?;
    tx.commit()?;
    Ok(())
}

pub fn history_clear() {
    let result = (|| -> Result<()> {
        let mut conn = db().lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO history_sync(video_id, local_updated_at, removed)
             SELECT video_id, ?1, 1 FROM history WHERE true
             ON CONFLICT(video_id) DO UPDATE SET removed=1, local_updated_at=excluded.local_updated_at",
            [chrono::Utc::now().timestamp_millis()],
        )?;
        tx.execute("DELETE FROM history", [])?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("streamtube: could not clear watch history: {error}");
    }
}

#[cfg(test)]
mod history_merge_tests {
    use super::*;
    use crate::api::history::{Entry, Position};

    const NOW: i64 = 1_700_000_000_000;
    const VIDEO_ID: &str = "test-video";

    fn entry(seconds: Option<f64>) -> Entry {
        Entry {
            video: VideoItem { id: VIDEO_ID.into(), duration_secs: Some(600), ..Default::default() },
            progress: seconds.map(|seconds| Position { seconds, precision: 1.0 }),
            removal_token: None,
        }
    }

    fn progress(conn: &Connection) -> Option<f64> {
        conn.query_row("SELECT progress FROM history WHERE video_id=?1", [VIDEO_ID], |row| row.get(0))
            .optional().unwrap()
    }

    #[test]
    fn uses_a_changed_remote_position_even_when_it_moves_backwards() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES).unwrap();
        merge_remote_history(&mut conn, "web", &[entry(Some(300.0))], None, NOW, 0).unwrap();
        assert_eq!(progress(&conn), Some(300.0));

        merge_remote_history(&mut conn, "web", &[entry(None)], None, NOW + 60_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(300.0));

        merge_remote_history(&mut conn, "web", &[entry(Some(80.0))], None, NOW + 120_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(80.0));
    }

    #[test]
    fn first_observation_and_in_flight_refresh_keep_local_progress() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES).unwrap();
        conn.execute("INSERT INTO history VALUES (?1, '{}', 123, 300, 600)", [VIDEO_ID]).unwrap();
        merge_remote_history(&mut conn, "web", &[entry(Some(450.0))], None, NOW, 0).unwrap();
        assert_eq!(progress(&conn), Some(300.0));

        conn.execute("UPDATE history_sync SET local_updated_at=?1 WHERE video_id=?2", params![NOW + 61_000, VIDEO_ID]).unwrap();
        merge_remote_history(&mut conn, "web", &[entry(Some(80.0))], None, NOW + 60_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(300.0));

        merge_remote_history(&mut conn, "web", &[entry(Some(80.0))], Some(VIDEO_ID), NOW + 120_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(300.0));
        merge_remote_history(&mut conn, "web", &[entry(Some(80.0))], None, NOW + 120_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(80.0));
    }

    #[test]
    fn an_unchanged_remote_entry_does_not_reappear_after_local_removal() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES).unwrap();
        merge_remote_history(&mut conn, "web", &[entry(Some(300.0))], None, NOW, 0).unwrap();
        conn.execute("DELETE FROM history", []).unwrap();
        conn.execute("UPDATE history_sync SET removed=1", []).unwrap();
        merge_remote_history(&mut conn, "web", &[entry(Some(300.0))], None, NOW + 60_000, 0).unwrap();
        assert_eq!(progress(&conn), None);

        merge_remote_history(&mut conn, "web", &[entry(Some(80.0))], None, NOW + 120_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(80.0));
    }

    #[test]
    fn does_not_treat_a_different_account_as_a_new_view_or_overwrite_a_local_replay() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES).unwrap();
        merge_remote_history(&mut conn, "web", &[entry(Some(300.0))], None, NOW, 0).unwrap();
        merge_remote_history(&mut conn, "another-account", &[entry(Some(450.0))], None, NOW + 60_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(300.0));
        conn.execute("UPDATE history SET progress=0 WHERE video_id=?1", [VIDEO_ID]).unwrap();
        conn.execute("UPDATE history_sync SET local_updated_at=?1 WHERE video_id=?2", params![NOW + 65_000, VIDEO_ID]).unwrap();
        merge_remote_history(&mut conn, "another-account", &[entry(Some(450.0))], None, NOW + 120_000, 0).unwrap();
        assert_eq!(progress(&conn), Some(0.0));
    }
}

// ───────────────────────── search history ─────────────────────────

pub fn search_history_add(q: &str) {
    if !settings_snapshot().search_history_enabled || q.trim().is_empty() {
        return;
    }
    let _ = db().lock().execute(
        "INSERT INTO search_history VALUES (?1, ?2) ON CONFLICT(query) DO UPDATE SET at=excluded.at",
        params![q.trim(), crate::api::json::now()],
    );
}

pub fn search_history(prefix: &str, limit: usize) -> Vec<String> {
    let conn = db().lock();
    let mut st = conn
        .prepare("SELECT query FROM search_history WHERE query LIKE ?1 ORDER BY at DESC LIMIT ?2")
        .unwrap();
    st.query_map(params![format!("{}%", prefix.replace('%', "")), limit as i64], |r| r.get(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect()
}

/// Adds searches made on other devices without moving existing entries;
/// `at` is when they should sort (older than anything searched here).
pub fn search_history_merge_remote(q: &str, at: i64) -> bool {
    if !settings_snapshot().search_history_enabled || q.trim().is_empty() {
        return false;
    }
    db().lock().execute("INSERT OR IGNORE INTO search_history VALUES (?1, ?2)", params![q.trim(), at]).map(|n| n > 0).unwrap_or(false)
}

pub fn search_history_remove(q: &str) {
    let _ = db().lock().execute("DELETE FROM search_history WHERE query=?1", [q]);
}

pub fn search_history_clear() {
    let _ = db().lock().execute("DELETE FROM search_history", []);
}

// ───────────────────────── local playlists ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct LocalPlaylist {
    pub id: i64,
    pub name: String,
    pub protected: bool,
    pub count: usize,
    pub thumbnail: Option<String>,
}

pub fn playlists() -> Vec<LocalPlaylist> {
    let conn = db().lock();
    let mut st = conn
        .prepare(
            "SELECT p.id, p.name, p.protected, (SELECT COUNT(*) FROM playlist_items i WHERE i.playlist_id=p.id),
                    (SELECT json FROM playlist_items i WHERE i.playlist_id=p.id ORDER BY position LIMIT 1)
             FROM playlists p ORDER BY p.protected DESC, p.created_at",
        )
        .unwrap();
    st.query_map([], |r| {
        let j: Option<String> = r.get(4)?;
        Ok(LocalPlaylist {
            id: r.get(0)?,
            name: r.get(1)?,
            protected: r.get::<_, i64>(2)? != 0,
            count: r.get::<_, i64>(3)? as usize,
            thumbnail: j.and_then(|j| serde_json::from_str::<VideoItem>(&j).ok()).map(|v| v.thumbnail),
        })
    })
    .unwrap()
    .filter_map(Result::ok)
    .collect()
}

pub fn playlist_create(name: &str) -> i64 {
    let conn = db().lock();
    let _ = conn.execute(
        "INSERT INTO playlists(name, created_at) VALUES (?1, ?2)",
        params![name, crate::api::json::now()],
    );
    let id = conn.last_insert_rowid();
    bump_generation();
    id
}

pub fn playlist_rename(id: i64, name: &str) {
    let _ = db().lock().execute("UPDATE playlists SET name=?2 WHERE id=?1", params![id, name]);
    bump_generation();
}

pub fn playlist_delete(id: i64) {
    let conn = db().lock();
    let _ = conn.execute("DELETE FROM playlists WHERE id=?1 AND protected=0", [id]);
    let _ = conn.execute("DELETE FROM playlist_items WHERE playlist_id=?1", [id]);
    bump_generation();
}

pub fn playlist_items(id: i64) -> Vec<VideoItem> {
    let conn = db().lock();
    let mut st = conn
        .prepare("SELECT json FROM playlist_items WHERE playlist_id=?1 ORDER BY position")
        .unwrap();
    st.query_map([id], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|j| serde_json::from_str(&j).ok())
        .collect()
}

pub fn playlist_add(id: i64, v: &VideoItem) {
    let conn = db().lock();
    let pos: i64 = conn
        .query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_items WHERE playlist_id=?1", [id], |r| r.get(0))
        .unwrap_or(0);
    if let Ok(j) = serde_json::to_string(v) {
        let _ = conn.execute(
            "INSERT OR IGNORE INTO playlist_items VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, v.id, j, crate::api::json::now(), pos],
        );
    }
}

pub fn playlist_remove(id: i64, video_id: &str) {
    let _ = db().lock().execute(
        "DELETE FROM playlist_items WHERE playlist_id=?1 AND video_id=?2",
        params![id, video_id],
    );
}

pub fn playlist_contains(id: i64, video_id: &str) -> bool {
    db().lock()
        .query_row(
            "SELECT 1 FROM playlist_items WHERE playlist_id=?1 AND video_id=?2",
            params![id, video_id],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
}

/// Moves an item up (-1) or down (+1).
pub fn playlist_move(id: i64, video_id: &str, delta: i64) {
    let items = playlist_items(id);
    let Some(idx) = items.iter().position(|v| v.id == video_id) else { return };
    let target = idx as i64 + delta;
    if target < 0 || target >= items.len() as i64 {
        return;
    }
    let mut ids: Vec<String> = items.iter().map(|v| v.id.clone()).collect();
    ids.swap(idx, target as usize);
    let conn = db().lock();
    for (i, vid) in ids.iter().enumerate() {
        let _ = conn.execute(
            "UPDATE playlist_items SET position=?3 WHERE playlist_id=?1 AND video_id=?2",
            params![id, vid, i as i64],
        );
    }
}

pub fn saved_playlists() -> Vec<crate::api::types::PlaylistItem> {
    let conn = db().lock();
    let mut st = conn.prepare("SELECT json FROM saved_playlists ORDER BY added_at DESC").unwrap();
    st.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|j| serde_json::from_str(&j).ok())
        .collect()
}

pub fn save_playlist(p: &crate::api::types::PlaylistItem, save: bool) {
    let conn = db().lock();
    let _ = if save {
        conn.execute(
            "INSERT OR REPLACE INTO saved_playlists VALUES (?1, ?2, ?3)",
            params![p.id, serde_json::to_string(p).unwrap_or_default(), crate::api::json::now()],
        )
    } else {
        conn.execute("DELETE FROM saved_playlists WHERE playlist_id=?1", [&p.id])
    };
}

pub fn is_playlist_saved(id: &str) -> bool {
    db().lock()
        .query_row("SELECT 1 FROM saved_playlists WHERE playlist_id=?1", [id], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

// ───────────────────────── per-channel speed ─────────────────────────

pub fn channel_speed(channel_id: &str) -> Option<f64> {
    db().lock()
        .query_row("SELECT speed FROM channel_speed WHERE channel_id=?1", [channel_id], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

pub fn set_channel_speed(channel_id: &str, speed: f64) {
    let _ = db().lock().execute(
        "INSERT INTO channel_speed VALUES (?1, ?2) ON CONFLICT(channel_id) DO UPDATE SET speed=excluded.speed",
        params![channel_id, speed],
    );
}

pub fn speed_reset_available(channel_id: &str) -> bool {
    channel_speed(channel_id).is_some()
}

pub fn clear_channel_speed(channel_id: &str) {
    let _ = db().lock().execute("DELETE FROM channel_speed WHERE channel_id=?1", [channel_id]);
}

// ───────────────────────── content filter ─────────────────────────

/// Returns true if the video should be hidden according to filtering settings.
pub fn is_filtered(v: &VideoItem, s: &Settings) -> bool {
    if s.hide_shorts && v.is_short {
        return true;
    }
    if s.hide_live && v.is_live {
        return true;
    }
    if s.hide_upcoming && v.is_upcoming {
        return true;
    }
    if s.hidden_videos.iter().any(|(id, _)| id == &v.id) {
        return true;
    }
    if let Some(cid) = &v.channel_id {
        if s.blocked_channels.iter().any(|(id, _)| id == cid) {
            return true;
        }
    }
    if !s.blocked_keywords.is_empty() {
        let t = v.title.to_lowercase();
        if s.blocked_keywords.iter().any(|k| !k.trim().is_empty() && t.contains(&k.trim().to_lowercase())) {
            return true;
        }
    }
    false
}

/// Local like state of a video: 1 liked, -1 disliked, 0 none.
pub fn rating(video_id: &str) -> i8 {
    kv_get(&format!("rating:{video_id}")).and_then(|s| s.parse().ok()).unwrap_or(0)
}

pub fn set_rating(video_id: &str, r: i8) {
    kv_set(&format!("rating:{video_id}"), &r.to_string());
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn theater_settings_round_trip_and_default_to_off() {
        let old = settings_from_value(serde_json::json!({}));
        assert!(!old.theater_mode);
        assert!(!old.theater_full_height);

        let enabled = Settings { theater_mode: true, theater_full_height: true, ..old };
        let saved = serde_json::to_value(enabled).unwrap();
        let restored = settings_from_value(saved);
        assert!(restored.theater_mode);
        assert!(restored.theater_full_height);
    }

    #[test]
    fn a_bad_field_keeps_the_others() {
        let raw = serde_json::json!({
            "hide_shorts": true,
            "blocked_keywords": ["spoiler"],
            "sb_actions": { "sponsor": "ask" },
            // wrong type, e.g. from another version
            "default_quality": "high",
            "not_a_setting": 1
        });
        let s = settings_from_value(raw);
        assert!(s.hide_shorts);
        assert_eq!(s.blocked_keywords, vec!["spoiler".to_string()]);
        assert_eq!(s.sb_actions.get("sponsor").map(String::as_str), Some("ask"));
        assert_eq!(s.default_quality, Settings::default().default_quality);
    }

    #[test]
    fn damaged_database_is_rebuilt() {
        let dir = std::env::temp_dir().join(format!("st-rebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("streamtube.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(TABLES).unwrap();
            for i in 0..300 {
                c.execute("INSERT INTO kv VALUES (?1, ?2)", params![format!("k{i}"), "x".repeat(200)]).unwrap();
            }
            c.execute("INSERT INTO subscriptions VALUES ('UC1', 'One', NULL, 1)", []).unwrap();
        }
        // scribble over a page in the middle of the file, as a damaged write would
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            // the header of a page in the middle of the kv table
            f.seek(SeekFrom::Start(4096 * 6)).unwrap();
            f.write_all(&[0xAB; 64]).unwrap();
        }
        let c = open_checked(&path);
        let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0)).unwrap();
        assert!(n > 100, "kept {n} rows");
        let damaged = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().contains("damaged"));
        assert!(damaged, "the damaged file is kept");
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_check_that_could_not_run_is_not_damage() {
        let fail = |code| Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None));
        assert!(!check_says_damaged(Ok("ok".into())));
        assert!(check_says_damaged(Ok("*** in database main ***".into())));
        assert!(check_says_damaged(fail(rusqlite::ffi::SQLITE_CORRUPT)));
        assert!(check_says_damaged(fail(rusqlite::ffi::SQLITE_NOTADB)));
        assert!(!check_says_damaged(fail(rusqlite::ffi::SQLITE_BUSY)));
        assert!(!check_says_damaged(fail(rusqlite::ffi::SQLITE_LOCKED)));
    }

    /// A damaged database left with its -wal (the app was closed mid-session): the
    /// rebuilt file must not get the old journal replayed onto it.
    #[test]
    fn damaged_database_with_its_journal_is_rebuilt() {
        let dir = std::env::temp_dir().join(format!("st-rebuild-wal-{}", std::process::id()));
        let live = dir.join("live");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&live).unwrap();
        let path = dir.join("streamtube.db");
        {
            let c = Connection::open(live.join("streamtube.db")).unwrap();
            c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
            c.execute_batch(TABLES).unwrap();
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            for i in 0..300 {
                c.execute("INSERT INTO kv VALUES (?1, ?2)", params![format!("k{i}"), "x".repeat(200)]).unwrap();
            }
            c.execute_batch("PRAGMA wal_checkpoint(PASSIVE)").unwrap();
            for i in 0..50 {
                c.execute("INSERT INTO history(video_id, json, watched_at) VALUES (?1, '{}', ?2)", params![format!("v{i}"), i]).unwrap();
            }
            // copy the files as they are on disk while the connection still has them open
            for ext in ["", "-wal"] {
                std::fs::copy(live.join(format!("streamtube.db{ext}")), dir.join(format!("streamtube.db{ext}"))).unwrap();
            }
        }
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(4096 * 6)).unwrap();
            f.write_all(&[0xAB; 64]).unwrap();
        }
        assert!(dir.join("streamtube.db-wal").exists());
        let c = open_checked(&path);
        let ok: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 50, "rows that were only in the journal are kept");
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().any(|n| n.contains("damaged")), "the damaged file is kept: {names:?}");
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod progress_queue_tests {
    use super::*;

    #[test]
    fn queued_progress_writes_land_in_order() {
        let v = crate::api::types::VideoItem { id: "queued-progress".into(), ..Default::default() };
        history_add(&v);
        for i in 1..=50 {
            history_progress_queued(&v.id, i as f64, 100.0, false);
        }
        history_progress_queued(&v.id, 77.0, 100.0, true);
        // `wait` gives up after 5 s so shutdown can't hang; on a busy CI runner other
        // tests can hold the database longer than that, so give the worker more time here
        let read = || history_list("", 500).into_iter().find(|h| h.video.id == v.id).map(|h| h.progress);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while read() != Some(77.0) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(read(), Some(77.0));
    }
}
