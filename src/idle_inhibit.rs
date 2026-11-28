//! Keeps the compositor from blanking the screen while a video is playing.
//!
//! Hyprland's `inhibitingIdle` flag comes from the Wayland
//! `zwp_idle_inhibit_manager_v1` protocol. GPUI's own idle guard uses the
//! desktop portal's `Suspend` flag instead, which is not visible to
//! Hyprland, so Stream Tube needs a surface inhibitor of its own.

use gpui_kit::{App, Global, Window};

#[derive(Clone, Copy, PartialEq, Eq)]
struct RawSurface {
    display: usize,
    surface: usize,
}

#[derive(Default)]
struct IdleState {
    main: Option<RawSurface>,
    pip: Option<RawSurface>,
    playing: bool,
    #[cfg(target_os = "linux")]
    wayland: Option<Wayland>,
    #[cfg(target_os = "linux")]
    guard: Option<InhibitorGuard>,
    #[cfg(target_os = "linux")]
    failed: bool,
}

/// App-global playback inhibition state.
#[derive(Default)]
pub struct IdleInhibit(IdleState);
impl Global for IdleInhibit {}

fn state(cx: &mut App) -> &mut IdleState {
    &mut cx.default_global::<IdleInhibit>().0
}

/// Remembers the main window's Wayland surface for later inhibitors.
pub fn set_main_window(cx: &mut App, window: &Window) {
    let Some(raw) = raw_surface(window) else {
        return;
    };
    let s = state(cx);
    if s.main == Some(raw) {
        return;
    }
    s.main = Some(raw);
    apply(s);
}

/// Switches inhibition to the PiP window, or back to the main window on close.
pub fn set_pip_window(cx: &mut App, window: Option<&Window>) {
    let raw = window.and_then(raw_surface);
    let s = state(cx);
    if s.pip == raw {
        return;
    }
    s.pip = raw;
    apply(s);
}

/// Called by the player whenever its "should stay awake" state changes.
pub fn set_playing(cx: &mut App, playing: bool) {
    let s = state(cx);
    if s.playing == playing {
        return;
    }
    s.playing = playing;
    apply(s);
}

/// Explicit release, used when the app is about to quit.
pub fn release(cx: &mut App) {
    let s = state(cx);
    s.playing = false;
    // Drop the inhibitor and the guest connection before GPUI tears down the
    // display it borrows; keeping Wayland proxies alive past that point can
    // crash on quit.
    #[cfg(target_os = "linux")]
    {
        s.guard = None;
        s.wayland = None;
    }
    apply(s);
}

fn raw_surface(window: &Window) -> Option<RawSurface> {
    #[cfg(target_os = "linux")]
    {
        use raw_window_handle::{
            HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
        };
        // GPUI's own `window_handle()` returns an AnyWindowHandle; use the raw
        // handle traits explicitly to get the Wayland surface pointer.
        let display = HasDisplayHandle::display_handle(window).ok()?;
        let surface = HasWindowHandle::window_handle(window).ok()?;
        match (display.as_raw(), surface.as_raw()) {
            (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => Some(RawSurface {
                display: d.display.as_ptr() as usize,
                surface: w.surface.as_ptr() as usize,
            }),
            _ => None,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window;
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn apply(_: &mut IdleState) {}

#[cfg(target_os = "linux")]
fn apply(s: &mut IdleState) {
    let Some(active) = s.pip.or(s.main) else {
        s.guard = None;
        return;
    };
    if !s.playing || s.failed {
        s.guard = None;
        return;
    }
    if s.guard.as_ref().is_some_and(|g| g.surface == active) {
        return;
    }
    s.guard = None;
    if s.wayland
        .as_ref()
        .map_or(true, |w| w.display != active.display)
    {
        s.wayland = None;
        match Wayland::new(active) {
            Ok(w) => s.wayland = Some(w),
            Err(e) => {
                s.failed = true;
                eprintln!("streamtube: idle inhibit unavailable: {e}");
                return;
            }
        }
    }
    let Some(w) = s.wayland.as_ref() else { return };
    match w.inhibit(active) {
        Ok(g) => s.guard = Some(g),
        Err(e) => {
            s.failed = true;
            eprintln!("streamtube: idle inhibit unavailable: {e}");
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::RawSurface;
    use anyhow::{Result, ensure};
    use std::ffi::c_void;
    use wayland_client::{
        Connection, Dispatch, Proxy, QueueHandle,
        backend::{Backend, ObjectId},
        delegate_noop,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_registry, wl_surface::WlSurface},
    };
    use wayland_protocols::wp::idle_inhibit::zv1::client::{
        zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
        zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
    };

    /// Empty state for the private event queue used to bind the protocol.
    #[derive(Default)]
    pub(super) struct IdleEvents;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for IdleEvents {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(IdleEvents: ZwpIdleInhibitManagerV1);
    delegate_noop!(IdleEvents: ZwpIdleInhibitorV1);

    pub(super) struct Wayland {
        pub(super) display: usize,
        conn: Connection,
        // The queue must stay alive for the manager/inhibitor proxies to work.
        _queue: wayland_client::EventQueue<IdleEvents>,
        qh: QueueHandle<IdleEvents>,
        manager: ZwpIdleInhibitManagerV1,
    }

    impl Wayland {
        pub(super) fn new(raw: RawSurface) -> Result<Self> {
            // GPUI already owns this display: wrap it as a guest so we never
            // close or replace its connection.
            let backend = unsafe { Backend::from_foreign_display(raw.display as *mut _) };
            let conn = Connection::from_backend(backend);
            let (globals, queue) = registry_queue_init::<IdleEvents>(&conn)?;
            let qh = queue.handle();
            let manager = globals.bind::<ZwpIdleInhibitManagerV1, IdleEvents, _>(&qh, 1..=1, ())?;
            Ok(Self {
                display: raw.display,
                conn,
                _queue: queue,
                qh,
                manager,
            })
        }

        pub(super) fn inhibit(&self, raw: RawSurface) -> Result<InhibitorGuard> {
            ensure!(raw.display == self.display, "Wayland display changed");
            let ptr = raw.surface as *mut c_void;
            let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), ptr.cast()) }?;
            let surface = WlSurface::from_id(&self.conn, id)?;
            let inhibitor = self.manager.create_inhibitor(&surface, &self.qh, ());
            self.conn.flush()?;
            Ok(InhibitorGuard {
                inhibitor,
                conn: self.conn.clone(),
                surface: raw,
            })
        }
    }

    pub(super) struct InhibitorGuard {
        inhibitor: ZwpIdleInhibitorV1,
        conn: Connection,
        pub(super) surface: RawSurface,
    }

    impl Drop for InhibitorGuard {
        fn drop(&mut self) {
            self.inhibitor.destroy();
            let _ = self.conn.flush();
        }
    }
}

#[cfg(target_os = "linux")]
use linux::{InhibitorGuard, Wayland};
