//! Google account sign-in using YouTube's TV device flow (the same method SmartTube
//! uses). The app opens youtube.com/activate in the default browser with the code
//! already filled in, so the user only picks an account and allows access. The
//! resulting token authorizes the TV Innertube client for the personal home feed,
//! subscriptions, likes and watch history.
use super::http;
use super::innertube::collect_items;
use super::types::*;
use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use url::Url;

// Public OAuth client of the YouTube TV app.
const CLIENT_ID: &str = "861556708454-d6dlm3lh05idd8npek18k6be8ba3oc68.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "SboVhoG9s0rNafixCSGGKXAT";
const SCOPE: &str = "http://gdata.youtube.com https://www.googleapis.com/auth/youtube-paid-content";
const TV_VERSION: &str = "7.20251120.13.00";
// YouTube's own activation page. Unlike google.com/device it also lets the user
// pick a brand account (a channel).
const ACTIVATE_URL: &str = "https://www.youtube.com/activate";
const TV_UA: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/25.lts.30.1034943-gold (unlike Gecko), Unknown_TV_Unknown_0/Unknown (Unknown, Unknown)";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub handle: Option<String>,
    pub avatar: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Stored {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    account: Option<Account>,
    #[serde(skip)]
    session: Arc<()>,
    #[serde(default)]
    history_key: Option<u64>,
}

#[derive(Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

impl DeviceCode {
    /// The activation page with the code filled in.
    pub fn sign_in_url(&self) -> Url {
        sign_in_url(&self.user_code)
    }

    /// Where to enter the code by hand, for another device.
    pub fn manual_url() -> &'static str {
        ACTIVATE_URL.trim_start_matches("https://www.")
    }
}

fn sign_in_url(user_code: &str) -> Url {
    let mut url = Url::parse(ACTIVATE_URL).expect("valid activation URL");
    url.query_pairs_mut().append_pair("user_code", user_code);
    url
}

fn state() -> &'static Mutex<Option<Stored>> {
    static S: std::sync::OnceLock<Mutex<Option<Stored>>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let mut stored: Option<Stored> = crate::store::kv_get("auth").and_then(|s| serde_json::from_str(&s).ok());
        if let Some(s) = stored.as_mut().filter(|s| s.history_key.is_none()) {
            s.history_key = Some(rand::random());
            persist(&stored);
        }
        Mutex::new(stored)
    })
}

fn persist(s: &Option<Stored>) {
    match s {
        Some(v) => crate::store::kv_set("auth", &serde_json::to_string(v).unwrap_or_default()),
        None => crate::store::kv_set("auth", ""),
    }
}

pub fn signed_in() -> bool {
    state().lock().as_ref().map(|s| !s.refresh_token.is_empty()).unwrap_or(false)
}

pub(crate) fn session_identity() -> Option<Arc<()>> {
    state()
        .lock()
        .as_ref()
        .filter(|s| !s.refresh_token.is_empty())
        .map(|s| s.session.clone())
}

pub(crate) fn is_current_session(session: &Arc<()>) -> bool {
    session_identity().is_some_and(|current| Arc::ptr_eq(&current, session))
}

pub fn account() -> Option<Account> {
    state().lock().as_ref().and_then(|s| s.account.clone())
}

pub fn sign_out() {
    let mut s = state().lock();
    *s = None;
    persist(&s);
    crate::api::streams::clear_cache();
}

fn oauth_post(url: &str, body: Value) -> Result<(u16, Value)> {
    http::post_json_status(url, &body, &[("User-Agent", TV_UA)])
}

#[cfg(test)]
mod sign_in_url_tests {
    use super::{DeviceCode, sign_in_url};

    #[test]
    fn opens_the_activation_page_with_the_code_filled_in() {
        assert_eq!(sign_in_url("HDZ-QKX-JSMD").as_str(), "https://www.youtube.com/activate?user_code=HDZ-QKX-JSMD");
        assert_eq!(sign_in_url("A&B C").as_str(), "https://www.youtube.com/activate?user_code=A%26B+C");
        assert_eq!(DeviceCode::manual_url(), "youtube.com/activate");
    }
}

/// Step 1: get a code for the user to approve at youtube.com/activate.
pub fn start_login() -> Result<DeviceCode> {
    let device_id: String = {
        use rand::Rng;
        let mut r = rand::rng();
        (0..32).map(|_| format!("{:x}", r.random_range(0..16u8))).collect()
    };
    let (_, v) = oauth_post(
        "https://www.youtube.com/o/oauth2/device/code",
        json!({ "client_id": CLIENT_ID, "scope": SCOPE, "device_id": device_id, "device_model": "ytlr::" }),
    )?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(DeviceCode {
        device_code: s("device_code").ok_or_else(|| anyhow!("no device code"))?,
        user_code: s("user_code").ok_or_else(|| anyhow!("no user code"))?,
        interval: v.get("interval").and_then(Value::as_u64).unwrap_or(5),
        expires_in: v.get("expires_in").and_then(Value::as_u64).unwrap_or(1800),
    })
}

pub enum PollResult {
    Pending,
    SlowDown,
    Approved(LoginGrant),
}

pub struct LoginGrant {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    account: Account,
}

impl LoginGrant {
    /// Activate only after the caller confirms this sign-in attempt is still open.
    pub fn activate(self) -> Account {
        let mut active = state().lock();
        *active = Some(Stored {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            account: Some(self.account.clone()),
            session: Arc::new(()),
            history_key: Some(rand::random()),
        });
        persist(&active);
        crate::api::streams::clear_cache();
        self.account
    }
}

/// Step 2: poll until the user has approved the code. The grant stays inactive until accepted by the app.
pub fn poll_login(dc: &DeviceCode) -> Result<PollResult> {
    let (_, v) = oauth_post(
        "https://www.youtube.com/o/oauth2/token",
        json!({
            "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET, "code": dc.device_code,
            "grant_type": "http://oauth.net/grant_type/device/1.0"
        }),
    )?;
    if let Some(err) = v.get("error").and_then(Value::as_str) {
        return match err {
            "authorization_pending" => Ok(PollResult::Pending),
            "slow_down" => Ok(PollResult::SlowDown),
            "expired_token" => Err(anyhow!("The code expired. Try again.")),
            "access_denied" => Err(anyhow!("Sign-in was cancelled.")),
            e => Err(anyhow!("Sign-in failed: {e}")),
        };
    }
    let access = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow!("no access token"))?;
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("Sign-in failed: no refresh token"))?;
    let expires = v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
    let expires_at = crate::api::json::now() + expires - 60;
    let account = fetch_account(access).unwrap_or_else(|_| Account { name: "YouTube account".into(), ..Default::default() });
    Ok(PollResult::Approved(LoginGrant {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        expires_at,
        account,
    }))
}

/// A valid access token, refreshed when needed.
pub fn access_token() -> Option<String> {
    let cur = state().lock().clone()?;
    access_token_for(cur)
}

fn access_token_for(cur: Stored) -> Option<String> {
    if cur.expires_at > crate::api::json::now() && !cur.access_token.is_empty() {
        return Some(cur.access_token);
    }
    let (status, v) = oauth_post(
        "https://www.youtube.com/o/oauth2/token",
        json!({
            "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET,
            "refresh_token": cur.refresh_token, "grant_type": "refresh_token"
        }),
    )
    .ok()?;
    if let Some(tok) = v.get("access_token").and_then(Value::as_str) {
        let exp = v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
        let mut s = state().lock();
        let st = s.as_mut().filter(|st| Arc::ptr_eq(&st.session, &cur.session))?;
        st.access_token = tok.to_string();
        st.expires_at = crate::api::json::now() + exp - 60;
        persist(&s);
        return Some(tok.to_string());
    }
    // the grant was revoked from the Google account page
    if status == 400 || status == 401 {
        let mut s = state().lock();
        if s.as_ref().is_some_and(|st| Arc::ptr_eq(&st.session, &cur.session)) {
            *s = None;
            persist(&s);
        }
    }
    None
}

fn tv_context() -> Value {
    let s = crate::store::settings_snapshot();
    json!({ "client": { "clientName": "TVHTML5", "clientVersion": TV_VERSION, "hl": s.language, "gl": s.region } })
}

/// Authorized call to the TV Innertube client.
pub fn tv_call(endpoint: &str, body: Value) -> Result<Value> {
    tv_call_for_session(endpoint, body, None)
}

fn tv_call_for_session(endpoint: &str, mut body: Value, session: Option<&Arc<()>>) -> Result<Value> {
    anyhow::ensure!(session.is_none_or(is_current_session), "YouTube account changed");
    let token = access_token().ok_or_else(|| anyhow!("not signed in"))?;
    anyhow::ensure!(session.is_none_or(is_current_session), "YouTube account changed");
    tv_call_with(&token, endpoint, body)
}

fn tv_call_with(token: &str, endpoint: &str, mut body: Value) -> Result<Value> {
    body["context"] = tv_context();
    let auth = format!("Bearer {token}");
    http::post_json(
        &format!("https://www.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"),
        &body,
        &[
            ("User-Agent", TV_UA),
            ("Authorization", &auth),
            ("X-YouTube-Client-Name", "7"),
            ("X-YouTube-Client-Version", TV_VERSION),
            ("Origin", "https://www.youtube.com"),
        ],
    )
}

#[derive(Clone)]
pub(super) struct HistorySession {
    identity: Arc<()>,
    pub(super) key: u64,
}

impl HistorySession {
    pub(super) fn current() -> Option<Self> {
        let active = state().lock();
        let s = active.as_ref().filter(|s| !s.refresh_token.is_empty())?;
        Some(Self { identity: s.session.clone(), key: s.history_key? })
    }

    pub(super) fn if_current<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let active = state().lock();
        active.as_ref().filter(|s| Arc::ptr_eq(&s.session, &self.identity)).map(|_| f())
    }

    pub(super) fn access_token(&self) -> Result<String> {
        let session = state().lock().as_ref().filter(|s| Arc::ptr_eq(&s.session, &self.identity)).cloned();
        session.and_then(access_token_for).ok_or_else(|| anyhow!("not signed in"))
    }

    pub(super) fn call(&self, endpoint: &str, body: Value) -> Result<Value> {
        let token = self.access_token()?;
        anyhow::ensure!(self.if_current(|| ()).is_some(), "YouTube history account changed");
        tv_call_with(&token, endpoint, body)
    }
}

fn fetch_account(token: &str) -> Result<Account> {
    let r = tv_call_with(token, "account/accounts_list", json!({ "accountReadMask": { "returnOwner": true } }))?;
    let item = crate::api::json::find(&r, "accountItem").or_else(|| crate::api::json::find(&r, "accountItemRenderer")).unwrap_or(&r);
    let name = item
        .get("accountName")
        .map(crate::api::json::text)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "YouTube account".into());
    let handle = item.get("channelHandle").map(crate::api::json::text).filter(|s| !s.is_empty());
    let avatar = item.get("accountPhoto").and_then(crate::api::json::best_thumb);
    Ok(Account { name, handle, avatar })
}

/// Channel id -> avatar url, kept across launches. TV feed tiles carry no
/// channel avatar, so they are filled in from here.
fn avatar_cache() -> &'static parking_lot::Mutex<std::collections::HashMap<String, String>> {
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let saved = crate::store::kv_get("channel_avatar_cache").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        parking_lot::Mutex::new(saved)
    })
}

/// Keys in the avatar cache: "id:<channel id>" -> avatar url and
/// "name:<lowercase name>" -> "<channel id>|<avatar url>".
fn name_key(n: &str) -> String {
    format!("name:{}", n.trim().to_lowercase())
}

fn fill_avatars(page: &mut Page) {
    let cache = avatar_cache();
    // what still needs a lookup: by id when we have one, otherwise by name
    let (by_id, by_name): (Vec<String>, Vec<String>) = {
        let mut c = cache.lock();
        for s in crate::store::subscriptions() {
            if let Some(a) = s.avatar {
                c.entry(format!("id:{}", s.channel_id)).or_insert_with(|| a.clone());
                c.entry(name_key(&s.name)).or_insert(format!("{}|{a}", s.channel_id));
            }
        }
        let (mut by_id, mut by_name) = (Vec::new(), Vec::new());
        for it in &page.items {
            let Item::Video(v) = it else { continue };
            match (&v.channel_id, &v.channel_avatar) {
                (Some(id), Some(a)) => {
                    c.entry(format!("id:{id}")).or_insert_with(|| a.clone());
                }
                (Some(id), None) if !c.contains_key(&format!("id:{id}")) && !by_id.contains(id) => by_id.push(id.clone()),
                (None, _) if !v.channel_name.is_empty() && !c.contains_key(&name_key(&v.channel_name)) && !by_name.contains(&v.channel_name) => {
                    by_name.push(v.channel_name.clone())
                }
                _ => {}
            }
        }
        (by_id, by_name)
    };
    if !by_id.is_empty() || !by_name.is_empty() {
        let found: Vec<(String, String)> = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for id in &by_id {
                handles.push(s.spawn(move || {
                    crate::api::innertube::channel_avatar(id).ok().flatten().map(|a| vec![(format!("id:{id}"), a)]).unwrap_or_default()
                }));
            }
            for name in &by_name {
                handles.push(s.spawn(move || match crate::api::innertube::channel_by_name(name) {
                    Ok(Some((id, Some(a)))) => vec![(name_key(name), format!("{id}|{a}")), (format!("id:{id}"), a)],
                    // remember misses too, so the same name isn't searched on every load
                    Ok(_) => vec![(name_key(name), String::new())],
                    Err(_) => vec![],
                }));
            }
            handles.into_iter().filter_map(|h| h.join().ok()).flatten().collect()
        });
        if !found.is_empty() {
            let mut c = cache.lock();
            // kept across launches, so bounded
            if c.len() > 5000 {
                c.retain(|k, v| k.starts_with("id:") && !v.is_empty());
                if c.len() > 5000 {
                    c.clear();
                }
            }
            c.extend(found);
            if let Ok(s) = serde_json::to_string(&*c) {
                crate::store::kv_set("channel_avatar_cache", &s);
            }
        }
    }
    let c = cache.lock();
    for it in page.items.iter_mut() {
        let Item::Video(v) = it else { continue };
        if v.channel_avatar.is_some() {
            continue;
        }
        if let Some(id) = &v.channel_id {
            v.channel_avatar = c.get(&format!("id:{id}")).cloned();
        } else if let Some((id, a)) = c.get(&name_key(&v.channel_name)).and_then(|e| e.split_once('|')) {
            v.channel_id = Some(id.to_string());
            v.channel_avatar = Some(a.to_string());
        }
    }
}

/// Videos only, deduplicated, in feed order, with channel avatars filled in.
fn videos(page: Page) -> Page {
    let mut page = videos_only(page);
    fill_avatars(&mut page);
    page
}

fn videos_only(page: Page) -> Page {
    let mut seen = std::collections::HashSet::new();
    Page {
        items: page
            .items
            .into_iter()
            .filter_map(|i| match i {
                Item::Video(mut v) if seen.insert(v.id.clone()) => {
                    if v.published_ts.is_none() {
                        v.published_ts = v.published_text.as_deref().and_then(crate::api::json::parse_relative_time);
                    }
                    Some(Item::Video(v))
                }
                _ => None,
            })
            .collect(),
        continuation: page.continuation,
    }
}

/// The signed-in user's personal home feed (YouTube's own recommendations).
pub fn home() -> Result<Page> {
    let r = tv_call("browse", json!({ "browseId": "FEwhat_to_watch" }))?;
    Ok(videos(collect_items(&r)))
}

/// Brings the account's recent searches (made on the phone, TV or web) into the
/// local search history. YouTube offers them as the suggestions for an empty
/// query when the request is signed in; signed out the list is empty.
pub fn sync_search_history() -> Result<usize> {
    if !crate::store::settings_snapshot().search_history_enabled {
        return Ok(0);
    }
    let session = HistorySession::current().ok_or_else(|| anyhow!("not signed in"))?;
    let token = session.access_token()?;
    let auth = format!("Bearer {token}");
    let hl = crate::store::settings_snapshot().language.clone();
    let url = format!("https://suggestqueries-clients6.youtube.com/complete/search?client=youtube-lr&ds=yt&xhr=t&q=&hl={}", http::urlencode(&hl));
    let body = http::get_bytes(&url, &[("Authorization", &auth), ("User-Agent", TV_UA)])?;
    let v: Value = serde_json::from_slice(&body)?;
    let queries: Vec<String> = v
        .get(1)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get(0).or(Some(e)).and_then(Value::as_str).map(str::to_string))
        .filter(|q| !q.trim().is_empty())
        .collect();
    // oldest first so the most recent remote search sorts highest among them
    let base = chrono::Utc::now().timestamp() - 7 * 86400;
    session.if_current(|| {
        queries.iter().enumerate()
            .filter(|(i, q)| crate::store::search_history_merge_remote(q, base - *i as i64 * 60))
            .count()
    }).ok_or_else(|| anyhow!("YouTube search history account changed"))
}

/// The signed-in user's subscriptions feed.
pub fn subscriptions_feed() -> Result<Page> {
    let r = tv_call("browse", json!({ "browseId": "FEsubscriptions" }))?;
    Ok(videos(collect_items(&r)))
}

/// The account's music picks: the Music tab of YouTube's TV app, which is
/// personal when signed in. Each video opens as its YouTube Mix (radio), so it
/// plays on into similar songs.
pub fn music() -> Result<Page> {
    let r = tv_call("browse", json!({ "browseId": "FEtopics_music" }))?;
    Ok(as_mixes(videos(collect_items(&r))))
}

pub fn music_continue(token: &str) -> Result<Page> {
    let r = tv_call("browse", json!({ "continuation": token }))?;
    Ok(as_mixes(videos(collect_items(&r))))
}

/// Opens each video with its YouTube Mix ("RD" + video id) queued after it.
fn as_mixes(mut page: Page) -> Page {
    for item in &mut page.items {
        if let Item::Video(v) = item {
            v.playlist_id = Some(format!("RD{}", v.id));
        }
    }
    page
}

pub fn browse_continue(token: &str) -> Result<Page> {
    let r = tv_call("browse", json!({ "continuation": token }))?;
    Ok(videos(collect_items(&r)))
}

/// Pulls a "UC…" channel id out of base64 protobuf browse params.
fn channel_id_from_params(params: &str) -> Option<String> {
    use base64::Engine;
    let p = params.replace("%3D", "=").replace("%3d", "=");
    let p = p.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(p)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(p))
        .ok()?;
    let s = String::from_utf8_lossy(&bytes);
    let i = s.find("UC")?;
    let id: String = s[i..].chars().take(24).collect();
    (id.len() == 24 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then_some(id)
}

/// Channels the account is subscribed to (the per-channel tabs of the TV subscriptions page).
pub fn subscribed_channels() -> Result<Vec<ChannelItem>> {
    let r = tv_call("browse", json!({ "browseId": "FEsubscriptions" }))?;
    let mut tabs = Vec::new();
    crate::api::json::find_all(&r, "tabRenderer", &mut tabs);
    let mut out: Vec<ChannelItem> = Vec::new();
    for t in tabs {
        let Some(ep) = t.pointer("/endpoint/browseEndpoint").or_else(|| crate::api::json::find(t, "browseEndpoint")) else { continue };
        // older responses link straight to the channel; newer ones keep the
        // channel id inside the protobuf `params` of an FEsubscriptions link
        let id = ep
            .get("browseId")
            .and_then(Value::as_str)
            .filter(|b| b.starts_with("UC"))
            .map(str::to_string)
            .or_else(|| ep.get("params").and_then(Value::as_str).and_then(channel_id_from_params));
        let Some(id) = id else { continue };
        if out.iter().any(|c| c.id == id) {
            continue;
        }
        let name = t.get("title").map(crate::api::json::text).unwrap_or_default();
        let avatar = t.get("thumbnail").and_then(crate::api::json::best_thumb);
        out.push(ChannelItem { id: id.to_string(), name, avatar, ..Default::default() });
    }
    Ok(out)
}

/// The account's subscriptions feed, following a few continuations.
pub fn subscriptions_videos(max_pages: usize) -> Result<(Vec<VideoItem>, Option<String>)> {
    let mut page = subscriptions_feed()?;
    let mut out: Vec<VideoItem> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for n in 0..max_pages {
        out.extend(page.items.drain(..).filter_map(|i| match i {
            Item::Video(v) if seen.insert(v.id.clone()) => Some(v),
            _ => None,
        }));
        match page.continuation.as_deref() {
            Some(tok) if n + 1 < max_pages => match browse_continue(tok) {
                Ok(p) => page = p,
                Err(_) => break,
            },
            _ => break,
        }
    }
    Ok((out, page.continuation))
}

/// Copies the account's subscriptions into the local list. Returns how many were new.
pub fn sync_subscriptions() -> Result<usize> {
    let chans = subscribed_channels()?;
    let mut added = 0;
    for c in chans {
        if !crate::store::is_subscribed(&c.id) {
            added += 1;
        }
        crate::store::subscribe(&c.id, &c.name, c.avatar.as_deref());
    }
    Ok(added)
}

pub fn like(video_id: &str, kind: &str) -> Result<()> {
    // kind: "like" | "dislike" | "removelike"
    tv_call(&format!("like/{kind}"), json!({ "target": { "videoId": video_id } }))?;
    Ok(())
}

pub fn subscribe(channel_id: &str, on: bool) -> Result<()> {
    let ep = if on { "subscription/subscribe" } else { "subscription/unsubscribe" };
    tv_call(ep, json!({ "channelIds": [channel_id] }))?;
    Ok(())
}

/// The signed-in account's playlists (own and saved), incl. Watch later and Liked.
pub fn my_playlists() -> Result<Vec<PlaylistItem>> {
    let r = tv_call("browse", json!({ "browseId": "FEplaylist_aggregation" }))?;
    let mut tiles = Vec::new();
    crate::api::json::find_all(&r, "tileRenderer", &mut tiles);
    let mut out: Vec<PlaylistItem> = Vec::new();
    for t in tiles {
        if t.get("contentType").and_then(Value::as_str) != Some("TILE_CONTENT_TYPE_PLAYLIST") {
            continue;
        }
        let Some(id) = t.get("contentId").and_then(Value::as_str) else { continue };
        if out.iter().any(|p| p.id == id) {
            continue;
        }
        let md = t.pointer("/metadata/tileMetadataRenderer");
        let title = md.and_then(|m| crate::api::json::opt_text(m.get("title"))).unwrap_or_default();
        let mut lines = Vec::new();
        if let Some(m) = md {
            crate::api::json::find_all(m, "lineItemRenderer", &mut lines);
        }
        let sub: Vec<String> = lines.iter().filter_map(|l| crate::api::json::opt_text(l.get("text"))).filter(|s| s.trim() != "•").collect();
        let thumb = t
            .pointer("/header/tileHeaderRenderer/thumbnail/thumbnails")
            .and_then(Value::as_array)
            .and_then(|a| a.last())
            .and_then(|x| x.get("url"))
            .and_then(Value::as_str)
            .map(|u| if u.starts_with("//") { format!("https:{u}") } else { u.to_string() });
        out.push(PlaylistItem {
            id: id.to_string(),
            title,
            thumbnail: thumb,
            video_count_text: if sub.is_empty() { None } else { Some(sub.join(" • ")) },
            ..Default::default()
        });
    }
    Ok(out)
}

/// A playlist as the signed-in account sees it (works for private / unlisted ones).
pub fn playlist(playlist_id: &str) -> Result<PlaylistPage> {
    let r = tv_call("browse", json!({ "browseId": format!("VL{playlist_id}") }))?;
    let page = videos_only(collect_items(&r));
    let mut info = PlaylistItem { id: playlist_id.to_string(), ..Default::default() };
    if let Some(m) = crate::api::json::find(&r, "entityMetadataRenderer") {
        info.title = crate::api::json::opt_text(m.get("title")).unwrap_or_default();
        let mut lines = Vec::new();
        crate::api::json::find_all(m, "lineItemRenderer", &mut lines);
        let parts: Vec<String> = lines.iter().filter_map(|l| crate::api::json::opt_text(l.get("text"))).filter(|s| s.trim() != "•").collect();
        if let Some(c) = parts.iter().find(|p| p.to_lowercase().contains("video")) {
            info.video_count_text = Some(c.clone());
        }
        if let Some(o) = parts.first().filter(|p| !p.to_lowercase().contains("video")) {
            info.channel_name = Some(o.clone());
        }
    }
    let videos: Vec<VideoItem> = page
        .items
        .into_iter()
        .filter_map(|i| match i {
            Item::Video(v) => Some(v),
            _ => None,
        })
        .collect();
    info.thumbnail = videos.first().map(|v| v.thumbnail.clone());
    info.first_video_id = videos.first().map(|v| v.id.clone());
    Ok(PlaylistPage { info, description: None, videos, continuation: page.continuation })
}

/// Adds a video to one of the signed-in account's playlists.
pub fn add_to_playlist(playlist_id: &str, video_id: &str) -> Result<()> {
    edit_playlist(playlist_id, json!([{ "action": "ACTION_ADD_VIDEO", "addedVideoId": video_id }]), None)
}

pub(crate) fn set_account_playlist_saved(session: &Arc<()>, playlist_id: &str, video_id: &str, saved: bool) -> Result<()> {
    let action = if saved {
        json!({ "action": "ACTION_ADD_VIDEO", "addedVideoId": video_id })
    } else {
        json!({ "action": "ACTION_REMOVE_VIDEO_BY_VIDEO_ID", "removedVideoId": video_id })
    };
    edit_playlist(playlist_id, json!([action]), Some(session))
}

fn edit_playlist(playlist_id: &str, actions: Value, session: Option<&Arc<()>>) -> Result<()> {
    let r = tv_call_for_session(
        "browse/edit_playlist",
        json!({ "playlistId": playlist_id, "actions": actions }),
        session,
    )?;
    if let Some(s) = r.get("status").and_then(Value::as_str).filter(|s| *s != "STATUS_SUCCEEDED") {
        return Err(anyhow!("edit_playlist: {s}"));
    }
    Ok(())
}

/// Removes several videos from one of the signed-in account's playlists.
pub fn remove_many_from_playlist(playlist_id: &str, video_ids: &[String]) -> Result<()> {
    for chunk in video_ids.chunks(50) {
        let actions: Vec<Value> = chunk.iter().map(|id| json!({ "action": "ACTION_REMOVE_VIDEO_BY_VIDEO_ID", "removedVideoId": id })).collect();
        edit_playlist(playlist_id, json!(actions), None)?;
    }
    Ok(())
}

pub(crate) fn account_playlist_contains(session: &Arc<()>, playlist_id: &str, video_id: &str) -> Result<bool> {
    let mut query = json!({ "browseId": format!("VL{playlist_id}") });
    let mut cursors = std::collections::HashSet::new();
    loop {
        let response = tv_call_for_session("browse", query, Some(session))?;
        let page = collect_items(&response);
        if page.items.iter().any(|item| matches!(item, Item::Video(video) if video.id == video_id)) {
            return Ok(true);
        }
        let Some(token) = page.continuation else { return Ok(false) };
        anyhow::ensure!(cursors.insert(token.clone()), "YouTube repeated the playlist continuation");
        query = json!({ "continuation": token });
    }
}

/// Posts a message to a live stream's chat as the signed-in account.
pub fn send_live_chat(channel_id: &str, video_id: &str, message: &str) -> Result<()> {
    // protobuf { 1: { 5: { 1: channel, 2: video } }, 2: 1, 3: 4 }
    fn field(out: &mut Vec<u8>, tag: u8, bytes: &[u8]) {
        out.push(tag);
        let mut n = bytes.len();
        while n >= 0x80 {
            out.push((n as u8 & 0x7f) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
        out.extend_from_slice(bytes);
    }
    use base64::Engine;
    let mut ids = Vec::new();
    field(&mut ids, 0x0a, channel_id.as_bytes());
    field(&mut ids, 0x12, video_id.as_bytes());
    let mut inner = Vec::new();
    field(&mut inner, 0x2a, &ids);
    let mut buf = Vec::new();
    field(&mut buf, 0x0a, &inner);
    buf.extend_from_slice(&[0x10, 0x01, 0x18, 0x04]);
    let b64 = base64::engine::general_purpose::STANDARD;
    let params = b64.encode(crate::api::http::urlencode(&b64.encode(&buf)));
    let client_id = format!("st{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let r = tv_call(
        "live_chat/send_message",
        json!({ "params": params, "clientMessageId": client_id, "richMessage": { "textSegments": [{ "text": message }] } }),
    )?;
    if let Some(err) = crate::api::json::find(&r, "liveChatRestrictedParticipationRenderer").or_else(|| crate::api::json::find(&r, "errorMessage")) {
        let t = crate::api::json::text(err.get("message").unwrap_or(err));
        anyhow::bail!(if t.is_empty() { "YouTube did not accept the message".to_string() } else { t });
    }
    Ok(())
}
