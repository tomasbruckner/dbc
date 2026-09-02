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
//! the viewport is placed at `-scroll_offset.y + …`.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{
    div, point, prelude::*, px, AnyElement, App, Bounds, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, UniformListDecoration, UniformListScrollHandle,
    Window,
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

        // The root spans the viewport so a drag keeps reporting moves while
        // the pointer is anywhere over the list, not just over the thumb.
        // Its listeners never stop propagation, so rows underneath keep
        // their clicks and hovers.
        let move_handle = self.handle.clone();
        let up_drag = self.handle.drag.clone();
        let up_out_drag = self.handle.drag.clone();
        div()
            .id("scrollbar-root")
            .absolute()
            .top(px(offset))
            .left(px(0.))
            .w(px(width))
            .h(px(viewport))
            .on_mouse_move(move |ev: &MouseMoveEvent, window, _cx| {
                let Some(drag) = *move_handle.drag.borrow() else { return };
                if ev.pressed_button != Some(MouseButton::Left) {
                    *move_handle.drag.borrow_mut() = None;
                    return;
                }
                let start = f32::from(ev.position.y) - viewport_top - drag.grab;
                move_handle.set_y(x, offset_for_thumb_start(viewport, content, start));
                window.refresh();
            })
            .on_mouse_up(MouseButton::Left, move |_: &MouseUpEvent, _window, _cx| {
                *up_drag.borrow_mut() = None;
            })
            .on_mouse_up_out(MouseButton::Left, move |_: &MouseUpEvent, _window, _cx| {
                *up_out_drag.borrow_mut() = None;
            })
            .child(track)
            .child(thumb)
            .into_any_element()
    }
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
