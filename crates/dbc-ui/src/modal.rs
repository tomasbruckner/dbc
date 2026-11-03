//! Resizable, size-remembering modal dialogs: the pure size math, the
//! [`ModalSizes`] store and the one [`frame`] every modal is rendered with.
//!
//! The handles sit OUTSIDE the panel's border, so they never lie over the
//! body's own controls or its scrollbar (a handle laid over another control
//! steals its clicks). A drag reads the pointer from window-wide listeners,
//! so it survives the pointer leaving the handle, the dialog or the window,
//! and it is stored only on release.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use dbc_state::ModalSize;
use gpui::{
    anchored, canvas, deferred, div, point, prelude::*, px, AnyElement, App, BorrowAppContext,
    Bounds, CursorStyle, DispatchPhase, Div, Global, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Size, Window,
};

use crate::{scrollbar, theme::ActiveTheme, ui};

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
    fit(
        stored.map_or(default_w, |s| f32::from(s.w)),
        stored.and_then(|s| s.h).map(f32::from),
        default_w,
        window,
    )
}

/// The clamp behind [`effective_size`], for any size — a stored one or the
/// one a drag in progress has reached. Painting a live drag and committing
/// it both go through here, so the stored size is the one the user saw.
/// A NaN `w` or `h` falls to its lower bound.
fn fit(w: f32, h: Option<f32>, default_w: f32, window: Size<Pixels>) -> (f32, Option<f32>) {
    let max_w = (f32::from(window.width) - 2.0 * MODAL_MARGIN).max(0.0);
    let min_w = default_w.min(max_w);
    let w = if w.is_nan() { min_w } else { w.clamp(min_w, max_w) };
    let max_h = max_height(window);
    let h = h.map(|h| if h.is_nan() { MODAL_MIN_H } else { h.clamp(MODAL_MIN_H, max_h) });
    (w, h)
}

/// The tallest a dialog may be in `window`: its height minus the margins,
/// never below `MODAL_MIN_H`.
fn max_height(window: Size<Pixels>) -> f32 {
    (f32::from(window.height) - 2.0 * MODAL_MARGIN).max(MODAL_MIN_H)
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

/// Every dialog's remembered size, keyed by its stable dialog id. A GPUI
/// global, because the dialogs live in several entities and all of them
/// read it; `AppView` owns persisting it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModalSizes(pub BTreeMap<String, ModalSize>);

impl Global for ModalSizes {}

/// What to write to `config.toml` after the store changed: the whole map when
/// it differs from `saved`, `None` when there is nothing to save. The store
/// is seeded from the config at startup, so it is a superset of what was
/// loaded and unknown dialog ids survive without any merging.
pub(crate) fn sizes_to_persist(
    store: &ModalSizes,
    saved: &BTreeMap<String, ModalSize>,
) -> Option<BTreeMap<String, ModalSize>> {
    (store.0 != *saved).then(|| store.0.clone())
}

/// Above every other deferred layer (menus use 1).
const MODAL_PRIORITY: usize = 10;
/// Thickness of the edge handles; the corner is twice this square.
const HANDLE: f32 = 6.0;

/// A resize drag in progress. `start` is the panel's painted size when the
/// handle was grabbed, `live` the unclamped size the pointer has reached.
#[derive(Clone, Copy, Debug)]
struct Drag {
    edge: Edge,
    start: (f32, f32),
    start_pointer: Point<Pixels>,
    live: (f32, f32),
}

impl Drag {
    /// Whether this drag paints and commits a fixed height: it moved the
    /// bottom edge, or the dialog already had one. A width-only drag on a
    /// never-heightened dialog keeps its content height.
    fn fixes_height(&self, stored: Option<ModalSize>) -> bool {
        self.edge != Edge::Right || stored.is_some_and(|s| s.h.is_some())
    }

    /// The (clamped) size this drag shows for an unclamped `size`.
    fn shown(
        &self,
        size: (f32, f32),
        stored: Option<ModalSize>,
        default_w: f32,
        window: Size<Pixels>,
    ) -> (f32, Option<f32>) {
        fit(size.0, self.fixes_height(stored).then_some(size.1), default_w, window)
    }
}

/// Per-dialog state that outlives a frame: the drag, and the panel's
/// painted bounds (a never-heightened panel's height is only known after
/// layout, and a drag starts from what is on screen).
struct FrameState {
    drag: Rc<RefCell<Option<Drag>>>,
    panel: Rc<Cell<Option<Bounds<Pixels>>>>,
}

/// The one modal dialog: dimmed window-wide backdrop, centred panel with a
/// scrolling body, and resize handles on the right edge, bottom edge and
/// corner. `id` is the stable key the size is remembered under (dialogs
/// sharing a renderer share it); `default_w` is the never-resized width,
/// which is also the minimum.
pub(crate) fn frame(id: &'static str, default_w: f32, body: impl IntoElement) -> ModalFrame {
    ModalFrame {
        id,
        default_w,
        body: body.into_any_element(),
        max_h: None,
        padding: DEFAULT_PADDING,
        body_fills: false,
        decorate: None,
        close_on_click: None,
    }
}

/// The panel's inset when a dialog does not ask for another: `p_4`, what
/// `ui::panel` gave every form dialog.
const DEFAULT_PADDING: f32 = 16.0;

type ClickHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub(crate) struct ModalFrame {
    id: &'static str,
    default_w: f32,
    body: AnyElement,
    max_h: Option<f32>,
    padding: f32,
    body_fills: bool,
    decorate: Option<Box<dyn FnOnce(Div) -> Div>>,
    close_on_click: Option<ClickHandler>,
}

impl ModalFrame {
    /// The panel's inset on all four sides (default 16). A dialog whose
    /// children carry their own padding — rows highlighted edge to edge,
    /// a separator under an input — passes 0.
    pub(crate) fn padding(mut self, p: f32) -> Self {
        self.padding = p;
        self
    }

    /// The body fills the panel's height instead of scrolling as a whole:
    /// it gets a definite height to lay out in, so a `flex_1` +
    /// `overflow_hidden` child takes what is left and the rest (an input,
    /// a footer, the buttons) stays pinned. For dialogs that scroll or clip
    /// their own content; a height drag grows that `flex_1` part.
    pub(crate) fn body_fills(mut self) -> Self {
        self.body_fills = true;
        self
    }

    /// Cap on the height of a dialog that was never heightened (the window
    /// caps it anyway).
    pub(crate) fn max_h(mut self, cap: f32) -> Self {
        self.max_h = Some(cap);
        self
    }

    /// Adds the caller's own focus, key context and actions to the
    /// backdrop — they are typed to the caller's entity, so they stay its.
    pub(crate) fn decorate(mut self, f: impl FnOnce(Div) -> Div + 'static) -> Self {
        self.decorate = Some(Box::new(f));
        self
    }

    /// Calls `f` on any left mouse-down in the dialog or on the backdrop,
    /// except on a resize handle (about / shortcuts close this way).
    pub(crate) fn close_on_click(
        mut self,
        f: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.close_on_click = Some(Box::new(f));
        self
    }
}

impl RenderOnce for ModalFrame {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { id, default_w, body, max_h, padding, body_fills, decorate, close_on_click } =
            self;
        let theme = *cx.theme();
        let win = window.viewport_size();
        let state = window.use_keyed_state(format!("modal-drag-{id}"), cx, |_, _| FrameState {
            drag: Rc::new(RefCell::new(None)),
            panel: Rc::new(Cell::new(None)),
        });
        let (drag, panel_bounds) = {
            let s = state.read(cx);
            (s.drag.clone(), s.panel.clone())
        };
        let stored = cx.try_global::<ModalSizes>().and_then(|s| s.0.get(id).copied());
        let current = *drag.borrow();
        let (w, h) = match current {
            Some(d) => d.shown(d.live, stored, default_w, win),
            None => effective_size(stored, default_w, win),
        };

        let mut panel = ui::surface(theme)
            .relative()
            .flex()
            .flex_col()
            .p(px(padding))
            .text_color(theme.text_primary)
            .w(px(w));
        panel = match h {
            Some(h) => panel.h(px(h)),
            None => panel.max_h(px(max_h.unwrap_or(f32::INFINITY).min(max_height(win)))),
        };

        let handle = |edge: Edge, el: Div, cursor: CursorStyle| {
            let drag = drag.clone();
            let panel_bounds = panel_bounds.clone();
            el.absolute().cursor(cursor).on_mouse_down(
                MouseButton::Left,
                move |ev: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    let Some(b) = panel_bounds.get() else { return };
                    let start = (f32::from(b.size.width), f32::from(b.size.height));
                    *drag.borrow_mut() =
                        Some(Drag { edge, start, start_pointer: ev.position, live: start });
                },
            )
        };
        let right = handle(
            Edge::Right,
            div().top_0().right(px(-HANDLE)).w(px(HANDLE)).h_full(),
            CursorStyle::ResizeLeftRight,
        );
        let bottom = handle(
            Edge::Bottom,
            div().left_0().bottom(px(-HANDLE)).h(px(HANDLE)).w_full(),
            CursorStyle::ResizeUpDown,
        );
        let corner = handle(
            Edge::Corner,
            div().right(px(-HANDLE)).bottom(px(-HANDLE)).size(px(2.0 * HANDLE)),
            CursorStyle::ResizeUpLeftDownRight,
        );

        // The drag itself: window-wide listeners re-registered every paint,
        // as in `scrollbar.rs` — an element's move listener is hover-gated
        // and the pointer leaves the handle on every real drag. A move
        // without the button held is a release the window never saw.
        let listeners = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let move_drag = drag.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let Some(mut d) = *move_drag.borrow() else { return };
                    if ev.pressed_button != Some(MouseButton::Left) {
                        finish(&move_drag, id, default_w, window.viewport_size(), cx);
                        window.refresh();
                        return;
                    }
                    d.live = dragged_size(d.start, d.start_pointer, ev.position, d.edge);
                    *move_drag.borrow_mut() = Some(d);
                    window.refresh();
                });
                let up_drag = drag.clone();
                window.on_mouse_event(move |ev: &MouseUpEvent, _phase, window, cx| {
                    if ev.button == MouseButton::Left && up_drag.borrow().is_some() {
                        finish(&up_drag, id, default_w, window.viewport_size(), cx);
                        window.refresh();
                    }
                });
            },
        )
        .absolute()
        .size_0();

        // A filling body must itself be `flex_1().min_h_0()` (a flex column)
        // for the height to reach its own `flex_1` part.
        let body = if body_fills {
            div().flex_1().min_h_0().flex().flex_col().child(body).into_any_element()
        } else {
            scrollbar::scroll_y(format!("modal-body-{id}"), body).into_any_element()
        };
        let panel = panel
            .child(body)
            .child(right)
            .child(bottom)
            .child(corner)
            .child(listeners);

        let mut backdrop = div()
            .w(win.width)
            .h(win.height)
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.bg_backdrop)
            .occlude()
            .on_children_prepainted(move |bounds, _, _| panel_bounds.set(bounds.first().copied()))
            .child(panel);
        if let Some(f) = close_on_click {
            backdrop = backdrop.on_mouse_down(MouseButton::Left, f);
        }
        if let Some(f) = decorate {
            backdrop = f(backdrop);
        }
        deferred(anchored().position(point(px(0.), px(0.))).child(backdrop))
            .with_priority(MODAL_PRIORITY)
    }
}

/// Ends the drag in `slot` and stores the size it showed — only when that
/// differs from the size at the grab and from what is stored, so a bare
/// click on a handle writes nothing.
fn finish(
    slot: &RefCell<Option<Drag>>,
    id: &'static str,
    default_w: f32,
    window: Size<Pixels>,
    cx: &mut App,
) {
    let Some(d) = slot.borrow_mut().take() else { return };
    let stored = cx.try_global::<ModalSizes>().and_then(|s| s.0.get(id).copied());
    let round = |(w, h): (f32, Option<f32>)| ModalSize {
        w: w.round() as u16,
        h: h.map(|h| h.round() as u16),
    };
    let before = round(d.shown(d.start, stored, default_w, window));
    let after = round(d.shown(d.live, stored, default_w, window));
    if after == before || stored == Some(after) {
        return;
    }
    cx.update_default_global::<ModalSizes, _>(|s, _| {
        s.0.insert(id.to_string(), after);
    });
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

/// The drag lives in GPUI mouse listeners, which the geometry tests above
/// cannot reach. These drive a real `frame` in a GPUI test window.
#[cfg(test)]
mod frame_tests {
    use super::*;
    use crate::theme::Theme;
    use gpui::{
        canvas, div, point, px, size, Bounds, Context, Modifiers, MouseButton, Render,
        TestAppContext, VisualTestContext, Window,
    };
    use std::{cell::Cell, rc::Rc};

    const WIN_W: f32 = 1000.0;
    const WIN_H: f32 = 800.0;
    /// x inside the right handle (panel x 300..700, handle 700..706).
    const RIGHT_X: f32 = 703.0;
    /// y inside the bottom handle. The panel is 400 x 234 (body 200 +
    /// 2 x 16 padding + 2 x 1 border), so y 283..517 and the handle 517..523.
    const BOTTOM_Y: f32 = 519.0;

    type Probe = Rc<Cell<Option<Bounds<Pixels>>>>;

    /// What the test dialog is built with. Counters count calls.
    #[derive(Clone, Default)]
    struct Setup {
        /// Body taller than the window, so the body scrolls.
        tall: bool,
        max_h: Option<f32>,
        close: Option<Rc<Cell<usize>>>,
        /// Counts right mouse-downs seen by a `.decorate` listener.
        decorated: Option<Rc<Cell<usize>>>,
        padding: Option<f32>,
        /// `.body_fills()`, with a body made of a `flex_1` +
        /// `overflow_hidden` part holding 2000 px of content (probed as
        /// `body`) and a 30 px footer probed here.
        fills_with_footer: Option<Probe>,
    }

    struct View {
        setup: Setup,
        /// The body's painted bounds, for the scrollbar test.
        body: Probe,
    }

    impl Render for View {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            fn probed(p: Probe) -> impl IntoElement {
                canvas(move |b, _, _| p.set(Some(b)), |_, _, _, _| {}).absolute().size_full()
            }
            let probe = self.body.clone();
            let mut f = match self.setup.fills_with_footer.clone() {
                Some(footer) => {
                    let content = div().w_full().h(px(2000.)).flex_shrink_0().child(probed(probe));
                    let body = div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(div().flex_1().overflow_hidden().child(content))
                        .child(div().relative().h(px(30.)).flex_shrink_0().child(probed(footer)));
                    frame("t", 400., body).body_fills()
                }
                None => frame(
                    "t",
                    400.,
                    crate::ui::panel_body()
                        .w_full()
                        .h(px(if self.setup.tall { 2000. } else { 200. }))
                        .child(probed(probe)),
                ),
            };
            if let Some(cap) = self.setup.max_h {
                f = f.max_h(cap);
            }
            if let Some(p) = self.setup.padding {
                f = f.padding(p);
            }
            if let Some(n) = self.setup.close.clone() {
                f = f.close_on_click(move |_, _, _| n.set(n.get() + 1));
            }
            if let Some(n) = self.setup.decorated.clone() {
                f = f.decorate(move |d| {
                    d.on_mouse_down(MouseButton::Right, move |_, _, _| n.set(n.get() + 1))
                });
            }
            div().size_full().child(f)
        }
    }

    struct Opened {
        cx: VisualTestContext,
        writes: Rc<Cell<usize>>,
        body: Probe,
    }

    fn open(cx: &mut TestAppContext, setup: Setup) -> Opened {
        cx.update(|cx| {
            cx.set_global(Theme::dark());
            cx.set_global(ModalSizes::default());
        });
        let writes = Rc::new(Cell::new(0));
        let w = writes.clone();
        cx.update(|cx| cx.observe_global::<ModalSizes>(move |_| w.set(w.get() + 1)).detach());
        let body: Probe = Rc::new(Cell::new(None));
        let probe = body.clone();
        let window =
            cx.open_window(size(px(WIN_W), px(WIN_H)), move |_, _| View { setup, body: probe });
        let vcx = VisualTestContext::from_window(*window, cx);
        vcx.run_until_parked();
        Opened { cx: vcx, writes, body }
    }

    fn stored(cx: &mut VisualTestContext) -> Option<ModalSize> {
        cx.update(|_, cx| cx.global::<ModalSizes>().0.get("t").copied())
    }

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    const L: MouseButton = MouseButton::Left;

    #[gpui::test]
    fn dragging_the_right_edge_stores_a_width(cx: &mut TestAppContext) {
        let Opened { mut cx, writes, .. } = open(cx, Setup::default());
        cx.simulate_mouse_down(at(RIGHT_X, 400.), L, Modifiers::none());
        cx.simulate_mouse_move(at(RIGHT_X + 50., 400.), L, Modifiers::none());
        cx.simulate_mouse_up(at(RIGHT_X + 50., 400.), L, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(stored(&mut cx), Some(ModalSize { w: 500, h: None }));
        assert_eq!(writes.get(), 1);
    }

    #[gpui::test]
    fn dragging_the_corner_stores_both(cx: &mut TestAppContext) {
        let Opened { mut cx, .. } = open(cx, Setup::default());
        cx.simulate_mouse_down(at(RIGHT_X, BOTTOM_Y), L, Modifiers::none());
        cx.simulate_mouse_move(at(RIGHT_X + 30., BOTTOM_Y + 20.), L, Modifiers::none());
        cx.simulate_mouse_up(at(RIGHT_X + 30., BOTTOM_Y + 20.), L, Modifiers::none());
        assert_eq!(stored(&mut cx), Some(ModalSize { w: 460, h: Some(274) }));
    }

    #[gpui::test]
    fn a_bare_click_on_a_handle_writes_nothing(cx: &mut TestAppContext) {
        let Opened { mut cx, writes, .. } = open(cx, Setup::default());
        for p in [at(RIGHT_X, 400.), at(500., BOTTOM_Y), at(RIGHT_X, BOTTOM_Y)] {
            cx.simulate_mouse_down(p, L, Modifiers::none());
            cx.simulate_mouse_up(p, L, Modifiers::none());
        }
        cx.run_until_parked();
        assert_eq!(stored(&mut cx), None);
        assert_eq!(writes.get(), 0, "no global update at all");
    }

    #[gpui::test]
    fn mouse_up_outside_the_window_ends_the_drag(cx: &mut TestAppContext) {
        let Opened { mut cx, writes, .. } = open(cx, Setup::default());
        cx.simulate_mouse_down(at(RIGHT_X, 400.), L, Modifiers::none());
        cx.simulate_mouse_move(at(RIGHT_X + 50., 400.), L, Modifiers::none());
        cx.simulate_mouse_up(at(1500., 400.), L, Modifiers::none());
        cx.run_until_parked();
        let after_up = stored(&mut cx);
        assert_eq!(after_up, Some(ModalSize { w: 500, h: None }));
        cx.simulate_mouse_move(at(RIGHT_X + 120., 400.), L, Modifiers::none());
        cx.simulate_mouse_up(at(RIGHT_X + 120., 400.), L, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(stored(&mut cx), after_up, "the drag is over");
        assert_eq!(writes.get(), 1, "committed once");
    }

    #[gpui::test]
    fn a_drag_from_the_body_never_resizes(cx: &mut TestAppContext) {
        let Opened { mut cx, writes, .. } = open(cx, Setup::default());
        cx.simulate_mouse_down(at(500., 400.), L, Modifiers::none());
        cx.simulate_mouse_move(at(RIGHT_X, 400.), L, Modifiers::none());
        cx.simulate_mouse_move(at(RIGHT_X + 50., 400.), L, Modifiers::none());
        cx.simulate_mouse_up(at(RIGHT_X + 50., 400.), L, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(stored(&mut cx), None);
        assert_eq!(writes.get(), 0);
    }

    #[gpui::test]
    fn right_handle_does_not_cover_the_scrollbar(cx: &mut TestAppContext) {
        let Opened { mut cx, body, .. } = open(cx, Setup { tall: true, ..Setup::default() });
        let b0 = body.get().expect("body painted");
        // The scrollbar overlays the right edge of the scroll viewport,
        // which is the body's right edge.
        let bar_x = f32::from(b0.origin.x + b0.size.width) - crate::scrollbar::BAR_WIDTH / 2.0;
        let y = f32::from(b0.origin.y) + 10.0;
        cx.simulate_mouse_down(at(bar_x, y), L, Modifiers::none());
        cx.simulate_mouse_move(at(bar_x, y + 100.), L, Modifiers::none());
        cx.simulate_mouse_up(at(bar_x, y + 100.), L, Modifiers::none());
        cx.run_until_parked();
        let b1 = body.get().unwrap();
        assert!(b1.origin.y < b0.origin.y, "the scrollbar drag scrolled: {b0:?} -> {b1:?}");
        assert_eq!(stored(&mut cx), None, "and did not resize");
    }

    #[gpui::test]
    fn close_on_click_ignores_handles(cx: &mut TestAppContext) {
        let n = Rc::new(Cell::new(0));
        let Opened { mut cx, .. } = open(cx, Setup { close: Some(n.clone()), ..Setup::default() });
        cx.simulate_click(at(500., 100.), Modifiers::none());
        assert_eq!(n.get(), 1, "a backdrop click closes");
        cx.simulate_mouse_down(at(RIGHT_X, 400.), L, Modifiers::none());
        cx.simulate_mouse_move(at(RIGHT_X + 50., 400.), L, Modifiers::none());
        cx.simulate_mouse_up(at(RIGHT_X + 50., 400.), L, Modifiers::none());
        assert_eq!(n.get(), 1, "a handle drag does not");
        assert_eq!(stored(&mut cx), Some(ModalSize { w: 500, h: None }));
    }

    /// The stored size is the clamped one the user saw, not where the
    /// pointer went.
    #[gpui::test]
    fn a_drag_past_the_window_stores_the_clamped_size(cx: &mut TestAppContext) {
        let Opened { mut cx, .. } = open(cx, Setup::default());
        cx.simulate_mouse_down(at(RIGHT_X, BOTTOM_Y), L, Modifiers::none());
        cx.simulate_mouse_move(at(RIGHT_X + 900., BOTTOM_Y + 900.), L, Modifiers::none());
        cx.simulate_mouse_up(at(RIGHT_X + 900., BOTTOM_Y + 900.), L, Modifiers::none());
        let (w, h) = (WIN_W - 2.0 * MODAL_MARGIN, WIN_H - 2.0 * MODAL_MARGIN);
        assert_eq!(stored(&mut cx), Some(ModalSize { w: w as u16, h: Some(h as u16) }));
    }

    /// `.max_h` caps a never-heightened dialog, and a bottom drag starts
    /// from that capped height (300 tall, so y 250..550, handle 550..556).
    #[gpui::test]
    fn max_h_caps_the_height_a_drag_starts_from(cx: &mut TestAppContext) {
        let setup = Setup { tall: true, max_h: Some(300.), ..Setup::default() };
        let Opened { mut cx, .. } = open(cx, setup);
        cx.simulate_mouse_down(at(500., 553.), L, Modifiers::none());
        cx.simulate_mouse_move(at(500., 563.), L, Modifiers::none());
        cx.simulate_mouse_up(at(500., 563.), L, Modifiers::none());
        assert_eq!(stored(&mut cx), Some(ModalSize { w: 400, h: Some(320) }));
    }

    #[gpui::test]
    fn decorate_reaches_the_backdrop(cx: &mut TestAppContext) {
        let n = Rc::new(Cell::new(0));
        let Opened { mut cx, .. } =
            open(cx, Setup { decorated: Some(n.clone()), ..Setup::default() });
        cx.simulate_mouse_down(at(500., 100.), MouseButton::Right, Modifiers::none());
        assert_eq!(n.get(), 1);
    }

    /// Panel x is 300 (400 wide in a 1000 window) plus a 1 px border: the
    /// body starts at 317 by default (panel 400 x 234, y 283) and at the
    /// border edge with `.padding(0.)`.
    #[gpui::test]
    fn padding_default_is_16(cx: &mut TestAppContext) {
        let Opened { body, .. } = open(cx, Setup::default());
        let b = body.get().expect("body painted");
        assert_eq!((f32::from(b.origin.x), f32::from(b.origin.y)), (317., 300.));
    }

    #[gpui::test]
    fn padding_option_changes_the_panel_inset(cx: &mut TestAppContext) {
        let Opened { body, .. } = open(cx, Setup { padding: Some(0.), ..Setup::default() });
        let b = body.get().expect("body painted");
        // 200 tall body + 2 border: panel y 299..501, body from 300.
        assert_eq!((f32::from(b.origin.x), f32::from(b.origin.y)), (301., 300.));
        assert_eq!(f32::from(b.size.width), 398., "body runs border to border");
    }

    /// A filling body lays out inside the capped panel (300 tall: y
    /// 250..550) with its footer pinned at the bottom, and nothing scrolls
    /// it as a whole.
    #[gpui::test]
    fn body_fills_does_not_scroll_and_pins_a_footer(cx: &mut TestAppContext) {
        let footer: Probe = Rc::new(Cell::new(None));
        let setup =
            Setup { max_h: Some(300.), fills_with_footer: Some(footer.clone()), ..Setup::default() };
        let Opened { mut cx, body, .. } = open(cx, setup);
        let f0 = footer.get().expect("footer painted");
        let c0 = body.get().expect("content painted");
        let bottom = f32::from(f0.origin.y + f0.size.height);
        // Panel 250..550, minus 1 border and 16 padding.
        assert_eq!(bottom, 533., "footer pinned to the panel's inner bottom: {f0:?}");
        assert_eq!(f32::from(c0.origin.y), 267., "content starts at the inner top");

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: at(500., 400.),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-200.))),
            ..Default::default()
        });
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: f0.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-200.))),
            ..Default::default()
        });
        cx.run_until_parked();
        assert_eq!(footer.get(), Some(f0), "the footer did not scroll");
        assert_eq!(body.get(), Some(c0), "the content did not scroll either");
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;

    fn map(entries: &[(&str, u16, Option<u16>)]) -> BTreeMap<String, ModalSize> {
        entries.iter().map(|(k, w, h)| (k.to_string(), ModalSize { w: *w, h: *h })).collect()
    }

    #[test]
    fn sizes_to_persist_none_when_equal() {
        let saved = map(&[("a", 500, Some(300))]);
        assert_eq!(sizes_to_persist(&ModalSizes(saved.clone()), &saved), None);
    }

    #[test]
    fn sizes_to_persist_some_when_changed() {
        let saved = map(&[("a", 500, Some(300))]);
        let store = ModalSizes(map(&[("a", 600, Some(300))]));
        assert_eq!(sizes_to_persist(&store, &saved), Some(store.0.clone()));
    }

    #[test]
    fn sizes_to_persist_keeps_unknown_keys() {
        // The store is seeded from config, so a key no dialog uses is still in
        // it and survives a round in which another dialog changed.
        let saved = map(&[("old-dialog", 410, None), ("a", 500, None)]);
        let mut store = ModalSizes(saved.clone());
        store.0.insert("a".into(), ModalSize { w: 520, h: Some(250) });
        let out = sizes_to_persist(&store, &saved).expect("changed");
        assert_eq!(out.get("old-dialog"), Some(&ModalSize { w: 410, h: None }));
        assert_eq!(out.get("a"), Some(&ModalSize { w: 520, h: Some(250) }));
    }
}

/// Source audit: every dialog goes through [`frame`]. A hand-built
/// backdrop is how the overlays drifted apart in the first place (each with
/// its own width, padding, scroll and click blocking), and none of them
/// could be resized.
#[cfg(test)]
mod audit_tests {
    /// Every `src/**/*.rs` of this crate, read at test time rather than
    /// listed by hand, so a file added later is audited too.
    fn crate_sources() -> Vec<(String, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                for ent in std::fs::read_dir(&path).expect("readable src dir") {
                    stack.push(ent.expect("readable entry").path());
                }
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel =
                path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(&path).expect("readable source");
            out.push((rel, text));
        }
        out
    }

    #[test]
    fn no_modal_builds_its_own_backdrop() {
        // Built at runtime — written as literals, these would match this
        // test's own source.
        let backdrop = format!("{}_{}", "bg", "backdrop");
        let panel = format!("ui::{}(", "panel");
        let sources = crate_sources();
        for must in ["main.rs", "modal.rs", "theme.rs", "grid.rs"] {
            assert!(sources.iter().any(|(n, _)| n == must), "audit did not see {must}");
        }
        let mut offenders = Vec::new();
        for (name, src) in &sources {
            for (i, line) in src.lines().enumerate() {
                let own_backdrop =
                    line.contains(&backdrop) && name != "modal.rs" && name != "theme.rs";
                if own_backdrop || line.contains(&panel) {
                    offenders.push(format!("{name}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a dialog builds its own backdrop / panel instead of going through \
             `modal::frame`:\n{}",
            offenders.join("\n")
        );
    }
}
