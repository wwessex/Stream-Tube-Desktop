//! Innertube (YouTube internal API) client: search, browse, watch-next, comments, resolve.
use super::http;
use super::json::*;
use super::types::*;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};

const BASE: &str = "https://www.youtube.com/youtubei/v1";
pub const WEB_VERSION: &str = "2.20251120.01.00";

fn region() -> (String, String) {
    let s = crate::store::settings_snapshot();
    (s.language.clone(), s.region.clone())
}

fn web_context() -> Value {
    let (hl, gl) = region();
    json!({ "client": { "clientName": "WEB", "clientVersion": WEB_VERSION, "hl": hl, "gl": gl } })
}

pub fn call(endpoint: &str, body: Value) -> Result<Value> {
    call_with_context(endpoint, body, web_context())
}

fn call_with_context(endpoint: &str, mut body: Value, context: Value) -> Result<Value> {
    body["context"] = context;
    http::post_json(
        &format!("{BASE}/{endpoint}?prettyPrint=false"),
        &body,
        &[
            ("X-YouTube-Client-Name", "1"),
            ("X-YouTube-Client-Version", WEB_VERSION),
            ("Origin", "https://www.youtube.com"),
        ],
    )
}

// ───────────────────────── item parsing ─────────────────────────

/// Walks any Innertube response and collects every recognizable item plus the last continuation token.
pub fn collect_items(v: &Value) -> Page {
    collect_items_with_video_nodes(v, |_, _| {})
}

pub(super) fn collect_items_with_video_nodes(v: &Value, mut node: impl FnMut(&VideoItem, &Value)) -> Page {
    let mut page = Page::default();
    walk_items(v, &mut page, &mut node);
    page
}

fn walk_items(v: &Value, page: &mut Page, node: &mut impl FnMut(&VideoItem, &Value)) {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                if AD_RENDERERS.contains(&k.as_str()) {
                    continue;
                }
                match k.as_str() {
                    "videoRenderer" | "compactVideoRenderer" | "gridVideoRenderer" | "playlistVideoRenderer"
                    | "playlistPanelVideoRenderer" | "videoWithContextRenderer" => {
                        if let Some(vi) = parse_video_renderer(c) {
                            node(&vi, c);
                            page.items.push(Item::Video(vi));
                        }
                    }
                    "reelItemRenderer" => {
                        if let Some(vi) = parse_reel_item(c) {
                            node(&vi, c);
                            page.items.push(Item::Video(vi));
                        }
                    }
                    "shortsLockupViewModel" => {
                        if let Some(vi) = parse_shorts_lockup(c) {
                            node(&vi, c);
                            page.items.push(Item::Video(vi));
                        }
                    }
                    "tileRenderer" => {
                        if let Some(v) = parse_tile(c) {
                            node(&v, c);
                            page.items.push(Item::Video(v));
                        }
                    }
                    "nextContinuationData" => {
                        if let Some(t) = c.get("continuation").and_then(Value::as_str) {
                            page.continuation = Some(t.to_string());
                        }
                    }
                    "lockupViewModel" => {
                        if let Some(it) = parse_lockup(c) {
                            if let Item::Video(video) = &it {
                                node(video, c);
                            }
                            page.items.push(it);
                        }
                    }
                    "channelRenderer" | "gridChannelRenderer" => {
                        if let Some(ch) = parse_channel_renderer(c) {
                            page.items.push(Item::Channel(ch));
                        }
                    }
                    "playlistRenderer" | "gridPlaylistRenderer" | "radioRenderer" | "compactPlaylistRenderer" | "gridShowRenderer" => {
                        if let Some(p) = parse_playlist_renderer(c) {
                            page.items.push(Item::Playlist(p));
                        }
                    }
                    "continuationItemRenderer" => {
                        if let Some(t) = continuation_token(c) {
                            page.continuation = Some(t);
                        }
                    }
                    // the newer form (playlists since late 2026):
                    // { trigger, continuationCommand: { innertubeCommand: { continuationCommand: { token } } } }
                    "continuationItemViewModel" => {
                        let mut tokens = Vec::new();
                        if let Some(cmd) = c.get("continuationCommand") {
                            find_all(cmd, "token", &mut tokens);
                        }
                        if let Some(t) = tokens.iter().find_map(|x| x.as_str()) {
                            page.continuation = Some(t.to_string());
                        }
                    }
                    // Skip noisy sections that duplicate items or are UI chrome.
                    "frameworkUpdates" | "topbar" | "header" | "engagementPanels" | "playerOverlays" | "menu"
                    | "endScreen" | "onResponseReceivedCommands" => {}
                    _ => walk_items(c, page, node),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| walk_items(c, page, node)),
        _ => {}
    }
}

pub fn continuation_token(c: &Value) -> Option<String> {
    c.pointer("/continuationEndpoint/continuationCommand/token")
        .or_else(|| c.pointer("/button/buttonRenderer/command/continuationCommand/token"))
        .or_else(|| find(c, "continuationCommand").and_then(|x| x.get("token")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// TV client tiles (home / subscriptions feeds of a signed-in account).
pub(crate) fn parse_tile(t: &Value) -> Option<VideoItem> {
    if is_ad(t) {
        return None;
    }
    let ctype = t.get("contentType").and_then(Value::as_str).unwrap_or("");
    if !ctype.is_empty() && ctype != "TILE_CONTENT_TYPE_VIDEO" {
        return None;
    }
    let id = t
        .get("contentId")
        .and_then(Value::as_str)
        .or_else(|| t.pointer("/onSelectCommand/watchEndpoint/videoId").and_then(Value::as_str))?
        .to_string();
    let meta = t.pointer("/metadata/tileMetadataRenderer")?;
    let title = meta.get("title").map(text).unwrap_or_default();
    let images = t.pointer("/header/tileHeaderRenderer/thumbnail");
    let thumbnail = images.and_then(best_thumb).unwrap_or_else(|| default_thumb(&id));
    let mut overlays = Vec::new();
    find_all(t, "thumbnailOverlayTimeStatusRenderer", &mut overlays);
    let badge = overlays.first().and_then(|o| o.get("text")).map(text);
    let is_live = overlays.iter().any(|o| o.get("style").and_then(Value::as_str) == Some("LIVE"));
    let mut parts: Vec<String> = Vec::new();
    let mut channel_id = None;
    for line in meta.get("lines").and_then(Value::as_array).into_iter().flatten() {
        for item in line.pointer("/lineRenderer/items").and_then(Value::as_array).into_iter().flatten() {
            let r = item.get("lineItemRenderer").unwrap_or(item);
            let tx = r.get("text").map(text).unwrap_or_default();
            if channel_id.is_none() {
                channel_id = find(r, "browseEndpoint").and_then(|b| b.get("browseId")).and_then(Value::as_str).map(str::to_string);
            }
            let tx = tx.trim().trim_matches('•').trim().to_string();
            if !tx.is_empty() {
                parts.push(tx);
            }
        }
    }
    if channel_id.is_none() {
        let mut eps = Vec::new();
        find_all(t, "browseEndpoint", &mut eps);
        channel_id = eps.iter().filter_map(|b| b.get("browseId").and_then(Value::as_str)).find(|b| b.starts_with("UC")).map(str::to_string);
    }
    // in any language: the date is the part that reads as a relative time, the
    // views the other part that starts with a number, and the channel comes first
    let published = parts
        .iter()
        .find(|p| p.to_lowercase().contains("ago") || p.to_lowercase().contains("streamed"))
        .or_else(|| parts.iter().find(|p| crate::api::json::parse_relative_time(p).is_some()))
        .cloned();
    let views = parts
        .iter()
        .find(|p| p.to_lowercase().contains("view") || p.to_lowercase().contains("watching"))
        .or_else(|| {
            parts.iter().skip(1).find(|p| Some(*p) != published.as_ref() && p.chars().next().is_some_and(|c| c.is_ascii_digit()))
        })
        .cloned();
    let channel_name = parts.iter().find(|p| Some(*p) != views.as_ref() && Some(*p) != published.as_ref()).cloned().unwrap_or_default();
    let duration_secs = badge.as_deref().and_then(parse_duration);
    // A show (podcast, series) is a tile too: it opens a playlist, its badge counts
    // episodes instead of giving a length, its title is the show's name and its two lines
    // are the latest episode and the channel. Show that episode like a video, with the
    // show and its size where the views would be.
    // Two kinds: a show page ("VL" + a playlist id), or the latest episode played with
    // the show's own list ("OL…"), which is what YouTube runs for recommended shows.
    let show_playlist = t
        .pointer("/onSelectCommand/browseEndpoint/browseId")
        .and_then(Value::as_str)
        .and_then(|b| b.strip_prefix("VL"))
        .or_else(|| t.pointer("/onSelectCommand/watchEndpoint/playlistId").and_then(Value::as_str).filter(|p| p.starts_with("OL")))
        .map(str::to_string);
    let is_show = show_playlist.is_some();
    let (title, channel_name, views) = if is_show && parts.len() == 2 && views.is_none() && published.is_none() {
        let size = badge.clone().filter(|_| duration_secs.is_none());
        let note = [Some(title), size].into_iter().flatten().collect::<Vec<_>>().join(" • ");
        (parts[0].clone(), parts[1].clone(), Some(note))
    } else {
        (title, channel_name, views)
    };
    Some(VideoItem {
        id,
        title,
        thumbnail,
        compact_thumbnail: images.and_then(compact_thumb),
        channel_name,
        channel_id,
        duration_text: badge.filter(|_| duration_secs.is_some()),
        duration_secs,
        views_text: views,
        published_text: published,
        is_live,
        playlist_id: show_playlist,
        ..Default::default()
    })
}

fn parse_video_renderer(r: &Value) -> Option<VideoItem> {
    if is_ad(r) {
        return None;
    }
    let id = r.get("videoId")?.as_str()?.to_string();
    let title = opt_text(r.get("title")).or_else(|| opt_text(r.get("headline"))).unwrap_or_default();
    let owner = r
        .get("ownerText")
        .or_else(|| r.get("longBylineText"))
        .or_else(|| r.get("shortBylineText"));
    let channel_name = owner.map(text).unwrap_or_default();
    let channel_id = owner
        .and_then(|o| o.pointer("/runs/0/navigationEndpoint/browseEndpoint/browseId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut duration_text = opt_text(r.get("lengthText"));
    if duration_text.is_none() {
        // overlay time status
        let mut ov = Vec::new();
        find_all(r, "thumbnailOverlayTimeStatusRenderer", &mut ov);
        duration_text = ov.first().and_then(|o| opt_text(o.get("text")));
    }
    let badges = r.get("badges").map(|b| b.to_string()).unwrap_or_default();
    let is_live = badges.contains("LIVE_NOW")
        || badges.contains("\"LIVE\"")
        || duration_text.as_deref().map(|d| d.eq_ignore_ascii_case("live")).unwrap_or(false);
    let is_upcoming = r.get("upcomingEventData").is_some();
    let duration_secs = duration_text.as_deref().and_then(parse_duration);
    let published_text = opt_text(r.get("publishedTimeText"));
    let views_text = opt_text(r.get("shortViewCountText")).or_else(|| opt_text(r.get("viewCountText")));
    let snippet = r
        .pointer("/detailedMetadataSnippets/0/snippetText")
        .map(text)
        .or_else(|| opt_text(r.get("descriptionSnippet")));
    let avatar = r
        .pointer("/channelThumbnailSupportedRenderers/channelThumbnailWithLinkRenderer/thumbnail")
        .or_else(|| r.get("channelThumbnail"))
        .and_then(small_thumb);
    let is_short = r
        .pointer("/navigationEndpoint/reelWatchEndpoint")
        .is_some();
    Some(VideoItem {
        thumbnail: r.get("thumbnail").and_then(best_thumb).unwrap_or_else(|| default_thumb(&id)),
        compact_thumbnail: r.get("thumbnail").and_then(compact_thumb),
        published_ts: published_text.as_deref().and_then(parse_relative_time),
        id,
        title,
        channel_name,
        channel_id,
        channel_avatar: avatar,
        duration_text: if is_live { None } else { duration_text },
        duration_secs,
        views_text,
        published_text,
        description_snippet: snippet.filter(|s| !s.is_empty()),
        is_live,
        is_short,
        is_upcoming,
        playlist_id: None,
    })
}

pub fn default_thumb(id: &str) -> String {
    format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg")
}

fn parse_reel_item(r: &Value) -> Option<VideoItem> {
    let id = r.get("videoId")?.as_str()?.to_string();
    Some(VideoItem {
        title: opt_text(r.get("headline")).unwrap_or_default(),
        thumbnail: format!("https://i.ytimg.com/vi/{id}/oar2.jpg"),
        views_text: opt_text(r.get("viewCountText")),
        id,
        is_short: true,
        ..Default::default()
    })
}

fn parse_shorts_lockup(r: &Value) -> Option<VideoItem> {
    let id = r
        .pointer("/onTap/innertubeCommand/reelWatchEndpoint/videoId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            r.get("entityId")
                .and_then(Value::as_str)
                .map(|e| e.trim_start_matches("shorts-shelf-item-").to_string())
        })?;
    let title = r
        .pointer("/overlayMetadata/primaryText/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let views = s(r, "/overlayMetadata/secondaryText/content");
    Some(VideoItem {
        thumbnail: r
            .pointer("/thumbnail")
            .and_then(best_thumb)
            .unwrap_or_else(|| format!("https://i.ytimg.com/vi/{id}/oar2.jpg")),
        id,
        title,
        views_text: views,
        is_short: true,
        ..Default::default()
    })
}

fn parse_lockup(l: &Value) -> Option<Item> {
    if is_ad(l) {
        return None;
    }
    let id = l.get("contentId")?.as_str()?.to_string();
    let ctype = l.get("contentType").and_then(Value::as_str).unwrap_or("");
    let meta = l.pointer("/metadata/lockupMetadataViewModel")?;
    let title = meta.pointer("/title/content").and_then(Value::as_str).unwrap_or("").to_string();
    let thumb_vm = l
        .pointer("/contentImage/thumbnailViewModel")
        .or_else(|| l.pointer("/contentImage/collectionThumbnailViewModel/primaryThumbnail/thumbnailViewModel"));
    let images = thumb_vm.and_then(|t| t.get("image"));
    let thumbnail = images.and_then(best_thumb);
    // badges (duration / LIVE / "N videos")
    let mut badge_texts = Vec::new();
    if let Some(t) = thumb_vm {
        let mut found = Vec::new();
        find_all(t, "thumbnailBadgeViewModel", &mut found);
        for b in found {
            if let Some(tx) = b.get("text").and_then(Value::as_str) {
                badge_texts.push(tx.to_string());
            }
        }
    }
    // metadata rows
    let mut channel_name = None;
    let mut channel_id = None;
    let mut views = None;
    let mut published = None;
    let mut others = Vec::new();
    if let Some(rows) = meta
        .pointer("/metadata/contentMetadataViewModel/metadataRows")
        .and_then(Value::as_array)
    {
        for row in rows {
            for part in row.get("metadataParts").and_then(Value::as_array).into_iter().flatten() {
                let Some(tx) = part.pointer("/text/content").and_then(Value::as_str) else { continue };
                let tx = tx.trim().to_string();
                if tx.is_empty() {
                    continue;
                }
                let browse = find(part, "browseEndpoint")
                    .and_then(|b| b.get("browseId"))
                    .and_then(Value::as_str);
                let low = tx.to_lowercase();
                if let Some(b) = browse.filter(|b| b.starts_with("UC")) {
                    channel_name = Some(tx);
                    channel_id = Some(b.to_string());
                } else if low.contains("view") || low.contains("watching") || low.contains("waiting") {
                    views = Some(tx);
                } else if low.contains("ago") || low.contains("streamed") || low.contains("premiere") || low.contains("scheduled") {
                    published = Some(tx);
                } else {
                    others.push(tx);
                }
            }
        }
    }
    if channel_id.is_none() {
        channel_id = meta
            .pointer("/image/decoratedAvatarViewModel/rendererContext/commandContext/onTap/innertubeCommand/browseEndpoint/browseId")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let avatar = meta
        .pointer("/image/decoratedAvatarViewModel/avatar/avatarViewModel/image")
        .and_then(small_thumb);
    if channel_id.is_none() {
        // TV lockups keep the channel link in the long-press menu
        let mut eps = Vec::new();
        find_all(l, "browseEndpoint", &mut eps);
        channel_id = eps
            .iter()
            .filter_map(|b| b.get("browseId").and_then(Value::as_str))
            .find(|b| b.starts_with("UC"))
            .map(str::to_string);
    }
    if channel_name.is_none() {
        channel_name = others.first().cloned();
    }
    match ctype {
        "LOCKUP_CONTENT_TYPE_VIDEO" | "LOCKUP_CONTENT_TYPE_MUSIC" | "LOCKUP_CONTENT_TYPE_SHORT" => {
            let is_short = ctype == "LOCKUP_CONTENT_TYPE_SHORT";
            let is_live = badge_texts.iter().any(|b| b.eq_ignore_ascii_case("live"));
            let is_upcoming = badge_texts.iter().any(|b| b.eq_ignore_ascii_case("upcoming"));
            let duration_text = badge_texts.iter().find(|b| parse_duration(b).is_some()).cloned();
            let published = published.or_else(|| others.iter().find(|o| parse_relative_time(o).is_some()).cloned());
            let views = views.or_else(|| {
                // compact form "1B" with PLAY_ARROW icon, or another language: no "views" word;
                // pick the numeric-ish part that isn't the date or the channel
                others
                    .iter()
                    .filter(|o| Some(*o) != published.as_ref() && Some(*o) != channel_name.as_ref())
                    .find(|o| o.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
                    .cloned()
            });
            let views = views.map(|v| {
                if v.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',' || "KMB".contains(c)) {
                    format!("{v} views")
                } else {
                    v
                }
            });
            Some(Item::Video(VideoItem {
                thumbnail: thumbnail.unwrap_or_else(|| default_thumb(&id)),
                compact_thumbnail: images.and_then(compact_thumb),
                duration_secs: duration_text.as_deref().and_then(parse_duration),
                duration_text,
                published_ts: published.as_deref().and_then(parse_relative_time),
                published_text: published,
                views_text: views,
                channel_name: channel_name.unwrap_or_default(),
                channel_id,
                channel_avatar: avatar,
                title,
                id,
                is_live,
                is_upcoming,
                is_short,
                ..Default::default()
            }))
        }
        "LOCKUP_CONTENT_TYPE_PLAYLIST" | "LOCKUP_CONTENT_TYPE_PODCAST" | "LOCKUP_CONTENT_TYPE_ALBUM" | "LOCKUP_CONTENT_TYPE_COURSE" | "LOCKUP_CONTENT_TYPE_SHOW" => {
            let first_video_id = find(l, "watchEndpoint")
                .and_then(|w| w.get("videoId"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(Item::Playlist(PlaylistItem {
                id,
                title,
                thumbnail,
                channel_name,
                channel_id,
                video_count_text: badge_texts.first().cloned(),
                first_video_id,
            }))
        }
        "LOCKUP_CONTENT_TYPE_CHANNEL" => Some(Item::Channel(ChannelItem {
            id,
            name: title,
            avatar: thumbnail,
            ..Default::default()
        })),
        _ => None,
    }
}

fn parse_channel_renderer(r: &Value) -> Option<ChannelItem> {
    let id = r.get("channelId")?.as_str()?.to_string();
    // In search results "subscriberCountText" often holds the handle and "videoCountText" holds subscribers.
    let a = opt_text(r.get("subscriberCountText"));
    let b = opt_text(r.get("videoCountText"));
    let (handle, subs) = match (&a, &b) {
        (Some(x), y) if x.starts_with('@') => (Some(x.clone()), y.clone()),
        _ => (None, a.clone()),
    };
    Some(ChannelItem {
        id,
        name: opt_text(r.get("title")).unwrap_or_default(),
        avatar: r.get("thumbnail").and_then(best_thumb),
        handle,
        subscribers_text: subs,
        video_count_text: if a.as_deref().map(|x| x.starts_with('@')).unwrap_or(false) { None } else { b },
        description: opt_text(r.get("descriptionSnippet")),
    })
}

fn parse_playlist_renderer(r: &Value) -> Option<PlaylistItem> {
    let id = r.get("playlistId").and_then(Value::as_str).or_else(|| {
        r.pointer("/navigationEndpoint/browseEndpoint/browseId")?
            .as_str()?
            .strip_prefix("VL")
            .filter(|id| !id.is_empty())
    })?.to_string();
    let thumbnail = r
        .pointer("/thumbnails/0")
        .and_then(best_thumb)
        .or_else(|| r.get("thumbnail").and_then(best_thumb))
        .or_else(|| r.pointer("/thumbnailRenderer/playlistVideoThumbnailRenderer/thumbnail").and_then(best_thumb))
        .or_else(|| r.pointer("/thumbnailRenderer/showCustomThumbnailRenderer/thumbnail").and_then(best_thumb));
    let owner = r.get("longBylineText").or_else(|| r.get("shortBylineText"));
    Some(PlaylistItem {
        id,
        title: opt_text(r.get("title")).unwrap_or_default(),
        thumbnail,
        channel_name: owner.map(text),
        channel_id: owner
            .and_then(|o| o.pointer("/runs/0/navigationEndpoint/browseEndpoint/browseId"))
            .and_then(Value::as_str)
            .map(str::to_string),
        video_count_text: opt_text(r.get("videoCountText"))
            .or_else(|| opt_text(r.get("videoCountShortText")))
            .or_else(|| opt_text(find(r.get("thumbnailOverlays")?, "thumbnailOverlayBottomPanelRenderer")?.get("text")))
            .or_else(|| r.get("videoCount").and_then(Value::as_str).map(|c| format!("{c} videos"))),
        first_video_id: r
            .pointer("/navigationEndpoint/watchEndpoint/videoId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

// ───────────────────────── search ─────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchFilter {
    /// 0 relevance, 1 rating, 2 upload date, 3 view count
    pub sort: u8,
    /// 0 any, 1 hour, 2 today, 3 week, 4 month, 5 year
    pub date: u8,
    /// 0 any, 1 video, 2 channel, 3 playlist, 4 movie
    pub kind: u8,
    /// 0 any, 1 short (<4m), 2 long (>20m), 3 medium (4–20m)
    pub duration: u8,
    pub hd: bool,
    pub subtitles: bool,
    pub creative_commons: bool,
    pub live: bool,
    pub four_k: bool,
    pub hdr: bool,
}

impl SearchFilter {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Encodes the protobuf `sp` parameter.
    pub fn encode(&self) -> Option<String> {
        if self.is_default() {
            return None;
        }
        fn varint(out: &mut Vec<u8>, mut v: u64) {
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    out.push(b);
                    break;
                }
                out.push(b | 0x80);
            }
        }
        fn field(out: &mut Vec<u8>, num: u64, v: u64) {
            varint(out, num << 3);
            varint(out, v);
        }
        let mut filters = Vec::new();
        if self.date > 0 { field(&mut filters, 1, self.date as u64); }
        if self.kind > 0 { field(&mut filters, 2, self.kind as u64); }
        if self.duration > 0 { field(&mut filters, 3, self.duration as u64); }
        if self.hd { field(&mut filters, 4, 1); }
        if self.subtitles { field(&mut filters, 5, 1); }
        if self.creative_commons { field(&mut filters, 6, 1); }
        if self.live { field(&mut filters, 8, 1); }
        if self.four_k { field(&mut filters, 14, 1); }
        if self.hdr { field(&mut filters, 25, 1); }
        let mut out = Vec::new();
        if self.sort > 0 { field(&mut out, 1, self.sort as u64); }
        if !filters.is_empty() {
            varint(&mut out, (2 << 3) | 2);
            varint(&mut out, filters.len() as u64);
            out.extend(filters);
        }
        use base64::Engine;
        Some(base64::engine::general_purpose::STANDARD.encode(out))
    }
}

pub fn search(query: &str, filter: &SearchFilter) -> Result<Page> {
    let mut body = json!({ "query": query });
    if let Some(p) = filter.encode() {
        body["params"] = json!(p);
    }
    let r = call("search", body)?;
    Ok(collect_items(&r))
}

pub fn search_continue(token: &str) -> Result<Page> {
    let r = call("search", json!({ "continuation": token }))?;
    Ok(search_continuation_page(&r))
}

fn search_continuation_page(response: &Value) -> Page {
    let mut page = collect_items(response);
    if let Some(commands) = response.get("onResponseReceivedCommands").and_then(Value::as_array) {
        for items in commands
            .iter()
            .filter_map(|command| command.pointer("/appendContinuationItemsAction/continuationItems"))
        {
            walk_items(items, &mut page, &mut |_, _| {});
        }
    }
    page
}

/// The Shorts player's own feed: a fresh, varied sequence each time.
/// `token` is `None` for the first page, then the returned continuation.
/// Items only carry ids; titles and channels come from `watch_next` when played.
/// The Shorts feed. With the browser sign-in it is the account's own Shorts
/// recommendations; otherwise it starts from `seed` (a Short from the local
/// history) so YouTube follows on from what was watched, or from nothing.
pub fn shorts_feed(token: Option<&str>, seed: Option<&str>) -> Result<(Vec<VideoItem>, Option<String>)> {
    let personal = crate::api::web::connected();
    let call = |ep: &str, body: Value| if personal { crate::api::web::web_call(ep, body) } else { call(ep, body) };
    let mut ids: Vec<String> = Vec::new();
    let seq = match token {
        Some(t) => t.to_string(),
        // a sequence that follows on from a Short: protobuf { 1: video id }
        None if seed.is_some() && !personal => {
            use base64::Engine;
            let id = seed.unwrap_or_default();
            let mut pb = vec![0x0a, id.len() as u8];
            pb.extend(id.as_bytes());
            base64::engine::general_purpose::URL_SAFE.encode(&pb)
        }
        None => {
            let body = json!({ "params": "CA8%3D", "disablePlayerResponse": true, "inputType": "REEL_WATCH_INPUT_TYPE_SEEDLESS" });
            let r = call("reel/reel_item_watch", body)?;
            let token = r.get("sequenceContinuation").and_then(Value::as_str).ok_or_else(|| anyhow!("no shorts sequence"))?.to_owned();
            if let Some(id) = r.pointer("/replacementEndpoint/reelWatchEndpoint/videoId").and_then(Value::as_str).filter(|id| !id.is_empty()) {
                // The seed is already playable. The page can request the sequence after showing it.
                let first = VideoItem { id: id.to_owned(), thumbnail: default_thumb(id), is_short: true, ..Default::default() };
                return Ok((vec![first], Some(token)));
            }
            token
        }
    };
    let s = call("reel/reel_watch_sequence", json!({ "sequenceParams": seq }))?;
    for e in s.get("entries").and_then(Value::as_array).into_iter().flatten() {
        if let Some(id) = e.pointer("/command/reelWatchEndpoint/videoId").and_then(Value::as_str) {
            if !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        }
    }
    let cont = s.pointer("/continuationEndpoint/continuationCommand/token").and_then(Value::as_str).map(str::to_string);
    let items = ids
        .into_iter()
        .map(|id| VideoItem { thumbnail: default_thumb(&id), id, is_short: true, ..Default::default() })
        .collect();
    Ok((items, cont))
}

pub fn suggestions(query: &str) -> Result<Vec<String>> {
    let (hl, gl) = region();
    let q = http::urlencode(query);
    // the endpoint youtube.com itself uses; answers with JSONP
    let yt = http::get_text(&format!(
        "https://suggestqueries-clients6.youtube.com/complete/search?client=youtube&ds=yt&hl={hl}&gl={gl}&q={q}"
    ))
    .ok()
    .and_then(|t| parse_suggestions_jsonp(&t));
    if let Some(v) = yt.filter(|v| !v.is_empty()) {
        return Ok(v);
    }
    let v = http::get_json(&format!("https://suggestqueries.google.com/complete/search?client=firefox&ds=yt&hl={hl}&gl={gl}&q={q}"))?;
    Ok(v.get(1)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// `window.google.ac.h(["q",[["sugg",0,[..]],..],{..}])` -> the suggestion strings.
fn parse_suggestions_jsonp(t: &str) -> Option<Vec<String>> {
    let start = t.find('(')? + 1;
    let end = t.rfind(')')?;
    let v: Value = serde_json::from_str(t.get(start..end)?).ok()?;
    Some(
        v.get(1)?
            .as_array()?
            .iter()
            .filter_map(|e| e.get(0).and_then(Value::as_str).or_else(|| e.as_str()).map(str::to_string))
            .collect(),
    )
}

#[cfg(test)]
mod suggest_tests {
    #[test]
    fn jsonp() {
        let t = r#"window.google.ac.h(["lofi",[["lofi",0,[512]],["lofi girl",0,[512,433]]],{"k":1}])"#;
        assert_eq!(super::parse_suggestions_jsonp(t).unwrap(), vec!["lofi", "lofi girl"]);
    }

    #[test]
    #[ignore]
    fn live() {
        let v = super::suggestions("lofi").unwrap();
        assert!(!v.is_empty(), "{v:?}");
    }
}

// ───────────────────────── browse ─────────────────────────

pub fn browse(browse_id: &str, params: Option<&str>) -> Result<Value> {
    let mut body = json!({ "browseId": browse_id });
    if let Some(p) = params {
        body["params"] = json!(p);
    }
    call("browse", body)
}

pub fn browse_continue(token: &str) -> Result<Page> {
    let r = call("browse", json!({ "continuation": token }))?;
    Ok(collect_items(&r))
}

/// Resolves a YouTube URL (/@handle, /c/name, /channel/UC.., /playlist?list=, /watch?v=) via the API.
pub fn resolve_url(url: &str) -> Result<ResolvedUrl> {
    let r = call("navigation/resolve_url", json!({ "url": url }))?;
    let ep = r.get("endpoint").ok_or_else(|| anyhow!("unresolvable url"))?;
    if let Some(id) = ep.pointer("/browseEndpoint/browseId").and_then(Value::as_str) {
        if let Some(pl) = id.strip_prefix("VL") {
            return Ok(ResolvedUrl::Playlist(pl.to_string()));
        }
        return Ok(ResolvedUrl::Channel(id.to_string()));
    }
    if let Some(id) = ep.pointer("/watchEndpoint/videoId").and_then(Value::as_str) {
        return Ok(ResolvedUrl::Video(id.to_string()));
    }
    Err(anyhow!("unsupported url"))
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedUrl {
    Video(String),
    Channel(String),
    Playlist(String),
}

fn is_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_browse_id(id: &str) -> bool {
    (2..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Fast local parse of common URL shapes before falling back to resolve_url.
pub fn parse_youtube_url(input: &str) -> Option<ResolvedUrl> {
    let s = input.trim();
    if !s.contains("youtu") {
        return None;
    }
    let get_param = |name: &str| -> Option<String> {
        let q = s.split_once('?')?.1;
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == name).then(|| v.split('#').next().unwrap_or(v).to_string())
        })
    };
    if let Some(rest) = s.split("youtu.be/").nth(1) {
        let id: String = rest.chars().take(11).collect();
        if is_video_id(&id) {
            return Some(ResolvedUrl::Video(id));
        }
    }
    for marker in ["/shorts/", "/live/", "/embed/"] {
        if let Some(rest) = s.split(marker).nth(1) {
            let id: String = rest.chars().take(11).collect();
            if is_video_id(&id) {
                return Some(ResolvedUrl::Video(id));
            }
        }
    }
    if let Some(v) = get_param("v").filter(|v| is_video_id(v)) {
        return Some(ResolvedUrl::Video(v));
    }
    // links can come from any web page (streamtube: links), so only take IDs
    // made of the characters YouTube uses
    if let Some(l) = get_param("list").filter(|l| is_browse_id(l)) {
        return Some(ResolvedUrl::Playlist(l));
    }
    if let Some(rest) = s.split("/channel/").nth(1) {
        let id: String = rest.chars().take_while(|c| *c != '/' && *c != '?' && *c != '#').collect();
        if id.starts_with("UC") && is_browse_id(&id) {
            return Some(ResolvedUrl::Channel(id));
        }
    }
    None
}

pub fn channel(channel_id: &str, tab_params: Option<&str>) -> Result<ChannelPage> {
    let r = browse(channel_id, tab_params)?;
    let mut info = ChannelItem { id: channel_id.to_string(), ..Default::default() };
    if let Some(md) = r.pointer("/metadata/channelMetadataRenderer") {
        info.name = s(md, "/title").unwrap_or_default();
        info.description = s(md, "/description");
        info.avatar = md.get("avatar").and_then(best_thumb);
        if let Some(ext) = s(md, "/externalId") {
            info.id = ext;
        }
        info.handle = s(md, "/vanityChannelUrl").and_then(|u| u.rsplit('/').next().map(str::to_string));
    }
    let mut banner = None;
    if let Some(ph) = r.pointer("/header/pageHeaderRenderer/content/pageHeaderViewModel") {
        if info.name.is_empty() {
            info.name = s(ph, "/title/dynamicTextViewModel/text/content").unwrap_or_default();
        }
        if info.avatar.is_none() {
            info.avatar = ph
                .pointer("/image/decoratedAvatarViewModel/avatar/avatarViewModel/image")
                .and_then(best_thumb);
        }
        banner = ph.pointer("/banner/imageBannerViewModel/image").and_then(best_thumb);
        if let Some(rows) = ph.pointer("/metadata/contentMetadataViewModel/metadataRows").and_then(Value::as_array) {
            for row in rows {
                for part in row.get("metadataParts").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(t) = part.pointer("/text/content").and_then(Value::as_str) {
                        let low = t.to_lowercase();
                        if t.starts_with('@') {
                            info.handle = Some(t.to_string());
                        } else if low.contains("subscriber") {
                            info.subscribers_text = Some(t.to_string());
                        } else if low.contains("video") {
                            info.video_count_text = Some(t.to_string());
                        }
                    }
                }
            }
        }
    } else if let Some(c4) = r.pointer("/header/c4TabbedHeaderRenderer") {
        banner = c4.get("banner").and_then(best_thumb);
        info.subscribers_text = opt_text(c4.get("subscriberCountText"));
    }
    let mut tabs = Vec::new();
    let mut page = Page::default();
    let mut sort_chips = Vec::new();
    if let Some(arr) = r.pointer("/contents/twoColumnBrowseResultsRenderer/tabs").and_then(Value::as_array) {
        for t in arr {
            let tr = t.get("tabRenderer").or_else(|| t.get("expandableTabRenderer"));
            let Some(tr) = tr else { continue };
            let title = s(tr, "/title").unwrap_or_default();
            let selected = tr.get("selected").and_then(Value::as_bool).unwrap_or(false);
            let params = s(tr, "/endpoint/browseEndpoint/params");
            if selected {
                if let Some(content) = tr.get("content") {
                    page = collect_items(content);
                    sort_chips = parse_sort_chips(content);
                }
            }
            if !title.is_empty() {
                let kind = tab_kind(tr, &title);
                tabs.push(ChannelTab { title, kind, params, selected });
            }
        }
    }
    // Fill channel info on videos that lack it
    for it in page.items.iter_mut() {
        if let Item::Video(v) = it {
            if v.channel_name.is_empty() {
                v.channel_name = info.name.clone();
            }
            if v.channel_id.is_none() {
                v.channel_id = Some(info.id.clone());
            }
            if v.channel_avatar.is_none() {
                v.channel_avatar = info.avatar.clone();
            }
        }
    }
    Ok(ChannelPage { info, banner, tabs, page, sort_chips })
}

/// The Latest / Popular / Oldest chips over a channel's videos, live or Shorts grid.
fn parse_sort_chips(content: &Value) -> Vec<SortChip> {
    let mut out = Vec::new();
    let mut chips = Vec::new();
    find_all(content, "chipViewModel", &mut chips);
    for c in chips {
        let title = s(c, "/text").unwrap_or_default();
        let mut tokens = Vec::new();
        if let Some(cmd) = c.get("tapCommand") {
            find_all(cmd, "token", &mut tokens);
        }
        if let (false, Some(token)) = (title.is_empty(), tokens.iter().find_map(|t| t.as_str())) {
            out.push(SortChip { title, token: token.to_string(), selected: c.get("selected").and_then(Value::as_bool).unwrap_or(false) });
        }
    }
    if out.is_empty() {
        // the older form
        let mut chips = Vec::new();
        find_all(content, "chipCloudChipRenderer", &mut chips);
        for c in chips {
            let title = opt_text(c.get("text")).unwrap_or_default();
            let token = c.pointer("/navigationEndpoint/continuationCommand/token").and_then(Value::as_str);
            if let (false, Some(token)) = (title.is_empty(), token) {
                out.push(SortChip { title, token: token.to_string(), selected: c.get("isSelected").and_then(Value::as_bool).unwrap_or(false) });
            }
        }
    }
    out
}

/// A channel tab's kind from its URL ("/@name/videos" -> "videos"). Titles follow
/// the content language ("Videolar", "Видео"), so they can't tell tabs apart.
fn tab_kind(tab: &Value, title: &str) -> String {
    s(tab, "/endpoint/commandMetadata/webCommandMetadata/url")
        .and_then(|u| u.split('?').next().and_then(|p| p.rsplit('/').next()).map(str::to_ascii_lowercase))
        .filter(|k| !k.is_empty() && !k.starts_with('@') && !k.starts_with("uc"))
        .unwrap_or_else(|| match title {
            "Home" => "featured".into(),
            "Live" => "streams".into(),
            other => other.to_ascii_lowercase(),
        })
}

/// Just a channel's avatar, for feeds whose items don't include one.
pub fn channel_avatar(channel_id: &str) -> Result<Option<String>> {
    let r = browse(channel_id, None)?;
    Ok(r
        .pointer("/metadata/channelMetadataRenderer/avatar")
        .and_then(small_thumb)
        .or_else(|| {
            r.pointer("/header/pageHeaderRenderer/content/pageHeaderViewModel/image/decoratedAvatarViewModel/avatar/avatarViewModel/image")
                .and_then(small_thumb)
        }))
}

/// Finds a channel by its exact display name: (channel id, avatar).
pub fn channel_by_name(name: &str) -> Result<Option<(String, Option<String>)>> {
    let page = search(name, &SearchFilter { kind: 2, ..Default::default() })?;
    let want = name.trim().to_lowercase();
    Ok(page.items.into_iter().find_map(|i| match i {
        Item::Channel(c) if c.name.trim().to_lowercase() == want => Some((c.id, c.avatar)),
        _ => None,
    }))
}

/// Searches within a channel.
pub fn channel_search(channel_id: &str, query: &str) -> Result<Page> {
    let r = call("browse", json!({ "browseId": channel_id, "params": "EgZzZWFyY2jyBgQKAloA", "query": query }))?;
    Ok(collect_items(&r))
}

pub fn playlist(playlist_id: &str) -> Result<PlaylistPage> {
    let r = browse(&format!("VL{playlist_id}"), None)?;
    let content = r.get("contents").cloned().unwrap_or(Value::Null);
    let page = collect_items(&content);
    let mut info = PlaylistItem { id: playlist_id.to_string(), ..Default::default() };
    if let Some(md) = r.pointer("/metadata/playlistMetadataRenderer") {
        info.title = s(md, "/title").unwrap_or_default();
    }
    let mut description = None;
    if let Some(h) = r.pointer("/header/pageHeaderRenderer") {
        if info.title.is_empty() {
            info.title = s(h, "/pageTitle").unwrap_or_default();
        }
        let mut parts = Vec::new();
        find_all(h, "metadataParts", &mut parts);
        for arr in parts.iter().filter_map(|p| p.as_array()) {
            for p in arr {
                if let Some(t) = p.pointer("/text/content").and_then(Value::as_str) {
                    if t.to_lowercase().contains("video") {
                        info.video_count_text = Some(t.to_string());
                    }
                    if let Some(b) = find(p, "browseEndpoint").and_then(|b| b.get("browseId")).and_then(Value::as_str) {
                        if b.starts_with("UC") {
                            info.channel_id = Some(b.to_string());
                            info.channel_name = Some(t.trim_start_matches("by ").to_string());
                        }
                    }
                }
            }
        }
        description = find(h, "descriptionPreviewViewModel")
            .and_then(|d| d.pointer("/description/content"))
            .and_then(Value::as_str)
            .map(str::to_string);
    } else if let Some(h) = r.pointer("/header/playlistHeaderRenderer") {
        info.video_count_text = opt_text(h.get("numVideosText"));
        info.channel_name = opt_text(h.get("ownerText"));
        description = opt_text(h.get("descriptionText"));
    }
    let videos: Vec<VideoItem> = page.items.into_iter().filter_map(|i| match i {
        Item::Video(v) => Some(v),
        _ => None,
    }).collect();
    info.thumbnail = videos.first().map(|v| v.thumbnail.clone());
    info.first_video_id = videos.first().map(|v| v.id.clone());
    Ok(PlaylistPage { info, description, videos, continuation: page.continuation })
}

/// Upper bound on continuation requests for one playlist (~10,000 videos).
pub const MAX_PLAYLIST_PAGES: usize = 100;

/// Fetches one continuation page of a playlist and extracts its videos.
pub fn playlist_continue(token: &str, authed: bool) -> Result<(Vec<VideoItem>, Option<String>)> {
    let fetch = || if authed { super::auth::browse_continue(token) } else { browse_continue(token) };
    let page = fetch().or_else(|_| fetch())?;
    let videos = page
        .items
        .into_iter()
        .filter_map(|i| match i {
            Item::Video(v) => Some(v),
            _ => None,
        })
        .collect();
    Ok((videos, page.continuation))
}

/// Appends videos whose id is not already present in `target`, returning how many were added.
pub fn append_unique_videos(target: &mut Vec<VideoItem>, incoming: Vec<VideoItem>) -> usize {
    let mut seen: std::collections::HashSet<String> = target.iter().map(|v| v.id.clone()).collect();
    let before = target.len();
    for v in incoming {
        if seen.insert(v.id.clone()) {
            target.push(v);
        }
    }
    target.len() - before
}

/// Tracks playlist continuation tokens while guarding against empty, repeated,
/// or cyclic tokens and capping the total number of pages fetched.
pub struct PlaylistPager {
    next: Option<String>,
    seen_tokens: std::collections::HashSet<String>,
    pages_loaded: usize,
    max_pages: usize,
}

impl PlaylistPager {
    pub fn new(first: Option<String>, max_pages: usize) -> Self {
        Self {
            next: first.filter(|t| !t.is_empty()),
            seen_tokens: std::collections::HashSet::new(),
            pages_loaded: 0,
            max_pages,
        }
    }

    pub fn take_next(&mut self) -> Option<String> {
        if self.pages_loaded >= self.max_pages {
            self.next = None;
            return None;
        }
        let tok = self.next.take().filter(|t| !t.is_empty())?;
        if !self.seen_tokens.insert(tok.clone()) {
            return None;
        }
        self.pages_loaded += 1;
        Some(tok)
    }

    pub fn advance(&mut self, next: Option<String>) {
        if self.pages_loaded >= self.max_pages {
            self.next = None;
            return;
        }
        self.next = next.filter(|t| !t.is_empty() && !self.seen_tokens.contains(t));
    }

    pub fn next_token(&self) -> Option<&str> {
        (self.pages_loaded < self.max_pages).then_some(self.next.as_deref()).flatten()
    }
}

// ───────────────────────── watch / next ─────────────────────────

fn description_link_url(command: &Value) -> Option<String> {
    if let Some(watch) = command.get("watchEndpoint") {
        let id = watch.get("videoId")?.as_str().filter(|id| is_video_id(id))?;
        let mut url = format!("https://www.youtube.com/watch?v={id}");
        if let Some(list) = watch.get("playlistId").and_then(Value::as_str).filter(|id| is_browse_id(id)) {
            url.push_str("&list=");
            url.push_str(list);
        }
        if let Some(start) = watch.get("startTimeSeconds").and_then(Value::as_u64).filter(|&start| start > 0) {
            url.push_str("&t=");
            url.push_str(&start.to_string());
        }
        return Some(url);
    }
    let browse = command.pointer("/browseEndpoint/browseId")?.as_str()?;
    if let Some(list) = browse.strip_prefix("VL").filter(|id| is_browse_id(id)) {
        Some(format!("https://www.youtube.com/playlist?list={list}"))
    } else if browse.starts_with("UC") && is_browse_id(browse) {
        Some(format!("https://www.youtube.com/channel/{browse}"))
    } else {
        None
    }
}

fn description_links(attributed: &Value, text: &str) -> Vec<DescriptionLink> {
    let Some(runs) = attributed.get("commandRuns").and_then(Value::as_array) else { return Vec::new() };
    // YouTube counts UTF-16 code units. A span cannot split a surrogate pair.
    let offsets: Vec<_> = text
        .char_indices()
        .flat_map(|(byte, c)| [Some(byte), None].into_iter().take(c.len_utf16()))
        .chain([Some(text.len())])
        .collect();
    let mut links: Vec<_> = runs
        .iter()
        .filter_map(|run| {
            let start = usize::try_from(run.get("startIndex").map_or(Some(0), Value::as_u64)?).ok()?;
            let length = usize::try_from(run.get("length")?.as_u64()?).ok()?;
            let start_byte = (*offsets.get(start)?)?;
            let end_byte = (*offsets.get(start.checked_add(length)?)?)?;
            if start_byte == end_byte {
                return None;
            }
            let url = description_link_url(run.pointer("/onTap/innertubeCommand")?)?;
            Some(DescriptionLink { range: start_byte..end_byte, url })
        })
        .collect();
    links.sort_by_key(|link| link.range.start);
    let mut end = 0;
    links.retain(|link| {
        if link.range.start < end {
            return false;
        }
        end = link.range.end;
        true
    });
    links
}

pub fn watch_next(video_id: &str, playlist_id: Option<&str>) -> Result<WatchInfo> {
    let mut body = json!({ "videoId": video_id, "contentCheckOk": true, "racyCheckOk": true });
    if let Some(p) = playlist_id {
        body["playlistId"] = json!(p);
    }
    let r = call("next", body)?;
    let mut w = WatchInfo { id: video_id.to_string(), ..Default::default() };
    let results = r.pointer("/contents/twoColumnWatchNextResults").cloned().unwrap_or(Value::Null);
    if let Some(p) = find(&results, "videoPrimaryInfoRenderer") {
        w.title = opt_text(p.get("title")).unwrap_or_default();
        w.views_text = p
            .pointer("/viewCount/videoViewCountRenderer/viewCount")
            .map(text)
            .unwrap_or_default();
        w.date_text = opt_text(p.get("dateText")).unwrap_or_default();
        w.is_live = p
            .pointer("/viewCount/videoViewCountRenderer/isLive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(like) = find(p, "likeButtonViewModel") {
            w.likes_text = find(like, "defaultButtonViewModel")
                .and_then(|b| b.pointer("/buttonViewModel/title"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    if let Some(sec) = find(&results, "videoSecondaryInfoRenderer") {
        if let Some(o) = sec.pointer("/owner/videoOwnerRenderer") {
            w.channel.name = opt_text(o.get("title")).unwrap_or_default();
            w.channel.id = o
                .pointer("/navigationEndpoint/browseEndpoint/browseId")
                .or_else(|| o.pointer("/title/runs/0/navigationEndpoint/browseEndpoint/browseId"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            w.channel.avatar = o.get("thumbnail").and_then(best_thumb);
            w.channel.subscribers_text = opt_text(o.get("subscriberCountText"));
            // collaborations: "A and B", with the channels listed in a dialog
            if w.channel.name.is_empty() {
                if let Some(t) = o.pointer("/attributedTitle/content").and_then(Value::as_str) {
                    w.channel.name = t.to_string();
                }
            }
            if w.channel.id.is_empty() || w.channel.avatar.is_none() {
                let mut list = Vec::new();
                find_all(o, "listItemViewModel", &mut list);
                if let Some(first) = list.first() {
                    if w.channel.id.is_empty() {
                        w.channel.id = find(first, "browseEndpoint")
                            .and_then(|b| b.get("browseId"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                    }
                    if w.channel.subscribers_text.is_none() {
                        w.channel.subscribers_text = first
                            .pointer("/subtitle/content")
                            .and_then(Value::as_str)
                            .and_then(|s| s.split('\u{2022}').nth(1))
                            .map(|s| s.chars().filter(|c| !('\u{2066}'..='\u{2069}').contains(c) && *c != '\u{200e}').collect::<String>().trim().to_string())
                            .filter(|s| !s.is_empty());
                    }
                }
                if w.channel.avatar.is_none() {
                    w.channel.avatar = o
                        .pointer("/avatarStack/avatarStackViewModel/avatars/0/avatarViewModel/image/sources/0/url")
                        .and_then(Value::as_str)
                        .map(|u| if u.starts_with("//") { format!("https:{u}") } else { u.to_string() });
                }
            }
        }
        if let Some((attributed, content)) = sec.get("attributedDescription").and_then(|d| Some((d, d.get("content")?.as_str()?))) {
            w.description = content.to_string();
            w.description_links = description_links(attributed, content);
        } else {
            w.description = opt_text(sec.get("description")).unwrap_or_default();
        }
    }
    // Related
    if let Some(sr) = results.pointer("/secondaryResults") {
        let page = collect_items(sr);
        w.related = page.items;
        w.related_continuation = page.continuation;
    }
    // Comments token
    let mut sections = Vec::new();
    find_all(&results, "itemSectionRenderer", &mut sections);
    for sec in sections {
        if sec.get("sectionIdentifier").and_then(Value::as_str) == Some("comment-item-section") {
            w.comments_token = sec.pointer("/contents/0/continuationItemRenderer").and_then(continuation_token);
        }
    }
    if let Some(panels) = r.get("engagementPanels") {
        if let Some(items) = find(panels, "sortFilterSubMenuRenderer").and_then(|m| m.get("subMenuItems")).and_then(Value::as_array) {
            let tok = |i: usize| {
                items
                    .get(i)
                    .and_then(|x| x.pointer("/serviceEndpoint/continuationCommand/token"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            w.comments_sort = (tok(0), tok(1));
        }
    }
    if let Some(lc) = find(&r, "liveChatRenderer").filter(|lc| !lc.get("isReplay").and_then(Value::as_bool).unwrap_or(false)) {
        // Only the main token works on its own; "Top chat" and "Live chat" differ
        // by one flag inside it (field 1 = 4 for top, 1 for everything).
        let first = lc.pointer("/continuations/0/reloadContinuationData/continuation").and_then(Value::as_str).map(str::to_string);
        let top = first.clone();
        let all = first.map(|t| t.replacen("ggEMCAQY", "ggEMCAEY", 1));
        if let (Some(t), Some(a)) = (top, all) {
            w.live_chat = Some((t, a));
        }
    }
    if let Some(cnt) = find(&results, "commentsEntryPointHeaderRenderer") {
        w.comment_count_text = opt_text(cnt.get("commentCount"));
    }
    // Chapters from player bar markers
    let mut chapters = Vec::new();
    let mut found = Vec::new();
    find_all(&r, "chapterRenderer", &mut found);
    for c in found {
        let start = c.get("timeRangeStartMillis").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0;
        chapters.push(Chapter {
            title: opt_text(c.get("title")).unwrap_or_default(),
            start,
            thumbnail: c.get("thumbnail").and_then(best_thumb),
        });
    }
    if chapters.is_empty() {
        let mut m = Vec::new();
        find_all(&r, "macroMarkersListItemRenderer", &mut m);
        for c in m {
            let t = opt_text(c.get("timeDescription")).and_then(|t| parse_duration(&t));
            if let Some(t) = t {
                chapters.push(Chapter {
                    title: opt_text(c.get("title")).unwrap_or_default(),
                    start: t as f64,
                    thumbnail: c.get("thumbnail").and_then(best_thumb),
                });
            }
        }
        // macro markers may include "key moments" duplicates; keep unique sorted
        chapters.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
        chapters.dedup_by(|a, b| a.start == b.start);
    }
    if chapters.is_empty() {
        chapters = chapters_from_description(&w.description);
    }
    w.chapters = chapters;
    // "Most replayed" heatmap
    let mut lists = Vec::new();
    find_all(&r, "macroMarkersListEntity", &mut lists);
    for l in lists {
        if l.pointer("/markersList/markerType").and_then(Value::as_str) != Some("MARKER_TYPE_HEATMAP") {
            continue;
        }
        let num = |v: Option<&Value>| v.and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64())).unwrap_or(0.0);
        for m in l.pointer("/markersList/markers").and_then(Value::as_array).into_iter().flatten() {
            w.heatmap.push((
                num(m.get("startMillis")) / 1000.0,
                num(m.get("durationMillis")) / 1000.0,
                num(m.get("intensityScoreNormalized")) as f32,
            ));
        }
        break;
    }
    // Playlist panel
    if let Some(pl) = results.pointer("/playlist/playlist") {
        let page = collect_items(pl.get("contents").unwrap_or(&Value::Null));
        w.playlist = Some(WatchPlaylist {
            id: s(pl, "/playlistId").unwrap_or_default(),
            title: s(pl, "/title").unwrap_or_default(),
            items: page.items.into_iter().filter_map(|i| match i {
                Item::Video(v) => Some(v),
                _ => None,
            }).collect(),
        });
    }
    if crate::store::settings_snapshot().original_video_titles {
        if let Ok(title) = original_video_title(video_id) {
            w.title = title;
        }
    }
    Ok(w)
}

fn original_video_title(video_id: &str) -> Result<String> {
    let response = call_with_context(
        "player",
        json!({ "videoId": video_id }),
        json!({ "client": { "clientName": "WEB", "clientVersion": WEB_VERSION } }),
    )?;
    let details = response.get("videoDetails").ok_or_else(|| anyhow!("Missing video details"))?;
    anyhow::ensure!(details.get("videoId").and_then(Value::as_str) == Some(video_id), "Unexpected video ID");
    details.get("title").and_then(Value::as_str).filter(|title| !title.trim().is_empty())
        .map(str::to_owned).ok_or_else(|| anyhow!("Missing original video title"))
}

#[cfg(test)]
mod original_title_tests {
    use super::*;

    #[test]
    #[ignore = "calls YouTube for the video reported in #120"]
    fn original_titles_preserve_the_reported_english_title_in_russian() {
        let id = "hJEmt03Rz1c";
        crate::store::init();
        crate::store::update_settings(|s| {
            s.language = "ru".into();
            s.region = "US".into();
            s.original_video_titles = false;
        });
        assert_eq!(watch_next(id, None).unwrap().title, "Диета без английского");
        crate::store::update_settings(|s| s.original_video_titles = true);
        assert_eq!(watch_next(id, None).unwrap().title, "The No English Diet");
    }
}

pub fn related_continue(token: &str) -> Result<Page> {
    let r = call("next", json!({ "continuation": token }))?;
    Ok(collect_items(&r))
}

/// Parses "0:00 Intro" style lines from a description.
pub fn chapters_from_description(desc: &str) -> Vec<Chapter> {
    let mut out = Vec::new();
    for line in desc.lines() {
        let line = line.trim();
        let mut ts_end = None;
        for (i, w) in line.split_whitespace().enumerate() {
            let w2 = w.trim_matches(|c: char| c == '(' || c == ')' || c == '[' || c == ']' || c == '-');
            if w2.contains(':') && parse_duration(w2).is_some() {
                ts_end = Some((i, parse_duration(w2).unwrap()));
                break;
            }
            if i > 1 {
                break;
            }
        }
        if let Some((idx, secs)) = ts_end {
            let title: String = line
                .split_whitespace()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .map(|(_, w)| w)
                .collect::<Vec<_>>()
                .join(" ")
                .trim_start_matches(|c: char| c == '-' || c == '–' || c == ':' || c == '|' || c.is_whitespace())
                .to_string();
            out.push(Chapter { title, start: secs as f64, thumbnail: None });
        }
    }
    if out.len() >= 3 && out[0].start == 0.0 { out } else { Vec::new() }
}

// ───────────────────────── comments ─────────────────────────

/// Channel (custom) emojis in a comment's text. Standard emojis are left alone:
/// they're already Unicode characters in the text.
fn comment_emojis(props: &Value) -> Vec<(String, String)> {
    let Some(content) = props.pointer("/content/content").and_then(Value::as_str) else { return Vec::new() };
    let Some(runs) = props.pointer("/content/attachmentRuns").and_then(Value::as_array) else { return Vec::new() };
    // run offsets count UTF-16 units
    let units: Vec<u16> = content.encode_utf16().collect();
    let mut out: Vec<(String, String)> = Vec::new();
    for r in runs {
        let Some(url) = r.pointer("/element/type/imageType/image/sources/0/url").and_then(Value::as_str) else { continue };
        if url.contains("/s/gaming/emoji/") {
            continue;
        }
        let start = r.get("startIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
        let len = r.get("length").and_then(Value::as_u64).unwrap_or(0) as usize;
        let Some(span) = units.get(start..start + len) else { continue };
        let code = String::from_utf16_lossy(span);
        if code.trim().is_empty() || out.iter().any(|(c, _)| *c == code) {
            continue;
        }
        // sharper than the 16px default, drawn at text size
        let url = match url.rsplit_once('=') {
            Some((base, _)) if url.contains("googleusercontent.com") || url.contains("ggpht.com") => format!("{base}=w40-h40-c-k-nd"),
            _ => url.to_string(),
        };
        out.push((code, url));
    }
    out
}

pub fn comments(token: &str) -> Result<CommentPage> {
    let r = call("next", json!({ "continuation": token }))?;
    Ok(parse_comments(&r))
}

pub(crate) fn account_comments(token: &str) -> Result<CommentPage> {
    if let Some(account) = super::web::comment_session_identity() {
        if let Ok(response) = super::web::comment_call_for_session(
            &account, super::web::CommentRequest::Load, json!({ "continuation": token }),
        ) {
            let mut page = parse_comments(&response);
            for comment in &mut page.comments {
                comment.account = Some(account.clone());
            }
            page.account = Some(account);
            return Ok(page);
        }
    }
    comments(token)
}

pub(crate) fn post_comment(account: &std::sync::Arc<()>, params: &str, text: &str) -> Result<()> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "Enter a comment first");
    let response = super::web::web_call_for_session(
        account, "comment/create_comment", json!({ "createCommentParams": params, "commentText": text }),
    )?;
    anyhow::ensure!(response.get("error").is_none(), "YouTube refused the comment");
    Ok(())
}

pub(crate) fn react_to_comment(account: &std::sync::Arc<()>, actions: &[String]) -> Result<()> {
    anyhow::ensure!(!actions.is_empty(), "This comment has no reaction command");
    let response = super::web::comment_call_for_session(account, super::web::CommentRequest::React, json!({ "actions": actions }))?;
    anyhow::ensure!(response.get("error").is_none(), "YouTube refused the reaction");
    Ok(())
}

fn create_comment_params(node: &Value) -> Option<String> {
    find(node, "createCommentEndpoint")?.get("createCommentParams")?.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn reaction_actions(node: Option<&Value>) -> Vec<String> {
    let Some(endpoint) = node.and_then(|n| find(n, "performCommentActionEndpoint")) else { return Vec::new() };
    if let Some(actions) = endpoint.get("actions").and_then(Value::as_array) {
        return actions.iter().filter_map(Value::as_str).map(str::to_owned).collect();
    }
    endpoint.get("action").and_then(Value::as_str).map(str::to_owned).into_iter().collect()
}

fn parse_comments(r: &Value) -> CommentPage {
    let mut page = CommentPage::default();
    // entity payloads
    let mut payloads = std::collections::HashMap::new();
    let mut toolbar_states = std::collections::HashMap::new();
    let mut toolbar_surfaces = std::collections::HashMap::new();
    let mut surfaces = std::collections::HashMap::new();
    if let Some(muts) = r.pointer("/frameworkUpdates/entityBatchUpdate/mutations").and_then(Value::as_array) {
        for m in muts {
            let key = m.get("entityKey").and_then(Value::as_str).unwrap_or_default().to_string();
            if let Some(p) = m.pointer("/payload/commentEntityPayload") {
                payloads.insert(key, p.clone());
            } else if let Some(p) = m.pointer("/payload/engagementToolbarStateEntityPayload") {
                toolbar_states.insert(key, p.clone());
            } else if let Some(p) = m.pointer("/payload/engagementToolbarSurfaceEntityPayload") {
                toolbar_surfaces.insert(key, p.clone());
            } else if let Some(p) = m.pointer("/payload/commentSurfaceEntityPayload") {
                surfaces.insert(key, p.clone());
            }
        }
    }
    let mut lists = Vec::new();
    for key in ["onResponseReceivedEndpoints", "onResponseReceivedActions"] {
        if let Some(arr) = r.get(key).and_then(Value::as_array) {
            for e in arr {
                for k in ["reloadContinuationItemsCommand", "appendContinuationItemsAction"] {
                    if let Some(items) = e.pointer(&format!("/{k}/continuationItems")).and_then(Value::as_array) {
                        lists.extend(items.iter().cloned());
                    }
                }
            }
        }
    }
    for it in &lists {
        if let Some(h) = it.get("commentsHeaderRenderer") {
            page.count_text = opt_text(h.get("countText"));
            page.create_params = h.get("createRenderer").and_then(create_comment_params);
            if let Some(items) = h
                .pointer("/sortMenu/sortFilterSubMenuRenderer/subMenuItems")
                .and_then(Value::as_array)
            {
                page.sort_top_token = items
                    .first()
                    .and_then(|i| i.pointer("/serviceEndpoint/continuationCommand/token"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                page.sort_new_token = items
                    .get(1)
                    .and_then(|i| i.pointer("/serviceEndpoint/continuationCommand/token"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        let (vm, thread) = if let Some(t) = it.get("commentThreadRenderer") {
            (t.get("commentViewModel").map(|c| c.get("commentViewModel").unwrap_or(c)), Some(t))
        } else if let Some(c) = it.get("commentViewModel") {
            (Some(c), None)
        } else {
            (None, None)
        };
        if let Some(vm) = vm {
            let key = vm.get("commentKey").and_then(Value::as_str).unwrap_or_default();
            if let Some(p) = payloads.get(key) {
                let props = p.get("properties").cloned().unwrap_or(Value::Null);
                let author = p.get("author").cloned().unwrap_or(Value::Null);
                let toolbar = p.get("toolbar").cloned().unwrap_or(Value::Null);
                let state_key = vm.get("toolbarStateKey").and_then(Value::as_str).unwrap_or_default();
                let state = toolbar_states.get(state_key);
                let hearted = state
                    .and_then(|s| s.get("heartState"))
                    .and_then(Value::as_str)
                    .map(|s| s.contains("HEARTED") && !s.contains("UNHEARTED"))
                    .unwrap_or(false);
                let reaction = match state.and_then(|s| s.get("likeState")).and_then(Value::as_str) {
                    Some("TOOLBAR_LIKE_STATE_LIKED") => Some(CommentReaction::Like),
                    Some("TOOLBAR_LIKE_STATE_DISLIKED") => Some(CommentReaction::Dislike),
                    _ => None,
                };
                let surface = vm.get("toolbarSurfaceKey").and_then(Value::as_str).and_then(|key| toolbar_surfaces.get(key));
                let actions = CommentActions {
                    like: reaction_actions(surface.and_then(|s| s.get("likeCommand"))),
                    unlike: reaction_actions(surface.and_then(|s| s.get("unlikeCommand"))),
                    dislike: reaction_actions(surface.and_then(|s| s.get("dislikeCommand"))),
                    undislike: reaction_actions(surface.and_then(|s| s.get("undislikeCommand"))),
                    reply: surface.and_then(|s| s.get("replyCommand")).and_then(create_comment_params),
                    reaction,
                    unliked_count: s(&toolbar, "/likeCountNotliked").unwrap_or_default().trim().to_string(),
                    liked_count: s(&toolbar, "/likeCountLiked").unwrap_or_default().trim().to_string(),
                };
                let replies_token = thread
                    .and_then(|t| t.pointer("/replies/commentRepliesRenderer/contents"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.iter().find_map(|c| c.get("continuationItemRenderer")))
                    .and_then(continuation_token);
                page.comments.push(Comment {
                    id: s(&props, "/commentId").or_else(|| s(vm, "/commentId")).unwrap_or_default(),
                    author: s(&author, "/displayName").unwrap_or_default(),
                    author_id: s(&author, "/channelId"),
                    author_avatar: s(&author, "/avatarThumbnailUrl"),
                    text: s(&props, "/content/content").unwrap_or_default(),
                    published: s(&props, "/publishedTime").unwrap_or_default(),
                    likes: actions.like_count().to_owned(),
                    reply_count: s(&toolbar, "/replyCount").unwrap_or_default(),
                    replies_token,
                    is_creator: author.get("isCreator").and_then(Value::as_bool).unwrap_or(false),
                    is_verified: author.get("isVerified").and_then(Value::as_bool).unwrap_or(false),
                    is_pinned: vm.get("pinnedText").is_some(),
                    hearted,
                    actions,
                    account: None,
                    emojis: comment_emojis(&props),
                    image: vm
                        .get("commentSurfaceKey")
                        .and_then(Value::as_str)
                        .and_then(|k| surfaces.get(k))
                        .and_then(|s| s.pointer("/imageAttachmentViewModel/commentImageAttachmentViewModel"))
                        .and_then(|a| {
                            let url = a.pointer("/image/sources/0/url")?.as_str()?.to_string();
                            let ratio = a.get("aspectRatio").and_then(Value::as_f64).unwrap_or(1.0) as f32;
                            Some((url, ratio.clamp(0.2, 5.0)))
                        }),
                });
            }
        }
        if let Some(c) = it.get("continuationItemRenderer") {
            page.continuation = continuation_token(c);
        }
    }
    page
}

// ───────────────────────── home / explore ─────────────────────────

/// Category "explore" channels (YouTube removed the Trending page in 2025).
pub const EXPLORE: &[(&str, &str)] = &[
    ("Music", "UC-9-kyTW8ZkZNDHQJ6FgpwQ"),
    ("Gaming", "UCOpNcN46UbXVtpKMrmU4Abg"),
    ("News", "UCYfdidRxbB8Qhf0Nx7ioOYw"),
    ("Sports", "UCEgdi0XIXXZ-qJOFPf4JSKw"),
    ("Live", "UC4R8DWoMoI7CAwX8_LjQHig"),
    ("Fashion & Beauty", "UCrpQ4p1Ql_hG8rKXIKM1MOQ"),
    ("Learning", "UCtFRv9O2AHqOZjjynzrv-xg"),
];

pub fn explore(channel_id: &str) -> Result<Page> {
    let r = browse(channel_id, None)?;
    Ok(collect_items(&r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_controls_use_the_commands_youtube_returns() {
        let mut response = json!({
            "onResponseReceivedEndpoints": [{
                "reloadContinuationItemsCommand": { "continuationItems": [
                    { "commentsHeaderRenderer": { "createRenderer": { "commentSimpleboxRenderer": {
                        "submitButton": { "buttonRenderer": { "serviceEndpoint": {
                            "createCommentEndpoint": { "createCommentParams": "new-comment" }
                        } } }
                    } } } },
                    { "commentViewModel": { "commentId": "id", "commentKey": "comment", "toolbarStateKey": "state", "toolbarSurfaceKey": "actions" } }
                ] }
            }],
            "frameworkUpdates": { "entityBatchUpdate": { "mutations": [
                { "entityKey": "comment", "payload": { "commentEntityPayload": {
                    "properties": { "content": { "content": "hello" } },
                    "toolbar": { "likeCountNotliked": "1", "likeCountLiked": "2" }
                } } },
                { "entityKey": "state", "payload": { "engagementToolbarStateEntityPayload": { "likeState": "TOOLBAR_LIKE_STATE_LIKED" } } },
                { "entityKey": "actions", "payload": { "engagementToolbarSurfaceEntityPayload": {
                    "likeCommand": { "performCommentActionEndpoint": { "action": "like" } },
                    "unlikeCommand": { "performCommentActionEndpoint": { "actions": ["unlike"] } },
                    "dislikeCommand": { "performCommentActionEndpoint": { "actions": ["dislike"] } },
                    "undislikeCommand": { "performCommentActionEndpoint": { "action": "undislike" } },
                    "replyCommand": { "openPopupAction": { "popup": { "commentReplyDialogRenderer": {
                        "replyButton": { "buttonRenderer": { "serviceEndpoint": {
                            "createCommentEndpoint": { "createCommentParams": "reply" }
                        } } }
                    } } } }
                } } }
            ] } }
        });
        let page = parse_comments(&response);
        assert_eq!(page.create_params.as_deref(), Some("new-comment"));
        let comment = &page.comments[0];
        assert_eq!(comment.id, "id");
        assert_eq!(comment.likes, "2");
        assert_eq!(comment.actions.reaction, Some(CommentReaction::Like));
        assert_eq!(comment.actions.like, ["like"]);
        assert_eq!(comment.actions.unlike, ["unlike"]);
        assert_eq!(comment.actions.dislike, ["dislike"]);
        assert_eq!(comment.actions.undislike, ["undislike"]);
        assert_eq!(comment.actions.reply.as_deref(), Some("reply"));
        assert!(page.account.is_none());

        for (state, expected) in [
            ("TOOLBAR_LIKE_STATE_DISLIKED", Some(CommentReaction::Dislike)),
            ("TOOLBAR_LIKE_STATE_INDIFFERENT", None),
        ] {
            *response.pointer_mut("/frameworkUpdates/entityBatchUpdate/mutations/1/payload/engagementToolbarStateEntityPayload/likeState")
                .unwrap() = json!(state);
            let reloaded = parse_comments(&response);
            assert_eq!(reloaded.comments[0].actions.reaction, expected);
            assert_eq!(reloaded.comments[0].likes, "1");
        }
    }

    fn lockup(id: &str, title: &str) -> Value {
        json!({ "lockupViewModel": {
            "contentId": id, "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
            "metadata": { "lockupMetadataViewModel": { "title": { "content": title } } }
        }})
    }

    #[test]
    fn ads_are_skipped() {
        let mut ad_lockup = lockup("adVid000001", "Buy now");
        ad_lockup["lockupViewModel"]["rendererContext"] = json!({ "loggingContext": { "adClientParams": { "x": 1 } } });
        let feed = json!({ "contents": [
            { "richItemRenderer": { "content": lockup("realVid0001", "A real video") } },
            // the home feed's in-feed ad slot
            { "richItemRenderer": { "content": { "adSlotRenderer": { "fulfillmentContent": { "fulfilledLayout": {
                "inFeedAdLayoutRenderer": { "renderingContent": lockup("adVid000002", "OLX") } } } } } } },
            // an ad drawn as a plain lockup, recognisable only by its ad fields
            { "richItemRenderer": { "content": ad_lockup } },
            // search: promoted video and a regular one marked with an ad badge
            { "searchPyvRenderer": { "ads": [{ "videoRenderer": { "videoId": "adVid000003", "title": { "simpleText": "Ad" } } }] } },
            { "videoRenderer": { "videoId": "adVid000004", "title": { "simpleText": "Sponsored" },
                "badges": [{ "metadataBadgeRenderer": { "style": "BADGE_STYLE_TYPE_AD", "label": "Ad" } }] } },
            { "videoRenderer": { "videoId": "realVid0002", "title": { "simpleText": "Another real one" },
                "badges": [{ "metadataBadgeRenderer": { "style": "BADGE_STYLE_TYPE_SIMPLE", "label": "New" } }] } },
        ]});
        let ids: Vec<String> = collect_items(&feed).items.iter().filter_map(|i| i.as_video().map(|v| v.id.clone())).collect();
        assert_eq!(ids, vec!["realVid0001", "realVid0002"]);
    }

    #[test]
    fn filter_encoding() {
        let f = SearchFilter { kind: 1, ..Default::default() };
        assert_eq!(f.encode().unwrap(), "EgIQAQ==");
        let f = SearchFilter { sort: 2, ..Default::default() };
        assert_eq!(f.encode().unwrap(), "CAI=");
    }

    #[test]
    fn search_continuation_reads_appended_results_and_next_token() {
        let response = json!({ "onResponseReceivedCommands": [
            { "unrelatedCommand": { "videoRenderer": { "videoId": "ignored" } } },
            { "appendContinuationItemsAction": { "continuationItems": [
                { "itemSectionRenderer": { "contents": [
                    { "videoRenderer": { "videoId": "page-two", "title": { "simpleText": "Second page" } } }
                ] } },
                { "continuationItemRenderer": { "continuationEndpoint": {
                    "continuationCommand": { "token": "page-three" }
                } } }
            ] } }
        ] });
        let page = search_continuation_page(&response);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].as_video().unwrap().id, "page-two");
        assert_eq!(page.continuation.as_deref(), Some("page-three"));
    }

    #[test]
    fn url_parse() {
        assert_eq!(parse_youtube_url("https://youtu.be/dQw4w9WgXcQ?t=1"), Some(ResolvedUrl::Video("dQw4w9WgXcQ".into())));
        assert_eq!(parse_youtube_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=x"), Some(ResolvedUrl::Video("dQw4w9WgXcQ".into())));
        assert_eq!(parse_youtube_url("https://www.youtube.com/shorts/oImytUi5y-w"), Some(ResolvedUrl::Video("oImytUi5y-w".into())));
    }

    fn description_run(start: u64, length: u64, command: Value) -> Value {
        json!({ "startIndex": start, "length": length, "onTap": { "innertubeCommand": command } })
    }

    #[test]
    fn description_metadata_keeps_link_labels_and_destinations() {
        let text = "🎬 Video | List | Channel";
        let runs = json!({ "commandRuns": [
            description_run(3, 5, json!({ "watchEndpoint": { "videoId": "_0do0jAoJt8", "playlistId": "PLExample", "startTimeSeconds": 42 } })),
            description_run(11, 4, json!({ "browseEndpoint": { "browseId": "VLPLExample" } })),
            description_run(18, 7, json!({ "browseEndpoint": { "browseId": "UCExample" } })),
        ] });
        let links = description_links(&runs, text);
        let links: Vec<_> = links.iter().map(|link| (&text[link.range.clone()], link.url.as_str())).collect();
        assert_eq!(links, [
            ("Video", "https://www.youtube.com/watch?v=_0do0jAoJt8&list=PLExample&t=42"),
            ("List", "https://www.youtube.com/playlist?list=PLExample"),
            ("Channel", "https://www.youtube.com/channel/UCExample"),
        ]);
    }

    #[test]
    fn description_metadata_ignores_invalid_and_overlapping_runs() {
        let text = "🎬 Video";
        let watch = json!({ "watchEndpoint": { "videoId": "_0do0jAoJt8" } });
        let runs = json!({ "commandRuns": [
            description_run(1, 1, watch.clone()),
            description_run(3, u64::MAX, watch.clone()),
            description_run(3, 0, watch.clone()),
            description_run(3, 5, json!({ "urlEndpoint": { "url": "file:///example" } })),
            description_run(3, 5, json!({ "browseEndpoint": { "browseId": "VLPLExample&extra=true" } })),
            description_run(4, 3, watch.clone()),
            description_run(3, 5, watch),
        ] });
        let links = description_links(&runs, text);
        assert_eq!(links.len(), 1);
        assert_eq!(&text[links[0].range.clone()], "Video");
    }

    #[test]
    #[ignore]
    fn live_search() {
        let mut page = search("rust programming", &SearchFilter::default()).unwrap();
        let mut seen = std::collections::HashSet::new();
        for number in 1..=3 {
            let before = seen.len();
            for video in page.items.iter().filter_map(Item::as_video).filter(|v| !v.is_short) {
                assert!(!video.title.is_empty());
                seen.insert(video.id.clone());
            }
            let added = seen.len() - before;
            println!("Page {number}: {added} new videos");
            assert!(added > 5);
            if number < 3 {
                page = search_continue(page.continuation.as_deref().unwrap()).unwrap();
            }
        }
    }

    #[test]
    #[ignore]
    fn live_watch() {
        let w = watch_next("dQw4w9WgXcQ", None).unwrap();
        println!("{} | {} | {:?} | {} related | likes {:?} | chapters {}", w.title, w.channel.name, w.channel.subscribers_text, w.related.len(), w.likes_text, w.chapters.len());
        assert!(!w.title.is_empty());
        assert!(w.related.len() > 3);
        let c = comments(w.comments_token.as_ref().unwrap()).unwrap();
        println!("{} comments, first: {:?}", c.comments.len(), c.comments.first());
        assert!(c.comments.len() > 5);
        let first_with_replies = c.comments.iter().find(|c| c.replies_token.is_some()).unwrap();
        let replies = comments(first_with_replies.replies_token.as_ref().unwrap()).unwrap();
        assert!(!replies.comments.is_empty());
    }

    #[test]
    #[ignore]
    fn live_channel_playlist() {
        let c = channel("UCsBjURrPoezykLs9EqgamOA", None).unwrap();
        println!("{:?} tabs={:?}", c.info, c.tabs.iter().map(|t| &t.title).collect::<Vec<_>>());
        let vt = c.tabs.iter().find(|t| t.kind == "videos").unwrap();
        let v = channel("UCsBjURrPoezykLs9EqgamOA", vt.params.as_deref()).unwrap();
        assert!(v.page.items.len() > 10);
        println!("{:#?}", v.page.items[0]);
        let p = playlist("PLBCF2DAC6FFB574DE").unwrap();
        println!("{:?} {}", p.info, p.videos.len());
        assert!(!p.videos.is_empty());
        for (name, id) in EXPLORE {
            let e = explore(id).map(|p| p.items.len()).unwrap_or(0);
            println!("explore {name}: {e}");
        }
    }
}

#[cfg(test)]
mod chapter_tests {
    use super::*;
    #[test]
    #[ignore]
    fn live_chapters() {
        for q in ["bayes theorem in artificial intelligence sudhakar atchala", "iphone 17 pro review", "rust tutorial full course"] {
            let p = search(q, &SearchFilter::default()).unwrap();
            for it in p.items.iter().take(4) {
                if let Item::Video(v) = it {
                    let w = watch_next(&v.id, None).unwrap();
                    println!("{} [{}] chapters={} first={:?}", v.title, v.id, w.chapters.len(), w.chapters.get(1).map(|c| (&c.title, c.start)));
                }
            }
        }
    }
}

#[cfg(test)]
mod tv_parse_tests {
    use super::*;
    #[test]
    #[ignore]
    fn parse_tv_file() {
        let path = std::env::var("TVJSON").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let p = collect_items(&v);
        for it in p.items.iter().take(6) {
            if let Item::Video(v) = it {
                println!("{} | {} | {:?} | {:?} | {:?} | {:?}", v.id, v.title, v.channel_name, v.channel_id, v.views_text, v.duration_text);
            }
        }
        println!("items={} cont={}", p.items.len(), p.continuation.is_some());
    }
}

// ───────────────────────── live chat ─────────────────────────

fn chat_text(msg: Option<&Value>) -> String {
    let Some(runs) = msg.and_then(|m| m.get("runs")).and_then(Value::as_array) else {
        return msg.map(text).unwrap_or_default();
    };
    let mut out = String::new();
    for r in runs {
        if let Some(t) = r.get("text").and_then(Value::as_str) {
            out.push_str(t);
        } else if let Some(e) = r.get("emoji") {
            let custom = e.get("isCustomEmoji").and_then(Value::as_bool).unwrap_or(false);
            let s = if custom {
                e.pointer("/shortcuts/0").and_then(Value::as_str)
            } else {
                e.get("emojiId").and_then(Value::as_str)
            };
            out.push_str(s.unwrap_or(""));
        }
    }
    out
}

fn chat_message(item: &Value) -> Option<ChatMessage> {
    let (r, paid) = if let Some(r) = item.get("liveChatTextMessageRenderer") {
        (r, None)
    } else if let Some(r) = item.get("liveChatPaidMessageRenderer") {
        (r, opt_text(r.get("purchaseAmountText")))
    } else {
        return None;
    };
    let mut role = ChatRole::Viewer;
    let mut verified = false;
    for b in r.get("authorBadges").and_then(Value::as_array).into_iter().flatten() {
        let b = b.get("liveChatAuthorBadgeRenderer").unwrap_or(b);
        match b.pointer("/icon/iconType").and_then(Value::as_str) {
            Some("OWNER") => role = ChatRole::Owner,
            Some("MODERATOR") if role != ChatRole::Owner => role = ChatRole::Moderator,
            Some("VERIFIED") => verified = true,
            None if b.get("customThumbnail").is_some() && role == ChatRole::Viewer => role = ChatRole::Member,
            _ => {}
        }
    }
    Some(ChatMessage {
        id: r.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
        author: opt_text(r.get("authorName")).unwrap_or_default(),
        author_photo: r.get("authorPhoto").and_then(best_thumb),
        text: chat_text(r.get("message")),
        role,
        verified,
        paid,
    })
}

/// One poll of a live chat. Reading needs no account.
pub fn live_chat(token: &str) -> Result<ChatBatch> {
    let r = call("live_chat/get_live_chat", json!({ "continuation": token }))?;
    let c = r.pointer("/continuationContents/liveChatContinuation").ok_or_else(|| anyhow!("chat has ended"))?;
    let mut batch = ChatBatch { timeout_ms: 5000, ..Default::default() };
    for a in c.get("actions").and_then(Value::as_array).into_iter().flatten() {
        if let Some(m) = a.pointer("/addChatItemAction/item").and_then(chat_message) {
            batch.messages.push(m);
        }
    }
    if let Some(next) = c.pointer("/continuations/0").and_then(Value::as_object).and_then(|o| o.values().next()) {
        batch.next = next.get("continuation").and_then(Value::as_str).map(str::to_string);
        batch.timeout_ms = next.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5000);
    }
    Ok(batch)
}

#[cfg(test)]
mod heatmap_tests {
    #[test]
    #[ignore]
    fn live_heatmap() {
        for id in ["dQw4w9WgXcQ", "kJQP7kiw5Fk", "LXb3EKWsInQ"] {
            let w = super::watch_next(id, None).unwrap();
            println!("{id}: heatmap={} chapters={}", w.heatmap.len(), w.chapters.len());
        }
    }
}

#[cfg(test)]
mod tile_tests {
    use serde_json::json;

    /// A show from the account's home feed: the tile's title is the show, the lines are the
    /// latest episode and the channel, and the badge counts episodes.
    #[test]
    fn a_show_tile_reads_as_its_latest_episode() {
        let tile = json!({
            "contentType": "TILE_CONTENT_TYPE_VIDEO",
            "contentId": "rTjvNXrxtas",
            "onSelectCommand": { "browseEndpoint": { "browseId": "VLPLgZ8mBDMf-VTsBkM3YatTRCblZjbIBN0Q" } },
            "header": { "tileHeaderRenderer": { "thumbnailOverlays": [
                { "thumbnailOverlayTimeStatusRenderer": { "text": { "runs": [{ "text": "13" }, { "text": " episodes" }] } } }
            ] } },
            "metadata": { "tileMetadataRenderer": {
                "title": { "simpleText": "Point" },
                "lines": [
                    { "lineRenderer": { "items": [{ "lineItemRenderer": { "text": { "simpleText": "Episode two" } } }] } },
                    { "lineRenderer": { "items": [{ "lineItemRenderer": { "text": { "simpleText": "Some Channel" } } }] } }
                ]
            } }
        });
        let v = super::parse_tile(&tile).unwrap();
        assert_eq!(v.title, "Episode two");
        assert_eq!(v.channel_name, "Some Channel");
        assert_eq!(v.views_text.as_deref(), Some("Point • 13 episodes"));
        assert!(v.duration_text.is_none());
        // opening the card shows the show's playlist, without the "VL" of the browse id
        assert_eq!(v.playlist_id.as_deref(), Some("PLgZ8mBDMf-VTsBkM3YatTRCblZjbIBN0Q"));
    }

    /// Some shows play their latest episode together with the show's own list.
    #[test]
    fn a_show_that_plays_its_own_list_keeps_that_list() {
        let line = |t: &str| json!({ "lineRenderer": { "items": [{ "lineItemRenderer": { "text": { "simpleText": t } } }] } });
        let tile = json!({
            "contentType": "TILE_CONTENT_TYPE_VIDEO",
            "contentId": "9DCrS24Zk-g",
            "onSelectCommand": { "watchEndpoint": { "videoId": "9DCrS24Zk-g", "playlistId": "OLa8Qyc44XfVYUHLvQqGvlBHDYTk1cx-n6Q" } },
            "header": { "tileHeaderRenderer": { "thumbnailOverlays": [
                { "thumbnailOverlayTimeStatusRenderer": { "text": { "runs": [{ "text": "49" }, { "text": " episodes" }] } } }
            ] } },
            "metadata": { "tileMetadataRenderer": { "title": { "simpleText": "A show" }, "lines": [line("Episode"), line("Channel")] } }
        });
        let v = super::parse_tile(&tile).unwrap();
        assert_eq!(v.title, "Episode");
        assert_eq!(v.channel_name, "Channel");
        assert_eq!(v.views_text.as_deref(), Some("A show • 49 episodes"));
        assert_eq!(v.playlist_id.as_deref(), Some("OLa8Qyc44XfVYUHLvQqGvlBHDYTk1cx-n6Q"));
    }

    /// An ordinary tile keeps its own title, channel, views and date.
    #[test]
    fn a_video_tile_is_unchanged() {
        let line = |t: &str| json!({ "lineRenderer": { "items": [{ "lineItemRenderer": { "text": { "simpleText": t } } }] } });
        let tile = json!({
            "contentType": "TILE_CONTENT_TYPE_VIDEO",
            "contentId": "abc",
            "onSelectCommand": { "watchEndpoint": { "videoId": "abc" } },
            "metadata": { "tileMetadataRenderer": { "title": { "simpleText": "A video" }, "lines": [line("Some Channel"), line("340K views"), line("4 days ago")] } }
        });
        let v = super::parse_tile(&tile).unwrap();
        assert_eq!(v.title, "A video");
        assert_eq!(v.channel_name, "Some Channel");
        assert_eq!(v.views_text.as_deref(), Some("340K views"));
        assert_eq!(v.published_text.as_deref(), Some("4 days ago"));
        assert!(v.playlist_id.is_none());
    }
}

#[cfg(test)]
mod long_playlist_tests {
    use super::*;

    fn video(id: &str) -> VideoItem {
        VideoItem { id: id.to_string(), ..Default::default() }
    }

    #[test]
    fn pager_drains_pages_and_deduplicates_videos() {
        let mut pager = PlaylistPager::new(Some("p2".into()), 10);
        let mut vids = vec![video("v1"), video("v2")];
        let pages: std::collections::HashMap<&str, (Vec<VideoItem>, Option<&str>)> = [
            ("p2", (vec![video("v2"), video("v3")], Some("p3"))),
            ("p3", (vec![video("v4")], None)),
        ]
        .into_iter()
        .collect();
        while let Some(tok) = pager.take_next() {
            let (batch, next) = pages.get(tok.as_str()).cloned().unwrap();
            append_unique_videos(&mut vids, batch);
            pager.advance(next.map(str::to_string));
        }
        let ids: Vec<&str> = vids.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, vec!["v1", "v2", "v3", "v4"]);
        assert_eq!(pager.next_token(), None);
    }

    #[test]
    fn pager_stops_on_duplicate_or_cyclic_tokens_and_empty_tokens() {
        let mut pager = PlaylistPager::new(Some("t1".into()), 10);
        assert_eq!(pager.take_next().as_deref(), Some("t1"));
        pager.advance(Some("t2".into()));
        assert_eq!(pager.take_next().as_deref(), Some("t2"));
        // cycle back to t1 must terminate immediately
        pager.advance(Some("t1".into()));
        assert_eq!(pager.next_token(), None);
        assert_eq!(pager.take_next(), None);

        let mut empty = PlaylistPager::new(Some(String::new()), 10);
        assert_eq!(empty.take_next(), None);
    }

    #[test]
    fn pager_respects_max_pages_cap() {
        let mut pager = PlaylistPager::new(Some("p1".into()), 2);
        assert_eq!(pager.take_next().as_deref(), Some("p1"));
        pager.advance(Some("p2".into()));
        assert_eq!(pager.take_next().as_deref(), Some("p2"));
        pager.advance(Some("p3".into()));
        assert_eq!(pager.next_token(), None);
        assert_eq!(pager.take_next(), None);
    }

    #[test]
    #[ignore = "network"]
    fn long_playlists_load_and_watch_next_sends_a_window() {
        let pid = "UUX6OQ3DkcsbYNE6H8uQQuVA";
        let mut page = super::playlist(pid).unwrap();
        let mut vids = page.videos.clone();
        while vids.len() < 420 {
            let Some(t) = page.continuation.take() else { break };
            let p = super::browse_continue(&t).unwrap();
            vids.extend(p.items.into_iter().filter_map(|i| i.as_video().cloned()));
            page.continuation = p.continuation;
        }
        eprintln!("loaded {}", vids.len());
        // playlists page past their first 100 videos
        assert!(vids.len() > 300, "only {} videos", vids.len());
        for at in [0usize, 150, 298, 350] {
            let Some(v) = vids.get(at) else { continue };
            let w = super::watch_next(&v.id, Some(pid)).unwrap();
            let items = w.playlist.map(|p| p.items).unwrap_or_default();
            let first = items.first().and_then(|f| vids.iter().position(|x| x.id == f.id));
            let last = items.last().and_then(|f| vids.iter().position(|x| x.id == f.id));
            let me = items.iter().position(|x| x.id == v.id);
            eprintln!("at {at}: window {} items, first={first:?} last={last:?} me={me:?}", items.len());
        }
    }
}

#[cfg(test)]
mod channel_tab_tests {
    use super::{Item, collect_items, json};

    #[test]
    fn channel_show_cards_keep_the_playlist_id_and_episode_count() {
        let page = collect_items(&json!({
            "richGridRenderer": {
                "contents": [{ "richItemRenderer": { "content": { "gridShowRenderer": {
                    "title": {"simpleText": "Creator show"},
                    "navigationEndpoint": { "browseEndpoint": { "browseId": "VLPLExampleShow" } },
                    "thumbnailRenderer": { "showCustomThumbnailRenderer": { "thumbnail": { "thumbnails": [{
                        "url": "https://i.ytimg.com/vi/example/hqdefault.jpg", "width": 480, "height": 270
                    }] } } },
                    "longBylineText": { "runs": [{ "text": "Creator" }] },
                    "thumbnailOverlays": [{ "thumbnailOverlayBottomPanelRenderer": {
                        "text": { "runs": [{ "text": "7" }, { "text": " episodes" }] }
                    } }]
                } } } }]
            }
        }));
        let [Item::Playlist(show)] = page.items.as_slice() else { panic!("missing show card") };
        assert_eq!(show.id, "PLExampleShow");
        assert_eq!(show.title, "Creator show");
        assert_eq!(show.channel_name.as_deref(), Some("Creator"));
        assert_eq!(show.video_count_text.as_deref(), Some("7 episodes"));
        assert_eq!(show.thumbnail.as_deref(), Some("https://i.ytimg.com/vi/example/hqdefault.jpg"));
    }

    #[test]
    #[ignore = "network"]
    fn channel_show_tabs_open_episode_lists() {
        for id in ["UCGGZ_POGmIWG1pQXTDzQv-g", "UCPD_bxCRGpmmeQcbe2kpPaA"] {
            let channel = super::channel(id, Some(crate::feed::VIDEOS_TAB)).unwrap();
            let tab = channel.tabs.iter().find(|tab| tab.kind == "shows").expect("shows tab");
            let page = super::channel(id, tab.params.as_deref()).unwrap();
            let show = page.page.items.iter().find_map(|item| {
                if let Item::Playlist(show) = item { Some(show) } else { None }
            }).expect("show card");
            let episodes = super::playlist(&show.id).unwrap().videos;
            eprintln!("{}: {} ({} episodes loaded)", channel.info.name, show.title, episodes.len());
            assert!(!episodes.is_empty());
        }
    }

    /// Tabs are told apart by kind in any content language.
    #[test]
    #[ignore = "network"]
    fn channel_tabs_have_kinds_in_other_languages() {
        crate::store::update_settings(|s| s.language = "tr".into());
        let page = super::channel("UCBJycsmduvYEL83R_U4JriQ", None).unwrap();
        crate::store::update_settings(|s| s.language = "en".into());
        let kinds: Vec<_> = page.tabs.iter().map(|t| (t.title.clone(), t.kind.clone())).collect();
        eprintln!("{kinds:?}");
        for k in ["featured", "videos", "shorts", "playlists"] {
            assert!(kinds.iter().any(|(_, kind)| kind == k), "no {k} tab in {kinds:?}");
        }
    }
}
