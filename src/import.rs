//! Import/export of subscriptions, history and playlists.
//! Supports YouTube Takeout (CSV + watch-history.json), NewPipe JSON, FreeTube .db (NDJSON), OPML,
//! and Stream Tube's own full backup JSON.
use crate::api::types::VideoItem;
use crate::store;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Default, Debug)]
pub struct ImportReport {
    pub subscriptions: usize,
    pub history: usize,
    pub playlists: usize,
    pub playlist_videos: usize,
}

impl std::fmt::Display for ImportReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::i18n::tf!(
            "Imported {0} subscriptions, {1} history entries, {2} playlists ({3} videos)",
            self.subscriptions,
            self.history,
            self.playlists,
            self.playlist_videos
        ))
    }
}

fn channel_id_from_url(u: &str) -> Option<String> {
    let i = u.find("/channel/")? + 9;
    let id: String = u[i..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    (id.len() == 24).then_some(id)
}

pub fn import_file(path: &Path) -> Result<ImportReport> {
    let raw = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&raw).trim_start_matches('\u{feff}').to_string();
    let mut rep = ImportReport::default();
    let trimmed = text.trim_start();

    // OPML
    if trimmed.starts_with("<?xml") || trimmed.starts_with("<opml") {
        for part in text.split("<outline").skip(1) {
            let get = |a: &str| -> Option<String> {
                let p = part.find(&format!("{a}=\""))? + a.len() + 2;
                let q = part[p..].find('"')?;
                Some(part[p..p + q].replace("&amp;", "&"))
            };
            if let Some(url) = get("xmlUrl") {
                if let Some(id) = url.split("channel_id=").nth(1).map(|s| s.chars().take(24).collect::<String>()) {
                    let name = get("title").or_else(|| get("text")).unwrap_or_else(|| id.clone());
                    store::subscribe(&id, &name, None);
                    rep.subscriptions += 1;
                }
            }
        }
        return Ok(rep);
    }

    // JSON documents
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            import_json_value(&v, &mut rep)?;
            let backup = v.get("streamtube").is_some() || v.get("ferrotube").is_some();
            if !backup && rep.subscriptions + rep.history + rep.playlists == 0 {
                return Err(anyhow!(crate::i18n::t("Nothing to import in this file").to_string()));
            }
            return Ok(rep);
        }
        // FreeTube .db = newline-delimited JSON
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                import_freetube_line(&v, &mut rep);
            }
        }
        if rep.subscriptions + rep.history + rep.playlists > 0 {
            return Ok(rep);
        }
        return Err(anyhow!("Unrecognized JSON format"));
    }

    // CSV (YouTube Takeout subscriptions.csv: Channel Id,Channel Url,Channel Title)
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default().to_lowercase();
    if header.contains("channel") {
        for line in lines {
            let cols = split_csv(line);
            if cols.is_empty() {
                continue;
            }
            let id = cols
                .iter()
                .find_map(|c| if c.starts_with("UC") && c.len() == 24 { Some(c.clone()) } else { channel_id_from_url(c) });
            if let Some(id) = id {
                let name = cols.get(2).cloned().filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
                store::subscribe(&id, &name, None);
                rep.subscriptions += 1;
            }
        }
        return Ok(rep);
    }
    Err(anyhow!("Unrecognized file format"))
}

fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if q && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => q = !q,
            ',' if !q => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    out
}

fn import_json_value(v: &Value, rep: &mut ImportReport) -> Result<()> {
    // Stream Tube backup (also from when the app was called Ferrotube)
    if v.get("streamtube").is_some() || v.get("ferrotube").is_some() {
        for s in v["subscriptions"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (s["channel_id"].as_str(), s["name"].as_str()) {
                store::subscribe(id, name, s["avatar"].as_str());
                rep.subscriptions += 1;
            }
        }
        for h in v["history"].as_array().into_iter().flatten() {
            if let Ok(vi) = serde_json::from_value::<VideoItem>(h["video"].clone()) {
                store::history_add(&vi);
                store::history_progress(&vi.id, h["progress"].as_f64().unwrap_or(0.0), h["duration"].as_f64().unwrap_or(0.0));
                rep.history += 1;
            }
        }
        for p in v["playlists"].as_array().into_iter().flatten() {
            let name = p["name"].as_str().unwrap_or("Imported");
            let id = existing_or_new_playlist(name);
            rep.playlists += 1;
            for item in p["videos"].as_array().into_iter().flatten() {
                if let Ok(vi) = serde_json::from_value::<VideoItem>(item.clone()) {
                    store::playlist_add(id, &vi);
                    rep.playlist_videos += 1;
                }
            }
        }
        for p in v["profiles"].as_array().into_iter().flatten() {
            let pid = store::create_profile(p["name"].as_str().unwrap_or("Profile"), p["color"].as_u64().unwrap_or(0x3ea6ff) as u32);
            for c in p["channels"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                store::set_profile_channel(pid, c, true);
            }
        }
        if let Some(s) = v.get("settings") {
            if s.is_object() {
                // a backup from another version keeps every setting that still fits
                let st = store::settings_from_value(s.clone());
                store::update_settings(|cur| *cur = st);
            }
        }
        return Ok(());
    }
    // NewPipe subscriptions (entries carry a channel "url"; a one-line FreeTube
    // profiles.db has "id" instead and is handled further down)
    if let Some(subs) = newpipe_subscriptions(v) {
        for s in subs {
            if let Some(id) = s["url"].as_str().and_then(channel_id_from_url) {
                store::subscribe(&id, s["name"].as_str().unwrap_or(&id), None);
                rep.subscriptions += 1;
            }
        }
        return Ok(());
    }
    // YouTube Takeout watch-history.json
    if let Some(arr) = v.as_array() {
        for e in arr {
            let Some(url) = e["titleUrl"].as_str() else { continue };
            let Some(id) = url.split("v=").nth(1).map(|s| s.chars().take(11).collect::<String>()) else { continue };
            let title = e["title"].as_str().unwrap_or("").trim_start_matches("Watched ").to_string();
            let (channel_name, channel_id) = e["subtitles"]
                .get(0)
                .map(|s| (s["name"].as_str().unwrap_or("").to_string(), s["url"].as_str().and_then(channel_id_from_url)))
                .unwrap_or_default();
            let vi = VideoItem {
                thumbnail: format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"),
                id,
                title,
                channel_name,
                channel_id,
                ..Default::default()
            };
            store::history_add(&vi);
            rep.history += 1;
        }
        // single FreeTube playlist export array
        if rep.history == 0 {
            for p in arr {
                import_freetube_line(p, rep);
            }
        }
        return Ok(());
    }
    import_freetube_line(v, rep);
    Ok(())
}

fn newpipe_subscriptions(v: &Value) -> Option<&Vec<Value>> {
    let subs = v.get("subscriptions")?.as_array()?;
    (v.get("_id").is_none() && subs.iter().any(|s| s.get("url").is_some())).then_some(subs)
}

fn existing_or_new_playlist(name: &str) -> i64 {
    store::playlists()
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.id)
        .unwrap_or_else(|| store::playlist_create(name))
}

fn freetube_video(v: &Value) -> Option<VideoItem> {
    let id = v["videoId"].as_str()?.to_string();
    Some(VideoItem {
        thumbnail: format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"),
        title: v["title"].as_str().unwrap_or("").to_string(),
        channel_name: v["author"].as_str().unwrap_or("").to_string(),
        channel_id: v["authorId"].as_str().map(str::to_string),
        duration_secs: v["lengthSeconds"].as_u64().or_else(|| v["lengthSeconds"].as_str().and_then(|s| s.parse().ok())),
        duration_text: v["lengthSeconds"]
            .as_u64()
            .or_else(|| v["lengthSeconds"].as_str().and_then(|s| s.parse().ok()))
            .map(crate::api::json::format_duration),
        id,
        ..Default::default()
    })
}

fn import_freetube_line(v: &Value, rep: &mut ImportReport) {
    // profile with subscriptions
    if let Some(subs) = v["subscriptions"].as_array() {
        let profile_name = v["name"].as_str().unwrap_or("");
        let pid = if v["_id"].as_str() != Some("allChannels") && !profile_name.is_empty() {
            Some(store::create_profile(profile_name, 0x3ea6ff))
        } else {
            None
        };
        for s in subs {
            if let Some(id) = s["id"].as_str() {
                store::subscribe(id, s["name"].as_str().unwrap_or(id), s["thumbnail"].as_str());
                if let Some(pid) = pid {
                    store::set_profile_channel(pid, id, true);
                } else {
                    rep.subscriptions += 1;
                }
            }
        }
        return;
    }
    // playlist
    if let Some(videos) = v["videos"].as_array() {
        let name = v["playlistName"].as_str().unwrap_or("Imported playlist");
        let name = if name == "Favorites" { "Favorites" } else { name };
        let id = existing_or_new_playlist(name);
        rep.playlists += 1;
        for item in videos {
            if let Some(vi) = freetube_video(item) {
                store::playlist_add(id, &vi);
                rep.playlist_videos += 1;
            }
        }
        return;
    }
    // history entry
    if v.get("timeWatched").is_some() {
        if let Some(mut vi) = freetube_video(v) {
            vi.views_text = v["viewCount"].as_u64().map(|c| format!("{} views", crate::api::json::compact_number(c)));
            store::history_add(&vi);
            let progress = v["watchProgress"].as_f64().unwrap_or(0.0);
            store::history_progress(&vi.id, progress, vi.duration_secs.unwrap_or(0) as f64);
            rep.history += 1;
        }
    }
}

pub fn export_backup(path: &Path) -> Result<()> {
    let subs = store::subscriptions();
    let history: Vec<Value> = store::history_list("", usize::MAX)
        .into_iter()
        .map(|h| json!({ "video": h.video, "watched_at": h.watched_at, "progress": h.progress, "duration": h.duration }))
        .collect();
    let playlists: Vec<Value> = store::playlists()
        .into_iter()
        .map(|p| json!({ "name": p.name, "videos": store::playlist_items(p.id) }))
        .collect();
    let profiles: Vec<Value> = store::profiles()
        .into_iter()
        .map(|p| json!({ "name": p.name, "color": p.color, "channels": p.channels }))
        .collect();
    let doc = json!({
        "streamtube": 1,
        "subscriptions": subs,
        "history": history,
        "playlists": playlists,
        "profiles": profiles,
        "settings": *store::settings_snapshot(),
    });
    std::fs::write(path, serde_json::to_vec_pretty(&doc)?)?;
    Ok(())
}

pub fn export_subscriptions(path: &Path) -> Result<()> {
    let subs = store::subscriptions();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let content = match ext.as_str() {
        "csv" => {
            let mut s = String::from("Channel Id,Channel Url,Channel Title\n");
            for c in &subs {
                s.push_str(&format!(
                    "{},http://www.youtube.com/channel/{},\"{}\"\n",
                    c.channel_id,
                    c.channel_id,
                    c.name.replace('"', "\"\"")
                ));
            }
            s
        }
        "opml" | "xml" => {
            let mut s = String::from("<opml version=\"1.1\"><body><outline text=\"YouTube Subscriptions\" title=\"YouTube Subscriptions\">\n");
            for c in &subs {
                let n = c.name.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
                s.push_str(&format!(
                    "<outline text=\"{n}\" title=\"{n}\" type=\"rss\" xmlUrl=\"https://www.youtube.com/feeds/videos.xml?channel_id={}\"/>\n",
                    c.channel_id
                ));
            }
            s.push_str("</outline></body></opml>\n");
            s
        }
        _ => serde_json::to_string_pretty(&json!({
            "app_version": "0.27.0",
            "app_version_int": 1000,
            "subscriptions": subs.iter().map(|c| json!({
                "service_id": 0,
                "url": format!("https://www.youtube.com/channel/{}", c.channel_id),
                "name": c.name
            })).collect::<Vec<_>>()
        }))?,
    };
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_split() {
        assert_eq!(split_csv("a,\"b, c\",d"), vec!["a", "b, c", "d"]);
        assert_eq!(channel_id_from_url("http://www.youtube.com/channel/UCsBjURrPoezykLs9EqgamOA"), Some("UCsBjURrPoezykLs9EqgamOA".into()));
    }

    #[test]
    fn one_line_freetube_profile_is_not_newpipe() {
        let freetube: Value = serde_json::from_str(
            r#"{"_id":"allChannels","name":"All Channels","subscriptions":[{"id":"UCsyntheticChannelAAAA01","name":"A","thumbnail":"https://example.invalid/a.jpg"}]}"#,
        )
        .unwrap();
        assert!(newpipe_subscriptions(&freetube).is_none());
        let newpipe: Value = serde_json::from_str(
            r#"{"app_version":"0.27","subscriptions":[{"service_id":0,"url":"https://www.youtube.com/channel/UCsBjURrPoezykLs9EqgamOA","name":"A"}]}"#,
        )
        .unwrap();
        assert_eq!(newpipe_subscriptions(&newpipe).map(Vec::len), Some(1));
    }
}
