//! Linux MPRIS: the video in GNOME's and KDE's media widgets, the lock screen
//! and `playerctl`, with its thumbnail, position and play / pause / next /
//! previous / seek, and the keyboard's media keys (the desktop routes those to
//! MPRIS players). Specification: specifications.freedesktop.org/mpris-spec.
//! The macOS counterpart is `now_playing`, the Windows one `media_controls`.
use crate::media_session::{self, Clock, Command, Snapshot, State};
use futures::{StreamExt, channel::mpsc};
use gpui_kit::App;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, connection, fdo, interface};

const PATH: &str = "/org/mpris/MediaPlayer2";
const NAME: &str = "org.mpris.MediaPlayer2.streamtube";
/// Microseconds per second: MPRIS positions and lengths are in µs.
const US: f64 = 1_000_000.0;

/// What the D-Bus side reads; written by the UI thread.
#[derive(Default)]
struct Shared {
    snapshot: Option<Snapshot>,
    clock: Clock,
    /// The video id and a file:// URL of its thumbnail.
    art: Option<(String, String)>,
}

type SharedRef = Arc<Mutex<Shared>>;

/// A change for the D-Bus side to announce.
enum Emit {
    Changed,
    /// Seconds.
    Seeked(f64),
}

/// The track id MPRIS wants for a video: an object path, so the id is hex-encoded
/// (video ids contain `-`, which object paths don't allow).
fn track_id(video: &str) -> OwnedObjectPath {
    let hex: String = video.bytes().map(|b| format!("{b:02x}")).collect();
    OwnedObjectPath::try_from(format!("/org/streamtube/video/v{hex}")).expect("hex is a valid object path element")
}

fn owned<'a>(v: impl Into<Value<'a>>) -> Option<OwnedValue> {
    v.into().try_into_owned().ok()
}

struct Root {
    send: mpsc::UnboundedSender<Command>,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {
        let _ = self.send.unbounded_send(Command::Raise);
    }

    fn quit(&self) {}

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn identity(&self) -> &str {
        "Stream Tube"
    }

    /// packaging/linux/streamtube.desktop; the widgets take the app's icon from it.
    #[zbus(property(emits_changed_signal = "const"))]
    fn desktop_entry(&self) -> &str {
        "streamtube"
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    shared: SharedRef,
    send: mpsc::UnboundedSender<Command>,
}

impl Player {
    fn with<T>(&self, f: impl FnOnce(Option<&Snapshot>) -> T) -> T {
        f(self.shared.lock().snapshot.as_ref())
    }

    fn command(&self, command: Command) {
        let _ = self.send.unbounded_send(command);
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        self.command(Command::Next);
    }

    fn previous(&self) {
        self.command(Command::Previous);
    }

    fn pause(&self) {
        self.command(Command::Pause);
    }

    fn play_pause(&self) {
        self.command(Command::Toggle);
    }

    fn stop(&self) {
        self.command(Command::Stop);
    }

    fn play(&self) {
        self.command(Command::Play);
    }

    /// By `offset` microseconds.
    fn seek(&self, offset: i64) {
        if self.with(|s| s.is_some_and(Snapshot::can_seek)) {
            self.command(Command::SeekBy(offset as f64 / US));
        }
    }

    /// To `position` microseconds, if `track_id` is still the current video.
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        let ok = self.with(|s| {
            s.is_some_and(|s| s.can_seek() && *self::track_id(&s.id) == track_id && (0..=(s.duration * US) as i64).contains(&position))
        });
        if ok {
            self.command(Command::Seek(position as f64 / US));
        }
    }

    fn open_uri(&self, _uri: &str) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("Stream Tube doesn't open URIs over MPRIS".into()))
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> &str {
        match self.with(|s| s.map(|s| s.state)) {
            Some(State::Playing) => "Playing",
            Some(State::Paused) => "Paused",
            Some(State::Stopped) | None => "Stopped",
        }
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        self.with(|s| s.map_or(1.0, |s| s.speed))
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn minimum_rate(&self) -> f64 {
        0.1
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn maximum_rate(&self) -> f64 {
        5.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let shared = self.shared.lock();
        let Some(s) = &shared.snapshot else { return HashMap::new() };
        let mut m = HashMap::new();
        let mut put = |k: &str, v: Option<OwnedValue>| {
            if let Some(v) = v {
                m.insert(k.to_string(), v);
            }
        };
        put("mpris:trackid", owned(track_id(&s.id)));
        put("xesam:title", owned(s.title.as_str()));
        put("xesam:artist", owned(vec![s.channel.as_str()]));
        put("xesam:url", owned(format!("https://www.youtube.com/watch?v={}", s.id)));
        if s.duration > 0.0 {
            put("mpris:length", owned((s.duration * US) as i64));
        }
        if let Some((_, url)) = shared.art.as_ref().filter(|(id, _)| *id == s.id) {
            put("mpris:artUrl", owned(url.as_str()));
        }
        m
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn volume(&self) -> f64 {
        1.0
    }

    /// Read on demand; jumps are announced with `Seeked`.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        let shared = self.shared.lock();
        let Some(s) = &shared.snapshot else { return 0 };
        let at = shared.clock.position(s.rate);
        let at = if s.duration > 0.0 { at.clamp(0.0, s.duration) } else { at.max(0.0) };
        (at * US) as i64
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.with(|s| s.is_some_and(|s| s.can_next))
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.with(|s| s.is_some_and(|s| s.can_previous))
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.with(|s| s.is_some())
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.with(|s| s.is_some())
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.with(|s| s.is_some_and(Snapshot::can_seek))
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

/// Puts the player on the session bus. If another instance holds the name, this
/// one takes a per-process name, as the specification allows.
async fn serve(shared: SharedRef, send: mpsc::UnboundedSender<Command>, name: &str) -> zbus::Result<(Connection, InterfaceRef<Player>)> {
    let build = |name: String| {
        let (shared, send) = (shared.clone(), send.clone());
        async move {
            connection::Builder::session()?
                .name(name)?
                .serve_at(PATH, Root { send: send.clone() })?
                .serve_at(PATH, Player { shared, send })?
                .build()
                .await
        }
    };
    let conn = match build(name.to_string()).await {
        Ok(c) => c,
        Err(_) => build(format!("{name}.instance{}", std::process::id())).await?,
    };
    let player = conn.object_server().interface::<_, Player>(PATH).await?;
    Ok((conn, player))
}

async fn emit(player: &InterfaceRef<Player>, what: Emit) -> zbus::Result<()> {
    let to = player.signal_emitter();
    match what {
        Emit::Seeked(at) => Player::seeked(to, (at.max(0.0) * US) as i64).await,
        Emit::Changed => {
            let p = player.get().await;
            p.playback_status_changed(to).await?;
            p.metadata_changed(to).await?;
            p.rate_changed(to).await?;
            p.can_go_next_changed(to).await?;
            p.can_go_previous_changed(to).await?;
            p.can_play_changed(to).await?;
            p.can_pause_changed(to).await?;
            p.can_seek_changed(to).await
        }
    }
}

/// Where the current video's thumbnail is kept for the widgets (they read files
/// more reliably than http URLs).
fn art_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("streamtube").join("mpris"))
}

/// Downloads the video's thumbnail next to nothing else (the previous one is
/// removed) and returns its file:// URL.
fn fetch_art(id: &str) -> Option<String> {
    let dir = art_dir()?;
    let bytes = crate::api::http::get_bytes(&media_session::thumbnail_url(id), &[]).ok()?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{id}.jpg"));
    std::fs::write(&path, bytes).ok()?;
    url::Url::from_file_path(&path).ok().map(String::from)
}

pub(crate) fn init(cx: &mut App) {
    let shared = SharedRef::default();
    let (send, mut received) = mpsc::unbounded();
    let (emit_send, mut to_emit) = mpsc::unbounded::<Emit>();

    let bus_shared = shared.clone();
    // the bus side runs on its own thread; zbus drives its I/O there
    let bus = move || futures::executor::block_on(async move {
        let player = match serve(bus_shared, send, NAME).await {
            Ok((conn, player)) => {
                // the connection lives as long as the app
                std::mem::forget(conn);
                player
            }
            Err(e) => {
                eprintln!("[mpris] not on the session bus: {e}");
                return;
            }
        };
        while let Some(what) = to_emit.next().await {
            if let Err(e) = emit(&player, what).await {
                eprintln!("[mpris] {e}");
            }
        }
    });
    if let Err(e) = std::thread::Builder::new().name("mpris".into()).spawn(bus) {
        eprintln!("[mpris] {e}");
    }

    media_session::follow(cx, move |snap, time, _| {
        let mut state = shared.lock();
        let rate = snap.as_ref().map_or(0.0, |s| s.rate);
        let changed = snap != state.snapshot;
        let same_video = snap.as_ref().map(|s| &s.id) == state.snapshot.as_ref().map(|s| &s.id);
        let drifted = state.clock.drifted(rate, time);
        if !changed && !drifted {
            return;
        }
        let new_video = snap.as_ref().map(|s| s.id.clone()).filter(|_| !same_video);
        state.clock.set(time);
        state.snapshot = snap;
        drop(state);
        if changed {
            let _ = emit_send.unbounded_send(Emit::Changed);
        }
        if drifted && same_video {
            let _ = emit_send.unbounded_send(Emit::Seeked(time));
        }
        if let Some(id) = new_video {
            let (shared, emit_send) = (shared.clone(), emit_send.clone());
            std::thread::spawn(move || {
                let Some(url) = fetch_art(&id) else { return };
                let mut state = shared.lock();
                if state.snapshot.as_ref().is_some_and(|s| s.id == id) {
                    state.art = Some((id, url));
                    drop(state);
                    let _ = emit_send.unbounded_send(Emit::Changed);
                }
            });
        }
    });

    cx.spawn(async move |cx| {
        while let Some(command) = received.next().await {
            let _ = cx.update(|cx| media_session::apply(command, cx));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::Proxy;

    #[test]
    fn track_ids_are_object_paths() {
        assert_eq!(track_id("dQw4w9WgXcQ").as_str(), "/org/streamtube/video/v6451773477395767586351");
        assert!(track_id("a-_b").as_str().ends_with("/v612d5f62"));
    }

    /// Talks to the player over a real session bus, like GNOME Shell does.
    /// Run under `dbus-run-session`.
    #[test]
    #[ignore = "needs a D-Bus session bus"]
    fn serves_mpris_on_the_session_bus() {
        futures::executor::block_on(async {
            let shared = SharedRef::default();
            {
                let mut s = shared.lock();
                s.snapshot = Some(Snapshot {
                    id: "dQw4w9WgXcQ".into(),
                    title: "Never Gonna Give You Up".into(),
                    channel: "Rick Astley".into(),
                    duration: 213.0,
                    rate: 1.0,
                    speed: 1.0,
                    state: State::Playing,
                    live: false,
                    can_next: true,
                    can_previous: true,
                });
                s.clock.set(42.0);
            }
            let (send, mut received) = mpsc::unbounded();
            let name = format!("{NAME}.test{}", std::process::id());
            let (_conn, player) = serve(shared.clone(), send, &name).await.unwrap();

            let client = Connection::session().await.unwrap();
            let proxy = Proxy::new(&client, name.as_str(), PATH, "org.mpris.MediaPlayer2.Player").await.unwrap();
            let status: String = proxy.get_property("PlaybackStatus").await.unwrap();
            assert_eq!(status, "Playing");
            let meta: HashMap<String, OwnedValue> = proxy.get_property("Metadata").await.unwrap();
            assert_eq!(String::try_from(meta["xesam:title"].clone()).unwrap(), "Never Gonna Give You Up");
            assert_eq!(i64::try_from(meta["mpris:length"].clone()).unwrap(), 213_000_000);
            let position: i64 = proxy.get_property("Position").await.unwrap();
            assert!((42_000_000..43_000_000).contains(&position), "{position}");
            assert!(proxy.get_property::<bool>("CanGoNext").await.unwrap());

            proxy.call_method("PlayPause", &()).await.unwrap();
            proxy.call_method("Next", &()).await.unwrap();
            proxy.call_method("Seek", &(-10_000_000i64)).await.unwrap();
            proxy.call_method("SetPosition", &(track_id("dQw4w9WgXcQ"), 100_000_000i64)).await.unwrap();
            // a stale track id is ignored
            proxy.call_method("SetPosition", &(track_id("other"), 5_000_000i64)).await.unwrap();
            let root = Proxy::new(&client, name.as_str(), PATH, "org.mpris.MediaPlayer2").await.unwrap();
            root.call_method("Raise", &()).await.unwrap();
            let got: Vec<Command> = (&mut received).take(5).collect().await;
            assert_eq!(got, [Command::Toggle, Command::Next, Command::SeekBy(-10.0), Command::Seek(100.0), Command::Raise]);

            // property changes reach listeners
            let mut changes = zbus::fdo::PropertiesProxy::builder(&client)
                .destination(name.as_str())
                .unwrap()
                .path(PATH)
                .unwrap()
                .build()
                .await
                .unwrap()
                .receive_properties_changed()
                .await
                .unwrap();
            shared.lock().snapshot.as_mut().unwrap().state = State::Paused;
            emit(&player, Emit::Changed).await.unwrap();
            let change = changes.next().await.unwrap();
            let args = change.args().unwrap();
            assert_eq!(args.interface_name.as_str(), "org.mpris.MediaPlayer2.Player");
            assert_eq!(String::try_from(args.changed_properties["PlaybackStatus"].clone()).unwrap(), "Paused");
        });
    }
}
