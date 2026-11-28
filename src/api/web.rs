//! Browser sign-in: the YouTube website's own session cookies.
//!
//! The TV sign-in reads the account (feeds, history, likes) and, through the TV
//! client in `streams`, plays videos as the account when a JavaScript runtime is
//! installed. Without one, the website's cookies are the way to report playback
//! the way youtube.com does: the video lands in the account's watch history and
//! the position you stopped at is what other devices resume from.
//!
//! Signing in happens in a real browser (Edge, Chrome, Brave or Chromium) started
//! with its own empty profile. The app reads that profile's youtube.com cookies over
//! the browser's local debugging pipe once sign-in is done, then closes the browser
//! and deletes the profile. Nothing else from the user's browsers is read.
use super::http;
use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

pub(super) const WEB_VERSION: &str = "2.20251120.01.00";
const ORIGIN: &str = "https://www.youtube.com";
const SIGN_IN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&uilel=3&passive=true&continue=https%3A%2F%2Fwww.youtube.com%2Fsignin%3Faction_handle_signin%3Dtrue%26app%3Ddesktop%26next%3Dhttps%253A%252F%252Fwww.youtube.com%252F";

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct Session {
    cookies: BTreeMap<String, String>,
    account: Option<String>,
    #[serde(default)]
    page_id: Option<String>,
    #[serde(default)]
    history_key: Option<u64>,
    #[serde(skip)]
    identity: Arc<()>,
}

fn session() -> &'static Mutex<Option<Arc<Session>>> {
    static S: OnceLock<Mutex<Option<Arc<Session>>>> = OnceLock::new();
    S.get_or_init(|| {
        let s: Option<Session> = crate::store::kv_get("web_session").and_then(|s| serde_json::from_str(&s).ok());
        let s = s.filter(|s| has_login(&s.cookies)).map(|mut s| {
            if s.history_key.is_none() {
                s.history_key = Some(rand::random());
                save(Some(&s));
            }
            Arc::new(s)
        });
        Mutex::new(s)
    })
}

/// Safari's YouTube connection for watch history and comment reactions.
/// It lasts only for the current Stream Tube session.
#[cfg(target_os = "macos")]
static SAFARI_SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);

fn save(s: Option<&Session>) {
    crate::store::kv_set("web_session", &s.map(|s| serde_json::to_string(s).unwrap_or_default()).unwrap_or_default());
}

fn has_login(c: &BTreeMap<String, String>) -> bool {
    has_login_names(|name| c.contains_key(name))
}

fn has_login_names(has: impl Fn(&str) -> bool) -> bool {
    (has("SAPISID") || has("__Secure-3PAPISID")) && (has("SID") || has("__Secure-3PSID"))
}

/// Whether the browser sign-in is connected.
pub fn connected() -> bool {
    session().lock().is_some()
}

/// The account name the browser session belongs to.
pub fn account_name() -> Option<String> {
    connected_account_session().and_then(|s| s.account.clone())
}

pub fn disconnect() {
    #[cfg(target_os = "macos")]
    SAFARI_SESSION.lock().take();
    let mut s = session().lock();
    *s = None;
    save(None);
}

/// Stores a set of youtube.com cookies and checks they belong to a signed-in account.
fn connect(cookies: BTreeMap<String, String>, page_id: Option<String>) -> Result<String> {
    let (candidate, name) = verify_session(cookies, page_id)?;
    #[cfg(target_os = "macos")]
    SAFARI_SESSION.lock().take();
    let mut s = session().lock();
    save(Some(&candidate));
    *s = Some(Arc::new(candidate));
    Ok(name)
}

#[cfg(target_os = "macos")]
pub(super) fn connect_safari(cookies: &[Value], cancel: &AtomicBool) -> Result<String> {
    let (candidate, name) = verify_session(youtube_cookies(cookies), None)?;
    let mut current = SAFARI_SESSION.lock();
    if cancel.load(Ordering::Acquire) {
        return Err(anyhow!("Safari sign-in cancelled"));
    }
    *current = Some(Arc::new(candidate));
    Ok(name)
}

/// Checks the cookies belong to a signed-in account, without replacing the current session.
fn verify_session(cookies: BTreeMap<String, String>, page_id: Option<String>) -> Result<(Session, String)> {
    if !has_login(&cookies) {
        return Err(anyhow!("These cookies aren't from a signed-in YouTube session."));
    }
    let mut candidate = Session { cookies, page_id, history_key: Some(rand::random()), ..Default::default() };
    let name = fetch_account_name(&candidate).context("YouTube didn't accept the session")?;
    candidate.account = Some(name.clone());
    Ok((candidate, name))
}

/// Writes the browser sign-in's youtube.com cookies as a Netscape cookies.txt for
/// yt-dlp, in the private profile folder under a name of its own. The caller
/// deletes it when yt-dlp is done. None when the browser sign-in isn't connected.
pub fn write_cookies_file() -> Option<PathBuf> {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(0);
    let text = {
        let s = session().lock();
        let s = s.as_ref()?;
        let mut t = String::from("# Netscape HTTP Cookie File\n");
        for (k, v) in &s.cookies {
            // domain, include subdomains, path, secure, expiry, name, value
            t.push_str(&format!(".youtube.com\tTRUE\t/\tTRUE\t2147483647\t{k}\t{v}\n"));
        }
        t
    };
    let path = crate::store::data_dir().join(format!("yt-dlp-cookies-{}-{}.txt", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    if opts.open(&path).ok()?.write_all(text.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

/// Deletes cookie files a crashed run left behind (they hold the sign-in).
pub fn sweep_cookie_files() {
    let Ok(dir) = std::fs::read_dir(crate::store::data_dir()) else { return };
    for e in dir.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("yt-dlp-cookies-").filter(|_| name.ends_with(".txt")) else { continue };
        // a file of another window that is still running stays
        let pid = rest.split('-').next().and_then(|p| p.parse::<u32>().ok());
        if pid.is_none_or(|p| p == std::process::id() || !crate::links::process_alive(p)) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn cookie_header(session: &Session) -> String {
    session.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
}

fn sha1_hex(s: &str) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The `Authorization` header youtube.com sends with its API calls.
fn sapisid_auth(session: &Session) -> Option<String> {
    let c = &session.cookies;
    let ts = chrono::Utc::now().timestamp();
    let parts: Vec<String> = [("SAPISIDHASH", "SAPISID"), ("SAPISID1PHASH", "__Secure-1PAPISID"), ("SAPISID3PHASH", "__Secure-3PAPISID")]
        .iter()
        .filter_map(|(scheme, name)| {
            let v = c.get(*name).or_else(|| if *name == "SAPISID" { c.get("__Secure-3PAPISID") } else { None })?;
            Some(format!("{scheme} {ts}_{}", sha1_hex(&format!("{ts} {v} {ORIGIN}"))))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// A call to the YouTube website's API as the signed-in user.
pub(crate) fn session_identity() -> Option<Arc<()>> {
    session().lock().as_ref().map(|connected| connected.identity.clone())
}

pub(crate) fn is_current_session(identity: &Arc<()>) -> bool {
    session_identity().is_some_and(|current| Arc::ptr_eq(&current, identity))
}

pub(crate) fn web_call_for_session(identity: &Arc<()>, endpoint: &str, body: Value) -> Result<Value> {
    let connected = session().lock().clone().filter(|connected| Arc::ptr_eq(&connected.identity, identity))
        .ok_or_else(|| anyhow!("browser session changed"))?;
    web_call_with_session(&connected, endpoint, body)
}

pub(crate) fn comment_session_identity() -> Option<Arc<()>> {
    connected_account_session().map(|session| session.identity.clone())
}

pub(crate) fn is_current_comment_session(identity: &Arc<()>) -> bool {
    comment_session_identity().is_some_and(|current| Arc::ptr_eq(&current, identity))
}

pub(super) enum CommentRequest {
    Load,
    React,
}

pub(super) fn comment_call_for_session(identity: &Arc<()>, request: CommentRequest, body: Value) -> Result<Value> {
    let connected = connected_account_session().filter(|connected| Arc::ptr_eq(&connected.identity, identity))
        .ok_or_else(|| anyhow!("comment session changed"))?;
    let endpoint = match request {
        CommentRequest::Load => "next",
        CommentRequest::React => "comment/perform_comment_action",
    };
    web_call_with_session(&connected, endpoint, body)
}

pub fn web_call(endpoint: &str, body: Value) -> Result<Value> {
    let connected = active_session().ok_or_else(|| anyhow!("browser sign-in not connected"))?;
    web_call_with_session(&connected, endpoint, body)
}

fn web_call_with_session(session: &Session, endpoint: &str, mut body: Value) -> Result<Value> {
    let cookie = cookie_header(session);
    let auth = sapisid_auth(session).ok_or_else(|| anyhow!("browser sign-in not connected"))?;
    let st = crate::store::settings_snapshot();
    body["context"] = json!({ "client": { "clientName": "WEB", "clientVersion": WEB_VERSION, "hl": st.language, "gl": st.region } });
    let mut headers: Vec<(&str, &str)> = vec![
        ("Cookie", &cookie),
        ("Authorization", &auth),
        ("Origin", ORIGIN),
        ("X-Origin", ORIGIN),
        ("Referer", "https://www.youtube.com/"),
        ("X-Goog-AuthUser", "0"),
        ("X-Youtube-Bootstrap-Logged-In", "true"),
        ("X-Youtube-Client-Name", "1"),
        ("X-Youtube-Client-Version", WEB_VERSION),
    ];
    add_page_id(&mut headers, session);
    let (status, v) = http::post_session_json(
        &format!("https://www.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"), &body, &headers,
    )?;
    if status == 401 || status == 403 {
        return Err(anyhow!("YouTube refused the browser session (HTTP {status}). Connect it again."));
    }
    if !(200..300).contains(&status) {
        return Err(anyhow!("HTTP {status}"));
    }
    anyhow::ensure!(v.is_object(), "invalid YouTube response");
    Ok(v)
}

fn valid_page_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|byte| byte.is_ascii_digit())
}

fn add_page_id<'a>(headers: &mut Vec<(&'static str, &'a str)>, session: &'a Session) {
    if let Some(id) = session.page_id.as_deref().filter(|id| valid_page_id(id)) {
        headers.push(("X-Goog-PageId", id));
    }
}

fn fetch_account_name(session: &Session) -> Result<String> {
    let r = web_call_with_session(session, "account/account_menu", json!({}))?;
    let header = crate::api::json::find(&r, "activeAccountHeaderRenderer").ok_or_else(|| anyhow!("not signed in"))?;
    let name = header.get("accountName").map(crate::api::json::text).filter(|s| !s.is_empty());
    let handle = header.get("channelHandle").map(crate::api::json::text).filter(|s| !s.is_empty());
    name.or(handle).ok_or_else(|| anyhow!("not signed in"))
}

fn active_session() -> Option<Arc<Session>> {
    session().lock().clone()
}

/// Watch history and comment reactions use the same connected YouTube account.
fn connected_account_session() -> Option<Arc<Session>> {
    #[cfg(target_os = "macos")]
    if let Some(safari) = SAFARI_SESSION.lock().clone() {
        return Some(safari);
    }
    active_session()
}

/// Runs `f` while no browser session is connected for the watch history.
pub(super) fn if_disconnected<T>(f: impl FnOnce() -> T) -> Option<T> {
    #[cfg(target_os = "macos")]
    let safari = SAFARI_SESSION.lock();
    #[cfg(target_os = "macos")]
    if safari.is_some() {
        return None;
    }
    let active = session().lock();
    active.is_none().then(f)
}

#[derive(Clone)]
pub(super) struct HistorySession(Arc<Session>);

impl HistorySession {
    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self(Arc::new(Session::default()))
    }

    pub(super) fn current() -> Option<Self> {
        connected_account_session().map(Self)
    }

    pub(super) fn key(&self) -> u64 {
        self.0.history_key.expect("connected browser session has a history key")
    }

    pub(super) fn if_current<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        // Safari's session, when there is one, comes first (the same lock order as `if_disconnected`)
        #[cfg(target_os = "macos")]
        {
            let safari = SAFARI_SESSION.lock();
            if let Some(safari) = safari.as_ref() {
                return Arc::ptr_eq(safari, &self.0).then(f);
            }
        }
        let active = session().lock();
        active.as_ref().filter(|s| Arc::ptr_eq(s, &self.0)).map(|_| f())
    }

    pub(super) fn call(&self, endpoint: &str, body: Value) -> Result<Value> {
        anyhow::ensure!(self.if_current(|| ()).is_some(), "YouTube history account changed");
        web_call_with_session(&self.0, endpoint, body)
    }

    pub(super) fn cookie_header(&self) -> String {
        cookie_header(&self.0)
    }
}

/// Regular videos of a page, without the Shorts shelf (its items carry no channel or length).
fn long_videos(p: crate::api::types::Page) -> crate::api::types::Page {
    use crate::api::types::Item;
    let mut seen = std::collections::HashSet::new();
    crate::api::types::Page {
        items: p.items.into_iter().filter(|i| matches!(i, Item::Video(v) if !v.is_short && !v.channel_name.is_empty() && seen.insert(v.id.clone()))).collect(),
        continuation: p.continuation,
    }
}

/// The account's own home feed, the same one youtube.com shows.
pub fn home() -> Result<crate::api::types::Page> {
    let r = web_call("browse", json!({ "browseId": "FEwhat_to_watch" }))?;
    Ok(long_videos(crate::api::innertube::collect_items(&r)))
}

/// Next page of a feed loaded with the browser session.
pub fn browse_continue(token: &str) -> Result<crate::api::types::Page> {
    let r = web_call("browse", json!({ "continuation": token }))?;
    Ok(long_videos(crate::api::innertube::collect_items(&r)))
}

// ───────────── sign-in through a browser ─────────────

#[cfg(target_os = "macos")]
fn find_macos_browser(applications: &std::path::Path, user_applications: Option<&std::path::Path>) -> Option<(String, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;

    let browsers = [
        ("Google Chrome", "Google Chrome"),
        ("Microsoft Edge", "Microsoft Edge"),
        ("Brave", "Brave Browser"),
        ("Chromium", "Chromium"),
        ("Vivaldi", "Vivaldi"),
    ];
    browsers.into_iter().find_map(|(name, bundle)| {
        [Some(applications), user_applications].into_iter().flatten().find_map(|directory| {
            let executable = directory.join(format!("{bundle}.app")).join("Contents/MacOS").join(bundle);
            executable
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .then(|| (name.to_string(), executable))
        })
    })
}

/// Chromium-based browsers the sign-in can use, most likely first.
pub(crate) fn find_browser() -> Option<(String, PathBuf)> {
    #[cfg(windows)]
    {
        let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let candidates = [
            ("Google Chrome", format!(r"{pf}\Google\Chrome\Application\chrome.exe")),
            ("Google Chrome", format!(r"{pf86}\Google\Chrome\Application\chrome.exe")),
            ("Google Chrome", format!(r"{local}\Google\Chrome\Application\chrome.exe")),
            ("Microsoft Edge", format!(r"{pf86}\Microsoft\Edge\Application\msedge.exe")),
            ("Microsoft Edge", format!(r"{pf}\Microsoft\Edge\Application\msedge.exe")),
            ("Brave", format!(r"{pf}\BraveSoftware\Brave-Browser\Application\brave.exe")),
            ("Brave", format!(r"{local}\BraveSoftware\Brave-Browser\Application\brave.exe")),
            ("Chromium", format!(r"{local}\Chromium\Application\chrome.exe")),
        ];
        candidates.into_iter().map(|(n, p)| (n.to_string(), PathBuf::from(p))).find(|(_, p)| p.is_file())
    }
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        {
            let user_applications = dirs::home_dir().map(|home| home.join("Applications"));
            if let Some(browser) = find_macos_browser(std::path::Path::new("/Applications"), user_applications.as_deref()) {
                return Some(browser);
            }
        }

        let names = [
            ("Google Chrome", "google-chrome"),
            ("Google Chrome", "google-chrome-stable"),
            ("Chromium", "chromium"),
            ("Chromium", "chromium-browser"),
            ("Brave", "brave-browser"),
            ("Brave", "brave"),
            ("Microsoft Edge", "microsoft-edge"),
            ("Microsoft Edge", "microsoft-edge-stable"),
            ("Vivaldi", "vivaldi"),
        ];
        let path = std::env::var_os("PATH")?;
        for (n, bin) in names {
            for dir in std::env::split_paths(&path) {
                let p = dir.join(bin);
                if p.is_file() {
                    return Some((n.to_string(), p));
                }
            }
        }
        None
    }
}

/// The sign-in browser's DevTools connection. It runs over a pair of pipes that
/// only this process holds (`--remote-debugging-pipe`), not a loopback port that
/// any local process could connect to and read the fresh Google cookies from.
struct Cdp {
    to_browser: std::io::PipeWriter,
    replies: mpsc::Receiver<Value>,
    next_id: i64,
}

impl Cdp {
    /// Adds the pipe flags to the browser's command line. Call `spawn` on the
    /// command, then `Cdp::started` with what this returns.
    fn attach(cmd: &mut std::process::Command) -> Result<(Self, ChildEnds)> {
        // the browser reads commands from the first pipe and writes replies to the second
        let (browser_in, to_browser) = std::io::pipe()?;
        let (from_browser, browser_out) = std::io::pipe()?;
        cmd.arg("--remote-debugging-pipe");
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            let (r, w) = (browser_in.as_raw_fd(), browser_out.as_raw_fd());
            unsafe extern "C" {
                fn dup(fd: i32) -> i32;
                fn dup2(from: i32, to: i32) -> i32;
                fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            }
            const F_SETFD: i32 = 2;
            // SAFETY: only async-signal-safe calls between fork and exec. Chromium
            // expects its input on fd 3 and its output on fd 4.
            unsafe {
                cmd.pre_exec(move || {
                    let (mut r, mut w) = (r, w);
                    if w == 3 {
                        w = dup(w);
                    }
                    if r == 4 {
                        r = dup(r);
                    }
                    if r < 0 || w < 0 || dup2(r, 3) < 0 || dup2(w, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // dup2 onto the same number keeps close-on-exec; clear it
                    fcntl(3, F_SETFD, 0);
                    fcntl(4, F_SETFD, 0);
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
            }
            const HANDLE_FLAG_INHERIT: u32 = 1;
            let (r, w) = (browser_in.as_raw_handle(), browser_out.as_raw_handle());
            // SAFETY: plain Win32 calls on handles this process owns. Only the
            // browser's ends become inheritable; ours stay private.
            unsafe {
                if SetHandleInformation(r, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0
                    || SetHandleInformation(w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            cmd.arg(format!("--remote-debugging-io-pipes={},{}", r as usize, w as usize));
        }
        let (tx, replies) = mpsc::channel();
        std::thread::Builder::new().name("signin-devtools".into()).spawn(move || {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(from_browser);
            let mut buf = Vec::new();
            // messages are JSON, each ended by a NUL byte
            while reader.read_until(0, &mut buf).is_ok_and(|n| n > 0) {
                if buf.last() == Some(&0) {
                    buf.pop();
                }
                if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
                buf.clear();
            }
        })?;
        Ok((Self { to_browser, replies, next_id: 0 }, ChildEnds(browser_in, browser_out)))
    }

    fn call(&mut self, method: &str) -> Result<Value> {
        self.request(method, json!({}), None)
    }

    fn request(&mut self, method: &str, params: Value, session_id: Option<&str>) -> Result<Value> {
        use std::io::Write;
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            message["sessionId"] = json!(session_id);
        }
        let mut msg = message.to_string().into_bytes();
        msg.push(0);
        self.to_browser.write_all(&msg)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.checked_duration_since(Instant::now()).ok_or_else(|| anyhow!("devtools: no reply"))?;
            let v = self.replies.recv_timeout(left).map_err(|_| anyhow!("devtools: no reply"))?;
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                anyhow::ensure!(v.get("error").is_none(), "DevTools rejected {method}");
                return Ok(v);
            }
        }
    }

    /// Asks the browser for its cookies.
    fn cookies(&mut self) -> Result<Vec<Value>> {
        let v = self.call("Storage.getCookies")?;
        Ok(v.pointer("/result/cookies").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// The selected channel comes from the YouTube page in this temporary profile.
    /// (macOS reads the cookies headless, with no YouTube page.)
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn selected_page_id(&mut self) -> Result<Option<String>> {
        let tabs = self.call("Target.getTargets")?;
        let target = tabs.pointer("/result/targetInfos").and_then(Value::as_array)
            .and_then(|tabs| tabs.iter().find(|tab| {
                tab.get("type").and_then(Value::as_str) == Some("page")
                    && tab.get("url").and_then(Value::as_str).is_some_and(|url| url == ORIGIN || url.starts_with("https://www.youtube.com/"))
            }))
            .and_then(|tab| tab.get("targetId")).and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Waiting for the YouTube page"))?;
        let attached = self.request("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
        let target_session = attached.pointer("/result/sessionId").and_then(Value::as_str).ok_or_else(|| anyhow!("YouTube page closed"))?;
        let evaluated = self.request("Runtime.evaluate", json!({
            "expression": "(location.origin === 'https://www.youtube.com' && window.ytcfg?.get('LOGGED_IN') === true) ? (window.ytcfg.get('DELEGATED_SESSION_ID') || '') : null",
            "returnByValue": true,
        }), Some(target_session));
        let _ = self.request("Target.detachFromTarget", json!({ "sessionId": target_session }), None);
        let evaluated = evaluated?;
        let id = evaluated.pointer("/result/result/value").and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Waiting for the signed-in YouTube page"))?;
        anyhow::ensure!(id.is_empty() || valid_page_id(id), "Invalid YouTube page ID");
        Ok((!id.is_empty()).then(|| id.to_string()))
    }

    fn close_browser(&mut self) {
        let _ = self.call("Browser.close");
    }
}

/// The browser's ends of the pipes. Dropped once it has started, so that only the
/// browser holds them.
struct ChildEnds(#[allow(dead_code)] std::io::PipeReader, #[allow(dead_code)] std::io::PipeWriter);

fn youtube_cookies(list: &[Value]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for c in list {
        let domain = c.get("domain").and_then(Value::as_str).unwrap_or("");
        if !(domain == "youtube.com" || domain.ends_with(".youtube.com")) {
            continue;
        }
        if let (Some(n), Some(v)) = (c.get("name").and_then(Value::as_str), c.get("value").and_then(Value::as_str)) {
            out.insert(n.to_string(), v.to_string());
        }
    }
    out
}

/// Opens a browser window for the user to sign in to YouTube and waits until they
/// have. Returns the account name. `cancel` stops waiting and closes the browser.
///
/// Google refuses to sign in a browser that is controlled over DevTools ("This
/// browser or app may not be secure"), so this is done in two steps on the same
/// temporary profile: the user signs in in a plain browser window, and once that
/// window is closed a hidden (headless) copy of the browser reads the cookies and
/// the selected channel over the private DevTools pipes.
pub fn browser_sign_in(exe: PathBuf, cancel: Arc<AtomicBool>, ready: Arc<AtomicBool>, confirmed: Arc<AtomicBool>) -> Result<String> {
    // the profile folder older versions used
    let _ = std::fs::remove_dir_all(crate::store::data_dir().join("signin-browser"));
    let profile = crate::store::data_dir().join(format!("signin-browser-{:016x}", rand::random::<u64>()));
    let mut dir = std::fs::DirBuilder::new();
    dir.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir.mode(0o700);
    }
    dir.create(&profile)?;
    let result = sign_in_window(&exe, &profile, &cancel, &ready, &confirmed).and_then(|()| read_signed_in_profile(&exe, &profile, &cancel));
    for _ in 0..8 {
        if std::fs::remove_dir_all(&profile).is_ok() || !profile.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let (cookies, page_id) = result?;
    anyhow::ensure!(!cancel.load(Ordering::Relaxed), "cancelled");
    connect(cookies, page_id)
}

fn sign_in_command(exe: &std::path::Path, profile: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .args(["--no-first-run", "--no-default-browser-check", "--disable-sync"]);
    // the window and the headless copy must encrypt cookies with the same key
    #[cfg(target_os = "linux")]
    cmd.arg("--password-store=basic");
    #[cfg(target_os = "macos")]
    cmd.arg("--use-mock-keychain");
    if let Some(p) = crate::store::proxy_setting() {
        cmd.arg(format!("--proxy-server={p}"));
    }
    cmd
}

/// Step one: a plain browser window, with nothing attached to it, for the user to
/// sign in. Returns once they have signed in and chose the channel (or closed the
/// window after signing in); the window is closed so the browser saves its cookies.
fn sign_in_window(exe: &std::path::Path, profile: &std::path::Path, cancel: &AtomicBool, ready: &AtomicBool, confirmed: &AtomicBool) -> Result<()> {
    let mut child = sign_in_command(exe, profile).args(["--new-window", SIGN_IN_URL]).spawn().context("couldn't start the browser")?;
    let started = Instant::now();
    let result = loop {
        std::thread::sleep(Duration::from_millis(1000));
        if cancel.load(Ordering::Relaxed) {
            break Err(anyhow!("cancelled"));
        }
        let signed_in = profile_has_login(profile);
        if signed_in {
            ready.store(true, Ordering::Relaxed);
            if confirmed.load(Ordering::Relaxed) {
                break Ok(());
            }
        }
        if let Ok(Some(_)) = child.try_wait() {
            // closing the window after signing in is as good as confirming
            break if signed_in || profile_has_login(profile) {
                Ok(())
            } else {
                Err(anyhow!("The browser was closed before sign-in finished."))
            };
        }
        if started.elapsed() > Duration::from_secs(20 * 60) {
            break Err(anyhow!("Sign-in timed out."));
        }
    };
    close_gracefully(&mut child);
    result
}

/// Asks the browser to close its windows, as clicking X would, so it writes its
/// cookies to disk; it is killed if it hasn't gone after a few seconds.
fn close_gracefully(child: &mut std::process::Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &child.id().to_string()])
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(unix)]
        {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    wait_for_browser_exit(child);
}

fn wait_for_browser_exit(child: &mut std::process::Child) {
    for _ in 0..40 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether the profile's cookie store holds a signed-in YouTube session. Only the
/// cookie names are read; the values stay encrypted on disk.
fn profile_has_login(profile: &std::path::Path) -> bool {
    ["Default/Network/Cookies", "Default/Cookies"].iter().any(|file| {
        let path = profile.join(file);
        if !path.is_file() {
            return false;
        }
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let Ok(db) = rusqlite::Connection::open_with_flags(&path, flags) else { return false };
        let _ = db.busy_timeout(Duration::from_millis(200));
        let Ok(mut query) = db.prepare("SELECT name FROM cookies WHERE host_key IN ('.youtube.com', 'youtube.com', 'www.youtube.com')") else {
            return false;
        };
        let Ok(rows) = query.query_map([], |r| r.get::<_, String>(0)) else { return false };
        let names: Vec<String> = rows.filter_map(Result::ok).collect();
        cookie_names_signed_in(&names)
    })
}

fn cookie_names_signed_in(names: &[String]) -> bool {
    let has = |name: &str| names.iter().any(|n| n == name);
    has("LOGIN_INFO") && (has("SAPISID") || has("__Secure-3PAPISID")) && (has("SID") || has("__Secure-3PSID"))
}

/// Step two: the same profile in a headless browser, which DevTools may control,
/// reads the cookies and the channel the user picked on youtube.com.
fn read_signed_in_profile(exe: &std::path::Path, profile: &std::path::Path, cancel: &AtomicBool) -> Result<(BTreeMap<String, String>, Option<String>)> {
    let mut cmd = sign_in_command(exe, profile);
    cmd.args(["--headless=new", ORIGIN]);
    let (mut cdp, browser_ends) = Cdp::attach(&mut cmd)?;
    let mut child = cmd.spawn().context("couldn't start the browser")?;
    drop(browser_ends);
    let started = Instant::now();
    let mut found: Option<BTreeMap<String, String>> = None;
    let result = loop {
        std::thread::sleep(Duration::from_millis(500));
        if cancel.load(Ordering::Relaxed) {
            break Err(anyhow!("cancelled"));
        }
        if let Ok(Some(_)) = child.try_wait() {
            break Err(anyhow!("The browser closed before the sign-in could be read."));
        }
        if found.is_none() {
            if let Ok(list) = cdp.cookies() {
                let cookies = youtube_cookies(&list);
                if has_login(&cookies) && cookies.contains_key("LOGIN_INFO") {
                    found = Some(cookies);
                }
            }
        }
        if let Some(cookies) = &found {
            // the channel picked in the window; the account's default if the page won't say
            match cdp.selected_page_id() {
                Ok(page_id) => break Ok((cookies.clone(), page_id)),
                Err(_) if started.elapsed() > Duration::from_secs(25) => break Ok((cookies.clone(), None)),
                Err(_) => {}
            }
        }
        if started.elapsed() > Duration::from_secs(30) {
            break Err(anyhow!("Couldn't read the YouTube sign-in from the browser. Try again, or import a cookies.txt file instead."));
        }
    };
    cdp.close_browser();
    wait_for_browser_exit(&mut child);
    result
}

#[cfg(test)]
mod sign_in_cookie_tests {
    #[test]
    fn needs_login_info_and_both_session_cookies() {
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(super::cookie_names_signed_in(&names(&["LOGIN_INFO", "SAPISID", "SID", "PREF"])));
        assert!(super::cookie_names_signed_in(&names(&["LOGIN_INFO", "__Secure-3PAPISID", "__Secure-3PSID"])));
        assert!(!super::cookie_names_signed_in(&names(&["SAPISID", "SID"])));
        assert!(!super::cookie_names_signed_in(&names(&["LOGIN_INFO", "PREF", "VISITOR_INFO1_LIVE"])));
    }
}

#[cfg(test)]
mod sign_in_tests {
    use super::*;

    #[test]
    fn waits_for_youtube_login_in_the_temporary_profile() {
        let profile = std::env::temp_dir().join(format!("streamtube-signin-test-{:016x}", rand::random::<u64>()));
        assert!(!profile_has_login(&profile));
        for file in ["Default/Cookies", "Default/Network/Cookies"] {
            let path = profile.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute("CREATE TABLE cookies (host_key TEXT, name TEXT)", []).unwrap();
            for host in [".google.com", ".youtube.com.evil.example"] {
                for name in ["LOGIN_INFO", "SID", "SAPISID"] {
                    db.execute("INSERT INTO cookies VALUES (?1, ?2)", [host, name]).unwrap();
                }
            }
            assert!(!profile_has_login(&profile), "other origins are not a YouTube login");
            for name in ["LOGIN_INFO", "__Secure-3PSID"] {
                db.execute("INSERT INTO cookies VALUES ('.youtube.com', ?1)", [name]).unwrap();
            }
            assert!(!profile_has_login(&profile), "the session is incomplete");
            db.execute("INSERT INTO cookies VALUES ('.youtube.com', '__Secure-3PAPISID')", []).unwrap();
            assert!(profile_has_login(&profile));
            drop(db);
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir_all(profile).unwrap();
    }
}

/// Reads a Netscape-format cookies.txt (as saved by "Get cookies.txt" style
/// browser extensions) and connects with its youtube.com cookies.
pub fn import_cookies_txt(path: &std::path::Path) -> Result<String> {
    let text = std::fs::read_to_string(path)?;
    let mut cookies = BTreeMap::new();
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            continue;
        }
        let domain = f[0].trim_start_matches('.');
        if domain == "youtube.com" || domain.ends_with(".youtube.com") {
            cookies.insert(f[5].to_string(), f[6].trim_end_matches('\r').to_string());
        }
    }
    if cookies.is_empty() {
        return Err(anyhow!("No youtube.com cookies in that file."));
    }
    connect(cookies, None)
}

#[cfg(test)]
mod page_id_tests {
    use super::*;
    #[test]
    fn devtools_uses_the_youtube_tab_and_keeps_its_page_id() {
        use std::io::{BufRead, BufReader};

        let (input, output) = std::io::pipe().unwrap();
        let (send, replies) = mpsc::channel();
        let mut cdp = Cdp { to_browser: output, replies, next_id: 0 };
        let browser = std::thread::spawn(move || {
            let mut reader = BufReader::new(input);
            for step in 0..4 {
                let mut bytes = Vec::new();
                reader.read_until(0, &mut bytes).unwrap();
                assert_eq!(bytes.pop(), Some(0));
                let request: Value = serde_json::from_slice(&bytes).unwrap();
                let result = match step {
                    0 => {
                        assert_eq!(request["method"], "Target.getTargets");
                        json!({"targetInfos": [
                            {"type": "page", "url": "https://www.youtube.com.example.org/", "targetId": "unrelated"},
                            {"type": "page", "url": "https://www.youtube.com/", "targetId": "youtube"}
                        ]})
                    }
                    1 => {
                        assert_eq!(request["params"]["targetId"], "youtube");
                        json!({"sessionId": "tube-tab"})
                    }
                    2 => {
                        assert_eq!(request["sessionId"], "tube-tab");
                        let expression = request["params"]["expression"].as_str().unwrap();
                        assert!(expression.contains("DELEGATED_SESSION_ID"));
                        json!({"result": { "value": "12345678901234567890" }})
                    }
                    _ => {
                        assert_eq!(request["method"], "Target.detachFromTarget");
                        json!({})
                    }
                };
                send.send(json!({"id": request["id"], "result": result})).unwrap();
            }
        });
        let page_id = cdp.selected_page_id().unwrap();
        let connected = Session { page_id, ..Default::default() };
        let mut headers = Vec::new();
        add_page_id(&mut headers, &connected);
        assert_eq!(headers, vec![("X-Goog-PageId", "12345678901234567890")]);
        browser.join().unwrap();
    }

    #[::core::prelude::v1::test]
    fn old_session_without_a_page_id_remains_valid() {
        let old: Session = serde_json::from_value(json!({"cookies": {}, "account": "Test"})).unwrap();
        let mut headers = Vec::new();
        add_page_id(&mut headers, &old);
        assert!(headers.is_empty());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod browser_tests {
    use super::find_macos_browser;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn finds_executable_browser_in_macos_app_bundle() {
        let applications = std::env::temp_dir().join(format!("streamtube-browser-test-{:x}", rand::random::<u64>()));
        let safari = applications.join("Safari.app/Contents/MacOS/Safari");
        let brave = applications.join("Brave Browser.app/Contents/MacOS/Brave Browser");
        for executable in [&safari, &brave] {
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(executable, b"").unwrap();
        }
        fs::set_permissions(&safari, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&brave, fs::Permissions::from_mode(0o600)).unwrap();
        let before = find_macos_browser(&applications, None);
        fs::set_permissions(&brave, fs::Permissions::from_mode(0o700)).unwrap();
        let after = find_macos_browser(&applications, None);
        fs::remove_dir_all(&applications).unwrap();

        assert!(before.is_none());
        assert_eq!(after, Some(("Brave".into(), brave)));
    }
}
pub(in crate::api) mod reporting;
pub use reporting::{PlaybackReport, flush_reports_on_exit, pending_reports};

#[cfg(test)]
mod devtools_pipe_tests {
    /// Starts the real browser (headless) and talks to it over the private pipes.
    #[test]
    #[ignore = "needs Chrome, Edge, Brave or Chromium installed"]
    fn talks_to_the_browser_over_pipes() {
        let (_, exe) = super::find_browser().expect("a browser");
        let profile = std::env::temp_dir().join(format!("st-cdp-test-{}", std::process::id()));
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .args(["--headless=new", "--no-first-run", "--no-default-browser-check", "about:blank"]);
        let (mut cdp, ends) = super::Cdp::attach(&mut cmd).unwrap();
        let mut child = cmd.spawn().unwrap();
        drop(ends);
        let mut ok = None;
        for _ in 0..20 {
            if let Ok(c) = cdp.cookies() {
                ok = Some(c);
                break;
            }
        }
        cdp.close_browser();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&profile);
        assert!(ok.is_some(), "no reply over the DevTools pipe");
    }
}
