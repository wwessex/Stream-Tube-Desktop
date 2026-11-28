use crate::player::controller::{Player, PlayerHandle, player};
use futures::{StreamExt, channel::mpsc};
use gpui_kit::{App, Entity, Subscription, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Foundation::TypedEventHandler;
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
    SystemMediaTransportControlsButton as Button, SystemMediaTransportControlsButtonPressedEventArgs,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;
use windows::core::{HSTRING, factory};

#[derive(Default)]
struct Published {
    video: Option<(String, String)>,
    playback: Option<(MediaPlaybackStatus, bool, bool)>,
}

impl Published {
    fn update(&mut self, transport: &SystemMediaTransportControls, player: &Player) -> windows::core::Result<()> {
        let video = player.video.as_ref().map(|v| (v.title.as_str(), v.channel_name.as_str()));
        let previous = self.video.as_ref().map(|(title, artist)| (title.as_str(), artist.as_str()));
        if video != previous {
            let display = transport.DisplayUpdater()?;
            if let Some((title, channel)) = video {
                display.SetType(MediaPlaybackType::Video)?;
                let properties = display.VideoProperties()?;
                properties.SetTitle(&HSTRING::from(title))?;
                properties.SetSubtitle(&HSTRING::from(channel))?;
                display.Update()?;
            } else {
                display.ClearAll()?;
            }
            self.video = video.map(|(title, channel)| (title.to_owned(), channel.to_owned()));
        }
        let status = if video.is_none() || player.error.is_some() {
            MediaPlaybackStatus::Closed
        } else if player.eof {
            MediaPlaybackStatus::Stopped
        } else if player.paused {
            MediaPlaybackStatus::Paused
        } else {
            MediaPlaybackStatus::Playing
        };
        let next = player.queue_pos.is_some_and(|i| i + 1 < player.queue.len());
        let previous = player.queue_pos.is_some_and(|i| i > 0);
        if self.playback != Some((status, next, previous)) {
            transport.SetIsEnabled(status != MediaPlaybackStatus::Closed)?;
            transport.SetIsPlayEnabled(status != MediaPlaybackStatus::Closed)?;
            transport.SetIsPauseEnabled(status != MediaPlaybackStatus::Closed)?;
            transport.SetIsNextEnabled(next)?;
            transport.SetIsPreviousEnabled(previous)?;
            transport.SetPlaybackStatus(status)?;
            self.playback = Some((status, next, previous));
        }
        Ok(())
    }
}

fn observe_player(active: &Entity<Player>, transport: &SystemMediaTransportControls, cx: &mut App) -> Subscription {
    let mut published = Published::default();
    let _ = published.update(transport, active.read(cx));
    let transport = transport.clone();
    cx.observe(active, move |entity, cx| {
        let _ = published.update(&transport, entity.read(cx));
    })
}

pub(crate) fn init(window: &Window, cx: &mut App) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    let Ok(interop) = factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>() else { return };
    // SAFETY: the HWND belongs to the live main window on this thread.
    let Ok(transport) = (unsafe { interop.GetForWindow::<SystemMediaTransportControls>(HWND(handle.hwnd.get() as _)) }) else {
        return;
    };
    let (send, mut buttons) = mpsc::unbounded();
    let handler = TypedEventHandler::<SystemMediaTransportControls, SystemMediaTransportControlsButtonPressedEventArgs>::new(move |_, event| {
        if let Some(event) = event.as_ref() {
            if let Ok(button) = event.Button() {
                let _ = send.unbounded_send(button);
            }
        }
        Ok(())
    });
    let Ok(token) = transport.ButtonPressed(&handler) else { return };
    let active = player(cx);
    let mut binding = (active.entity_id(), observe_player(&active, &transport, cx));
    let observed = transport.clone();
    cx.observe_global::<PlayerHandle>(move |cx| {
        let active = player(cx);
        if binding.0 != active.entity_id() {
            binding = (active.entity_id(), observe_player(&active, &observed, cx));
        }
    }).detach();
    cx.spawn(async move |cx| {
        while let Some(button) = buttons.next().await {
            let _ = cx.update(|cx| player(cx).update(cx, |p, cx| {
                if p.video.is_none() {
                    return;
                }
                match button {
                    Button::Play if p.eof => p.toggle_pause(cx),
                    Button::Play => p.set_paused(false, cx),
                    Button::Pause => p.set_paused(true, cx),
                    Button::Next => {
                        if let Some(index) = p.queue_pos.filter(|i| i + 1 < p.queue.len()) {
                            p.play_queue_index(index + 1, cx);
                        }
                    }
                    Button::Previous => {
                        if let Some(index) = p.queue_pos.filter(|i| *i > 0) {
                            p.play_queue_index(index - 1, cx);
                        }
                    }
                    _ => return,
                }
                cx.notify();
            }));
        }
    }).detach();
    cx.on_app_quit(move |_| {
        let _ = transport.RemoveButtonPressed(token);
        let _ = transport.SetIsEnabled(false);
        std::future::ready(())
    }).detach();
}
