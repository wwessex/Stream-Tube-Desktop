//! The video players live outside the page and app trees (in `app::Shell`) so that
//! video frames only re-render the player itself, never the (cached) views around
//! it. Pages place a `player_slot()` where the video should appear; `SlotFollower`
//! draws the player over that slot in the same frame (it reads the slot bounds
//! during prepaint, after the views that record them).
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone, Default)]
pub struct PlayerSlot(pub Rc<Cell<Option<Bounds<Pixels>>>>);

impl Global for PlayerSlot {}

/// Where the mini player's video goes (inside the mini player's frame).
#[derive(Clone, Default)]
pub struct MiniSlot(pub Rc<Cell<Option<Bounds<Pixels>>>>);

impl Global for MiniSlot {}

/// The page area: the main player and previews are clipped to it, as they were
/// when they were drawn inside it.
#[derive(Clone, Default)]
pub struct PageClip(pub Rc<Cell<Option<Bounds<Pixels>>>>);

impl Global for PageClip {}

/// Clears the recorded player slot bounds (call on navigation so the old page's
/// slot size is never briefly drawn on the new page).
pub fn clear_slot(cx: &App) {
    if let Some(s) = cx.try_global::<PlayerSlot>() {
        s.0.set(None);
    }
}

/// Placeholder element that records where the player should be drawn.
pub fn player_slot(cx: &App) -> impl IntoElement {
    slot_canvas(cx.global::<PlayerSlot>().0.clone())
}

/// Fills its parent and records its bounds in `slot` while it is drawn.
pub fn slot_canvas(slot: Rc<Cell<Option<Bounds<Pixels>>>>) -> impl IntoElement {
    canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {}).size_full()
}

pub struct SlotFollower {
    slot: Rc<Cell<Option<Bounds<Pixels>>>>,
    child: AnyElement,
    clip: Option<Rc<Cell<Option<Bounds<Pixels>>>>>,
}

impl SlotFollower {
    pub fn new(cx: &App, content: impl IntoElement) -> Self {
        Self::with_slot(cx.global::<PlayerSlot>().0.clone(), content)
    }

    /// Follows any slot (e.g. the thumbnail being previewed).
    pub fn with_slot(slot: Rc<Cell<Option<Bounds<Pixels>>>>, content: impl IntoElement) -> Self {
        let child = div().size_full().child(content).into_any_element();
        Self { slot, child, clip: None }
    }

    /// Draws only inside the bounds `clip` holds (when it holds any).
    pub fn clip(mut self, clip: Rc<Cell<Option<Bounds<Pixels>>>>) -> Self {
        self.clip = Some(clip);
        self
    }

    fn mask(&self) -> Option<ContentMask<Pixels>> {
        self.clip.as_ref().and_then(|c| c.get()).map(|bounds| ContentMask { bounds })
    }
}

impl IntoElement for SlotFollower {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for SlotFollower {
    type RequestLayoutState = ();
    type PrepaintState = bool;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.position = Position::Absolute;
        style.inset.top = px(0.).into();
        style.inset.left = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(slot) = self.slot.get() else {
            window.request_animation_frame();
            return false;
        };
        // The page records the slot during prepaint, so lay out the player
        // against that size in the same frame.
        let mask = self.mask();
        window.with_content_mask(mask, |window| {
            self.child.prepaint_as_root(slot.origin, slot.size.map(AvailableSpace::Definite), window, cx);
        });
        true
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        ready: &mut bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        if *ready {
            let mask = self.mask();
            window.with_content_mask(mask, |window| self.child.paint(window, cx));
        }
    }
}
