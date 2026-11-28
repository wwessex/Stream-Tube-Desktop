//! Export the selected YouTube caption track without downloading audio or video.
use super::{Job, sanitize};
use crate::api::{
    http, streams,
    types::{CaptionTrack, StreamInfo},
};
use crate::i18n::t;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

struct Cue {
    start: u64,
    duration: Option<u64>,
    text: String,
}

fn cues(payload: &Value) -> Result<Vec<Cue>> {
    let events = payload["events"]
        .as_array()
        .context("Invalid JSON3 caption data")?;
    let mut cues = Vec::new();
    for event in events {
        let (Some(start), Some(segs)) = (event["tStartMs"].as_u64(), event["segs"].as_array())
        else {
            continue;
        };
        // Word fragments already contain their own spaces. Join before normalizing line breaks.
        let text: String = segs.iter().filter_map(|s| s["utf8"].as_str()).collect();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            cues.push(Cue {
                start,
                duration: event["dDurationMs"].as_u64().filter(|d| *d > 0),
                text,
            });
        }
    }
    if cues.is_empty() {
        bail!("{}", t("No transcript available"));
    }
    cues.sort_by_key(|cue| cue.start);
    Ok(cues)
}

fn plain_text(cues: &[Cue]) -> String {
    let mut text = String::new();
    let mut paragraph_start = 0;
    for (i, cue) in cues.iter().enumerate() {
        if i == 0 {
            paragraph_start = cue.start;
        } else {
            let elapsed = cue.start.saturating_sub(paragraph_start);
            let previous = &cues[i - 1].text;
            if elapsed >= 30_000
                || (elapsed >= 15_000
                    && previous
                        .trim_end_matches(['"', '»', '”', ')'])
                        .ends_with(['.', '!', '?', '…', '。', '！', '？']))
            {
                text.push_str("\n\n");
                paragraph_start = cue.start;
            } else {
                text.push(' ');
            }
        }
        text.push_str(&cue.text);
    }
    text.push('\n');
    text
}

fn timestamp(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn srt(cues: &[Cue], auto: bool) -> String {
    let mut out = String::new();
    for (i, cue) in cues.iter().enumerate() {
        let next = cues.get(i + 1).map(|cue| cue.start);
        let end = cue
            .duration
            .map(|d| cue.start.saturating_add(d))
            .or(next)
            .unwrap_or_else(|| cue.start.saturating_add(2000));
        // Automatic captions often overlap to keep the previous line on screen.
        let end = if auto {
            end.min(next.unwrap_or(end))
        } else {
            end
        };
        let _ = writeln!(
            out,
            "{}\n{} --> {}\n{}\n",
            i + 1,
            timestamp(cue.start),
            timestamp(end.max(cue.start.saturating_add(1))),
            cue.text
        );
    }
    out
}

pub fn display_name(track: &CaptionTrack) -> &str {
    let name = track
        .name
        .trim_end_matches(" (auto-generated)")
        .trim_end_matches(" (Original)")
        .trim();
    if name.is_empty() {
        track.lang.trim_end_matches("-orig")
    } else {
        name
    }
}

#[derive(Clone)]
pub struct Choice {
    pub track: CaptionTrack,
    pub original: bool,
}

impl Choice {
    pub fn label(&self) -> String {
        let mut label = display_name(&self.track).to_owned();
        if self.original {
            label.push_str(&format!(" ({})", t("original")));
        }
        if self.track.auto {
            label.push_str(&format!(" ({})", t("auto-generated")));
        }
        label
    }

    pub fn matches(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        display_name(&self.track).to_lowercase().contains(&q)
            || self.track.lang.to_lowercase().contains(&q)
    }
}

fn choices(info: StreamInfo) -> Vec<Choice> {
    let original = info
        .original_language
        .as_deref()
        .or_else(|| {
            info.captions
                .iter()
                .find(|c| c.lang.ends_with("-orig"))
                .map(|c| c.lang.as_str())
        })
        .or_else(|| {
            info.default_caption_index
                .and_then(|i| info.captions.get(i))
                .map(|c| c.lang.as_str())
        })
        .or_else(|| {
            info.captions
                .iter()
                .find(|c| c.auto)
                .map(|c| c.lang.as_str())
        })
        .map(|s| s.trim_end_matches("-orig").to_lowercase());
    let mut urls = std::collections::HashSet::new();
    let mut tracks: Vec<_> = info
        .captions
        .into_iter()
        .filter(|c| {
            c.json3_url
                .as_deref()
                .is_some_and(|url| !url.is_empty() && urls.insert(url.to_owned()))
        })
        .map(|track| {
            let original = original.as_deref().is_some_and(|lang| {
                track
                    .lang
                    .trim_end_matches("-orig")
                    .eq_ignore_ascii_case(lang)
            });
            Choice { track, original }
        })
        .collect();
    tracks.sort_by_cached_key(|c| {
        (
            !c.original,
            display_name(&c.track).to_lowercase(),
            c.track.auto,
        )
    });
    tracks
}

pub fn available(video_id: &str) -> Result<Vec<Choice>> {
    let primary = streams::get_streams(video_id).map(choices);
    if primary.as_ref().is_ok_and(|tracks| !tracks.is_empty()) || streams::ytdlp_path().is_none() {
        return primary;
    }
    streams::ytdlp_streams(video_id).map(choices)
}

// YouTube's timedtext name distinguishes same-language variants across backends.
// Expired or missing tracks must never silently produce a different language or variant.
fn equivalent(a: &CaptionTrack, b: &CaptionTrack) -> bool {
    fn variant(track: &CaptionTrack) -> &str {
        track
            .url
            .split_once('?')
            .and_then(|(_, query)| query.split('&').find_map(|p| p.strip_prefix("name=")))
            .unwrap_or("")
    }
    a.auto == b.auto
        && a.lang
            .trim_end_matches("-orig")
            .eq_ignore_ascii_case(b.lang.trim_end_matches("-orig"))
        && variant(a) == variant(b)
}

fn read_track(track: &CaptionTrack) -> Result<Vec<Cue>> {
    let url = track
        .json3_url
        .as_deref()
        .context("This track has no JSON3 captions")?;
    // Avoid showing the signed caption URL in a failed Downloads row.
    let payload = http::get_json(url).map_err(|_| anyhow!("{}", t("Could not load captions")))?;
    cues(&payload)
}

pub(super) fn export(
    job: &Job,
    dir: &Path,
    selected: Option<&CaptionTrack>,
    cancel: &AtomicBool,
) -> Result<String> {
    let check_cancel = || -> Result<()> {
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        Ok(())
    };
    check_cancel()?;
    let default;
    let track = if let Some(track) = selected {
        track
    } else {
        default = available(&job.video.id)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!(t("No transcript available").to_owned()))?;
        &default.track
    };
    let cues = match read_track(track) {
        Err(_) if streams::ytdlp_path().is_some() => {
            check_cancel()?;
            let refreshed = streams::ytdlp_streams(&job.video.id)?;
            let mut matches = refreshed
                .captions
                .iter()
                .filter(|other| equivalent(track, other) && other.json3_url.is_some());
            let first = matches
                .next()
                .ok_or_else(|| anyhow!(t("Could not load captions").to_owned()))?;
            if matches.next().is_some() {
                bail!("{}", t("Could not load captions"));
            }
            read_track(first)?
        }
        result => result?,
    };
    check_cancel()?;
    let content = if job.format == "srt" {
        srt(&cues, track.auto)
    } else {
        plain_text(&cues)
    };
    let title = sanitize(&job.video.title);
    let mut limit = title.len().min(150);
    while !title.is_char_boundary(limit) {
        limit -= 1;
    }
    let base = format!(
        "[{}] {} ({})",
        sanitize(&job.video.id),
        &title[..limit],
        sanitize(track.lang.trim_end_matches("-orig"))
    );
    for n in 1.. {
        let name = if n == 1 {
            base.clone()
        } else {
            format!("{base} ({n})")
        };
        let path = dir.join(format!("{name}.{}", job.format));
        let mut file = match OpenOptions::new().create_new(true).write(true).open(&path) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            result => result?,
        };
        if let Err(e) = file
            .write_all(content.as_bytes())
            .map_err(anyhow::Error::from)
            .and_then(|_| check_cancel())
        {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        return Ok(path.to_string_lossy().into_owned());
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn track(code: &str, name: &str, auto: bool, uri: &str) -> CaptionTrack {
        CaptionTrack {
            lang: code.into(),
            name: name.into(),
            auto,
            url: uri.into(),
            json3_url: Some(uri.into()),
        }
    }

    #[test]
    fn language_menu_filters_unavailable_and_keeps_original_variants_first() {
        let manual = track("en", "English", false, "manual");
        let auto = track("en-orig", "English (auto-generated)", true, "asr");
        let russian = track("ru", "Русский", false, "ru");
        let info = StreamInfo {
            captions: vec![
                russian.clone(),
                auto,
                CaptionTrack::default(),
                manual.clone(),
                manual,
            ],
            original_language: Some("en".into()),
            ..Default::default()
        };
        let list = choices(info);
        assert_eq!(list.len(), 3);
        assert!(list[0].original && !list[0].track.auto);
        assert!(list[1].original && list[1].track.auto);
        assert_eq!(display_name(&list[1].track), "English");
        assert!(!list[2].original);
        assert!(list[2].matches("  РУС ") && list[2].matches("ru"));
        assert!(!list[2].matches("English"));
        assert!(choices(StreamInfo::default()).is_empty());
        assert_eq!(
            choices(StreamInfo {
                captions: vec![russian],
                ..Default::default()
            })
            .len(),
            1
        );
    }

    #[test]
    fn auto_only_videos_keep_each_available_language() {
        let english = track("en-orig", "English (auto-generated)", true, "english");
        let french = track("fr-orig", "French (auto-generated)", true, "french");
        let mut info = StreamInfo {
            original_language: Some("en".into()),
            ..Default::default()
        };
        info.captions.push(english);
        let single = choices(info.clone());
        assert_eq!(single.len(), 1);
        assert!(single[0].track.auto && single[0].original);

        info.captions.push(french);
        let multiple = choices(info);
        assert_eq!(multiple.len(), 2);
        assert!(multiple.iter().all(|c| c.track.auto));
        assert_eq!(display_name(&multiple[0].track), "English");
        assert_eq!(display_name(&multiple[1].track), "French");
    }

    #[test]
    fn refreshed_tracks_must_match_language_region_kind_and_variant() {
        let chosen = track("en-GB", "English", false, "url?lang=en-GB&name=CC1");
        for (code, auto, url) in [
            ("en", false, "url?name=CC1"),
            ("fr", false, "url?name=CC1"),
            ("en-GB", true, "url?name=CC1"),
            ("en-GB", false, "url?name=CC2"),
        ] {
            assert!(!equivalent(&chosen, &track(code, "English", auto, url)));
        }
        assert!(equivalent(
            &chosen,
            &track("en-GB", "English", false, "new?name=CC1&sig=new")
        ));
        assert!(equivalent(
            &track("ru-orig", "Russian", true, "url"),
            &track("ru", "Russian", true, "new")
        ));
    }

    #[test]
    fn exports_unicode_without_rolling_caption_duplicates() {
        let c = cues(&json!({"events":[
            {"tStartMs":0,"dDurationMs":8000,"segs":[{"utf8":"При"},{"utf8":"вет, "},{"utf8":"мир!"}]},
            {"tStartMs":3490,"segs":[{"utf8":"\n"}]},
            {"tStartMs":3500,"dDurationMs":1000,"segs":[{"utf8":"Hello."}]},
            {"tStartMs":17500,"segs":[{"utf8":"Next sentence."}]}
        ]})).unwrap();
        assert_eq!(plain_text(&c), "Привет, мир! Hello.\n\nNext sentence.\n");
        assert_eq!(
            srt(&c, true),
            "1\n00:00:00,000 --> 00:00:03,500\nПривет, мир!\n\n2\n00:00:03,500 --> 00:00:04,500\nHello.\n\n3\n00:00:17,500 --> 00:00:19,500\nNext sentence.\n\n"
        );
        assert!(srt(&c, false).contains("00:00:08,000"));
        assert!(cues(&json!({"events":[]})).is_err());
    }
}
