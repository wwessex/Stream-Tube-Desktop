//! Keep the opening media for the next Shorts in memory until playback uses it.
use super::proxy;
use crate::api::{streams, types::VideoItem};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

pub const MAX_AHEAD: u8 = 5;

const PLAYLIST_LIMIT: usize = 512 * 1024;
const SEGMENT_LIMIT: usize = 4 * 1024 * 1024;
const STREAM_PREFIX: usize = 512 * 1024;
const PER_SHORT_LIMIT: usize = 8 * 1024 * 1024;
const MAX_ATTEMPTS: u8 = 2;

#[derive(Clone)]
pub(super) struct Media {
    pub bytes: Arc<[u8]>,
    pub total: u64,
    pub mime: String,
    // For a media playlist, set only after all files needed for its first segment are cached.
    opening_seconds: Option<f64>,
    served: Arc<AtomicU64>,
}

impl Media {
    pub(super) fn record_served(&self, bytes: u64) {
        self.served.fetch_add(bytes, Ordering::Relaxed);
    }
}

type Assets = Mutex<HashMap<String, Media>>;

#[derive(Default)]
struct Entry {
    assets: Arc<Assets>,
    attempts: u8,
    finished: bool,
}

#[derive(Default)]
struct Preloads {
    upcoming: Vec<String>,
    videos: HashMap<String, Entry>,
    quality: u32,
    audio_only: bool,
    running: bool,
    ready: bool,
}

static STATE: OnceLock<Mutex<Preloads>> = OnceLock::new();

impl Preloads {
    fn next(&self) -> Option<&String> {
        self.upcoming
            .iter()
            .filter(|id| {
                self.videos
                    .get(*id)
                    .is_none_or(|e| !e.finished && e.attempts < MAX_ATTEMPTS)
            })
            .min_by_key(|id| self.videos.get(*id).map_or(0, |e| e.attempts))
    }
}

pub(super) fn suspend() {
    if let Some(state) = STATE.get() {
        state.lock().ready = false;
    }
}

pub fn clear() {
    if let Some(state) = STATE.get() {
        let mut state = state.lock();
        state.ready = false;
        state.upcoming.clear();
        state.videos.clear();
    }
}

pub fn update(feed: &[VideoItem], quality: u32, audio_only: bool, ready: bool) {
    let ahead = crate::store::settings_snapshot().shorts_preload.min(MAX_AHEAD) as usize;
    if ahead == 0 || feed.is_empty() {
        clear();
        return;
    }
    let current = &feed[0].id;
    let next = &feed[1..feed.len().min(1 + ahead)];
    let state = STATE.get_or_init(Default::default);
    let mut s = state.lock();
    s.ready = ready;
    if (s.quality, s.audio_only) != (quality, audio_only) {
        s.videos.clear();
        s.quality = quality;
        s.audio_only = audio_only;
    }
    s.videos.retain(|id, _| id == current || next.iter().any(|v| v.id == *id));
    if !s.upcoming.iter().eq(next.iter().map(|v| &v.id)) {
        s.upcoming = next.iter().map(|v| v.id.clone()).collect();
    }
    if !s.ready || s.running || s.next().is_none() {
        return;
    }
    s.running = true;
    // One worker always takes the nearest missing Short from the latest feed position.
    std::thread::spawn(move || {
        loop {
            let (id, assets, quality, audio_only) = {
                let mut s = state.lock();
                if !s.ready {
                    s.running = false;
                    return;
                }
                let Some(id) = s.next().cloned() else {
                    s.running = false;
                    return;
                };
                let assets = Arc::downgrade(&s.videos.entry(id.clone()).or_default().assets);
                (id, assets, s.quality, s.audio_only)
            };
            let finished = preload(&id, assets.clone(), quality, audio_only).is_some();
            let mut s = state.lock();
            let ready = s.ready;
            if let Some(entry) = s
                .videos
                .get_mut(&id)
                .filter(|e| Weak::ptr_eq(&assets, &Arc::downgrade(&e.assets)))
            {
                entry.finished = finished;
                if ready {
                    entry.attempts += 1;
                }
            }
        }
    });
}

pub(super) fn cached(url: &str) -> Option<Media> {
    STATE
        .get()?
        .lock()
        .videos
        .values()
        .find_map(|entry| entry.assets.lock().get(url).cloned())
}

/// Counts media bytes actually written from this Short's cache to the player.
pub(super) fn served_bytes(id: &str) -> u64 {
    let Some(state) = STATE.get() else { return 0 };
    let state = state.lock();
    state.videos.get(id).map_or(0, |entry| {
        entry
            .assets
            .lock()
            .values()
            .map(|m| m.served.load(Ordering::Relaxed))
            .sum()
    })
}

/// The stream resolver already fetches the master to list qualities and audio tracks.
pub(crate) fn remember_master(id: &str, url: &str, text: &str) {
    if text.len() > PLAYLIST_LIMIT || !text.starts_with("#EXTM3U") {
        return;
    }
    let Some(assets) = STATE
        .get()
        .and_then(|s| s.lock().videos.get(id).map(|e| e.assets.clone()))
    else {
        return;
    };
    let media = Media {
        bytes: Arc::from(text.as_bytes()),
        total: text.len() as u64,
        mime: "application/vnd.apple.mpegurl".into(),
        opening_seconds: None,
        served: Default::default(),
    };
    let _ = Download(Arc::downgrade(&assets)).store(url, media);
}

struct Download(Weak<Assets>);

impl Download {
    fn can_fetch(&self) -> bool {
        STATE.get().is_some_and(|state| {
            let s = state.lock();
            s.ready
                && s.upcoming.iter().any(|id| {
                    s.videos
                        .get(id)
                        .is_some_and(|e| self.0.as_ptr() == Arc::as_ptr(&e.assets))
                })
        })
    }

    fn store(&self, url: &str, media: Media) -> Option<()> {
        let owner = self.0.upgrade()?;
        let mut assets = owner.lock();
        if assets.contains_key(url) {
            return Some(());
        }
        let used: usize = assets.values().map(|a| a.bytes.len()).sum();
        if used + media.bytes.len() > PER_SHORT_LIMIT {
            return None;
        }
        assets.insert(url.to_owned(), media);
        Some(())
    }

    fn fetch(&self, url: &str, limit: usize, prefix: bool) -> Option<Media> {
        if !self.can_fetch() || !url.starts_with("https://") {
            return None;
        }
        let mut req = proxy::agent().get(url).header("User-Agent", crate::api::http::BROWSER_UA);
        if prefix {
            req = req.header("Range", &format!("bytes=0-{}", limit - 1));
        }
        let mut resp = req.call().ok()?;
        let range = match resp.status().as_u16() {
            200 => None,
            206 if prefix => {
                let (end, total) = resp
                    .headers()
                    .get("Content-Range")?
                    .to_str()
                    .ok()?
                    .strip_prefix("bytes 0-")?
                    .split_once('/')?;
                Some((end.parse::<u64>().ok()?.checked_add(1)?, total.parse::<u64>().ok()?))
            }
            _ => return None,
        };
        let mime = resp
            .headers()
            .get("Content-Type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        if resp
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
            .is_some_and(|len| len > limit as u64)
        {
            return None;
        }
        let mut bytes = Vec::new();
        let mut reader = resp.body_mut().as_reader();
        let mut chunk = [0; 32 * 1024];
        loop {
            if !self.can_fetch() {
                return None;
            }
            let n = reader.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > limit {
                return None;
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        let len = bytes.len() as u64;
        let (expected, total) = range.unwrap_or((len, len));
        if len == 0 || len != expected || total < len {
            return None;
        }
        Some(Media {
            bytes: bytes.into(),
            total,
            mime,
            opening_seconds: None,
            served: Default::default(),
        })
    }

    fn playlist(&self, url: &str) -> Option<String> {
        if let Some(media) = self.0.upgrade()?.lock().get(url).cloned() {
            return std::str::from_utf8(&media.bytes).ok().map(str::to_owned);
        }
        let media = self.fetch(url, PLAYLIST_LIMIT, false)?;
        let text = std::str::from_utf8(&media.bytes).ok()?.to_owned();
        if !text.starts_with("#EXTM3U") {
            return None;
        }
        self.store(url, media)?;
        Some(text)
    }

    fn cache_media(&self, url: &str, limit: usize, prefix: bool) -> Option<()> {
        if self.0.upgrade()?.lock().contains_key(url) {
            return Some(());
        }
        self.store(url, self.fetch(url, limit, prefix)?)
    }

    fn opening_segments(&self, url: &str, playlist: &str) -> Option<()> {
        let mut duration = None;
        for line in playlist.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if let Some(value) = line.strip_prefix("#EXTINF:") {
                duration = value
                    .split(',')
                    .next()?
                    .parse::<f64>()
                    .ok()
                    .filter(|d| d.is_finite() && *d > 0.0);
            } else if line.starts_with("#EXT-X-MAP:") || line.starts_with("#EXT-X-KEY:") {
                if let Some(uri) = proxy::hls_attr(line, "URI") {
                    self.cache_media(&proxy::playlist_url(url, &uri)?, SEGMENT_LIMIT, false)?;
                }
            } else if !line.starts_with('#') {
                self.cache_media(&proxy::playlist_url(url, line)?, SEGMENT_LIMIT, false)?;
                self.0.upgrade()?.lock().get_mut(url)?.opening_seconds = duration;
                return Some(());
            }
        }
        None
    }
}

fn rendition_uris(selected: &str) -> impl Iterator<Item = String> + '_ {
    selected.lines().filter_map(|line| {
        if line.starts_with("#EXT-X-MEDIA:") {
            proxy::hls_attr(line, "URI")
        } else if !line.is_empty() && !line.starts_with('#') {
            Some(line.to_owned())
        } else {
            None
        }
    })
}

/// Use the shortest verified opening among the audio and video renditions mpv will request.
pub(super) fn hls_opening_seconds(url: &str, quality: u32, audio: Option<&str>) -> Option<f64> {
    let master = cached(url)?;
    if let Some(seconds) = master.opening_seconds {
        return Some(seconds);
    }
    let selected = proxy::select_master(std::str::from_utf8(&master.bytes).ok()?, quality, audio, false)?;
    let mut seconds = f64::INFINITY;
    for uri in rendition_uris(&selected) {
        seconds = seconds.min(cached(&proxy::playlist_url(url, &uri)?)?.opening_seconds?);
    }
    seconds.is_finite().then_some(seconds)
}

fn preload(id: &str, assets: Weak<Assets>, quality: u32, audio_only: bool) -> Option<()> {
    let download = Download(assets);
    if !download.can_fetch() {
        return None;
    }
    let info = streams::get_streams(id).ok()?;
    if info.is_live {
        return Some(());
    }
    let codec = &crate::store::settings_snapshot().codec_pref;
    let formats = match streams::playback_stream(&info, quality, codec, None, audio_only, false)? {
        streams::PlaybackStream::Hls(url) => {
            let master = download.playlist(url)?;
            let audio = streams::hls_audio_choice(&info, None);
            if let Some(selected) = proxy::select_master(&master, quality, audio.as_deref(), false) {
                for uri in rendition_uris(&selected) {
                    let variant = proxy::playlist_url(url, &uri)?;
                    download.opening_segments(&variant, &download.playlist(&variant)?)?;
                }
                return Some(());
            }
            return download.opening_segments(url, &master);
        }
        streams::PlaybackStream::Muxed(format) => [Some(format), None],
        streams::PlaybackStream::Adaptive(video, audio) => [video, audio],
    };
    for format in formats.into_iter().flatten() {
        if !format.url.contains("googlevideo.com") {
            return None;
        }
        download.cache_media(&format.url, STREAM_PREFIX, true)?;
    }
    Some(())
}
