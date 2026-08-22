// G1 Task 4: multiline SQL editor element.
//
// Originally ported from Zed's `crates/gpui/examples/input.rs` at pinned rev
// 907ed09c9f4476caf250e6ce4bbffb23b4622f3b (single-line `TextInput`). This
// rework keeps the Element/ElementInputHandler/TextRun plumbing from that
// example but delegates ALL text state and mutation to `MultilineBuffer`
// (text_model.rs) instead of the example's own `content`/`selected_range`
// fields, and renders one `ShapedLine` per visible buffer line with a
// line-based vertical scroll window instead of a single shaped line.
//
// Notable adaptations from the single-line example / prior v1 port:
// - `content: SharedString` + `selected_range` + `selection_reversed` are
//   gone; `MultilineBuffer` owns cursor/selection and this file only ever
//   reads `buffer.text()` / `buffer.selection()` / `buffer.cursor()`.
// - `text()` returns the real multiline text (the v1 `\n` -> ' ' hack is
//   deleted); `set_text()` is new, for future history-load.
// - `MultilineBuffer` exposes `set_cursor`/`select_range` (added in G1 Task 4
//   fix round 1, per review) for placing the cursor/selection at an
//   arbitrary absolute byte offset in a single O(n) pass over the buffer.
//   Mouse clicks/drags and IME range replacement use these via
//   `SqlInput::seek` (clicks/drags) or directly (IME), instead of the
//   original stepwise replay of `move_up`/`move_down`/`move_right`, which
//   was O(line_count) buffer-rescanning calls per seek — quadratic overall
//   during drag-select across a large pasted buffer.
// - `previous_boundary`/`next_boundary` (grapheme-boundary scanning for
//   Left/Right) are deleted — the model does this internally now.
// - New actions `Up`/`Down`/`SelectUp`/`SelectDown` (vertical movement) and
//   `Newline` (Enter inserts `\n`, scoped to this element's key context so
//   the app-level `ctrl-enter` -> RunQuery binding in main.rs is unaffected
//   — different keystroke, same precedence as today).
// - Rendering: `TextElement::prepaint` shapes one `ShapedLine` per visible
//   line (scroll offset tracked in lines), builds a per-line selection
//   background quad (first/middle/last-line spans, extending to indicate
//   the selection continues onto the next line) and a single cursor quad on
//   whichever visible line contains the cursor. `paint` stashes the
//   per-line hit-test data (`CachedLine`: absolute line index + byte start
//   + `ShapedLine`) back onto the entity for the next click/IME query.
// - Mouse wheel scrolling added via `on_scroll_wheel`. The cursor-follow
//   clamp that pulls the visible window back to the cursor's line is gated
//   by `SqlInput::follow_cursor` (set by every edit/keyboard move/click/IME
//   op, consumed and cleared by the next `prepaint`) so it only fires on
//   frames where the cursor actually moved — otherwise it would immediately
//   undo a wheel scroll on the very next `prepaint` (G1 Task 4 review,
//   issue 1). `on_scroll_wheel` only clamps `scroll_offset_lines` to the
//   valid `[0, max_scroll]` range and never sets the flag.
// - `if focus_handle.is_focused(window) && let Some(...)` let-chains from
//   the example are still written as nested `if`s — this workspace pins
//   edition 2021, which rejects let-chains.

use std::ops::Range;

use gpui::{
    actions, div, fill, hsla, point, prelude::*, px, relative, rgba, size, App, Bounds,
    ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ScrollDelta, ScrollWheelEvent, ShapedLine, SharedString, Style, TextRun, UTF16Selection,
    UnderlineStyle, Window,
};

use crate::text_model::MultilineBuffer;

actions!(
    sql_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        Newline,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
    ]
);

/// Bind SqlInput's editing keys. Callers should invoke this once alongside
/// their app-level `cx.bind_keys` (e.g. for `RunQuery` / `CancelQuery`).
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("up", Up, None),
        KeyBinding::new("down", Down, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("shift-up", SelectUp, None),
        KeyBinding::new("shift-down", SelectDown, None),
        KeyBinding::new("cmd-a", SelectAll, None),
        KeyBinding::new("ctrl-a", SelectAll, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("ctrl-v", Paste, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("ctrl-c", Copy, None),
        KeyBinding::new("cmd-x", Cut, None),
        KeyBinding::new("ctrl-x", Cut, None),
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
        KeyBinding::new("enter", Newline, None),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
    ]);
}

/// Computes (line index, byte column within line) for an arbitrary byte
/// `offset` into `text`. Mirrors `MultilineBuffer::cursor_position`'s
/// algorithm exactly, generalized to any offset rather than just the
/// buffer's own cursor — duplicated here (rather than added to
/// text_model.rs) because the model's public surface is frozen for this
/// task; see the file header.
fn line_col_of(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for (i, ch) in text.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf8();
        }
    }
    (line, col)
}

/// For a single rendered line spanning absolute byte range
/// `[line_start, line_end)` (exclusive of its trailing newline), returns the
/// `(x_start, x_end)` pixel span of `sel`'s overlap with this line, or
/// `None` if `sel` doesn't touch this line. A selection that continues past
/// `line_end` (onto the next line) is drawn out to the shaped line's full
/// width plus a small extension, so multi-line selections read as
/// continuous first/middle/last-line spans.
fn per_line_selection_bounds(
    sel: &Range<usize>,
    line_start: usize,
    line_end: usize,
    shaped: &ShapedLine,
) -> Option<(Pixels, Pixels)> {
    if sel.start == sel.end {
        return None;
    }
    if sel.end <= line_start || sel.start > line_end {
        return None;
    }
    let local_start = sel.start.saturating_sub(line_start).min(shaped.len());
    let is_last_line_of_sel = sel.end <= line_end;
    let x0 = shaped.x_for_index(local_start);
    let x1 = if is_last_line_of_sel {
        let local_end = (sel.end - line_start).min(shaped.len());
        shaped.x_for_index(local_end)
    } else {
        shaped.width() + px(6.)
    };
    Some((x0, x1))
}

/// Builds the (up to 3) `TextRun`s for one line's shaped text: a plain run,
/// or plain/underlined-marked/plain when `marked_local` (the marked range
/// clipped to this line, in line-local byte coordinates) is present.
fn build_runs(run: &TextRun, display_len: usize, marked_local: Option<Range<usize>>) -> Vec<TextRun> {
    if let Some(mr) = marked_local {
        let start = mr.start.min(display_len);
        let end = mr.end.min(display_len);
        vec![
            TextRun {
                len: start,
                ..run.clone()
            },
            TextRun {
                len: end - start,
                underline: Some(UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: display_len - end,
                ..run.clone()
            },
        ]
        .into_iter()
        .filter(|r| r.len > 0)
        .collect()
    } else {
        vec![TextRun {
            len: display_len,
            ..run.clone()
        }]
    }
}

/// Per-line hit-test / IME-query cache, populated by `TextElement::paint`
/// for whichever lines were actually visible last frame.
struct CachedLine {
    index: usize,
    start: usize,
    shaped: ShapedLine,
}

pub struct SqlInput {
    focus_handle: FocusHandle,
    placeholder: SharedString,
    buffer: MultilineBuffer,
    marked_range: Option<Range<usize>>,
    scroll_offset_lines: usize,
    /// Set by every cursor-moving/editing action (insert, backspace,
    /// delete, move_*, click/drag, `set_text`, paste, IME); consumed and
    /// cleared by the next `TextElement::prepaint`, which only re-clamps
    /// the visible window to the cursor's line when this is set. Keeps a
    /// deliberate mouse-wheel scroll from being immediately undone by the
    /// cursor-follow clamp on the next unrelated re-render (G1 Task 4
    /// review, issue 1).
    follow_cursor: bool,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Option<Pixels>,
    last_visible_line_count: usize,
    last_lines: Vec<CachedLine>,
    is_selecting: bool,
}

impl SqlInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            placeholder: placeholder.into(),
            buffer: MultilineBuffer::new(),
            marked_range: None,
            scroll_offset_lines: 0,
            follow_cursor: true,
            last_bounds: None,
            last_line_height: None,
            last_visible_line_count: 1,
            last_lines: Vec::new(),
            is_selecting: false,
        }
    }

    /// Real multiline text — no newline-to-space substitution.
    pub fn text(&self) -> String {
        self.buffer.text().to_string()
    }

    /// Replaces the whole buffer — used by the history panel (G3 Task 3) to
    /// load a clicked entry's SQL into the editor without running it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer.set_text(text);
        self.marked_range = None;
        self.scroll_offset_lines = 0;
        self.follow_cursor = true;
        cx.notify();
    }

    fn current_selected_range(&self) -> Range<usize> {
        self.buffer
            .selection()
            .unwrap_or_else(|| self.buffer.cursor()..self.buffer.cursor())
    }

    /// Moves the model's cursor (and, if `extend`, its selection's active
    /// end) to an arbitrary absolute byte `target_offset`, via
    /// `MultilineBuffer::set_cursor`/`select_range` — O(n) single-pass
    /// operations against the buffer's stored text. See file header.
    ///
    /// When extending, the anchor (the selection's fixed end) is derived
    /// from the current cursor/selection rather than tracked separately:
    /// `buffer.cursor()` always equals whichever end of the (ordered)
    /// selection is the active one (every mutation that sets a selection
    /// also moves the cursor to its active end), so the *other* end — or,
    /// with no active selection, the cursor itself — is the anchor to
    /// extend from. This correctly preserves the anchor across a drag
    /// (repeated `seek(_, true)` calls) and across shift-click after either
    /// a mouse- or keyboard-established selection.
    fn seek(&mut self, target_offset: usize, extend: bool) {
        self.follow_cursor = true;
        if extend {
            let anchor = match self.buffer.selection() {
                Some(sel) if !sel.is_empty() => {
                    if self.buffer.cursor() == sel.start {
                        sel.end
                    } else {
                        sel.start
                    }
                }
                _ => self.buffer.cursor(),
            };
            self.buffer.select_range(anchor..target_offset);
        } else {
            self.buffer.set_cursor(target_offset);
        }
    }

    /// Maps a window-space point to an absolute byte offset, using the
    /// previous frame's cached visible-line layout (`last_lines`).
    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        if self.buffer.text().is_empty() {
            return 0;
        }
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        let Some(line_height) = self.last_line_height else {
            return 0;
        };
        if self.last_lines.is_empty() {
            return self.buffer.text().len();
        }
        let mut rel_y = position.y - bounds.top();
        if rel_y < px(0.) {
            rel_y = px(0.);
        }
        let row = (rel_y.as_f32() / line_height.as_f32()).floor() as usize;
        let row = row.min(self.last_lines.len() - 1);
        let entry = &self.last_lines[row];
        let mut rel_x = position.x - bounds.left();
        if rel_x < px(0.) {
            rel_x = px(0.);
        }
        let local = entry.shaped.closest_index_for_x(rel_x);
        entry.start + local
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_left(false);
        self.follow_cursor = true;
        cx.notify();
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_right(false);
        self.follow_cursor = true;
        cx.notify();
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_up(false);
        self.follow_cursor = true;
        cx.notify();
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_down(false);
        self.follow_cursor = true;
        cx.notify();
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_left(true);
        self.follow_cursor = true;
        cx.notify();
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_right(true);
        self.follow_cursor = true;
        cx.notify();
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_up(true);
        self.follow_cursor = true;
        cx.notify();
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_down(true);
        self.follow_cursor = true;
        cx.notify();
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_all();
        self.follow_cursor = true;
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_home(false);
        self.follow_cursor = true;
        cx.notify();
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_end(false);
        self.follow_cursor = true;
        cx.notify();
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.insert("\n");
        self.marked_range = None;
        self.follow_cursor = true;
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.backspace();
        self.marked_range = None;
        self.follow_cursor = true;
        cx.notify();
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.delete();
        self.marked_range = None;
        self.follow_cursor = true;
        cx.notify();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        let offset = self.offset_for_position(event.position);
        self.seek(offset, event.modifiers.shift);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.offset_for_position(event.position);
            self.seek(offset, true);
            cx.notify();
        }
    }

    fn on_scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self.last_line_height.unwrap_or(px(20.)).as_f32().max(1.0);
        let delta_lines = match event.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / line_height,
        };
        let visible = self.last_visible_line_count.max(1);
        let max_scroll = self.buffer.line_count().saturating_sub(visible);
        let current = self.scroll_offset_lines as f32;
        let new_scroll = (current - delta_lines).round();
        let clamped = new_scroll.max(0.0).min(max_scroll as f32) as usize;
        if clamped != self.scroll_offset_lines {
            self.scroll_offset_lines = clamped;
            cx.notify();
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.buffer.insert(&text);
            self.marked_range = None;
            self.follow_cursor = true;
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = self.buffer.selection() {
            if !sel.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    self.buffer.text()[sel].to_string(),
                ));
            }
        }
    }

    fn cut(&mut self, _: &Cut, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = self.buffer.selection() {
            if !sel.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    self.buffer.text()[sel].to_string(),
                ));
                self.buffer.delete();
                self.marked_range = None;
                self.follow_cursor = true;
                cx.notify();
            }
        }
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.buffer.text().chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.buffer.text().chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

impl EntityInputHandler for SqlInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.buffer.text()[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.current_selected_range()),
            // The model doesn't expose selection direction; degraded IME
            // positioning here is acceptable per the task brief.
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.current_selected_range());

        self.buffer.select_range(range);
        self.buffer.insert(new_text);
        self.marked_range = None;
        self.follow_cursor = true;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.current_selected_range());

        self.buffer.select_range(range.clone());
        self.buffer.insert(new_text);

        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }

        if let Some(new_range_utf16) = new_selected_range_utf16.as_ref() {
            let new_range = self.range_from_utf16(new_range_utf16);
            let sel_start = range.start + new_range.start;
            let sel_end = range.start + new_range.end;
            self.buffer.select_range(sel_start..sel_end);
        }

        self.follow_cursor = true;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let line_height = self.last_line_height?;
        let entry = self
            .last_lines
            .iter()
            .find(|e| range.start >= e.start && range.start <= e.start + e.shaped.len())?;
        let row = entry.index - self.scroll_offset_lines;
        let local_start = range.start - entry.start;
        let local_end = range.end.min(entry.start + entry.shaped.len()) - entry.start;
        let top = bounds.top() + line_height * row;
        Some(Bounds::from_corners(
            point(bounds.left() + entry.shaped.x_for_index(local_start), top),
            point(
                bounds.left() + entry.shaped.x_for_index(local_end),
                top + line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let utf8_index = self.offset_for_position(point);
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<SqlInput>,
}

struct LineRenderData {
    index: usize,
    start: usize,
    shaped: ShapedLine,
    selection_quad: Option<PaintQuad>,
    cursor_quad: Option<PaintQuad>,
}

struct PrepaintState {
    lines: Vec<LineRenderData>,
    line_height: Pixels,
    scroll_offset_lines: usize,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let text = input.buffer.text().to_string();
        let selection = input.buffer.selection();
        let cursor = input.buffer.cursor();
        let marked_range = input.marked_range.clone();
        let placeholder = input.placeholder.clone();
        let scroll_offset_lines = input.scroll_offset_lines;
        let follow_cursor = input.follow_cursor;

        let style = window.text_style();
        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let line_count = if text.is_empty() {
            1
        } else {
            text.matches('\n').count() + 1
        };
        let visible_line_count = ((bounds.size.height.as_f32() / line_height.as_f32()).floor()
            as usize)
            .max(1);

        // Keep the cursor's line inside the visible window, but only on
        // frames where the cursor actually moved (`follow_cursor`, set by
        // every edit/keyboard-move/click/IME op and cleared below once
        // consumed). Otherwise this clamp would immediately undo a
        // deliberate mouse-wheel scroll on the very next `prepaint` (the
        // `cx.notify()` `on_scroll_wheel` triggers to paint its own
        // `scroll_offset_lines` change) — see G1 Task 4 review, issue 1.
        let (cursor_line, _) = line_col_of(&text, cursor);
        let mut scroll = scroll_offset_lines;
        if follow_cursor {
            if cursor_line < scroll {
                scroll = cursor_line;
            } else if cursor_line >= scroll + visible_line_count {
                scroll = cursor_line + 1 - visible_line_count;
            }
        }
        // Always clamp to the valid range, regardless of `follow_cursor` —
        // this keeps `on_scroll_wheel`'s own clamped value valid too (e.g.
        // after the window is resized smaller between frames).
        let max_scroll = line_count.saturating_sub(visible_line_count);
        if scroll > max_scroll {
            scroll = max_scroll;
        }

        self.input.update(cx, |input, _cx| {
            input.scroll_offset_lines = scroll;
            input.last_line_height = Some(line_height);
            input.last_visible_line_count = visible_line_count;
            input.follow_cursor = false;
        });

        let end_line = (scroll + visible_line_count).min(line_count);
        let mut lines = Vec::with_capacity(end_line.saturating_sub(scroll));
        let is_empty = text.is_empty();

        let mut running_offset = 0usize;
        for (idx, line_text) in text.split('\n').enumerate() {
            if idx >= end_line {
                break;
            }
            if idx >= scroll {
                let line_start = running_offset;
                let line_end = running_offset + line_text.len();
                let is_placeholder = is_empty && idx == 0;

                let (display_text, color): (SharedString, _) = if is_placeholder {
                    (placeholder.clone(), hsla(0., 0., 0., 0.2))
                } else {
                    (line_text.to_string().into(), style.color)
                };
                let display_len = display_text.len();

                let run = TextRun {
                    len: 0,
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };

                let marked_local = if is_placeholder {
                    None
                } else {
                    marked_range.as_ref().and_then(|mr| {
                        if mr.end > line_start && mr.start < line_end {
                            let s = mr.start.saturating_sub(line_start).min(line_text.len());
                            let e = mr.end.saturating_sub(line_start).min(line_text.len());
                            if e > s {
                                Some(s..e)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                };
                let runs = build_runs(&run, display_len, marked_local);

                let shaped = window
                    .text_system()
                    .shape_line(display_text, font_size, &runs, None);

                let row = idx - scroll;
                let selection_quad = selection.as_ref().and_then(|sel| {
                    per_line_selection_bounds(sel, line_start, line_end, &shaped)
                        .map(|(x0, x1)| {
                            fill(
                                Bounds::new(
                                    point(bounds.left() + x0, bounds.top() + line_height * row),
                                    size(x1 - x0, line_height),
                                ),
                                rgba(0x3311ff30),
                            )
                        })
                });

                lines.push(LineRenderData {
                    index: idx,
                    start: line_start,
                    shaped,
                    selection_quad,
                    cursor_quad: None,
                });
            }
            running_offset += line_text.len() + 1;
        }

        // Cursor is only drawn when there's no active (non-empty) selection,
        // on whichever visible line it falls on.
        let selection_is_empty = selection.as_ref().map_or(true, |s| s.is_empty());
        if selection_is_empty && cursor_line >= scroll && cursor_line < end_line {
            if let Some(entry) = lines.iter_mut().find(|l| l.index == cursor_line) {
                let local = cursor.saturating_sub(entry.start).min(entry.shaped.len());
                let x = entry.shaped.x_for_index(local);
                let row = cursor_line - scroll;
                entry.cursor_quad = Some(fill(
                    Bounds::new(
                        point(bounds.left() + x, bounds.top() + line_height * row),
                        size(px(2.), line_height),
                    ),
                    gpui::blue(),
                ));
            }
        }

        PrepaintState {
            lines,
            line_height,
            scroll_offset_lines: scroll,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        let is_focused = focus_handle.is_focused(window);
        let line_height = prepaint.line_height;
        let scroll_offset_lines = prepaint.scroll_offset_lines;
        let mut cached_lines = Vec::with_capacity(prepaint.lines.len());

        for mut line in prepaint.lines.drain(..) {
            if let Some(selection_quad) = line.selection_quad.take() {
                window.paint_quad(selection_quad);
            }

            let row = line.index - scroll_offset_lines;
            line.shaped
                .paint(
                    point(bounds.left(), bounds.top() + line_height * row),
                    line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .unwrap();

            if is_focused {
                if let Some(cursor_quad) = line.cursor_quad.take() {
                    window.paint_quad(cursor_quad);
                }
            }

            cached_lines.push(CachedLine {
                index: line.index,
                start: line.start,
                shaped: line.shaped,
            });
        }

        self.input.update(cx, |input, _cx| {
            input.last_bounds = Some(bounds);
            input.last_lines = cached_lines;
        });
    }
}

impl Render for SqlInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .key_context("SqlInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .bg(gpui::white())
            .line_height(px(20.))
            .text_size(px(14.))
            .size_full()
            .overflow_hidden()
            .child(
                div()
                    .size_full()
                    .px(px(6.))
                    .py(px(4.))
                    .child(TextElement { input: cx.entity() }),
            )
    }
}

impl Focusable for SqlInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
