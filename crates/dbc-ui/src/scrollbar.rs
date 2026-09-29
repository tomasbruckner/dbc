//! Scrollbars for every scrolling surface (user, 2026-09-02: „chybí
//! scrollbary třeba vertikální ve stromě nebo v historii atd. prostě všude
//! kde je něco na scroll"). GPUI ships none; Zed's lives in its `ui` crate
//! with theme/settings/animation baggage this app does not want. This is
//! the small version the user chose: always visible while the content
//! overflows, overlaid on the right edge (no layout shift), draggable,
//! click-to-page in the track.
//!
//! Pure geometry first (`thumb`, `offset_for_thumb_start`) — that is where
//! scrollbars break (empty list, content exactly the viewport, offset past
//! the end) and the only part a test can reach. Then one
//! `UniformListDecoration` for the eight `uniform_list`s. The grid's
//! horizontal bar predates this module (`grid::h_thumb`) and uses the same
//! formula.
//!
//! Coordinates: `UniformList` hands its decoration bounds whose origin is
//! ALREADY shifted by the scroll offset (the decoration lives in content
//! space and scrolls with the rows), so everything that must stay put in
//! the viewport is a CHILD placed at `-scroll_offset.y + …` — a child, not
//! the root, because taffy pins a root node at (0, 0) whatever its inset
//! says (the root's `top()` is silently ignored).
//!
//! The drag is window-wide: once the thumb is held, moves are read from a
//! listener registered on the window itself (`Window::on_mouse_event`,
//! reachable from a `canvas` paint), not from any element's hover. An
//! element-bound listener stops the moment the pointer leaves the list —
//! which it does on every real drag (user, 2026-09-14: „vyjedu myší do
//! prostředního panelu a nefunguje").

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{
    canvas, div, fill, point, prelude::*, px, size, AnyElement, App, Bounds, DispatchPhase,
    ElementId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollHandle, UniformListDecoration, UniformListScrollHandle, Window,
};

use crate::theme::ActiveTheme;

/// Track width — the overlay's footprint on the right edge.
pub const BAR_WIDTH: f32 = 10.0;
/// A thumb never gets shorter than this: in a 100 000-row grid the honest
/// proportion would be a pixel nobody can grab.
pub const THUMB_MIN: f32 = 24.0;
const THUMB_INSET: f32 = 2.0;
/// A click in the track moves by this much of the viewport.
const PAGE_FRACTION: f32 = 0.9;

/// `(thumb_start, thumb_len)` along the axis, or `None` when nothing
/// overflows — the caller draws nothing then.
pub fn thumb(viewport: f32, content: f32, offset: f32) -> Option<(f32, f32)> {
    if viewport <= 1.0 || content <= viewport + 1.0 {
        return None;
    }
    let max_off = content - viewport;
    let offset = offset.clamp(0.0, max_off);
    let len = (viewport / content * viewport).max(THUMB_MIN).min(viewport);
    let travel = (viewport - len).max(0.0);
    Some(((offset / max_off) * travel, len))
}

/// Inverse of [`thumb`]: the content offset that puts the thumb's start at
/// `start`. Clamped to the track, so a drag past either end parks the
/// thumb there.
pub fn offset_for_thumb_start(viewport: f32, content: f32, start: f32) -> f32 {
    let Some((_, len)) = thumb(viewport, content, 0.0) else { return 0.0 };
    let travel = (viewport - len).max(0.0);
    if travel <= 0.0 {
        return 0.0;
    }
    (start.clamp(0.0, travel) / travel) * (content - viewport)
}

/// A thumb drag in progress: where inside the thumb the pointer grabbed it,
/// so the thumb does not jump to centre itself under the cursor.
#[derive(Clone, Copy, Debug)]
struct Drag {
    grab: f32,
}

/// The scroll handle plus the drag state that has to outlive one frame
/// (the decoration itself is rebuilt every prepaint). One per list, held
/// by the list's owner; `list` is what `.track_scroll()` takes.
#[derive(Clone)]
pub struct ScrollbarHandle {
    pub list: UniformListScrollHandle,
    drag: Rc<RefCell<Option<Drag>>>,
}

impl Default for ScrollbarHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollbarHandle {
    pub fn new() -> Self {
        Self::with_list(UniformListScrollHandle::new())
    }

    /// Wraps a handle the owner already has (the grid's `scroll_handle`,
    /// which `scroll_to_item` and friends keep using) — both share one
    /// `Rc`, so there is still exactly one scroll position.
    pub fn with_list(list: UniformListScrollHandle) -> Self {
        Self { list, drag: Rc::new(RefCell::new(None)) }
    }

    /// The element for `.with_decoration(…)`. Cheap; build it every render.
    pub fn decoration(&self) -> ListScrollbar {
        ListScrollbar { handle: self.clone() }
    }

    fn set_y(&self, x: Pixels, y_offset: f32) {
        self.list.0.borrow().base_handle.set_offset(point(x, px(-y_offset)));
    }
}

pub struct ListScrollbar {
    handle: ScrollbarHandle,
}

impl UniformListDecoration for ListScrollbar {
    fn compute(
        &self,
        _visible_range: Range<usize>,
        bounds: Bounds<Pixels>,
        scroll_offset: Point<Pixels>,
        item_height: Pixels,
        item_count: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let viewport = f32::from(bounds.size.height);
        let content = f32::from(item_height) * item_count as f32;
        let offset = -f32::from(scroll_offset.y);
        let Some((start, len)) = thumb(viewport, content, offset) else {
            *self.handle.drag.borrow_mut() = None;
            return div().into_any_element();
        };
        let theme = *cx.theme();
        let width = f32::from(bounds.size.width);
        // `bounds.origin` is the scrolled origin; the viewport's top in
        // window coordinates is one scroll offset above it.
        let viewport_top = f32::from(bounds.origin.y) + offset;
        let x = scroll_offset.x;

        let track_handle = self.handle.clone();
        let track = div()
            .id("scrollbar-track")
            .absolute()
            .top(px(offset))
            .right(px(0.))
            .w(px(BAR_WIDTH))
            .h(px(viewport))
            .bg(theme.scrollbar_track)
            .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, window, cx| {
                // Page towards the click: above the thumb goes up, below goes
                // down. The thumb itself sits on top and stops propagation,
                // so this only ever sees the bare track.
                let y = f32::from(ev.position.y) - viewport_top;
                let page = viewport * PAGE_FRACTION;
                let next = if y < start { offset - page } else { offset + page };
                track_handle.set_y(x, next.clamp(0.0, content - viewport));
                cx.stop_propagation();
                window.refresh();
            });

        let thumb_drag = self.handle.drag.clone();
        let thumb = div()
            .id("scrollbar-thumb")
            .absolute()
            .top(px(offset + start))
            .right(px(THUMB_INSET))
            .w(px(BAR_WIDTH - 2.0 * THUMB_INSET))
            .h(px(len))
            .rounded(px(3.))
            .bg(theme.scrollbar_thumb)
            .on_mouse_down(MouseButton::Left, move |ev: &MouseDownEvent, _window, cx| {
                *thumb_drag.borrow_mut() =
                    Some(Drag { grab: f32::from(ev.position.y) - (viewport_top + start) });
                cx.stop_propagation();
            });

        // The drag itself: window-wide listeners, alive for one frame and
        // re-registered on every paint (see the module doc). They do nothing
        // unless a drag is on, and never stop propagation, so rows and
        // everything else keep their clicks and hovers. A move without the
        // button held ends a drag whose release was never seen (the pointer
        // left the window).
        let move_handle = self.handle.clone();
        let up_drag = self.handle.drag.clone();
        let listeners = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, _cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let Some(drag) = *move_handle.drag.borrow() else { return };
                    if ev.pressed_button != Some(MouseButton::Left) {
                        *move_handle.drag.borrow_mut() = None;
                        return;
                    }
                    let start = f32::from(ev.position.y) - viewport_top - drag.grab;
                    move_handle.set_y(x, offset_for_thumb_start(viewport, content, start));
                    window.refresh();
                });
                window.on_mouse_event(move |ev: &MouseUpEvent, _phase, _window, _cx| {
                    if ev.button == MouseButton::Left {
                        *up_drag.borrow_mut() = None;
                    }
                });
            },
        );

        div()
            .w(px(width))
            .h(px(viewport))
            .child(track)
            .child(thumb)
            .child(listeners)
            .into_any_element()
    }
}

/// Vertical scrolling for an ordinary `div` — the dialog case, where the
/// content is one tall panel rather than `uniform_list` rows. Capped at
/// the parent's height (`max_h_full`), so a panel that fits looks exactly
/// as before and one that does not scrolls instead of hanging off both
/// ends of the window (user, 2026-09-29).
///
/// Same bar as [`ListScrollbar`]: overlaid on the right edge, draggable,
/// click-to-page. It is painted by a `canvas` that reads the div's
/// `ScrollHandle` at paint time — after the div's prepaint has measured
/// it, so the numbers are this frame's — and its listeners are
/// window-wide for the same reason as the list's (see the module doc).
///
/// The handle and drag live in `use_keyed_state` under `id`, so a caller
/// that renders a fresh element every frame (every modal does) keeps its
/// scroll position without a field of its own. The id must therefore be
/// stable across frames and unique among what is on screen together.
#[derive(IntoElement)]
pub struct ScrollY {
    id: ElementId,
    child: AnyElement,
}

pub fn scroll_y(id: impl Into<ElementId>, child: impl IntoElement) -> ScrollY {
    ScrollY { id: id.into(), child: child.into_any_element() }
}

struct ScrollYState {
    handle: ScrollHandle,
    drag: Rc<RefCell<Option<Drag>>>,
}

impl RenderOnce for ScrollY {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| ScrollYState {
            handle: ScrollHandle::new(),
            drag: Rc::new(RefCell::new(None)),
        });
        let (handle, drag) = {
            let s = state.read(cx);
            (s.handle.clone(), s.drag.clone())
        };
        let theme = *cx.theme();
        let bar_handle = handle.clone();
        div()
            .id(self.id)
            .max_h_full()
            .overflow_y_scroll()
            .track_scroll(&handle)
            .child(self.child)
            .child(
                canvas(|_, _, _| (), move |_, _, window, _| paint_y_bar(&bar_handle, &drag, theme, window))
                    .absolute()
                    .size_0(),
            )
    }
}

fn paint_y_bar(
    handle: &ScrollHandle,
    drag: &Rc<RefCell<Option<Drag>>>,
    theme: crate::theme::Theme,
    window: &mut Window,
) {
    let b = handle.bounds();
    let viewport = f32::from(b.size.height);
    let content = viewport + f32::from(handle.max_offset().y);
    let offset = -f32::from(handle.offset().y);
    let Some((start, len)) = thumb(viewport, content, offset) else {
        *drag.borrow_mut() = None;
        return;
    };
    let top = f32::from(b.origin.y);
    let right = f32::from(b.origin.x) + f32::from(b.size.width);
    let track = Bounds::new(point(px(right - BAR_WIDTH), px(top)), size(px(BAR_WIDTH), px(viewport)));
    let thumb_bounds = Bounds::new(
        point(px(right - BAR_WIDTH + THUMB_INSET), px(top + start)),
        size(px(BAR_WIDTH - 2.0 * THUMB_INSET), px(len)),
    );
    window.paint_quad(fill(track, theme.scrollbar_track));
    window.paint_quad(fill(thumb_bounds, theme.scrollbar_thumb).corner_radii(px(3.)));

    let set_y = {
        let handle = handle.clone();
        move |y_offset: f32| {
            let x = handle.offset().x;
            handle.set_offset(point(x, px(-y_offset)));
        }
    };
    let down_drag = drag.clone();
    let down_set = set_y.clone();
    window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left || !track.contains(&ev.position) {
            return;
        }
        let y = f32::from(ev.position.y) - top;
        if y >= start && y <= start + len {
            *down_drag.borrow_mut() = Some(Drag { grab: y - start });
        } else {
            let page = viewport * PAGE_FRACTION;
            let next = if y < start { offset - page } else { offset + page };
            down_set(next.clamp(0.0, content - viewport));
        }
        cx.stop_propagation();
        window.refresh();
    });
    let move_drag = drag.clone();
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, _cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let Some(d) = *move_drag.borrow() else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            *move_drag.borrow_mut() = None;
            return;
        }
        let start = f32::from(ev.position.y) - top - d.grab;
        set_y(offset_for_thumb_start(viewport, content, start));
        window.refresh();
    });
    let up_drag = drag.clone();
    window.on_mouse_event(move |ev: &MouseUpEvent, _phase, _window, _cx| {
        if ev.button == MouseButton::Left {
            *up_drag.borrow_mut() = None;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_scroll_means_no_thumb() {
        assert_eq!(thumb(200.0, 0.0, 0.0), None, "empty list");
        assert_eq!(thumb(200.0, 200.0, 0.0), None, "content exactly the viewport");
        assert_eq!(thumb(200.0, 200.5, 0.0), None, "half a pixel over is not worth a bar");
        assert_eq!(thumb(0.0, 1000.0, 0.0), None, "viewport not measured yet");
    }

    #[test]
    fn the_thumb_is_proportional_and_travels_the_whole_track() {
        // 400 of 800 visible: half-length thumb, at the top when unscrolled…
        let (start, len) = thumb(400.0, 800.0, 0.0).unwrap();
        assert_eq!((start, len), (0.0, 200.0));
        // …and flush with the bottom at the maximum offset.
        let (start, len) = thumb(400.0, 800.0, 400.0).unwrap();
        assert_eq!(start + len, 400.0);
        // Halfway is halfway.
        let (start, _) = thumb(400.0, 800.0, 200.0).unwrap();
        assert_eq!(start, 100.0);
    }

    #[test]
    fn a_huge_list_still_gets_a_grabbable_thumb() {
        let (_, len) = thumb(400.0, 100_000.0 * 22.0, 0.0).unwrap();
        assert_eq!(len, THUMB_MIN);
    }

    #[test]
    fn an_offset_past_the_end_is_clamped_not_drawn_off_the_track() {
        let (start, len) = thumb(400.0, 800.0, 9_999.0).unwrap();
        assert_eq!(start + len, 400.0);
        let (start, _) = thumb(400.0, 800.0, -50.0).unwrap();
        assert_eq!(start, 0.0);
    }

    #[test]
    fn dragging_the_thumb_round_trips_through_the_offset() {
        for offset in [0.0, 37.0, 200.0, 400.0] {
            let (start, _) = thumb(400.0, 800.0, offset).unwrap();
            let back = offset_for_thumb_start(400.0, 800.0, start);
            assert!((back - offset).abs() < 1e-3, "{offset} -> {start} -> {back}");
        }
        // Past either end of the track parks at the end.
        assert_eq!(offset_for_thumb_start(400.0, 800.0, -100.0), 0.0);
        assert_eq!(offset_for_thumb_start(400.0, 800.0, 10_000.0), 400.0);
        // No overflow, no travel: the offset is always zero.
        assert_eq!(offset_for_thumb_start(400.0, 300.0, 50.0), 0.0);
    }
}

/// The drag itself lives in GPUI mouse listeners, which the geometry tests
/// above cannot reach — and that is exactly where it broke (user,
/// 2026-09-14: „nemůžu ho chytnout a posunovat"). These drive the real
/// `uniform_list` + decoration in a GPUI test window.
#[cfg(test)]
mod drag_tests {
    use super::*;
    use crate::theme::Theme;
    use gpui::{size, uniform_list, Context, Modifiers, Render, TestAppContext, VisualTestContext};

    const ROWS: usize = 1000;
    const ROW_H: f32 = 20.0;
    /// The list's width; the window is wider, with a plain panel beside it
    /// standing in for the editor.
    const WIN_W: f32 = 300.0;
    const WIN_H: f32 = 400.0;
    const PANEL_W: f32 = 300.0;
    const CONTENT: f32 = ROWS as f32 * ROW_H;

    struct List {
        bar: ScrollbarHandle,
    }

    impl Render for List {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_row()
                .size_full()
                .child(
                    uniform_list("rows", ROWS, |range, _, _| {
                        range.map(|i| div().h(px(ROW_H)).w_full().child(format!("{i}"))).collect()
                    })
                    .track_scroll(&self.bar.list)
                    .with_decoration(self.bar.decoration())
                    .w(px(WIN_W))
                    .h_full(),
                )
                .child(div().id("panel").w(px(PANEL_W)).h_full().on_mouse_move(|_, _, _| {}))
        }
    }

    fn open(cx: &mut TestAppContext) -> (ScrollbarHandle, VisualTestContext) {
        cx.update(|cx| cx.set_global(Theme::dark()));
        let bar = ScrollbarHandle::new();
        let handle = bar.clone();
        let window =
            cx.open_window(size(px(WIN_W + PANEL_W), px(WIN_H)), move |_, _| List { bar });
        let vcx = VisualTestContext::from_window(*window, cx);
        vcx.run_until_parked();
        (handle, vcx)
    }

    fn offset(handle: &ScrollbarHandle) -> f32 {
        -f32::from(handle.list.0.borrow().base_handle.offset().y)
    }

    /// Pointer x in the middle of the bar; y in the middle of the thumb
    /// for the current offset.
    fn thumb_centre(handle: &ScrollbarHandle) -> Point<Pixels> {
        let (start, len) = thumb(WIN_H, CONTENT, offset(handle)).expect("overflows");
        point(px(WIN_W - BAR_WIDTH / 2.0), px(start + len / 2.0))
    }

    #[gpui::test]
    fn the_thumb_can_be_grabbed_and_keeps_following_the_pointer(cx: &mut TestAppContext) {
        let (handle, mut cx) = open(cx);
        let x = px(WIN_W - BAR_WIDTH / 2.0);
        assert_eq!(offset(&handle), 0.0);

        // Grab.
        let grab_at = thumb_centre(&handle);
        cx.simulate_mouse_down(grab_at, MouseButton::Left, Modifiers::none());
        assert!(handle.drag.borrow().is_some(), "mouse down on the thumb starts a drag");

        // First pull: 100px down the track.
        let y1 = grab_at.y + px(100.0);
        cx.simulate_mouse_move(point(x, y1), MouseButton::Left, Modifiers::none());
        let (start0, _) = thumb(WIN_H, CONTENT, 0.0).unwrap();
        let want1 = offset_for_thumb_start(WIN_H, CONTENT, start0 + 100.0);
        let got1 = offset(&handle);
        assert!((got1 - want1).abs() < 1.0, "first pull: got {got1}, want {want1}");

        // Second pull, now that the list is scrolled: must keep following.
        let y2 = y1 + px(100.0);
        cx.simulate_mouse_move(point(x, y2), MouseButton::Left, Modifiers::none());
        let want2 = offset_for_thumb_start(WIN_H, CONTENT, start0 + 200.0);
        let got2 = offset(&handle);
        assert!(
            (got2 - want2).abs() < 1.0,
            "second pull (list already scrolled): got {got2}, want {want2}"
        );

        // Release: further moves are plain hovering.
        cx.simulate_mouse_up(point(x, y2), MouseButton::Left, Modifiers::none());
        assert!(handle.drag.borrow().is_none(), "mouse up ends the drag");
        cx.simulate_mouse_move(point(x, y2 + px(50.0)), None, Modifiers::none());
        assert_eq!(offset(&handle), got2, "no drag, no scroll");
    }

    #[gpui::test]
    fn a_scrolled_list_can_still_be_grabbed(cx: &mut TestAppContext) {
        let (handle, mut cx) = open(cx);
        handle.set_y(px(0.0), 8000.0);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(offset(&handle), 8000.0);

        let grab_at = thumb_centre(&handle);
        cx.simulate_mouse_down(grab_at, MouseButton::Left, Modifiers::none());
        assert!(handle.drag.borrow().is_some(), "mouse down on the thumb starts a drag");
        cx.simulate_mouse_move(
            grab_at - point(px(0.0), px(50.0)),
            MouseButton::Left,
            Modifiers::none(),
        );
        let (start, _) = thumb(WIN_H, CONTENT, 8000.0).unwrap();
        let want = offset_for_thumb_start(WIN_H, CONTENT, start - 50.0);
        let got = offset(&handle);
        assert!((got - want).abs() < 1.0, "pull up from 8000: got {got}, want {want}");
    }

    /// User, 2026-09-14: „když ho chytnu a vyjedu myší do prostředního
    /// panelu, tak to nefunguje". A drag is a window-wide affair.
    #[gpui::test]
    fn the_drag_survives_the_pointer_leaving_the_list(cx: &mut TestAppContext) {
        let (handle, mut cx) = open(cx);
        let grab_at = thumb_centre(&handle);
        cx.simulate_mouse_down(grab_at, MouseButton::Left, Modifiers::none());
        assert!(handle.drag.borrow().is_some());

        // Straight into the panel beside the list, 100px further down.
        let over_panel = point(px(WIN_W + PANEL_W / 2.0), grab_at.y + px(100.0));
        cx.simulate_mouse_move(over_panel, MouseButton::Left, Modifiers::none());
        let (start0, _) = thumb(WIN_H, CONTENT, 0.0).unwrap();
        let want = offset_for_thumb_start(WIN_H, CONTENT, start0 + 100.0);
        let got = offset(&handle);
        assert!((got - want).abs() < 1.0, "pull over the panel: got {got}, want {want}");

        // Released over the panel: the drag ends there too.
        cx.simulate_mouse_up(over_panel, MouseButton::Left, Modifiers::none());
        assert!(handle.drag.borrow().is_none(), "mouse up over the panel ends the drag");
        cx.simulate_mouse_move(over_panel + point(px(0.0), px(50.0)), None, Modifiers::none());
        assert_eq!(offset(&handle), got, "no drag, no scroll");
    }

    #[gpui::test]
    fn a_click_in_the_bare_track_pages_towards_it(cx: &mut TestAppContext) {
        let (handle, mut cx) = open(cx);
        let x = px(WIN_W - BAR_WIDTH / 2.0);
        // Well below the thumb (which is at the top): one page down.
        cx.simulate_click(point(x, px(WIN_H - 10.0)), Modifiers::none());
        assert_eq!(offset(&handle), WIN_H * PAGE_FRACTION);
        assert!(handle.drag.borrow().is_none(), "a track click is not a drag");
        // Above the thumb: one page back up (clamped at the top).
        cx.simulate_click(point(x, px(1.0)), Modifiers::none());
        assert_eq!(offset(&handle), 0.0);
    }
}

/// Modal dialogs taller than the window (user, 2026-09-29: „když nemám
/// okno ve full screen a otevřu nastavení, tak je to uříznuté"). The
/// overlays centre their panel, so an oversized one hangs off BOTH ends.
#[cfg(test)]
mod scroll_y_tests {
    use super::*;
    use crate::theme::Theme;
    use gpui::{
        size, Context, Modifiers, Render, ScrollDelta, ScrollWheelEvent, TestAppContext,
        TouchPhase, VisualTestContext,
    };

    const WIN: f32 = 400.0;
    const PANEL_W: f32 = 300.0;

    /// The shape of every modal overlay: a full-window backdrop centring
    /// one fixed-width panel, the panel wrapped in [`scroll_y`].
    struct Overlay {
        filler: f32,
    }

    impl Render for Overlay {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let panel = div()
                .w(px(PANEL_W))
                .flex()
                .flex_col()
                .child(div().h(px(20.)).debug_selector(|| "top".into()))
                .child(div().h(px(self.filler)))
                .child(div().h(px(20.)).debug_selector(|| "bottom".into()));
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(scroll_y("modal", panel))
        }
    }

    fn open(cx: &mut TestAppContext, filler: f32) -> VisualTestContext {
        cx.update(|cx| cx.set_global(Theme::dark()));
        let window = cx.open_window(size(px(WIN), px(WIN)), move |_, _| Overlay { filler });
        let vcx = VisualTestContext::from_window(*window, cx);
        vcx.run_until_parked();
        vcx
    }

    fn y(cx: &mut VisualTestContext, sel: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(sel).expect("marker painted")
    }

    /// Right edge of the panel, where the bar sits.
    const BAR_X: f32 = (WIN + PANEL_W) / 2.0 - BAR_WIDTH / 2.0;

    #[gpui::test]
    fn an_oversized_panel_starts_at_its_top(cx: &mut TestAppContext) {
        let mut cx = open(cx, 800.0);
        let top = y(&mut cx, "top");
        assert!(f32::from(top.origin.y) >= 0.0, "panel top is above the window: {top:?}");
    }

    #[gpui::test]
    fn the_wheel_reaches_the_bottom_of_an_oversized_panel(cx: &mut TestAppContext) {
        let mut cx = open(cx, 800.0);
        assert!(f32::from(y(&mut cx, "bottom").bottom()) > WIN, "starts off-screen");
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(WIN / 2.0), px(WIN / 2.0)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-2000.))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
        });
        let bottom = y(&mut cx, "bottom");
        assert!(f32::from(bottom.bottom()) <= WIN + 0.5, "bottom still off-screen: {bottom:?}");
    }

    #[gpui::test]
    fn the_bar_can_be_dragged(cx: &mut TestAppContext) {
        let mut cx = open(cx, 800.0);
        let top0 = f32::from(y(&mut cx, "top").origin.y);
        // Content 840 in a 400 viewport: the thumb starts at the top.
        let (_, len) = thumb(WIN, 840.0, 0.0).unwrap();
        let grab = point(px(BAR_X), px(len / 2.0));
        cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(grab + point(px(0.), px(100.)), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(grab + point(px(0.), px(100.)), MouseButton::Left, Modifiers::none());
        let want = offset_for_thumb_start(WIN, 840.0, 100.0);
        let moved = top0 - f32::from(y(&mut cx, "top").origin.y);
        assert!((moved - want).abs() < 1.0, "scrolled {moved}, want {want}");
    }

    #[gpui::test]
    fn a_panel_that_fits_stays_centred(cx: &mut TestAppContext) {
        let mut cx = open(cx, 100.0);
        // 140 tall in 400: centred means 130 above.
        let top = y(&mut cx, "top");
        assert_eq!(f32::from(top.origin.y), 130.0);
    }
}
