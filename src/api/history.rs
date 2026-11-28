//! Account watch history: read and reported through the browser session when
//! one is connected, otherwise through the TV sign-in.
use super::{auth, innertube, json::find, types::VideoItem, web};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Clone)]
pub struct Account(Session);

#[derive(Clone)]
enum Session {
    Web(web::HistorySession),
    Tv(auth::HistorySession),
}

#[derive(Clone)]
pub struct Entry {
    pub video: VideoItem,
    pub progress: Option<Position>,
    pub removal_token: Option<String>,
}

#[derive(Clone, Copy)]
pub struct Position {
    pub seconds: f64,
    /// The progress bar may report whole percentages instead of seconds.
    pub precision: f64,
}

#[derive(Clone)]
pub struct Page {
    pub entries: Vec<Entry>,
    pub continuation: Option<String>,
}

impl Account {
    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self(Session::Web(web::HistorySession::for_test()))
    }

    /// Read from the account used for writes whenever it is connected.
    pub fn current() -> Option<Self> {
        web::HistorySession::current()
            .map(Session::Web)
            .or_else(|| auth::HistorySession::current().map(Session::Tv))
            .map(Self)
    }

    pub fn key(&self) -> u64 {
        match &self.0 {
            Session::Web(s) => s.key(),
            Session::Tv(s) => s.key,
        }
    }

    pub(super) fn if_current<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        match &self.0 {
            Session::Web(s) => s.if_current(f),
            Session::Tv(s) => web::if_disconnected(|| s.if_current(f)).flatten(),
        }
    }

    pub(super) fn call(&self, endpoint: &str, body: Value) -> Result<Value> {
        anyhow::ensure!(
            self.if_current(|| ()).is_some(),
            "YouTube history account changed"
        );
        match &self.0 {
            Session::Web(s) => s.call(endpoint, body),
            Session::Tv(s) => s.call(endpoint, body),
        }
    }

    /// The player response playback is reported against: the website's for the
    /// browser session, the TV client's (as the account) for the TV sign-in.
    /// The player response for a video, as the account. Each client needs its
    /// own player's `signatureTimestamp`: the web player's for a browser session,
    /// the TV player's for the TV sign-in.
    pub(super) fn player_response(&self, video_id: &str) -> Result<Value> {
        anyhow::ensure!(self.if_current(|| ()).is_some(), "YouTube history account changed");
        match &self.0 {
            Session::Web(s) => {
                let mut body = json!({ "videoId": video_id, "racyCheckOk": true, "contentCheckOk": true });
                if let Some(sts) = super::web::reporting::sts() {
                    body["playbackContext"] = json!({ "contentPlaybackContext": { "signatureTimestamp": sts, "html5Preference": "HTML5_PREF_WANTS" } });
                }
                s.call("player", body)
            }
            Session::Tv(s) => {
                let token = s.access_token()?;
                let sts = super::jsc::player().ok().and_then(|p| p.sts);
                super::streams::tv_player_response(video_id, Some(&token), sts)
            }
        }
    }

    /// Headers that make a stats ping the account's: the session cookies, or
    /// the TV sign-in's bearer token.
    pub(super) fn ping_headers(&self) -> Result<Vec<(&'static str, String)>> {
        Ok(match &self.0 {
            Session::Web(s) => vec![("Cookie", s.cookie_header())],
            Session::Tv(s) => vec![("Authorization", format!("Bearer {}", s.access_token()?))],
        })
    }

    /// The client a stats ping claims to come from, matching `player_response`.
    pub(super) fn ping_client(&self) -> (&'static str, &'static str, &'static str) {
        match &self.0 {
            Session::Web(_) => ("WEB", web::WEB_VERSION, "DESKTOP"),
            Session::Tv(_) => ("TVHTML5", super::streams::TV5_VERSION, "TV"),
        }
    }

    pub fn page(&self, continuation: Option<&str>) -> Result<Page> {
        let body = match continuation {
            Some(token) => json!({ "continuation": token }),
            None => json!({ "browseId": "FEhistory" }),
        };
        let response = self.call("browse", body)?;
        anyhow::ensure!(
            self.if_current(|| ()).is_some(),
            "YouTube history account changed"
        );
        if matches!(self.0, Session::Web(_))
            && response
                .pointer("/responseContext/mainAppWebResponseContext/loggedOut")
                .and_then(Value::as_bool)
                == Some(true)
        {
            return Err(anyhow!(
                "YouTube refused the browser session. Connect it again."
            ));
        }
        Ok(parse_page(&response))
    }

    pub fn sync(
        &self,
        protected_video: Option<&str>,
        include_older: bool,
        recording_revision: u64,
    ) -> Result<Page> {
        let requested_at = chrono::Utc::now().timestamp_millis();
        let first_page = self.page(None)?;
        self.merge(
            &first_page.entries,
            protected_video,
            requested_at,
            0,
            recording_revision,
        )?;
        if !include_older {
            return Ok(first_page);
        }
        let mut continuation = first_page.continuation.clone();
        let mut seen: HashSet<String> = first_page
            .entries
            .iter()
            .map(|entry| entry.video.id.clone())
            .collect();
        let mut offset = first_page.entries.len();
        for _ in 0..2 {
            let Some(token) = continuation else { break };
            let Ok(page) = self.page(Some(&token)) else {
                break;
            };
            let entries: Vec<_> = page
                .entries
                .into_iter()
                .filter(|entry| seen.insert(entry.video.id.clone()))
                .collect();
            self.merge(
                &entries,
                protected_video,
                requested_at,
                offset,
                recording_revision,
            )?;
            offset += entries.len();
            continuation = page.continuation.filter(|next| next != &token);
        }
        Ok(first_page)
    }

    fn merge(
        &self,
        entries: &[Entry],
        protected_video: Option<&str>,
        requested_at: i64,
        offset: usize,
        recording_revision: u64,
    ) -> Result<()> {
        self.if_current(|| {
            crate::store::history_merge_remote(
                self.key(),
                entries,
                protected_video,
                requested_at,
                offset,
                recording_revision,
            )
        })
        .ok_or_else(|| anyhow!("YouTube history account changed"))??;
        Ok(())
    }

    pub fn remove(&self, video_id: &str, token: &str) -> Result<()> {
        let response = self.call("feedback", json!({ "feedbackTokens": [token] }))?;
        if response
            .pointer("/feedbackResponses/0/isProcessed")
            .and_then(Value::as_bool)
            == Some(false)
        {
            return Err(anyhow!("YouTube did not remove this video"));
        }
        self.if_current(|| crate::store::try_history_remove(video_id))
            .ok_or_else(|| anyhow!("YouTube history account changed"))?
    }
}

fn parse_page(response: &Value) -> Page {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut collect = |video: &VideoItem, node: &Value| {
        if seen.insert(video.id.clone()) {
            entries.push(Entry {
                video: video.clone(),
                progress: position(video, node),
                removal_token: removal_token(node),
            });
        }
    };
    let mut continuation =
        innertube::collect_items_with_video_nodes(response, &mut collect).continuation;
    // On WEB continuations can be returned as append/reload commands.
    for command in response
        .get("onResponseReceivedCommands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(items) = command
            .pointer("/appendContinuationItemsAction/continuationItems")
            .or_else(|| command.pointer("/reloadContinuationItemsCommand/continuationItems"))
        {
            continuation = innertube::collect_items_with_video_nodes(items, &mut collect)
                .continuation
                .or(continuation);
        }
    }
    Page {
        entries,
        continuation,
    }
}

fn position(video: &VideoItem, node: &Value) -> Option<Position> {
    let mut endpoints = Vec::new();
    super::json::find_all(node, "watchEndpoint", &mut endpoints);
    let seconds = endpoints
        .into_iter()
        .filter(|endpoint| {
            endpoint.get("videoId").and_then(Value::as_str) == Some(video.id.as_str())
        })
        .find_map(|endpoint| {
            let value = endpoint.get("startTimeSeconds")?;
            value.as_f64().or_else(|| value.as_str()?.parse().ok())
        });
    if let Some(seconds) = seconds.filter(|seconds| {
        seconds.is_finite()
            && *seconds >= 0.0
            && video
                .duration_secs
                .filter(|&d| d > 0)
                .is_none_or(|d| *seconds <= d as f64)
    }) {
        return Some(Position {
            seconds,
            precision: 1.0,
        });
    }
    let duration = video.duration_secs.filter(|&seconds| seconds > 0)? as f64;
    let percent = find(node, "thumbnailOverlayResumePlaybackRenderer")
        .and_then(|o| o.get("percentDurationWatched"))
        .or_else(|| {
            find(node, "thumbnailOverlayProgressBarViewModel").and_then(|o| o.get("startPercent"))
        })
        .and_then(Value::as_f64)
        .filter(|percent| (0.0..=100.0).contains(percent))?;
    Some(Position {
        seconds: duration * percent / 100.0,
        precision: (duration / 100.0).max(1.0),
    })
}

fn removal_token(node: &Value) -> Option<String> {
    let mut items = Vec::new();
    super::json::find_all(node, "menuServiceItemRenderer", &mut items);
    super::json::find_all(node, "listItemViewModel", &mut items);
    items.into_iter().find_map(|item| {
        let icon = find(item, "iconType")
            .or_else(|| find(item, "imageName"))
            .and_then(Value::as_str);
        if !matches!(icon, Some("DELETE" | "REMOVE_FROM_HISTORY")) {
            return None;
        }
        find(item, "feedbackEndpoint")?
            .get("feedbackToken")?
            .as_str()
            .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_web_and_tv_history_with_exact_or_approximate_positions() {
        let response = json!({
            "contents": [
                {"videoRenderer": {
                    "videoId": "webHistory1",
                    "title": {"simpleText": "Web video"},
                    "lengthText": {"simpleText": "10:00"},
                    "navigationEndpoint": {"watchEndpoint": {"videoId": "webHistory1", "startTimeSeconds": "83"}},
                    "thumbnailOverlays": [{"thumbnailOverlayResumePlaybackRenderer": {"percentDurationWatched": 15}}],
                    "menu": {"menuRenderer": {"items": [
                        {"menuServiceItemRenderer": {
                            "icon": {"iconType": "NOT_INTERESTED"},
                            "serviceEndpoint": {"feedbackEndpoint": {"feedbackToken": "unrelated"}}
                        }},
                        {"menuServiceItemRenderer": {
                            "icon": {"iconType": "DELETE"},
                            "serviceEndpoint": {"feedbackEndpoint": {"feedbackToken": "remove-video"}}
                        }}
                    ]}}
                }},
                {"tileRenderer": {
                    "contentId": "tvHistory01",
                    "metadata": {"tileMetadataRenderer": {"title": {"simpleText": "TV video"}}},
                    "header": {"tileHeaderRenderer": {"thumbnail": {"overlays": [
                        {"thumbnailOverlayTimeStatusRenderer": {"text": {"simpleText": "10:00"}}},
                        {"thumbnailOverlayResumePlaybackRenderer": {"percentDurationWatched": 30}}
                    ]}}}
                }},
                {"videoRenderer": {
                    "videoId": "noPosition1",
                    "title": {"simpleText": "Missing progress"}
                }}
            ]
        });
        let page = parse_page(&response);
        assert_eq!(page.entries.len(), 3);
        let web = &page.entries[0];
        assert_eq!(web.progress.unwrap().seconds, 83.0);
        assert_eq!(web.removal_token.as_deref(), Some("remove-video"));
        let tv = page.entries[1].progress.unwrap();
        assert_eq!((tv.seconds, tv.precision), (180.0, 6.0));
        assert!(page.entries[2].progress.is_none());
    }

    #[test]
    fn parses_web_continuation_commands_and_new_progress_bar() {
        let response = json!({
            "onResponseReceivedCommands": [
                {"appendContinuationItemsAction": {"continuationItems": [
                    {"lockupViewModel": {
                        "contentId": "videoAfter1",
                        "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
                        "metadata": {"lockupMetadataViewModel": {"title": {"content": "Next video"}}},
                        "contentImage": {"thumbnailViewModel": {"overlays": [
                            {"thumbnailBadgeViewModel": {"text": "5:00"}},
                            {"thumbnailOverlayProgressBarViewModel": {"startPercent": 40}}
                        ]}}
                    }},
                    {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "next"}}}}
                ]}}
            ]
        });
        let page = parse_page(&response);
        assert_eq!(page.continuation.as_deref(), Some("next"));
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].progress.unwrap().seconds, 120.0);
    }
}
