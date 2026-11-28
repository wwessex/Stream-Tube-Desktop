use super::{handle, main_surface, open, window_closed};
use crate::{
    player::{controller, view::PlayerView},
    ui::{
        Pal,
        slot::{PlayerSlot, player_slot},
    },
};
use gpui::{
    AppContext, Context, Entity, IntoElement, Modifiers, ParentElement, Pixels, Point, Render,
    Styled, TestAppContext, VisualTestContext, Window, div, point, px,
};

struct MainPlayer(Entity<PlayerView>);

impl Render for MainPlayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(player_slot(cx))
            .child(main_surface(&self.0, cx))
    }
}

enum Exit {
    ClosePanel,
    ReturnToPlayer,
}

#[gpui::test]
fn closing_pip_preserves_playing_video(cx: &mut TestAppContext) {
    check_pip_handoff(cx, Exit::ClosePanel, false);
}

#[gpui::test]
fn closing_pip_preserves_paused_video(cx: &mut TestAppContext) {
    check_pip_handoff(cx, Exit::ClosePanel, true);
}

#[gpui::test]
fn return_to_player_restores_playing_video(cx: &mut TestAppContext) {
    check_pip_handoff(cx, Exit::ReturnToPlayer, false);
}

#[gpui::test]
fn return_to_player_restores_paused_video(cx: &mut TestAppContext) {
    check_pip_handoff(cx, Exit::ReturnToPlayer, true);
}

fn check_pip_handoff(cx: &mut TestAppContext, exit: Exit, paused_in_pip: bool) {
    let main_player = cx.update(|cx| {
        gpui_kit::init(cx);
        cx.set_global(Pal::new(true, 0x3ea6ff));
        cx.set_global(crate::ui::app::UiState::default());
        cx.set_global(PlayerSlot::default());
        controller::init(cx);
        cx.on_window_closed(|cx, id| window_closed(id, cx)).detach();
        let player = controller::player(cx);
        player.update(cx, |player, _| {
            player.duration = 160.0;
            player.seek_to(34.0);
            player.paused = !paused_in_pip;
        });
        player
    });
    let main_window = cx.add_window(|_, cx| {
        MainPlayer(cx.new(|cx| PlayerView::new(main_player.clone(), false, cx)))
    });
    let mut main = VisualTestContext::from_window(main_window.into(), cx);
    let main_size = main.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.viewport_size()
    });
    let video_area = point(main_size.width / 2.0, main_size.height / 4.0);

    cx.update(open);
    let panel = cx.read(handle).expect("PiP window opened");
    let mut pip = VisualTestContext::from_window(panel, cx);
    let panel_size = pip.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.viewport_size()
    });
    let centre = point(panel_size.width / 2.0, panel_size.height / 2.0);
    click(&mut pip, centre);
    assert_eq!(
        main_player.read_with(cx, |player, _| player.paused),
        paused_in_pip
    );

    // Stand in for progress delivered by mpv while the panel owns the video surface.
    main_player.update(cx, |player, _| player.seek_to(83.25));
    match exit {
        Exit::ClosePanel => click(&mut pip, point(panel_size.width - px(16.0), px(16.0))),
        Exit::ReturnToPlayer => {
            click(&mut main, video_area);
            assert_eq!(
                main_player.read_with(cx, |player, _| player.paused),
                paused_in_pip,
                "the placeholder must replace the main playback surface while PiP is open"
            );
            // The Return to player button sits below the centred placeholder heading.
            click(
                &mut main,
                point(main_size.width / 2.0, main_size.height / 2.0 + px(40.0)),
            );
        }
    }

    assert!(
        cx.read(handle).is_none(),
        "the closed panel must release the main player"
    );
    assert!(
        main_window.root(cx).is_ok(),
        "the main window must remain open"
    );
    assert_eq!(
        main_player.read_with(cx, |player, _| (player.time, player.paused)),
        (83.25, paused_in_pip)
    );

    if matches!(exit, Exit::ReturnToPlayer) {
        click(&mut main, video_area);
        assert_eq!(
            main_player.read_with(cx, |player, _| player.paused),
            !paused_in_pip,
            "the restored main player must respond to playback controls"
        );
    }
}

fn click(window: &mut VisualTestContext, position: Point<Pixels>) {
    window.simulate_mouse_move(position, None, Modifiers::default());
    window.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
    window.simulate_click(position, Modifiers::default());
}
