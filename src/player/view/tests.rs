use super::PlayerView;
use crate::{player::controller, ui::Pal};
use gpui::{Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext, point, px, size};

fn open_player(
    cx: &mut TestAppContext,
) -> (
    Entity<controller::Player>,
    Entity<PlayerView>,
    VisualTestContext,
) {
    let player = cx.update(|cx| {
        cx.set_global(Pal::new(true, 0x3ea6ff));
        cx.set_global(crate::ui::app::UiState::default());
        controller::init(cx);
        let player = controller::player(cx);
        player.update(cx, |player, _| player.duration = 160.0);
        player
    });
    let window = cx.open_window(size(px(900.0), px(520.0)), |_, cx| {
        PlayerView::new(player.clone(), false, cx)
    });
    let view = window.root(cx).unwrap();
    let mut ui = VisualTestContext::from_window(window.into(), cx);
    ui.update(|window, cx| window.draw(cx).clear(cx));
    (player, view, ui)
}

#[gpui::test]
fn seek_drag_ends_outside_the_track(cx: &mut TestAppContext) {
    let (player, view, mut ui) = open_player(cx);
    let track = view.read_with(cx, |view, _| view.seek_bounds.get());
    assert!(track.size.width > px(0.0));
    let start = point(track.origin.x + track.size.width * 0.25, track.center().y);
    let release = point(
        track.origin.x + track.size.width * 0.75,
        track.bottom() + px(24.0),
    );
    let after_release = point(start.x, release.y);
    let modifiers = Modifiers::default();

    for receive_mouse_up in [true, false] {
        player.update(cx, |player, _| player.seek_to(12.0));
        ui.simulate_mouse_move(start, None, modifiers);
        ui.simulate_mouse_down(start, MouseButton::Left, modifiers);
        ui.simulate_mouse_move(release, MouseButton::Left, modifiers);
        assert_eq!(player.read_with(cx, |player, _| player.time), 12.0);
        assert_eq!(view.read_with(cx, |view, _| view.drag_seek), Some(0.75));

        if receive_mouse_up {
            ui.simulate_mouse_up(release, MouseButton::Left, modifiers);
            assert_eq!(player.read_with(cx, |player, _| player.time), 120.0);
            assert_eq!(view.read_with(cx, |view, _| view.drag_seek), None);
        }
        // A move without the button also recovers a release the player missed.
        ui.simulate_mouse_move(after_release, None, modifiers);
        assert_eq!(player.read_with(cx, |player, _| player.time), 120.0);
        assert_eq!(view.read_with(cx, |view, _| view.drag_seek), None);
    }
}

#[gpui::test]
fn volume_drag_ends_outside_the_track(cx: &mut TestAppContext) {
    let (player, view, mut ui) = open_player(cx);
    let track = view.read_with(cx, |view, _| view.vol_bounds.get());
    assert!(track.size.width > px(0.0));
    let start = point(track.origin.x + track.size.width * 0.25, track.center().y);
    // above the track: below it is off the bottom of the window
    let release = point(
        track.origin.x + track.size.width * 0.625,
        track.top() - px(12.0),
    );
    let after_release = point(start.x, release.y);
    let modifiers = Modifiers::default();

    for receive_mouse_up in [true, false] {
        ui.simulate_mouse_move(start, None, modifiers);
        ui.simulate_mouse_down(start, MouseButton::Left, modifiers);
        ui.simulate_mouse_move(release, MouseButton::Left, modifiers);
        assert_eq!(player.read_with(cx, |player, _| player.volume), 62.5);
        assert!(view.read_with(cx, |view, _| view.drag_vol));

        if receive_mouse_up {
            ui.simulate_mouse_up(release, MouseButton::Left, modifiers);
            assert!(!view.read_with(cx, |view, _| view.drag_vol));
        }
        ui.simulate_mouse_move(after_release, None, modifiers);
        assert_eq!(player.read_with(cx, |player, _| player.volume), 62.5);
        assert!(!view.read_with(cx, |view, _| view.drag_vol));
    }
}
