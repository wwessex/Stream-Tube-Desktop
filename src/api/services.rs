//! Third-party services: SponsorBlock, DeArrow, Return YouTube Dislike, and RSS feeds.
use super::http;
use super::json::*;
use super::types::*;
use anyhow::Result;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

fn sha_prefix(id: &str) -> String {
    let h = Sha256::digest(id.as_bytes());
    h.iter().take(2).map(|b| format!("{b:02x}")).collect()
}

// ───────────────────────── SponsorBlock ─────────────────────────

pub const SB_CATEGORIES: &[(&str, &str, u32)] = &[
    ("sponsor", "Sponsor", 0x00d400),
    ("selfpromo", "Unpaid/Self Promotion", 0xffff00),
    ("interaction", "Interaction Reminder", 0xcc00ff),
    ("intro", "Intermission/Intro", 0x00ffff),
    ("outro", "Endcards/Credits", 0x0202ed),
    ("preview", "Preview/Recap", 0x008fd6),
    ("hook", "Hook/Greetings", 0x395699),
    ("music_offtopic", "Music: Non-Music", 0xff9900),
    ("filler", "Filler Tangent", 0x7300ff),
    ("poi_highlight", "Highlight", 0xff1684),
    ("exclusive_access", "Exclusive Access", 0x008a5c),
];

#[derive(Clone, Debug)]
pub struct Segment {
    pub category: String,
    pub action: String,
    pub start: f64,
    pub end: f64,
    pub uuid: String,
}

pub fn sponsor_segments(video_id: &str, server: &str, categories: &[String]) -> Result<Vec<Segment>> {
    if categories.is_empty() {
        return Ok(Vec::new());
    }
    let cats = format!(
        "[{}]",
        categories.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",")
    );
    let url = format!(
        "{}/api/skipSegments/{}?categories={}&actionTypes=%5B%22skip%22,%22mute%22,%22poi%22,%22full%22%5D",
        server.trim_end_matches('/'),
        sha_prefix(video_id),
        http::urlencode(&cats)
    );
    let (status, v) = http::get_json_status(&url)?;
    if status == 404 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in v.as_array().into_iter().flatten() {
        if entry.get("videoID").and_then(Value::as_str) != Some(video_id) {
            continue;
        }
        for seg in entry.get("segments").and_then(Value::as_array).into_iter().flatten() {
            let arr = seg.get("segment").and_then(Value::as_array);
            let (Some(a), Some(b)) = (
                arr.and_then(|a| a.first()).and_then(Value::as_f64),
                arr.and_then(|a| a.get(1)).and_then(Value::as_f64),
            ) else {
                continue;
            };
            out.push(Segment {
                category: s(seg, "/category").unwrap_or_default(),
                action: s(seg, "/actionType").unwrap_or_else(|| "skip".into()),
                start: a,
                end: b,
                uuid: s(seg, "/UUID").unwrap_or_default(),
            });
        }
    }
    out.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
    Ok(out)
}

pub fn sponsor_category_color(cat: &str) -> u32 {
    SB_CATEGORIES.iter().find(|c| c.0 == cat).map(|c| c.2).unwrap_or(0xaaaaaa)
}

pub fn sponsor_category_name(cat: &str) -> &'static str {
    SB_CATEGORIES.iter().find(|c| c.0 == cat).map(|c| c.1).unwrap_or("Segment")
}

/// Report that a segment was skipped (helps SponsorBlock stats, like SmartTube does).
pub fn sponsor_viewed(server: &str, uuid: &str) {
    let url = format!("{}/api/viewedVideoSponsorTime?UUID={}", server.trim_end_matches('/'), uuid);
    let _ = http::post_json(&url, &Value::Null, &[]);
}

// ───────────────────────── DeArrow ─────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Branding {
    pub title: Option<String>,
    pub thumbnail_time: Option<f64>,
}

fn branding_cache() -> &'static Mutex<HashMap<String, Option<Branding>>> {
    static C: OnceLock<Mutex<HashMap<String, Option<Branding>>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

pub fn branding_cached(video_id: &str) -> Option<Option<Branding>> {
    branding_cache().lock().get(video_id).cloned()
}

pub fn dearrow_branding(video_id: &str) -> Option<Branding> {
    if let Some(b) = branding_cached(video_id) {
        return b;
    }
    let url = format!("https://sponsor.ajay.app/api/branding/{}", sha_prefix(video_id));
    let res = http::get_json(&url).ok().and_then(|v| {
        let e = v.get(video_id)?;
        let title = e
            .get("titles")
            .and_then(Value::as_array)
            .and_then(|t| {
                t.iter().find(|x| {
                    !x.get("original").and_then(Value::as_bool).unwrap_or(false)
                        && (x.get("locked").and_then(Value::as_bool).unwrap_or(false)
                            || x.get("votes").and_then(Value::as_i64).unwrap_or(0) >= 0)
                })
            })
            .and_then(|x| x.get("title").and_then(Value::as_str))
            .map(|t| t.replace('>', "").trim().to_string());
        let thumbnail_time = e
            .get("thumbnails")
            .and_then(Value::as_array)
            .and_then(|t| t.iter().find(|x| !x.get("original").and_then(Value::as_bool).unwrap_or(false)))
            .and_then(|x| x.get("timestamp").and_then(Value::as_f64));
        Some(Branding { title, thumbnail_time })
    });
    {
        let mut c = branding_cache().lock();
        // one entry per video ever shown; start over now and then so a long
        // session doesn't keep growing
        if c.len() > 4000 {
            c.clear();
        }
        c.insert(video_id.to_string(), res.clone());
    }
    res
}

pub fn dearrow_thumbnail_url(video_id: &str, time: f64) -> String {
    format!("https://dearrow-thumb.ajay.app/api/v1/getThumbnail?videoID={video_id}&time={time}")
}

// ───────────────────────── Return YouTube Dislike ─────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Votes {
    pub likes: u64,
    pub dislikes: u64,
    pub views: u64,
    pub rating: f64,
}

pub fn ryd_votes(video_id: &str) -> Result<Votes> {
    let v = http::get_json(&format!("https://returnyoutubedislikeapi.com/votes?videoId={video_id}"))?;
    // a video RYD hasn't seen yet comes back with every count at zero: no data,
    // not "0 likes", so YouTube's own count is shown instead
    let count = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    if count("likes") == 0 && count("dislikes") == 0 && count("viewCount") == 0 {
        anyhow::bail!("Return YouTube Dislike has no counts for this video yet");
    }
    Ok(Votes {
        likes: v.get("likes").and_then(Value::as_u64).unwrap_or(0),
        dislikes: v.get("dislikes").and_then(Value::as_u64).unwrap_or(0),
        views: v.get("viewCount").and_then(Value::as_u64).unwrap_or(0),
        rating: v.get("rating").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

// ───────────────────────── RSS ─────────────────────────

/// Fetches a channel's latest uploads via RSS (fast, light, 15 items).
pub fn channel_rss(channel_id: &str) -> Result<(String, Vec<VideoItem>)> {
    let xml = http::get_text(&format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}"))?;
    let tag = |s: &str, t: &str| -> Option<String> {
        let start = s.find(&format!("<{t}"))?;
        let after = &s[start..];
        let open_end = after.find('>')? + 1;
        let close = after.find(&format!("</{t}>"))?;
        Some(unescape(&after[open_end..close]))
    };
    let attr = |s: &str, t: &str, a: &str| -> Option<String> {
        let start = s.find(&format!("<{t} "))?;
        let after = &s[start..];
        let end = after.find('>')?;
        let el = &after[..end];
        let p = el.find(&format!("{a}=\""))? + a.len() + 2;
        let q = el[p..].find('"')?;
        Some(unescape(&el[p..p + q]))
    };
    let channel_name = xml
        .split("<entry>")
        .next()
        .and_then(|h| tag(h, "title"))
        .unwrap_or_default();
    let mut out = Vec::new();
    for entry in xml.split("<entry>").skip(1) {
        let Some(id) = tag(entry, "yt:videoId") else { continue };
        let published = tag(entry, "published").and_then(|p| parse_iso8601(&p));
        let views = attr(entry, "media:statistics", "views").and_then(|v| v.parse::<u64>().ok());
        let link = attr(entry, "link", "href").unwrap_or_default();
        out.push(VideoItem {
            title: tag(entry, "title").unwrap_or_default(),
            channel_name: channel_name.clone(),
            channel_id: Some(channel_id.to_string()),
            thumbnail: format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"),
            views_text: views.map(|v| format!("{} views", compact_number(v))),
            published_text: published.map(relative_from_ts),
            published_ts: published,
            is_short: link.contains("/shorts/"),
            id,
            ..Default::default()
        });
    }
    Ok((channel_name, out))
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// "2024-01-02T03:04:05+00:00" -> unix ts
pub fn parse_iso8601(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore]
    fn live_services() {
        let segs = sponsor_segments("dQw4w9WgXcQ", "https://sponsor.ajay.app", &["sponsor".into(), "intro".into(), "outro".into(), "music_offtopic".into()]).unwrap();
        println!("segments {segs:?}");
        println!("branding {:?}", dearrow_branding("dQw4w9WgXcQ"));
        println!("votes {:?}", ryd_votes("dQw4w9WgXcQ").unwrap());
        let (name, vids) = channel_rss("UCsBjURrPoezykLs9EqgamOA").unwrap();
        println!("{name} {} {:?}", vids.len(), vids.first());
        assert!(!vids.is_empty());
    }
}

/// Votes on a SponsorBlock segment (1 = upvote, 0 = downvote) with the user's private ID.
pub fn sponsor_vote(server: &str, uuid: &str, user_id: &str, up: bool) -> Result<()> {
    let url = format!(
        "{}/api/voteOnSponsorTime?UUID={}&userID={}&type={}",
        server.trim_end_matches('/'),
        super::http::urlencode(uuid),
        super::http::urlencode(user_id),
        if up { 1 } else { 0 }
    );
    let (status, _) = super::http::post_json_status(&url, &serde_json::json!({}), &[])?;
    if status >= 400 {
        anyhow::bail!("server answered {status}");
    }
    Ok(())
}

/// A new random SponsorBlock private user ID (36 alphanumerics, like the browser extension).
pub fn new_sb_user_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(36);
    let mut seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    while out.len() < 36 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(seed);
        seed = h.finish();
        out.push(CHARS[(seed % CHARS.len() as u64) as usize] as char);
    }
    out
}

/// A SponsorBlock contributor's public stats (`/api/userInfo`).
#[derive(Clone, Debug, Default)]
pub struct SbUserInfo {
    pub user_name: String,
    pub segments: u64,
    pub views: u64,
    pub minutes_saved: f64,
    pub reputation: f64,
    pub warnings: u64,
    pub vip: bool,
}

pub fn sponsor_user_info(server: &str, private_id: &str) -> Result<SbUserInfo> {
    let url = format!("{}/api/userInfo?userID={}", server.trim_end_matches('/'), http::urlencode(private_id));
    let (status, v) = http::get_json_status(&url)?;
    if status == 404 {
        // a fresh ID that never submitted anything
        return Ok(SbUserInfo::default());
    }
    if status >= 400 {
        anyhow::bail!("server answered {status}");
    }
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let name = v.get("userName").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let public = v.get("userID").and_then(|x| x.as_str()).unwrap_or("");
    Ok(SbUserInfo {
        // without a username set, the server echoes the public ID
        user_name: if name == public { String::new() } else { name },
        segments: num("segmentCount") as u64,
        views: num("viewCount") as u64,
        minutes_saved: num("minutesSaved"),
        reputation: num("reputation"),
        warnings: num("warnings") as u64,
        vip: v.get("vip").and_then(|x| x.as_bool()).unwrap_or(false),
    })
}

/// "3 days 4 hours", "5 hours 12 minutes", "42 minutes"
pub fn format_saved_minutes(m: f64) -> String {
    let total = m.max(0.0).round() as u64;
    let (d, h, min) = (total / 1440, (total % 1440) / 60, total % 60);
    let unit = |n: u64, one, many| crate::i18n::format(if n == 1 { one } else { many }, &[&n]);
    if d > 0 {
        format!(
            "{} {}",
            unit(d, "{0} day", "{0} days"),
            unit(h, "{0} hour", "{0} hours")
        )
    } else if h > 0 {
        format!(
            "{} {}",
            unit(h, "{0} hour", "{0} hours"),
            unit(min, "{0} minute", "{0} minutes")
        )
    } else {
        unit(min, "{0} minute", "{0} minutes")
    }
}

/// 1234 -> "1.2K"
pub fn compact_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3).replace(".0K", "K"),
        _ => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
    }
}
