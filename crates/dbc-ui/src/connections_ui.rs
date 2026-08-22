// G1 Task 7: connection manager UI — dialog, folders, top-bar switcher,
// master-password modals.
//
// Layout of this file:
//   1. Pure folder/favourite grouping logic for the dropdown (GPUI-free,
//      unit-tested directly).
//   2. `TextField`: a tiny single-line text input, backed by the same
//      `MultilineBuffer` model `SqlInput` uses (text_model.rs), with an
//      optional `masked` flag that renders `•` per grapheme instead of the
//      real characters. Modeled closely on Zed's `examples/input.rs`
//      single-line `TextInput`, adapted to delegate storage/editing to
//      `MultilineBuffer` instead of owning `content`/`selected_range`
//      directly (matching the SqlInput port's approach in sql_input.rs).
//      Newline insertion is never wired to a keybinding here, and pasted /
//      IME text has "\n" stripped, so it structurally can't become
//      multi-line.
//   3. Modal state (`ModalState`) + the connection-dialog's field bundle
//      (`ConnectionDialogUi`) and its plain-data snapshot
//      (`ConnectionFormData`, used both to persist and to carry a pending
//      save across a master-password prompt).
//   4. `impl AppView` — event handlers (open/close dialog, save, test,
//      switch connection, vault unlock/create) and render helpers (top bar,
//      dropdown overlay, modal overlay). These live here rather than in
//      main.rs to keep connection-manager concerns in one file; Rust allows
//      inherent `impl` blocks for the same type to be split across modules
//      within a crate.
//   5. Free helper functions: panel renderers for each modal, small style
//      helpers, engine cycling, id generation, form-field parsing, and
//      `test_connect_spec` — builds the `ConnectSpec` the Test button /
//      dropdown connection-switch dispatch through `QueryRunner::test_connect`
//      (off the UI thread; see its doc comment and the Task 8 review fix
//      round for why the earlier synchronous `pending_connect` was replaced).

use std::collections::BTreeMap;
use std::ops::Range;

use dbc_state::{ConnectionConfig, Engine, SshTunnelConfig, Vault};
use gpui::{
    actions, div, fill, hsla, point, prelude::*, px, relative, rgb, rgba, size, App, AnyElement,
    Bounds, ClipboardItem, Context, CursorStyle, Div, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Stateful, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::runner::ConnectSpec;
use crate::text_model::MultilineBuffer;
use crate::AppView;

// ---------------------------------------------------------------------
// 1. Pure grouping logic (favourites first, then folders alphabetically,
//    nested via `Vec<String>` path — a parent folder's `Vec` is always
//    `Ord`-less-than its children's, per Rust's lexicographic `Vec<T>: Ord`,
//    so a `BTreeMap` keyed on the path already yields parent-before-child,
//    alphabetical-within-siblings ordering).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderGroup {
    pub path: Vec<String>,
    pub connections: Vec<ConnectionConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupedConnections {
    pub favourites: Vec<ConnectionConfig>,
    pub folders: Vec<FolderGroup>,
}

pub fn group_connections(conns: &[ConnectionConfig]) -> GroupedConnections {
    let mut favourites: Vec<ConnectionConfig> =
        conns.iter().filter(|c| c.favourite).cloned().collect();
    favourites.sort_by(|a, b| a.name.cmp(&b.name));

    let mut by_folder: BTreeMap<Vec<String>, Vec<ConnectionConfig>> = BTreeMap::new();
    for c in conns.iter().filter(|c| !c.favourite) {
        by_folder.entry(c.folder.clone()).or_default().push(c.clone());
    }
    let folders = by_folder
        .into_iter()
        .map(|(path, mut cs)| {
            cs.sort_by(|a, b| a.name.cmp(&b.name));
            FolderGroup { path, connections: cs }
        })
        .collect();
    GroupedConnections { favourites, folders }
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    fn conn(id: &str, name: &str, folder: &[&str], favourite: bool) -> ConnectionConfig {
        ConnectionConfig {
            id: id.into(),
            name: name.into(),
            folder: folder.iter().map(|s| s.to_string()).collect(),
            engine: Engine::Postgres,
            host: "localhost".into(),
            port: Some(5432),
            database: "db".into(),
            user: "u".into(),
            read_only: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            favourite,
        }
    }

    #[test]
    fn favourites_come_first_regardless_of_folder() {
        let conns = vec![
            conn("1", "zzz-fav", &["work"], true),
            conn("2", "aaa-normal", &[], false),
        ];
        let g = group_connections(&conns);
        assert_eq!(g.favourites.len(), 1);
        assert_eq!(g.favourites[0].id, "1");
        assert_eq!(g.folders.len(), 1);
        assert_eq!(g.folders[0].connections[0].id, "2");
    }

    #[test]
    fn favourites_sorted_alphabetically() {
        let conns = vec![
            conn("1", "zebra", &[], true),
            conn("2", "alpha", &[], true),
        ];
        let g = group_connections(&conns);
        assert_eq!(g.favourites.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["alpha", "zebra"]);
    }

    #[test]
    fn folders_are_alphabetical_and_nested_parent_before_child() {
        let conns = vec![
            conn("1", "c1", &["zzz"], false),
            conn("2", "c2", &["work", "prod"], false),
            conn("3", "c3", &["work"], false),
            conn("4", "c4", &[], false),
        ];
        let g = group_connections(&conns);
        let paths: Vec<Vec<String>> = g.folders.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                Vec::<String>::new(),
                vec!["work".to_string()],
                vec!["work".to_string(), "prod".to_string()],
                vec!["zzz".to_string()],
            ]
        );
    }

    #[test]
    fn connections_within_a_folder_sorted_alphabetically() {
        let conns = vec![
            conn("1", "beta", &["work"], false),
            conn("2", "alpha", &["work"], false),
        ];
        let g = group_connections(&conns);
        assert_eq!(g.folders.len(), 1);
        assert_eq!(
            g.folders[0].connections.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }
}

// ---------------------------------------------------------------------
// 2. TextField: single-line input, optionally masked.
// ---------------------------------------------------------------------

const BULLET: &str = "\u{2022}";
const BULLET_LEN: usize = 3; // '•' is 3 bytes in UTF-8; every grapheme maps to one.

fn masked_display(real: &str) -> String {
    BULLET.repeat(real.graphemes(true).count())
}

fn real_to_masked_offset(real: &str, real_offset: usize) -> usize {
    real.grapheme_indices(true)
        .take_while(|(i, _)| *i < real_offset)
        .count()
        * BULLET_LEN
}

fn masked_to_real_offset(real: &str, masked_offset: usize) -> usize {
    let n = masked_offset / BULLET_LEN;
    real.grapheme_indices(true).nth(n).map(|(i, _)| i).unwrap_or(real.len())
}

actions!(
    text_field,
    [Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, Paste, Cut, Copy]
);

/// Bind TextField's editing keys, scoped to key context "TextField" so they
/// never contend with SqlInput's (unscoped) or ResultGrid's bindings — same
/// reasoning as grid.rs's scoped `ctrl-c` binding.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextField")),
        KeyBinding::new("delete", Delete, Some("TextField")),
        KeyBinding::new("left", Left, Some("TextField")),
        KeyBinding::new("right", Right, Some("TextField")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextField")),
        KeyBinding::new("shift-right", SelectRight, Some("TextField")),
        KeyBinding::new("cmd-a", SelectAll, Some("TextField")),
        KeyBinding::new("ctrl-a", SelectAll, Some("TextField")),
        KeyBinding::new("cmd-v", Paste, Some("TextField")),
        KeyBinding::new("ctrl-v", Paste, Some("TextField")),
        KeyBinding::new("cmd-c", Copy, Some("TextField")),
        KeyBinding::new("ctrl-c", Copy, Some("TextField")),
        KeyBinding::new("cmd-x", Cut, Some("TextField")),
        KeyBinding::new("ctrl-x", Cut, Some("TextField")),
        KeyBinding::new("home", Home, Some("TextField")),
        KeyBinding::new("end", End, Some("TextField")),
    ]);
}

pub struct TextField {
    focus_handle: FocusHandle,
    placeholder: SharedString,
    buffer: MultilineBuffer,
    marked_range: Option<Range<usize>>,
    masked: bool,
    last_bounds: Option<Bounds<Pixels>>,
    last_shaped: Option<ShapedLine>,
    is_selecting: bool,
}

impl TextField {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>, masked: bool) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            placeholder: placeholder.into(),
            buffer: MultilineBuffer::new(),
            marked_range: None,
            masked,
            last_bounds: None,
            last_shaped: None,
            is_selecting: false,
        }
    }

    pub fn text(&self) -> String {
        self.buffer.text().to_string()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buffer.set_text(&text.replace('\n', ""));
        self.marked_range = None;
        cx.notify();
    }

    fn current_selected_range(&self) -> Range<usize> {
        self.buffer.selection().unwrap_or_else(|| self.buffer.cursor()..self.buffer.cursor())
    }

    fn seek(&mut self, target_offset: usize, extend: bool) {
        if extend {
            let anchor = match self.buffer.selection() {
                Some(sel) if !sel.is_empty() => {
                    if self.buffer.cursor() == sel.start { sel.end } else { sel.start }
                }
                _ => self.buffer.cursor(),
            };
            self.buffer.select_range(anchor..target_offset);
        } else {
            self.buffer.set_cursor(target_offset);
        }
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let real = self.buffer.text();
        if real.is_empty() {
            return 0;
        }
        let Some(bounds) = self.last_bounds else { return 0 };
        let Some(shaped) = self.last_shaped.as_ref() else { return 0 };
        let mut rel_x = position.x - bounds.left();
        if rel_x < px(0.) {
            rel_x = px(0.);
        }
        let display_offset = shaped.closest_index_for_x(rel_x);
        if self.masked {
            masked_to_real_offset(real, display_offset)
        } else {
            display_offset
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_left(false);
        cx.notify();
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_right(false);
        cx.notify();
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_left(true);
        cx.notify();
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_right(true);
        cx.notify();
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_all();
        cx.notify();
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_home(false);
        cx.notify();
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.move_end(false);
        cx.notify();
    }
    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.backspace();
        self.marked_range = None;
        cx.notify();
    }
    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.delete();
        self.marked_range = None;
        cx.notify();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
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

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.buffer.insert(&text.replace('\n', ""));
            self.marked_range = None;
            cx.notify();
        }
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = self.buffer.selection() {
            if !sel.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(self.buffer.text()[sel].to_string()));
            }
        }
    }
    fn cut(&mut self, _: &Cut, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = self.buffer.selection() {
            if !sel.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(self.buffer.text()[sel].to_string()));
                self.buffer.delete();
                self.marked_range = None;
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

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        let real = &self.buffer.text()[range];
        if self.masked {
            Some(masked_display(real))
        } else {
            Some(real.to_string())
        }
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.current_selected_range()),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
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
        let new_text = new_text.replace('\n', "");
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.current_selected_range());
        self.buffer.select_range(range);
        self.buffer.insert(&new_text);
        self.marked_range = None;
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
        let new_text = new_text.replace('\n', "");
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.current_selected_range());

        self.buffer.select_range(range.clone());
        self.buffer.insert(&new_text);

        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }

        if let Some(new_range_utf16) = new_selected_range_utf16.as_ref() {
            let new_range = self.range_from_utf16(new_range_utf16);
            self.buffer.select_range(range.start + new_range.start..range.start + new_range.end);
        }

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let shaped = self.last_shaped.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let real = self.buffer.text();
        let (s, e) = if self.masked {
            (real_to_masked_offset(real, range.start), real_to_masked_offset(real, range.end))
        } else {
            (range.start, range.end)
        };
        Some(Bounds::from_corners(
            point(bounds.left() + shaped.x_for_index(s), bounds.top()),
            point(bounds.left() + shaped.x_for_index(e), bounds.bottom()),
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

struct FieldElement {
    input: Entity<TextField>,
}

struct FieldPrepaint {
    shaped: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for FieldElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FieldElement {
    type RequestLayoutState = ();
    type PrepaintState = FieldPrepaint;

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
        style.size.height = window.line_height().into();
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
        let real = input.buffer.text().to_string();
        let selection = input.buffer.selection();
        let cursor = input.buffer.cursor();
        let marked_range = input.marked_range.clone();
        let placeholder = input.placeholder.clone();
        let masked = input.masked;
        let is_empty = real.is_empty();

        let style = window.text_style();
        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());

        let (display_text, color): (SharedString, _) = if is_empty {
            (placeholder.clone(), hsla(0., 0., 0., 0.2))
        } else if masked {
            (masked_display(&real).into(), style.color)
        } else {
            (real.clone().into(), style.color)
        };
        let display_len = display_text.len();

        let disp_cursor = if masked && !is_empty {
            real_to_masked_offset(&real, cursor)
        } else {
            cursor.min(display_len)
        };
        let disp_marked = if is_empty {
            None
        } else if masked {
            marked_range
                .as_ref()
                .map(|r| real_to_masked_offset(&real, r.start)..real_to_masked_offset(&real, r.end))
        } else {
            marked_range.clone()
        };
        let disp_selection = if is_empty {
            None
        } else if masked {
            selection
                .as_ref()
                .map(|s| real_to_masked_offset(&real, s.start)..real_to_masked_offset(&real, s.end))
        } else {
            selection.clone()
        };

        let run = TextRun {
            len: 0,
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs: Vec<TextRun> = if let Some(mr) = disp_marked.as_ref() {
            let s = mr.start.min(display_len);
            let e = mr.end.min(display_len);
            vec![
                TextRun { len: s, ..run.clone() },
                TextRun {
                    len: e - s,
                    underline: Some(UnderlineStyle { color: Some(run.color), thickness: px(1.0), wavy: false }),
                    ..run.clone()
                },
                TextRun { len: display_len - e, ..run.clone() },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect()
        } else {
            vec![TextRun { len: display_len, ..run.clone() }]
        };

        let shaped = window.text_system().shape_line(display_text, font_size, &runs, None);

        let cursor_x = shaped.x_for_index(disp_cursor.min(shaped.len()));
        let (selection_quad, cursor_quad) = match &disp_selection {
            Some(sel) if !sel.is_empty() => {
                let x0 = shaped.x_for_index(sel.start.min(shaped.len()));
                let x1 = shaped.x_for_index(sel.end.min(shaped.len()));
                (
                    Some(fill(
                        Bounds::from_corners(point(bounds.left() + x0, bounds.top()), point(bounds.left() + x1, bounds.bottom())),
                        rgba(0x3311ff30),
                    )),
                    None,
                )
            }
            _ => (
                None,
                Some(fill(
                    Bounds::new(point(bounds.left() + cursor_x, bounds.top()), size(px(2.), bounds.bottom() - bounds.top())),
                    gpui::blue(),
                )),
            ),
        };

        FieldPrepaint { shaped: Some(shaped), cursor: cursor_quad, selection: selection_quad }
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
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);

        if let Some(sel) = prepaint.selection.take() {
            window.paint_quad(sel);
        }
        let shaped = prepaint.shaped.take().unwrap();
        shaped.paint(bounds.origin, window.line_height(), gpui::TextAlign::Left, None, window, cx).unwrap();

        if focus_handle.is_focused(window) {
            if let Some(cq) = prepaint.cursor.take() {
                window.paint_quad(cq);
            }
        }

        self.input.update(cx, |input, _cx| {
            input.last_bounds = Some(bounds);
            input.last_shaped = Some(shaped);
        });
    }
}

impl Render for TextField {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .key_context("TextField")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .bg(gpui::white())
            .text_color(gpui::black())
            .line_height(px(18.))
            .text_size(px(13.))
            .w_full()
            .child(div().w_full().px(px(6.)).py(px(3.)).child(FieldElement { input: cx.entity() }))
    }
}

impl Focusable for TextField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---------------------------------------------------------------------
// 3. Modal state.
// ---------------------------------------------------------------------

#[derive(Clone)]
pub struct ConnectionDialogUi {
    pub editing_id: Option<String>,
    pub name: Entity<TextField>,
    pub host: Entity<TextField>,
    pub port: Entity<TextField>,
    pub database: Entity<TextField>,
    pub user: Entity<TextField>,
    pub password: Entity<TextField>,
    pub folder: Entity<TextField>,
    pub timeout_secs: Entity<TextField>,
    pub auto_limit: Entity<TextField>,
    pub ssh_host: Entity<TextField>,
    pub ssh_port: Entity<TextField>,
    pub ssh_user: Entity<TextField>,
    pub ssh_key_path: Entity<TextField>,
    pub engine: Engine,
    pub read_only: bool,
    pub favourite: bool,
    pub ssh_enabled: bool,
    pub test_result: Option<Result<String, String>>,
    /// `true` while a Test-button connect dispatched via
    /// `QueryRunner::test_connect` is in flight (Task 8 review issue #1) —
    /// drives the "testuji…" status line and makes `on_test_clicked` a
    /// no-op re-click guard until the result arrives.
    pub testing: bool,
}

impl ConnectionDialogUi {
    fn to_form_data(&self, cx: &mut Context<AppView>) -> ConnectionFormData {
        let id = self.editing_id.clone().unwrap_or_else(generate_connection_id);
        let ssh = if self.ssh_enabled {
            let key_path = self.ssh_key_path.read(cx).text();
            Some(SshTunnelConfig {
                host: self.ssh_host.read(cx).text(),
                port: parse_u16(&self.ssh_port.read(cx).text()).unwrap_or(22),
                user: self.ssh_user.read(cx).text(),
                key_path: if key_path.is_empty() { None } else { Some(key_path) },
            })
        } else {
            None
        };
        ConnectionFormData {
            id,
            name: self.name.read(cx).text(),
            engine: self.engine,
            host: self.host.read(cx).text(),
            port: parse_u16(&self.port.read(cx).text()),
            database: self.database.read(cx).text(),
            user: self.user.read(cx).text(),
            password: self.password.read(cx).text(),
            folder: parse_folder(&self.folder.read(cx).text()),
            read_only: self.read_only,
            favourite: self.favourite,
            timeout_secs: parse_u64(&self.timeout_secs.read(cx).text()),
            auto_limit: parse_u64(&self.auto_limit.read(cx).text()),
            ssh,
        }
    }
}

/// Plain-data snapshot of the dialog's fields — used both to persist a save
/// and to carry a pending save across a master-password prompt/creation
/// modal (which replaces `AppView::modal`, so the dialog's `Entity<TextField>`
/// handles themselves don't need to survive that detour).
#[derive(Clone)]
pub struct ConnectionFormData {
    pub id: String,
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    /// Empty means "keep existing secret" (edit) or "no secret" (new).
    pub password: String,
    pub folder: Vec<String>,
    pub read_only: bool,
    pub favourite: bool,
    pub timeout_secs: Option<u64>,
    pub auto_limit: Option<u64>,
    pub ssh: Option<SshTunnelConfig>,
}

/// Hand-written `Debug` (instead of `#[derive(Debug)]`) so `password` is
/// redacted rather than printed in plaintext — same pattern as
/// `dbc_state::vault::Vault`'s hand-written `Debug`. Guards against a stray
/// `dbg!`/`tracing::debug!` on this struct or `PendingAfterUnlock` leaking
/// the master/connection password to logs or stdout.
impl std::fmt::Debug for ConnectionFormData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionFormData")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("engine", &self.engine)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("folder", &self.folder)
            .field("read_only", &self.read_only)
            .field("favourite", &self.favourite)
            .field("timeout_secs", &self.timeout_secs)
            .field("auto_limit", &self.auto_limit)
            .field("ssh", &self.ssh)
            .finish()
    }
}

impl ConnectionFormData {
    fn to_connection_config(&self) -> ConnectionConfig {
        ConnectionConfig {
            id: self.id.clone(),
            name: self.name.clone(),
            folder: self.folder.clone(),
            engine: self.engine,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            user: self.user.clone(),
            read_only: self.read_only,
            timeout_secs: self.timeout_secs,
            auto_limit: self.auto_limit,
            ssh: self.ssh.clone(),
            favourite: self.favourite,
        }
    }
}

/// What to do once the vault becomes available (unlocked or freshly created).
#[derive(Clone)]
pub enum PendingAfterUnlock {
    Connect(String),
    SaveConnection(Box<ConnectionFormData>),
}

#[derive(Clone)]
pub enum ModalState {
    ConnectionDialog(ConnectionDialogUi),
    MasterPasswordPrompt {
        input: Entity<TextField>,
        error: Option<String>,
        pending: PendingAfterUnlock,
    },
    CreateMasterPassword {
        input1: Entity<TextField>,
        input2: Entity<TextField>,
        error: Option<String>,
        pending: PendingAfterUnlock,
    },
}

// ---------------------------------------------------------------------
// 4. AppView handlers + render helpers.
// ---------------------------------------------------------------------

impl AppView {
    pub(crate) fn current_connection_label(&self) -> String {
        if let Some(id) = &self.active_connection_id {
            if let Some(c) = self.config.connections.iter().find(|c| &c.id == id) {
                return format!("{} ({})", c.name, engine_label(c.engine));
            }
        }
        if let Some(url) = &self.conn_url {
            return url.clone();
        }
        "Bez připojení".to_string()
    }

    /// Recompute the cached folder/favourite grouping from `self.config`.
    /// Called on dropdown-open and after any config mutation, rather than
    /// per render frame (`render_dropdown_overlay` may be re-invoked many
    /// times per second while the dropdown stays open, e.g. on hover).
    pub(crate) fn refresh_grouped_cache(&mut self) {
        self.grouped_cache = group_connections(&self.config.connections);
    }

    pub(crate) fn render_top_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self.current_connection_label();
        div()
            .id("top-bar")
            .h(px(32.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .bg(rgb(0x181825))
            .text_color(rgb(0xcdd6f4))
            .cursor_pointer()
            .child(format!("Připojení: {label} ▾"))
            .on_click(cx.listener(|view, _, _, cx| {
                view.dropdown_open = !view.dropdown_open;
                if view.dropdown_open {
                    view.refresh_grouped_cache();
                }
                cx.notify();
            }))
    }

    pub(crate) fn render_dropdown_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let grouped = self.grouped_cache.clone();
        let mut panel = div()
            .absolute()
            .top(px(32.))
            .left(px(4.))
            .w(px(340.))
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded_md()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .text_color(rgb(0xcdd6f4))
            .occlude()
            .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                view.dropdown_open = false;
                cx.notify();
            }));

        if !grouped.favourites.is_empty() {
            panel = panel.child(div().text_color(rgb(0xf9e2af)).child("Oblíbené"));
            for c in &grouped.favourites {
                panel = panel.child(dropdown_item(c, 1, cx));
            }
        }
        for folder in &grouped.folders {
            let header = if folder.path.is_empty() { "Bez složky".to_string() } else { folder.path.join("/") };
            let depth = folder.path.len();
            panel = panel.child(
                div()
                    .text_color(rgb(0x89b4fa))
                    .child(format!("{}{}", "  ".repeat(depth), header)),
            );
            for c in &folder.connections {
                panel = panel.child(dropdown_item(c, depth + 1, cx));
            }
        }
        panel = panel.child(
            div()
                .id("dropdown-new")
                .mt_1()
                .cursor_pointer()
                .text_color(rgb(0xa6e3a1))
                .hover(|s| s.bg(rgb(0x313244)))
                .child("Nové spojení…")
                .on_click(cx.listener(|view, _, window, cx| {
                    view.open_connection_dialog(None, window, cx);
                })),
        );
        panel.into_any_element()
    }

    pub(crate) fn render_modal_overlay(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let modal = self.modal.clone()?;
        let panel = match modal {
            ModalState::ConnectionDialog(ui) => render_connection_dialog_panel(ui, cx),
            ModalState::MasterPasswordPrompt { input, error, .. } => render_master_password_panel(input, error, cx),
            ModalState::CreateMasterPassword { input1, input2, error, .. } => {
                render_create_master_password_panel(input1, input2, error, cx)
            }
        };
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x00000099))
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    pub(crate) fn open_connection_dialog(
        &mut self,
        editing: Option<ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| TextField::new(cx, "např. Produkce", false));
        let host = cx.new(|cx| TextField::new(cx, "localhost", false));
        let port = cx.new(|cx| TextField::new(cx, "5432", false));
        let database = cx.new(|cx| TextField::new(cx, "", false));
        let user = cx.new(|cx| TextField::new(cx, "", false));
        let password = cx.new(|cx| TextField::new(cx, "", true));
        let folder = cx.new(|cx| TextField::new(cx, "a/b", false));
        let timeout_secs = cx.new(|cx| TextField::new(cx, "30", false));
        let auto_limit = cx.new(|cx| TextField::new(cx, "1000", false));
        let ssh_host = cx.new(|cx| TextField::new(cx, "", false));
        let ssh_port = cx.new(|cx| TextField::new(cx, "22", false));
        let ssh_user = cx.new(|cx| TextField::new(cx, "", false));
        let ssh_key_path = cx.new(|cx| TextField::new(cx, "~/.ssh/id_ed25519", false));

        let (editing_id, engine, read_only, favourite, ssh_enabled) = if let Some(c) = &editing {
            name.update(cx, |f, cx| f.set_text(&c.name, cx));
            host.update(cx, |f, cx| f.set_text(&c.host, cx));
            port.update(cx, |f, cx| f.set_text(&c.port.map(|p| p.to_string()).unwrap_or_default(), cx));
            database.update(cx, |f, cx| f.set_text(&c.database, cx));
            user.update(cx, |f, cx| f.set_text(&c.user, cx));
            folder.update(cx, |f, cx| f.set_text(&c.folder.join("/"), cx));
            timeout_secs.update(cx, |f, cx| f.set_text(&c.timeout_secs.map(|v| v.to_string()).unwrap_or_default(), cx));
            auto_limit.update(cx, |f, cx| f.set_text(&c.auto_limit.map(|v| v.to_string()).unwrap_or_default(), cx));
            let ssh_enabled = c.ssh.is_some();
            if let Some(ssh) = &c.ssh {
                ssh_host.update(cx, |f, cx| f.set_text(&ssh.host, cx));
                ssh_port.update(cx, |f, cx| f.set_text(&ssh.port.to_string(), cx));
                ssh_user.update(cx, |f, cx| f.set_text(&ssh.user, cx));
                ssh_key_path.update(cx, |f, cx| f.set_text(ssh.key_path.as_deref().unwrap_or(""), cx));
            }
            (Some(c.id.clone()), c.engine, c.read_only, c.favourite, ssh_enabled)
        } else {
            (None, Engine::Postgres, false, false, false)
        };

        let name_focus = name.focus_handle(cx);
        let ui = ConnectionDialogUi {
            editing_id,
            name,
            host,
            port,
            database,
            user,
            password,
            folder,
            timeout_secs,
            auto_limit,
            ssh_host,
            ssh_port,
            ssh_user,
            ssh_key_path,
            engine,
            read_only,
            favourite,
            ssh_enabled,
            test_result: None,
            testing: false,
        };
        self.modal = Some(ModalState::ConnectionDialog(ui));
        self.dropdown_open = false;
        window.focus(&name_focus, cx);
        cx.notify();
    }

    pub(crate) fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        cx.notify();
    }

    fn cycle_engine(&mut self, cx: &mut Context<Self>) {
        if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
            ui.engine = next_engine(ui.engine);
        }
        cx.notify();
    }
    fn toggle_read_only(&mut self, cx: &mut Context<Self>) {
        if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
            ui.read_only = !ui.read_only;
        }
        cx.notify();
    }
    fn toggle_favourite(&mut self, cx: &mut Context<Self>) {
        if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
            ui.favourite = !ui.favourite;
        }
        cx.notify();
    }
    fn toggle_ssh_enabled(&mut self, cx: &mut Context<Self>) {
        if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
            ui.ssh_enabled = !ui.ssh_enabled;
        }
        cx.notify();
    }

    /// Dispatches the Test button's connect off the UI thread via
    /// `QueryRunner::test_connect` (Task 8 review issue #1: this used to
    /// call `pending_connect` synchronously, freezing the whole window for
    /// however long an unreachable host's TCP handshake took). Sets
    /// `testing = true` immediately (drives the "testuji…" status line and
    /// guards against a second click starting a redundant in-flight test),
    /// then updates `test_result` once the result comes back over a oneshot
    /// channel — same "UI thread only ever awaits a channel via `cx.spawn`"
    /// shape as `run_query`'s `QueryEvent` drain.
    fn on_test_clicked(&mut self, cx: &mut Context<Self>) {
        let Some(ModalState::ConnectionDialog(ui)) = &self.modal else { return };
        if ui.testing {
            return;
        }
        let ui_snapshot = ui.clone();
        let data = ui_snapshot.to_form_data(cx);
        let secret = if !data.password.is_empty() {
            Some(data.password.clone())
        } else {
            self.vault.as_ref().and_then(|v| v.get_secret(&data.id))
        };
        let engine_lbl = engine_label(data.engine);
        let editing_id = ui_snapshot.editing_id.clone();
        let cfg = data.to_connection_config();

        match test_connect_spec(cfg, secret) {
            Err(msg) => {
                if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
                    ui.test_result = Some(Err(msg));
                }
                cx.notify();
            }
            Ok(spec) => {
                if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
                    ui.testing = true;
                    ui.test_result = None;
                }
                cx.notify();

                let rx = self.runner.test_connect(spec);
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        // Guard: the dialog may have been closed/reopened
                        // (a different `editing_id`, or no dialog at all)
                        // while this was in flight — don't resurrect a
                        // stale result onto an unrelated/closed dialog.
                        if let Some(ModalState::ConnectionDialog(ui)) = &mut view.modal {
                            if ui.editing_id == editing_id {
                                ui.testing = false;
                                ui.test_result = Some(match result {
                                    Ok(Ok(())) => Ok(format!("Připojeno ({engine_lbl})")),
                                    Ok(Err(e)) => Err(e.to_string()),
                                    Err(_) => Err("connect zrušen".to_string()),
                                });
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    fn on_save_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ModalState::ConnectionDialog(ui)) = self.modal.clone() else { return };
        let data = ui.to_form_data(cx);
        if data.password.is_empty() || self.vault.is_some() {
            self.finish_save(data, cx);
            return;
        }
        if Vault::exists(&self.vault_path) {
            let input = cx.new(|cx| TextField::new(cx, "Heslo", true));
            let focus = input.focus_handle(cx);
            self.modal = Some(ModalState::MasterPasswordPrompt {
                input,
                error: None,
                pending: PendingAfterUnlock::SaveConnection(Box::new(data)),
            });
            window.focus(&focus, cx);
        } else {
            let input1 = cx.new(|cx| TextField::new(cx, "Nové heslo", true));
            let input2 = cx.new(|cx| TextField::new(cx, "Zopakujte heslo", true));
            let focus = input1.focus_handle(cx);
            self.modal = Some(ModalState::CreateMasterPassword {
                input1,
                input2,
                error: None,
                pending: PendingAfterUnlock::SaveConnection(Box::new(data)),
            });
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn finish_save(&mut self, data: ConnectionFormData, cx: &mut Context<Self>) {
        if self.config_load_error.is_some() {
            // final-review must-fix #2: never silently overwrite a config
            // file that failed to parse at startup. Move it aside first;
            // if that fails (permissions, file vanished, etc.), abort the
            // whole save rather than risk clobbering data the user may
            // still want to recover by hand.
            let backup = self.config_path.with_extension("toml.corrupt-bak");
            match std::fs::rename(&self.config_path, &backup) {
                Ok(()) => self.config_load_error = None,
                Err(e) => {
                    self.status = format!(
                        "error: nelze zálohovat poškozený config.toml ({e}) – uložení zrušeno"
                    );
                    cx.notify();
                    return;
                }
            }
        }
        if !data.password.is_empty() {
            let Some(vault) = self.vault.as_mut() else {
                // finish_save is only reached with a non-empty password once
                // the vault is unlocked/created (see on_save_clicked /
                // resume_pending); this branch is a defensive no-op guard,
                // not a normal path.
                self.status = "error: vault not unlocked".into();
                cx.notify();
                return;
            };
            if let Err(e) = vault.set_secret(&data.id, &data.password) {
                self.status = format!("error: {}", e.message);
                cx.notify();
                return;
            }
        }
        let cfg = data.to_connection_config();
        if let Some(existing) = self.config.connections.iter_mut().find(|c| c.id == cfg.id) {
            *existing = cfg;
        } else {
            self.config.connections.push(cfg);
        }
        self.status = match self.config.save(&self.config_path) {
            Ok(()) => "Uloženo".to_string(),
            Err(e) => format!("error saving config: {}", e.message),
        };
        self.refresh_grouped_cache();
        self.modal = None;
        self.dropdown_open = false;
        cx.notify();
    }

    pub(crate) fn on_dropdown_item_click(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let needs_secret = self
            .config
            .connections
            .iter()
            .find(|c| c.id == id)
            .map_or(false, |c| c.engine != Engine::Sqlite);
        if needs_secret && self.vault.is_none() && Vault::exists(&self.vault_path) {
            let input = cx.new(|cx| TextField::new(cx, "Heslo", true));
            let focus = input.focus_handle(cx);
            self.modal = Some(ModalState::MasterPasswordPrompt {
                input,
                error: None,
                pending: PendingAfterUnlock::Connect(id),
            });
            self.dropdown_open = false;
            window.focus(&focus, cx);
            cx.notify();
            return;
        }
        self.switch_to_connection(&id, cx);
    }

    /// Dispatches the dropdown connection-switch's validating connect off
    /// the UI thread via `QueryRunner::test_connect` (Task 8 review issue
    /// #1, same as `on_test_clicked`). Shows the existing "connecting…"
    /// status synchronously, then flips to the connected/error status and
    /// (only on success) switches `active_connection_id` once the result
    /// comes back.
    fn switch_to_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(cfg) = self.config.connections.iter().find(|c| c.id == id).cloned() else { return };
        let secret = self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id));
        let engine_lbl = engine_label(cfg.engine);
        let target_id = cfg.id.clone();
        self.dropdown_open = false;

        match test_connect_spec(cfg, secret) {
            Err(msg) => {
                self.status = format!("error: {msg}");
                cx.notify();
            }
            Ok(spec) => {
                self.status = "connecting…".into();
                self.switch_generation += 1;
                let my_generation = self.switch_generation;
                cx.notify();

                let rx = self.runner.test_connect(spec);
                cx.spawn(async move |this, cx| {
                    let result = rx.await;
                    let _ = this.update(cx, |view, cx| {
                        // A newer switch was dispatched meanwhile — this
                        // result is stale, drop it (last-dispatched wins).
                        if view.switch_generation != my_generation {
                            return;
                        }
                        match result {
                            Ok(Ok(())) => {
                                view.status = format!("Připojeno ({engine_lbl})");
                                view.active_connection_id = Some(target_id);
                                view.conn_url = None;
                            }
                            Ok(Err(e)) => view.status = format!("error: {e}"),
                            Err(_) => view.status = "error: connect zrušen".into(),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    fn resume_pending(&mut self, pending: PendingAfterUnlock, cx: &mut Context<Self>) {
        match pending {
            PendingAfterUnlock::Connect(id) => self.switch_to_connection(&id, cx),
            PendingAfterUnlock::SaveConnection(data) => self.finish_save(*data, cx),
        }
    }

    fn on_master_password_submit(&mut self, cx: &mut Context<Self>) {
        let Some(ModalState::MasterPasswordPrompt { input, pending, .. }) = self.modal.clone() else { return };
        let pwd = input.read(cx).text();
        match Vault::unlock(&self.vault_path, &pwd) {
            Ok(vault) => {
                self.vault = Some(vault);
                self.modal = None;
                self.resume_pending(pending, cx);
            }
            Err(e) => {
                input.update(cx, |f, cx| f.set_text("", cx));
                if let Some(ModalState::MasterPasswordPrompt { error, .. }) = &mut self.modal {
                    *error = Some(e.message);
                }
                cx.notify();
            }
        }
    }

    fn set_create_master_error(&mut self, msg: &str, cx: &mut Context<Self>) {
        if let Some(ModalState::CreateMasterPassword { error, .. }) = &mut self.modal {
            *error = Some(msg.to_string());
        }
        cx.notify();
    }

    fn on_create_master_password_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ModalState::CreateMasterPassword { input1, input2, pending, .. }) = self.modal.clone() else { return };
        let p1 = input1.read(cx).text();
        let p2 = input2.read(cx).text();
        if p1.is_empty() {
            self.set_create_master_error("Heslo nesmí být prázdné.", cx);
            return;
        }
        if p1 != p2 {
            self.set_create_master_error("Hesla se neshodují.", cx);
            return;
        }
        // Vault::create silently overwrites an existing vault file, so this
        // check MUST happen right before create() — if a vault appeared
        // between opening this modal and submitting (e.g. another instance
        // of the app created one), fall back to unlocking it instead of
        // clobbering it.
        if Vault::exists(&self.vault_path) {
            let input = cx.new(|cx| TextField::new(cx, "Heslo", true));
            let focus = input.focus_handle(cx);
            self.modal = Some(ModalState::MasterPasswordPrompt { input, error: None, pending });
            window.focus(&focus, cx);
            cx.notify();
            return;
        }
        match Vault::create(&self.vault_path, &p1) {
            Ok(vault) => {
                self.vault = Some(vault);
                self.modal = None;
                self.resume_pending(pending, cx);
            }
            Err(e) => self.set_create_master_error(&e.message, cx),
        }
    }
}

// ---------------------------------------------------------------------
// 5. Free helper functions.
// ---------------------------------------------------------------------

fn engine_label(e: Engine) -> &'static str {
    match e {
        Engine::Postgres => "pg",
        Engine::Mssql => "mssql",
        Engine::Sqlite => "sqlite",
    }
}

fn next_engine(e: Engine) -> Engine {
    match e {
        Engine::Postgres => Engine::Mssql,
        Engine::Mssql => Engine::Sqlite,
        Engine::Sqlite => Engine::Postgres,
    }
}

fn parse_u16(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.is_empty() { None } else { s.parse().ok() }
}
fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() { None } else { s.parse().ok() }
}
fn parse_folder(s: &str) -> Vec<String> {
    s.trim().split('/').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}
fn generate_connection_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("conn-{nanos:x}")
}

/// Builds the `ConnectSpec` for a Test/switch validation, short-circuiting
/// the permanent MSSQL-unsupported case client-side (rather than letting it
/// fall through to `open_config`'s own MSSQL rejection) so it doesn't need
/// to bounce through the runner's async plumbing for a case with zero I/O.
/// This is a permanent behaviour per the brief (the MSSQL driver is a
/// separate roadmap item), not a placeholder this function is expected to
/// eventually replace.
///
/// Used by both `on_test_clicked` and `switch_to_connection` — Task 8's
/// review found the synchronous `pending_connect` this replaces froze the
/// whole window on an unreachable/firewalled host (no bound on the
/// UI-thread `block_on` call); both call sites now dispatch the returned
/// spec through `QueryRunner::test_connect`, which runs entirely off the UI
/// thread and is bounded by `connect::open_config`'s `connect_timeout`.
fn test_connect_spec(cfg: ConnectionConfig, secret: Option<String>) -> Result<ConnectSpec, String> {
    if cfg.engine == Engine::Mssql {
        return Err("MSSQL driver zatím není k dispozici".into());
    }
    Ok(ConnectSpec::Config { cfg: Box::new(cfg), secret })
}

fn field_row(label: &str, field: Entity<TextField>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(div().w(px(130.)).text_color(rgb(0xa6adc8)).child(label.to_string()))
        .child(div().flex_1().child(field))
}

fn styled_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .bg(rgb(0x313244))
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0x45475a)))
        .child(label)
}

fn checkbox(id: &'static str, label: &'static str, checked: bool) -> Stateful<Div> {
    let mark = if checked { "☑" } else { "☐" };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .cursor_pointer()
        .child(format!("{mark} {label}"))
}

fn dropdown_item(c: &ConnectionConfig, depth: usize, cx: &mut Context<AppView>) -> impl IntoElement {
    let id = c.id.clone();
    let editing = c.clone();
    let label = format!("{}{} — {} {}", "  ".repeat(depth), c.name, engine_label(c.engine), c.host);
    div()
        .id(SharedString::from(format!("dropdown-item-row-{}", c.id)))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .hover(|s| s.bg(rgb(0x313244)))
        .child(
            div()
                .id(SharedString::from(format!("dropdown-item-{}", c.id)))
                .flex_1()
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.on_dropdown_item_click(id.clone(), window, cx);
                })),
        )
        .child(
            // Edit affordance (must-fix #1 from the whole-branch review):
            // the row's own on_click always connects, so editing (or
            // fixing a typo, or setting a favourite) an existing
            // connection needs a separate path into `open_connection_dialog`.
            // `cx.stop_propagation()` keeps this click from also bubbling
            // to the row's connect handler above.
            div()
                .id(SharedString::from(format!("dropdown-item-edit-{}", c.id)))
                .px_1()
                .cursor_pointer()
                .text_color(rgb(0xa6adc8))
                .hover(|s| s.bg(rgb(0x45475a)))
                .child("✎")
                .on_click(cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_connection_dialog(Some(editing.clone()), window, cx);
                })),
        )
}

fn render_connection_dialog_panel(ui: ConnectionDialogUi, cx: &mut Context<AppView>) -> AnyElement {
    let title = if ui.editing_id.is_some() { "Připojení — úprava" } else { "Připojení — nové" };
    // While `testing`, the in-flight-status line takes priority over
    // whatever the previous `test_result` was (a stale ✓/✗ from an earlier
    // click shouldn't linger under a fresh "testuji…").
    let test_line = if ui.testing {
        Some(("testuji…".to_string(), None))
    } else {
        ui.test_result.clone().map(|r| match r {
            Ok(msg) => (format!("✓ {msg}"), Some(true)),
            Err(e) => (format!("✗ {e}"), Some(false)),
        })
    };
    let testing = ui.testing;

    let mut panel: Div = div()
        .w(px(480.))
        .bg(rgb(0x1e1e2e))
        .border_1()
        .border_color(rgb(0x45475a))
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(rgb(0xcdd6f4))
        .child(div().text_size(px(16.)).child(title))
        .child(field_row("Název", ui.name.clone()))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(130.)).text_color(rgb(0xa6adc8)).child("Engine"))
                .child(
                    div()
                        .id("engine-cycle")
                        .px_2()
                        .py_1()
                        .bg(rgb(0x313244))
                        .rounded_md()
                        .cursor_pointer()
                        .child(engine_label(ui.engine))
                        .on_click(cx.listener(|view, _, _, cx| view.cycle_engine(cx))),
                ),
        )
        .child(field_row("Host", ui.host.clone()))
        .child(field_row("Port", ui.port.clone()))
        .child(field_row("Databáze", ui.database.clone()))
        .child(field_row("Uživatel", ui.user.clone()))
        .child(field_row("Heslo", ui.password.clone()))
        .child(field_row("Složka", ui.folder.clone()))
        .child(field_row("Timeout (s)", ui.timeout_secs.clone()))
        .child(field_row("Auto-limit řádků", ui.auto_limit.clone()))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_4()
                .child(checkbox("chk-read-only", "Pouze pro čtení", ui.read_only).on_click(cx.listener(|v, _, _, cx| v.toggle_read_only(cx))))
                .child(checkbox("chk-favourite", "Oblíbené", ui.favourite).on_click(cx.listener(|v, _, _, cx| v.toggle_favourite(cx)))),
        )
        .child(
            checkbox("chk-ssh", "SSH tunel (jen klíč/agent)", ui.ssh_enabled)
                .on_click(cx.listener(|v, _, _, cx| v.toggle_ssh_enabled(cx))),
        );

    if ui.ssh_enabled {
        panel = panel
            .child(field_row("SSH host", ui.ssh_host.clone()))
            .child(field_row("SSH port", ui.ssh_port.clone()))
            .child(field_row("SSH uživatel", ui.ssh_user.clone()))
            .child(field_row("SSH klíč (cesta)", ui.ssh_key_path.clone()));
    }

    if let Some((text, ok)) = test_line {
        let color = match ok {
            Some(true) => rgb(0xa6e3a1),
            Some(false) => rgb(0xf38ba8),
            None => rgb(0xa6adc8),
        };
        panel = panel.child(div().text_color(color).child(text));
    }

    let test_label = if testing { "Testuji…" } else { "Test" };
    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("dlg-test", test_label).on_click(cx.listener(|v, _, _, cx| v.on_test_clicked(cx))))
            .child(styled_button("dlg-save", "Uložit").on_click(cx.listener(|v, _, window, cx| v.on_save_clicked(window, cx))))
            .child(styled_button("dlg-cancel", "Zrušit").on_click(cx.listener(|v, _, _, cx| v.close_modal(cx)))),
    );

    panel.into_any_element()
}

fn render_master_password_panel(input: Entity<TextField>, error: Option<String>, cx: &mut Context<AppView>) -> AnyElement {
    let mut panel: Div = div()
        .w(px(360.))
        .bg(rgb(0x1e1e2e))
        .border_1()
        .border_color(rgb(0x45475a))
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(rgb(0xcdd6f4))
        .child(div().text_size(px(16.)).child("Master heslo"))
        .child(field_row("Heslo", input));
    if let Some(e) = error {
        panel = panel.child(div().text_color(rgb(0xf38ba8)).child(e));
    }
    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("mpp-cancel", "Zrušit").on_click(cx.listener(|v, _, _, cx| v.close_modal(cx))))
            .child(styled_button("mpp-submit", "Odemknout").on_click(cx.listener(|v, _, _, cx| v.on_master_password_submit(cx)))),
    );
    panel.into_any_element()
}

fn render_create_master_password_panel(
    input1: Entity<TextField>,
    input2: Entity<TextField>,
    error: Option<String>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let mut panel: Div = div()
        .w(px(360.))
        .bg(rgb(0x1e1e2e))
        .border_1()
        .border_color(rgb(0x45475a))
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(rgb(0xcdd6f4))
        .child(div().text_size(px(16.)).child("Vytvořit master heslo"))
        .child(field_row("Nové heslo", input1))
        .child(field_row("Zopakujte heslo", input2));
    if let Some(e) = error {
        panel = panel.child(div().text_color(rgb(0xf38ba8)).child(e));
    }
    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("cmp-cancel", "Zrušit").on_click(cx.listener(|v, _, _, cx| v.close_modal(cx))))
            .child(styled_button("cmp-submit", "Vytvořit").on_click(cx.listener(|v, _, window, cx| v.on_create_master_password_submit(window, cx)))),
    );
    panel.into_any_element()
}
