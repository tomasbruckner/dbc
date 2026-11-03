//! Resizable, size-remembering modal dialogs: the pure size math.

use dbc_state::ModalSize;
use gpui::{Pixels, Point, Size};

/// Free space kept on each side of the window around a dialog.
pub(crate) const MODAL_MARGIN: f32 = 32.0;
/// A dialog never gets shorter than this.
pub(crate) const MODAL_MIN_H: f32 = 120.0;

/// Which handle is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    Right,
    Bottom,
    Corner,
}

/// The width and the optional fixed height to paint a dialog with.
///
/// Width is the stored one (else `default_w`), kept within
/// `[min(default_w, max_w), max_w]` where `max_w` leaves `MODAL_MARGIN` free
/// on both sides of the window. Height is the stored one within
/// `[MODAL_MIN_H, window height - 2 * MODAL_MARGIN]`, `None` when never
/// stored. `f32::max`/`min` drop a NaN operand and the bounds are never NaN,
/// so `clamp` cannot panic (same reasoning as `clamp_editor_height`).
pub(crate) fn effective_size(
    stored: Option<ModalSize>,
    default_w: f32,
    window: Size<Pixels>,
) -> (f32, Option<f32>) {
    let max_w = (f32::from(window.width) - 2.0 * MODAL_MARGIN).max(0.0);
    let min_w = default_w.min(max_w);
    let w = stored.map_or(default_w, |s| f32::from(s.w));
    // A NaN `w` (NaN default) falls to the lower bound.
    let w = if w.is_nan() { min_w } else { w.clamp(min_w, max_w) };

    let max_h = (f32::from(window.height) - 2.0 * MODAL_MARGIN).max(MODAL_MIN_H);
    let h = stored
        .and_then(|s| s.h)
        .map(|h| f32::from(h).clamp(MODAL_MIN_H, max_h));
    (w, h)
}

/// The size after dragging `edge` from `start_pointer` to `pointer`. The
/// dialog is centred, so the moved edge only follows the pointer when the
/// size changes by twice the pointer delta.
pub(crate) fn dragged_size(
    start: (f32, f32),
    start_pointer: Point<Pixels>,
    pointer: Point<Pixels>,
    edge: Edge,
) -> (f32, f32) {
    let dx = f32::from(pointer.x - start_pointer.x);
    let dy = f32::from(pointer.y - start_pointer.y);
    let w = if matches!(edge, Edge::Right | Edge::Corner) { start.0 + 2.0 * dx } else { start.0 };
    let h = if matches!(edge, Edge::Bottom | Edge::Corner) { start.1 + 2.0 * dy } else { start.1 };
    (w, h)
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    use gpui::{point, px, size};

    fn win(w: f32, h: f32) -> Size<Pixels> {
        size(px(w), px(h))
    }
    fn ms(w: u16, h: Option<u16>) -> Option<ModalSize> {
        Some(ModalSize { w, h })
    }

    #[test]
    fn never_resized_is_todays_look() {
        assert_eq!(effective_size(None, 560., win(1200., 800.)), (560., None));
    }

    #[test]
    fn stored_size_is_used() {
        assert_eq!(
            effective_size(ms(640, Some(500)), 560., win(1200., 800.)),
            (640., Some(500.))
        );
    }

    #[test]
    fn clamp_fits_a_smaller_window() {
        assert_eq!(
            effective_size(ms(900, Some(700)), 560., win(800., 600.)),
            (736., Some(536.))
        );
    }

    #[test]
    fn cannot_shrink_below_default_width() {
        assert_eq!(effective_size(ms(300, None), 560., win(1200., 800.)).0, 560.);
    }

    #[test]
    fn narrow_window_beats_default_width() {
        assert_eq!(effective_size(None, 560., win(500., 800.)).0, 436.);
    }

    #[test]
    fn height_has_a_floor() {
        assert_eq!(
            effective_size(ms(640, Some(10)), 560., win(1200., 800.)).1,
            Some(120.)
        );
    }

    #[test]
    fn nan_never_panics() {
        let (w, h) = effective_size(ms(640, Some(500)), 560., win(f32::NAN, f32::NAN));
        assert!(w.is_finite());
        assert!(h.unwrap().is_finite());
        let (w, h) = effective_size(None, f32::NAN, win(1200., 800.));
        assert!(w.is_finite());
        assert_eq!(h, None);
    }

    #[test]
    fn right_edge_grows_by_twice_the_delta() {
        let (p0, p1) = (point(px(100.), px(100.)), point(px(150.), px(120.)));
        assert_eq!(dragged_size((560., 300.), p0, p1, Edge::Right), (660., 300.));
        assert_eq!(dragged_size((560., 300.), p0, p1, Edge::Bottom), (560., 340.));
        assert_eq!(dragged_size((560., 300.), p0, p1, Edge::Corner), (660., 340.));
    }
}
