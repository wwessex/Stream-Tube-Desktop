use crate::features::youtube::is_video_id;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;
use url::Url;

const SPONSORBLOCK: &str = "https://sponsor.ajay.app";
const AUTO_SKIP: &[&str] = &["sponsor", "selfpromo", "interaction"];
const MARK_ONLY: &[&str] = &[
    "intro",
    "outro",
    "preview",
    "filler",
    "music_offtopic",
    "hook",
    "poi_highlight",
];

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Extras {
    pub sponsorblock: bool,
    pub dearrow_titles: bool,
    pub dearrow_thumbnails: bool,
    pub dislike: bool,
}

impl Default for Extras {
    fn default() -> Self {
        Self {
            sponsorblock: true,
            dearrow_titles: false,
            dearrow_thumbnails: false,
            dislike: true,
        }
    }
}

#[derive(Serialize)]
pub struct Segment {
    start: f64,
    end: f64,
    auto_skip: bool,
}

#[derive(Default)]
pub struct Branding {
    pub title: Option<String>,
    pub thumbnail_time: Option<f64>,
}

#[derive(Serialize, Default)]
pub struct PlaybackExtras {
    pub segments: Vec<Segment>,
    pub title: Option<String>,
    pub dislikes: Option<u64>,
}

pub fn playback(id: &str, settings: Extras) -> PlaybackExtras {
    if !is_video_id(id) {
        return PlaybackExtras::default();
    }
    std::thread::scope(|scope| {
        let segments = scope.spawn(|| {
            if settings.sponsorblock {
                sponsor_segments(id)
            } else {
                Vec::new()
            }
        });
        let title = scope.spawn(|| {
            if settings.dearrow_titles {
                branding(id).and_then(|b| b.title)
            } else {
                None
            }
        });
        let dislikes = scope.spawn(|| {
            if settings.dislike {
                dislike_count(id)
            } else {
                None
            }
        });
        PlaybackExtras {
            segments: segments.join().unwrap_or_default(),
            title: title.join().ok().flatten(),
            dislikes: dislikes.join().ok().flatten(),
        }
    })
}

fn prefix(id: &str) -> String {
    let hash = Sha256::digest(id.as_bytes());
    format!("{:02x}{:02x}", hash[0], hash[1])
}

fn sponsor_segments(id: &str) -> Vec<Segment> {
    let mut url = Url::parse(&format!("{SPONSORBLOCK}/api/skipSegments/{}", prefix(id)))
        .expect("fixed SponsorBlock origin and hash");
    let categories = serde_json::to_string(&[AUTO_SKIP, MARK_ONLY].concat())
        .expect("category names are strings");
    url.query_pairs_mut()
        .append_pair("categories", &categories)
        .append_pair("actionTypes", "[\"skip\",\"poi\"]");
    request(&url).map_or_else(Vec::new, |response| parse_segments(&response, id))
}

fn parse_segments(response: &Value, id: &str) -> Vec<Segment> {
    let mut segments: Vec<_> = response
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry.get("videoID").and_then(Value::as_str) == Some(id))
        .filter_map(|entry| entry.get("segments").and_then(Value::as_array))
        .flatten()
        .filter_map(|segment| {
            let category = segment.get("category")?.as_str()?;
            let action = segment.get("actionType")?.as_str()?;
            let range = segment.get("segment")?.as_array()?;
            let start = range.first()?.as_f64()?;
            let end = range.get(1)?.as_f64()?;
            if !start.is_finite() || !end.is_finite() || start < 0.0 || end < start || end > 86400.0
            {
                return None;
            }
            let auto_skip = action == "skip" && end > start && AUTO_SKIP.contains(&category);
            let marker = matches!(action, "skip" | "poi") && MARK_ONLY.contains(&category);
            (auto_skip || marker).then_some(Segment {
                start,
                end,
                auto_skip,
            })
        })
        .take(128)
        .collect();
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    segments
}

pub fn branding(id: &str) -> Option<Branding> {
    if !is_video_id(id) {
        return None;
    }
    let url = Url::parse(&format!("{SPONSORBLOCK}/api/branding/{}", prefix(id))).ok()?;
    let response = request(&url)?;
    let item = response.get(id)?;
    let approved = |entry: &&Value| {
        !entry
            .get("original")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && (entry
                .get("locked")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || entry.get("votes").and_then(Value::as_i64).unwrap_or(0) >= 0)
    };
    let title = item
        .get("titles")
        .and_then(Value::as_array)
        .and_then(|entries| entries.iter().find(approved))
        .and_then(|entry| entry.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(|title| title.chars().take(240).collect());
    let thumbnail_time = item
        .get("thumbnails")
        .and_then(Value::as_array)
        .and_then(|entries| entries.iter().find(approved))
        .and_then(|entry| entry.get("timestamp"))
        .and_then(Value::as_f64)
        .filter(|time| time.is_finite() && (0.0..=86400.0).contains(time));
    Some(Branding {
        title,
        thumbnail_time,
    })
}

pub fn branded_thumbnail(id: &str, timestamp: f64) -> Option<Url> {
    if !is_video_id(id) || !timestamp.is_finite() || !(0.0..=86400.0).contains(&timestamp) {
        return None;
    }
    let mut url = Url::parse("https://dearrow-thumb.ajay.app/api/v1/getThumbnail").ok()?;
    url.query_pairs_mut()
        .append_pair("videoID", id)
        .append_pair("time", &timestamp.to_string());
    Some(url)
}

fn dislike_count(id: &str) -> Option<u64> {
    let mut url = Url::parse("https://returnyoutubedislikeapi.com/votes").ok()?;
    url.query_pairs_mut().append_pair("videoId", id);
    request(&url)?.get("dislikes")?.as_u64()
}

fn request(url: &Url) -> Option<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(4)))
        .max_redirects(0)
        .build()
        .into();
    let bytes = agent
        .get(url.as_str())
        .call()
        .ok()?
        .body_mut()
        .with_config()
        .limit(512 * 1024)
        .read_to_vec()
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sponsor_segments_only_apply_to_the_requested_video() {
        let input = json!([
            {"videoID":"dQw4w9WgXcQ","segments":[
                {"category":"sponsor","actionType":"skip","segment":[25.0,34.0]},
                {"category":"intro","actionType":"skip","segment":[3.0,9.0]},
                {"category":"interaction","actionType":"skip","segment":[50.0,40.0]},
                {"category":"interaction","actionType":"mute","segment":[50.0,55.0]}
            ]},
            {"videoID":"M7lc1UVf-VE","segments":[
                {"category":"sponsor","actionType":"skip","segment":[1.0,10.0]}
            ]}
        ]);
        let segments = parse_segments(&input, "dQw4w9WgXcQ");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].start, 3.0);
        assert!(!segments[0].auto_skip);
        assert_eq!(segments[1].end, 34.0);
        assert!(segments[1].auto_skip);
    }
}
