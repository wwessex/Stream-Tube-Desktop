//! Coalesces history refreshes from startup, activation, and the player.
use crate::api::history::{Account, Page};
use crate::player::controller::player;
use crate::store;
use futures::future::{BoxFuture, FutureExt, Shared};
use gpui_kit::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type Snapshot = Arc<Result<Page, String>>;
pub type Refresh = Shared<BoxFuture<'static, Snapshot>>;
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct AccountHistory {
    pub account_key: Option<u64>,
    pub snapshot: Option<Snapshot>,
    requested_at: Option<Instant>,
    pending: Option<Refresh>,
    synced: bool,
}

impl Global for AccountHistory {}

pub fn refresh_current(cx: &mut App, browsing: bool) {
    let p = player(cx).read(cx);
    let protected_video = p
        .video
        .as_ref()
        .filter(|_| !p.loading && !p.paused)
        .map(|video| video.id.clone());
    refresh(cx, protected_video, browsing);
}

pub fn refresh_for_playback(cx: &mut App) -> Option<Refresh> {
    refresh(cx, None, false)
}

fn refresh(cx: &mut App, protected_video: Option<String>, browsing: bool) -> Option<Refresh> {
    let account = Account::current();
    let key = account.as_ref().map(Account::key);
    let current = cx.try_global::<AccountHistory>();
    if current.is_none_or(|state| state.account_key != key) {
        cx.set_global(AccountHistory {
            account_key: key,
            ..Default::default()
        });
    }
    let account = account?;
    let recording_revision = store::history_recording_revision();
    if recording_revision.is_none() && !browsing {
        return None;
    }
    let state = cx.global::<AccountHistory>();
    if let Some(pending) = &state.pending {
        return Some(pending.clone());
    }
    if state
        .requested_at
        .is_some_and(|time| time.elapsed() < MIN_REFRESH_INTERVAL)
    {
        return None;
    }
    let include_older = !state.synced;
    let requested_at = Instant::now();
    let refresh = super::blocking(move || {
        let result = match recording_revision {
            Some(revision) => account.sync(protected_video.as_deref(), include_older, revision),
            None => account.page(None),
        };
        Arc::new(result.map_err(|error| error.to_string()))
    })
    .boxed()
    .shared();
    {
        let state = cx.global_mut::<AccountHistory>();
        state.requested_at = Some(requested_at);
        state.pending = Some(refresh.clone());
    }
    let pending = refresh.clone();
    cx.spawn(async move |cx| {
        let snapshot = pending.await;
        let _ = cx.update(|cx| {
            let active_key = Account::current().as_ref().map(Account::key);
            let state = cx.global_mut::<AccountHistory>();
            if state.account_key != key || state.requested_at != Some(requested_at) {
                return;
            }
            state.pending = None;
            if active_key != key {
                *state = AccountHistory {
                    account_key: active_key,
                    ..Default::default()
                };
                return;
            }
            let merged = recording_revision.is_some()
                && recording_revision == store::history_recording_revision()
                && snapshot.is_ok();
            state.synced |= merged;
            state.snapshot = Some(snapshot);
            if merged {
                let mut progress = store::progress_map();
                let p = player(cx).read(cx);
                if !p.paused && !p.loading && p.time > 0.0 {
                    if let Some(video) = &p.video {
                        progress.insert(video.id.clone(), (p.time, p.duration));
                    }
                }
                if cx
                    .try_global::<super::cards::Watched>()
                    .is_none_or(|watched| watched.0 != progress)
                {
                    cx.set_global(super::cards::Watched(progress));
                    super::app::refresh_all_pages(cx);
                }
            }
        });
    })
    .detach();
    Some(refresh)
}
