use gpui_kit::component::input::InputState;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Default)]
enum Gesture {
    #[default]
    Idle,
    Pressed {
        origin: Point<Pixels>,
        ready_at: Instant,
    },
    Moving,
}

pub(super) fn wrap(
    bar: Stateful<Div>,
    search: Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    let gesture = window
        .use_state(cx, |_, _| Rc::new(Cell::new(Gesture::Idle)))
        .read(cx)
        .clone();

    // Capture motion before the input extends its selection, even if the pointer
    // has already left the bar. Only a press that started on the bar can move it.
    let motion = canvas(|_, _, _| (), {
        let gesture = gesture.clone();
        move |_, _, window, _| {
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.capture() || event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                match gesture.get() {
                    Gesture::Pressed { origin, ready_at }
                        if (event.position - origin).magnitude() >= 5. =>
                    {
                        if Instant::now() < ready_at {
                            // This press is selecting text, even if the user pauses mid-selection.
                            gesture.set(Gesture::Idle);
                            return;
                        }
                        gesture.set(Gesture::Moving);
                        gpui_kit::base::TextSelection::end(window, cx);
                        window.start_window_move();
                        cx.stop_propagation();
                    }
                    Gesture::Moving => cx.stop_propagation(),
                    _ => {}
                }
            });
        }
    });

    // This hitbox lets clicks through to the controls. Register the release
    // handler last so GPUI clears their pending presses before we consume a drag.
    let buttons = canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |_, hitbox, window, _| {
            let press = gesture.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.capture() {
                    return;
                }
                press.set(Gesture::Idle);
                if hitbox.is_hovered(window)
                    && event.button == MouseButton::Left
                    && event.click_count == 1
                    && !event.modifiers.modified()
                {
                    let input = search.read(cx);
                    let mut ready_at = Instant::now();
                    if input.input_bounds().contains(&event.position) && input.text().len() > 0 {
                        ready_at += Duration::from_millis(250);
                    }
                    press.set(Gesture::Pressed {
                        origin: event.position,
                        ready_at,
                    });
                }
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Left
                    && matches!(gesture.replace(Gesture::Idle), Gesture::Moving)
                {
                    cx.stop_propagation();
                    window.refresh();
                }
            });
        },
    );
    div()
        .relative()
        .flex_none()
        .child(motion.absolute())
        .child(bar)
        .child(buttons.absolute().inset_0())
}
