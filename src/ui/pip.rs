//! Picture-in-picture: the player in a small always-on-top window that stays
//! visible over other apps.
use crate::player::controller::{KeyboardFeedback, Player, player};
use crate::player::view::PlayerView;
use crate::ui::*;
use gpui_kit::component::button::Button;

#[derive(Default)]
pub struct PipWindow(Option<(AnyWindowHandle, Entity<Player>)>);
impl Global for PipWindow {}

pub struct PipView {
    view: Entity<PlayerView>,
    player: Entity<Player>,
    focus: FocusHandle,
}

pub fn handle(cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<PipWindow>().and_then(|p| p.0.as_ref().map(|(handle, _)| *handle))
}

pub(crate) fn owner(cx: &App) -> Option<Entity<Player>> {
    cx.try_global::<PipWindow>().and_then(|p| p.0.as_ref().map(|(_, player)| player.clone()))
}

pub fn open(cx: &mut App) {
    let active = player(cx);
    if let Some(h) = handle(cx) {
        if owner(cx).as_ref() == Some(&active) {
            if h.update(cx, |_, window, _| window.activate_window()).is_ok() { return; }
        } else {
            let _ = h.update(cx, |_, window, cx| close(window, cx));
        }
    }
    let size = size(px(480.), px(270.));
    let origin = cx
        .primary_display()
        .map(|d| {
            let b = d.bounds();
            point(
                b.origin.x + b.size.width - size.width - px(24.),
                b.origin.y + b.size.height - size.height - px(72.),
            )
        })
        .unwrap_or(point(px(80.), px(80.)));
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(origin, size))),
        titlebar: None,
        kind: WindowKind::PopUp,
        // Keep the main window active when ordering the Mac panel onscreen.
        focus: !cfg!(target_os = "macos"),
        is_resizable: true,
        is_minimizable: false,
        window_min_size: Some(gpui_kit::size(px(256.), px(144.))),
        // never the active window on macOS (see focus), which GPUI otherwise limits to ~30 fps
        inactive_frame_interval: None,
        ..Default::default()
    };
    let res = gpui_kit::open_window(opts, cx, |window, cx| {
        #[cfg(target_os = "macos")]
        {
            window.set_window_title("Picture in Picture");
            crate::macos::configure_pip_window(window);
        }
        let pl = active.clone();
        let view = cx.new(|cx| {
            let mut v = PlayerView::new(pl.clone(), false, cx);
            v.pip = true;
            v
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        cx.new(|_| PipView { view, player: pl, focus })
    });
    match res {
        Ok((h, _)) => {
            cx.set_global(PipWindow(Some((h, active.clone()))));
            active.update(cx, |p, cx| p.set_in_pip(true, cx));
            let _ = h.update(cx, |_, window, cx| {
                crate::idle_inhibit::set_pip_window(cx, Some(window));
            });
        }
        Err(e) => toast(cx, crate::ui::friendly_error(&e.to_string())),
    }
    cx.refresh_windows();
}

pub fn close(window: &mut Window, cx: &mut App) {
    clear(cx);
    window.remove_window();
    cx.refresh_windows();
}

fn clear(cx: &mut App) {
    let previous = owner(cx);
    cx.set_global(PipWindow(None));
    crate::idle_inhibit::set_pip_window(cx, None);
    if let Some(previous) = previous {
        previous.update(cx, |p, cx| p.set_in_pip(false, cx));
    }
    player(cx).update(cx, |p, cx| p.refresh_idle_guard(cx));
}

/// Starts moving the window under the pointer, as if its title bar was pressed.
/// GPUI's Windows backend has no `start_window_move`, so Windows asks the
/// system directly. The request is posted, not sent: the system's move loop
/// must start from the message loop, not inside this event handler (it calls
/// back into GPUI, which is busy handling the event).
pub(crate) fn begin_window_move(window: &Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        #[repr(C)]
        struct Pt {
            x: i32,
            y: i32,
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn ReleaseCapture() -> i32;
            fn GetCursorPos(p: *mut Pt) -> i32;
            fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
        }
        const WM_NCLBUTTONDOWN: u32 = 0x00A1;
        const HTCAPTION: usize = 2;
        if let Ok(h) = HasWindowHandle::window_handle(window) {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                // SAFETY: plain Win32 calls on a live window owned by this process
                unsafe {
                    let mut p = Pt { x: 0, y: 0 };
                    GetCursorPos(&mut p);
                    let lparam = ((p.y as u16 as isize) << 16) | (p.x as u16 as isize);
                    ReleaseCapture();
                    PostMessageW(w.hwnd.get(), WM_NCLBUTTONDOWN, HTCAPTION, lparam);
                }
            }
        }
    }
    #[cfg(not(windows))]
    window.start_window_move();
}

pub fn window_closed(id: WindowId, cx: &mut App) {
    if handle(cx).is_some_and(|h| h.window_id() == id) {
        clear(cx);
        cx.refresh_windows();
    }
}

/// What the main window shows where the player sits: the video, or while
/// the PiP window plays it, a placeholder (like YouTube) so the picture
/// isn't drawn twice.
pub(super) fn main_surface(view: &Entity<PlayerView>, cx: &App) -> super::slot::SlotFollower {
    let content = if handle(cx).is_some() {
        placeholder().into_any_element()
    } else {
        view.clone().into_any_element()
    };
    super::slot::SlotFollower::new(cx, content)
}

fn placeholder() -> impl IntoElement {
    div()
        .size_full()
        .bg(black())
        .text_color(white())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .child(icon("picture-in-picture").size_8())
        .child(crate::i18n::t("Playing in picture-in-picture").to_string())
        .child(Button::new("return-from-pip").label(crate::i18n::t("Return to player").to_string()).on_click(|_, _, cx| {
            if let Some(panel) = handle(cx) {
                let _ = panel.update(cx, |_, window, _| window.remove_window());
            }
        }))
}

impl PipView {
    /// The main window's playback keys, for when the PiP window has the focus.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &ev.keystroke.modifiers;
        if m.control || m.platform || m.alt {
            return;
        }
        let k = super::app::shortcut_key(&ev.keystroke.key);
        if k == "escape" {
            close(window, cx);
            return;
        }
        let key = match ev.keystroke.key_char.as_deref() {
            Some(c @ ("-" | "+" | "<" | ">")) => c,
            _ => k,
        };
        let step = crate::store::settings_snapshot().seek_step;
        let handled = self.player.update(cx, |pl, cx| {
            if pl.video.is_none() {
                return false;
            }
            match (key, m.shift) {
                ("space", _) | ("k", false) => {
                    if !ev.is_held {
                        pl.toggle_pause(cx);
                    }
                }
                ("j", false) => pl.seek_by(-10.0),
                ("l", false) => pl.seek_by(10.0),
                ("left" | "right", _) => {
                    let before = pl.time;
                    pl.seek_by(if key == "left" { -step } else { step });
                    pl.show_keyboard_feedback(KeyboardFeedback::Seek(pl.time - before), cx);
                }
                ("up" | "down", _) => {
                    pl.set_volume(pl.volume + if key == "up" { 5.0 } else { -5.0 });
                    pl.show_keyboard_feedback(KeyboardFeedback::Volume(pl.volume), cx);
                }
                ("m", false) => pl.toggle_mute(),
                ("," | "." | "=", true) | ("<" | ">" | "-" | "+", _) => {
                    pl.step_speed(matches!(key, "." | "=" | ">" | "+"));
                    crate::ui::toast(cx, crate::i18n::tf!("Speed {0}x", format_args!("{:.2}", pl.speed)));
                }
                _ => return false,
            }
            cx.notify();
            true
        });
        if handled {
            cx.stop_propagation();
        }
    }
}

impl Render for PipView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("pip")
            .group("pip")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)))
            .relative()
            .size_full()
            .bg(black())
            .child(self.view.clone())
            // top strip: drag to move, close button
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .invisible()
                    .group_hover("pip", |s| s.visible())
                    .bg(linear_gradient(180., linear_color_stop(hsla(0., 0., 0., 0.6), 0.), linear_color_stop(hsla(0., 0., 0., 0.), 1.)))
                    .child({
                        let drag = div().id("pip-drag").flex_1().h_full();
                        #[cfg(target_os = "macos")]
                        let drag = drag.on_mouse_down(MouseButton::Left, |_, window, cx| {
                            cx.stop_propagation();
                            window.start_window_move();
                        });
                        #[cfg(not(target_os = "macos"))]
                        let drag = drag.window_control_area(WindowControlArea::Drag);
                        drag
                    })
                    .child(
                        div()
                            .id("pip-close")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(32.))
                            .cursor_pointer()
                            .hover(|s| s.bg(hsla(0., 0., 1., 0.15)))
                            .child(icon("x").size_4().text_color(white()))
                            .on_click(|_, window, cx| {
                                cx.stop_propagation();
                                window.remove_window();
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests;
