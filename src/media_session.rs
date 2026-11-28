//! What the system media controls show and send, shared by macOS Now Playing
//! (`now_playing`) and Linux MPRIS (`mpris`): a snapshot of the active player,
//! the commands they send back, and following the active player tab.
use crate::player::controller::{Player, PlayerHandle, player};
use gpui_kit::{App, Entity, Subscription};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

/// The published position is corrected when the player's clock drifts this far
/// from what the system extrapolates (a seek, a stall).
const DRIFT: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Command {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    /// To a position in seconds.
    Seek(f64),
    /// By an offset in seconds.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    SeekBy(f64),
    /// Bring the window to the front.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Raise,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum State {
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub id: String,
    pub title: String,
    pub channel: String,
    /// Seconds; 0 for live streams and while unknown.
    pub duration: f64,
    /// How fast the position moves: the speed while playing, 0 otherwise.
    pub rate: f64,
    /// The playback speed, also while paused.
    pub speed: f64,
    pub state: State,
    pub live: bool,
    pub can_next: bool,
    pub can_previous: bool,
}

impl Snapshot {
    pub fn of(p: &Player) -> Option<Snapshot> {
        let video = p.video.as_ref()?;
        let state = if p.eof {
            State::Stopped
        } else if p.paused || p.error.is_some() {
            State::Paused
        } else {
            State::Playing
        };
        // while loading or buffering the clock stands still, and so should the widget
        let moving = state == State::Playing && !p.loading && !p.buffering && !p.seeking;
        let live = video.is_live || p.streams.as_ref().is_some_and(|s| s.is_live);
        Some(Snapshot {
            id: video.id.clone(),
            title: video.title.clone(),
            channel: video.channel_name.clone(),
            duration: if live { 0.0 } else { p.duration.max(0.0).round() },
            rate: if moving { p.speed } else { 0.0 },
            speed: p.speed,
            state,
            live,
            // as the player's own buttons: the next queue item, else YouTube's "Up next"
            can_next: p.queue_pos.is_some_and(|i| i + 1 < p.queue.len()) || p.autoplay_next.is_some(),
            // the previous queue item, else the video watched before, else the start
            can_previous: true,
        })
    }

    pub fn can_seek(&self) -> bool {
        !self.live && self.duration > 0.0
    }
}

/// The 16:9 thumbnail, without the letterbox bars of the default one.
pub(crate) fn thumbnail_url(id: &str) -> String {
    format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg")
}

/// Where the published position was last set, to extrapolate it the way the
/// system does and notice when the player's clock jumps away from it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Clock {
    anchor: Option<(f64, Instant)>,
}

impl Clock {
    pub fn set(&mut self, time: f64) {
        self.anchor = Some((time, Instant::now()));
    }

    /// The position the system shows now.
    pub fn position(&self, rate: f64) -> f64 {
        self.anchor.map_or(0.0, |(at, when)| at + when.elapsed().as_secs_f64() * rate)
    }

    /// Whether the player's clock is no longer where the system thinks it is.
    pub fn drifted(&self, rate: f64, time: f64) -> bool {
        self.anchor.is_none() || (self.position(rate) - time).abs() > DRIFT
    }
}

/// Calls `on_change` with the active player's snapshot and position whenever
/// that player changes, following the active tab.
pub(crate) fn follow(cx: &mut App, on_change: impl FnMut(Option<Snapshot>, f64, &mut App) + 'static) {
    let on_change: OnChange = Rc::new(RefCell::new(on_change));
    let active = player(cx);
    let mut binding = (active.entity_id(), observe(&active, &on_change, cx));
    cx.observe_global::<PlayerHandle>(move |cx| {
        let active = player(cx);
        if binding.0 != active.entity_id() {
            binding = (active.entity_id(), observe(&active, &on_change, cx));
        }
    })
    .detach();
}

type OnChange = Rc<RefCell<dyn FnMut(Option<Snapshot>, f64, &mut App)>>;

fn observe(active: &Entity<Player>, on_change: &OnChange, cx: &mut App) -> Subscription {
    let call = {
        let on_change = on_change.clone();
        move |entity: &Entity<Player>, cx: &mut App| {
            let p = entity.read(cx);
            let (snapshot, time) = (Snapshot::of(p), p.time);
            (on_change.borrow_mut())(snapshot, time, cx);
        }
    };
    call(active, cx);
    cx.observe(active, move |entity, cx| call(&entity, cx))
}

/// Carries out a command from the system's media controls on the active player,
/// the same way the player's own buttons and shortcuts do.
pub(crate) fn apply(command: Command, cx: &mut App) {
    if player(cx).read(cx).video.is_none() {
        return;
    }
    if let Command::Next | Command::Previous | Command::Raise = command {
        // moving through "Up next" opens the watch page, which needs the main window
        let pip = crate::ui::pip::handle(cx);
        let Some(main) = cx.windows().into_iter().find(|w| Some(*w) != pip) else { return };
        let _ = main.update(cx, |_, window, cx| match command {
            Command::Next => crate::ui::app::play_next(window, cx),
            Command::Previous => crate::ui::app::play_previous(window, cx),
            _ => {
                crate::ui::app::restore_window(window);
                window.activate_window();
            }
        });
        return;
    }
    player(cx).update(cx, |p, cx| {
        match command {
            Command::Play if p.eof || p.paused => p.toggle_pause(cx),
            Command::Pause | Command::Stop if !p.paused => p.set_paused(true, cx),
            Command::Toggle => p.toggle_pause(cx),
            Command::Seek(t) => p.seek_to(t),
            Command::SeekBy(by) => {
                let t = p.time + by;
                p.seek_to(t);
            }
            _ => return,
        }
        cx.notify();
    });
}
