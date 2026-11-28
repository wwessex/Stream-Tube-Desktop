//! macOS Now Playing: the video in Control Center's media widget and on the
//! lock screen, with its thumbnail, position and play / pause / next / previous /
//! scrub buttons, and the keyboard's media keys. The Linux counterpart is `mpris`,
//! the Windows one `media_controls`.
use crate::media_session::{self, Clock, Command, Snapshot, State};
use block2::RcBlock;
use futures::{StreamExt, channel::mpsc};
use gpui_kit::App;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSData, NSNumber, NSSize, NSString};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

#[link(name = "MediaPlayer", kind = "framework")]
unsafe extern "C" {
    static MPMediaItemPropertyTitle: &'static NSString;
    static MPMediaItemPropertyArtist: &'static NSString;
    static MPMediaItemPropertyPlaybackDuration: &'static NSString;
    static MPMediaItemPropertyArtwork: &'static NSString;
    static MPNowPlayingInfoPropertyElapsedPlaybackTime: &'static NSString;
    static MPNowPlayingInfoPropertyPlaybackRate: &'static NSString;
    static MPNowPlayingInfoPropertyDefaultPlaybackRate: &'static NSString;
    static MPNowPlayingInfoPropertyMediaType: &'static NSString;
    static MPNowPlayingInfoPropertyIsLiveStream: &'static NSString;
}

// MPNowPlayingInfoMediaType / MPNowPlayingPlaybackState / MPRemoteCommandHandlerStatus
const MEDIA_TYPE_VIDEO: usize = 2;
const STATE_PLAYING: usize = 1;
const STATE_PAUSED: usize = 2;
const STATE_STOPPED: usize = 3;
const HANDLED: isize = 0;
const NO_SUCH_CONTENT: isize = 100;

#[derive(Default)]
struct Published {
    snapshot: Option<Snapshot>,
    clock: Clock,
    /// The video id and its `MPMediaItemArtwork`.
    artwork: Option<(String, Retained<AnyObject>)>,
    /// The video whose thumbnail is being fetched.
    fetching: Option<String>,
}

fn center() -> Retained<AnyObject> {
    unsafe { msg_send![class!(MPNowPlayingInfoCenter), defaultCenter] }
}

fn commands() -> Retained<AnyObject> {
    unsafe { msg_send![class!(MPRemoteCommandCenter), sharedCommandCenter] }
}

impl Published {
    fn update(&mut self, snap: Option<Snapshot>, time: f64) {
        let rate = snap.as_ref().map_or(0.0, |s| s.rate);
        if snap == self.snapshot && !self.clock.drifted(rate, time) {
            return;
        }
        let center = center();
        match &snap {
            None => unsafe {
                let _: () = msg_send![&center, setNowPlayingInfo: None::<&AnyObject>];
                let _: () = msg_send![&center, setPlaybackState: STATE_STOPPED];
            },
            Some(s) => {
                self.publish(&center, s, time);
                let before = self.snapshot.as_ref().map(|o| (o.can_next, o.can_seek()));
                if before != Some((s.can_next, s.can_seek())) {
                    let commands = commands();
                    unsafe {
                        let next: Retained<AnyObject> = msg_send![&commands, nextTrackCommand];
                        let _: () = msg_send![&next, setEnabled: s.can_next];
                        // live streams have no position to scrub to
                        let scrub: Retained<AnyObject> = msg_send![&commands, changePlaybackPositionCommand];
                        let _: () = msg_send![&scrub, setEnabled: s.can_seek()];
                    }
                }
            }
        }
        self.clock.set(time);
        self.snapshot = snap;
    }

    fn publish(&self, center: &AnyObject, s: &Snapshot, time: f64) {
        unsafe {
            let info: Retained<AnyObject> = msg_send![class!(NSMutableDictionary), new];
            let set = |key: &NSString, value: &AnyObject| {
                let _: () = msg_send![&info, setObject: value, forKey: key];
            };
            set(MPMediaItemPropertyTitle, &NSString::from_str(&s.title));
            set(MPMediaItemPropertyArtist, &NSString::from_str(&s.channel));
            set(MPNowPlayingInfoPropertyMediaType, &NSNumber::new_usize(MEDIA_TYPE_VIDEO));
            set(MPNowPlayingInfoPropertyIsLiveStream, &NSNumber::new_bool(s.live));
            if s.duration > 0.0 {
                set(MPMediaItemPropertyPlaybackDuration, &NSNumber::new_f64(s.duration));
                set(MPNowPlayingInfoPropertyElapsedPlaybackTime, &NSNumber::new_f64(time.clamp(0.0, s.duration)));
            }
            set(MPNowPlayingInfoPropertyPlaybackRate, &NSNumber::new_f64(s.rate));
            set(MPNowPlayingInfoPropertyDefaultPlaybackRate, &NSNumber::new_f64(1.0));
            if let Some((_, artwork)) = self.artwork.as_ref().filter(|(id, _)| *id == s.id) {
                set(MPMediaItemPropertyArtwork, artwork);
            }
            let state = match s.state {
                State::Playing => STATE_PLAYING,
                State::Paused => STATE_PAUSED,
                State::Stopped => STATE_STOPPED,
            };
            let _: () = msg_send![center, setNowPlayingInfo: &*info];
            let _: () = msg_send![center, setPlaybackState: state];
        }
    }
}

/// An `MPMediaItemArtwork` for a JPEG thumbnail, or None if it doesn't decode.
fn artwork(bytes: &[u8]) -> Option<Retained<AnyObject>> {
    unsafe {
        let data = NSData::with_bytes(bytes);
        let image: Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
        let image: Option<Retained<AnyObject>> = msg_send![image, initWithData: &*data];
        let image = image?;
        let size: NSSize = msg_send![&image, size];
        if size.width <= 0.0 || size.height <= 0.0 {
            return None;
        }
        // the system asks for the size it draws at; one image serves every size
        let handler = RcBlock::new(move |_: NSSize| -> *mut AnyObject { Retained::as_ptr(&image).cast_mut() });
        let artwork: Allocated<AnyObject> = msg_send![class!(MPMediaItemArtwork), alloc];
        msg_send![artwork, initWithBoundsSize: size, requestHandler: &*handler]
    }
}

/// Publishes the change and fetches the new video's thumbnail.
fn refresh(published: &Rc<RefCell<Published>>, snap: Option<Snapshot>, time: f64, cx: &mut App) {
    let id = snap.as_ref().map(|s| s.id.clone());
    let mut state = published.borrow_mut();
    state.update(snap, time);
    let Some(id) = id else { return };
    if state.artwork.as_ref().is_some_and(|(a, _)| *a == id) || state.fetching.as_deref() == Some(id.as_str()) {
        return;
    }
    state.fetching = Some(id.clone());
    drop(state);
    let published = published.clone();
    cx.spawn(async move |cx| {
        let url = media_session::thumbnail_url(&id);
        let bytes = crate::ui::blocking(move || crate::api::http::get_bytes(&url, &[])).await;
        let _ = cx.update(|_| {
            let mut state = published.borrow_mut();
            if state.fetching.as_deref() != Some(id.as_str()) {
                return;
            }
            state.fetching = None;
            let Some(artwork) = bytes.ok().and_then(|b| artwork(&b)) else { return };
            state.artwork = Some((id, artwork));
            // republish with the picture
            if let Some(snap) = state.snapshot.take() {
                let time = state.clock.position(snap.rate);
                state.update(Some(snap), time);
            }
        });
    })
    .detach();
}

/// Registers a handler for one remote command, forwarding it to the app.
fn add_handler(
    command: Retained<AnyObject>,
    send: &mpsc::UnboundedSender<Command>,
    map: impl Fn(NonNull<AnyObject>) -> Option<Command> + 'static,
) {
    let send = send.clone();
    let block = RcBlock::new(move |event: NonNull<AnyObject>| -> isize {
        match map(event) {
            Some(c) if send.unbounded_send(c).is_ok() => HANDLED,
            _ => NO_SUCH_CONTENT,
        }
    });
    unsafe {
        let _: () = msg_send![&command, setEnabled: true];
        // the command center keeps the block; the returned target is only for removal
        let _: Retained<AnyObject> = msg_send![&command, addTargetWithHandler: &*block];
    }
}

pub(crate) fn init(cx: &mut App) {
    let (send, mut received) = mpsc::unbounded();
    let commands = commands();
    unsafe {
        add_handler(msg_send![&commands, playCommand], &send, |_| Some(Command::Play));
        add_handler(msg_send![&commands, pauseCommand], &send, |_| Some(Command::Pause));
        add_handler(msg_send![&commands, togglePlayPauseCommand], &send, |_| Some(Command::Toggle));
        add_handler(msg_send![&commands, stopCommand], &send, |_| Some(Command::Stop));
        add_handler(msg_send![&commands, nextTrackCommand], &send, |_| Some(Command::Next));
        add_handler(msg_send![&commands, previousTrackCommand], &send, |_| Some(Command::Previous));
        add_handler(msg_send![&commands, changePlaybackPositionCommand], &send, |event| {
            let position: f64 = msg_send![event.as_ref(), positionTime];
            position.is_finite().then_some(Command::Seek(position))
        });
    }

    let published = Rc::new(RefCell::new(Published::default()));
    media_session::follow(cx, move |snap, time, cx| refresh(&published, snap, time, cx));

    cx.spawn(async move |cx| {
        while let Some(command) = received.next().await {
            let _ = cx.update(|cx| media_session::apply(command, cx));
        }
    })
    .detach();

    cx.on_app_quit(|_| {
        let center = center();
        unsafe {
            let _: () = msg_send![&center, setNowPlayingInfo: None::<&AnyObject>];
            let _: () = msg_send![&center, setPlaybackState: STATE_STOPPED];
        }
        std::future::ready(())
    })
    .detach();
}
