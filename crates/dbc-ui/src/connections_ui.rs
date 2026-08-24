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

use dbc_buffer::ResultBuffer;
use dbc_state::{ConnectionConfig, Engine, MssqlOptions, SshTunnelConfig, Vault};
use gpui::{
    actions, div, fill, hsla, point, prelude::*, px, relative, size, App, AnyElement,
    Bounds, ClipboardItem, Context, CursorStyle, Div, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Stateful, Style, TextRun, UTF16Selection, UnderlineStyle,
    uniform_list, Window,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::chart_data::ChartKind;
use crate::runner::ConnectSpec;
use crate::text_model::MultilineBuffer;
use crate::theme::{ActiveTheme, Theme};
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
            mssql: None,
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

/// Whether `copy`/`cut` must refuse to touch the clipboard — security
/// follow-up #4 (final-review.md): a masked (password) `TextField` used to
/// write the REAL buffer text to the clipboard on Ctrl+C/Ctrl+X even though
/// the field only ever displays `•` bullets, leaking the plaintext password
/// onto the system clipboard (visible to any other app / clipboard
/// history). Standard password-field behaviour is to disable copy/cut
/// entirely rather than try to redact "some of it" — pulled out as a pure
/// function so the decision is unit-tested without a GPUI window.
fn blocks_clipboard_write(masked: bool) -> bool {
    masked
}

#[cfg(test)]
mod clipboard_guard_tests {
    use super::*;

    #[test]
    fn masked_field_blocks_clipboard_write() {
        assert!(blocks_clipboard_write(true));
    }

    #[test]
    fn unmasked_field_allows_clipboard_write() {
        assert!(!blocks_clipboard_write(false));
    }
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
        if blocks_clipboard_write(self.masked) {
            return;
        }
        if let Some(sel) = self.buffer.selection() {
            if !sel.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(self.buffer.text()[sel].to_string()));
            }
        }
    }
    fn cut(&mut self, _: &Cut, _window: &mut Window, cx: &mut Context<Self>) {
        if blocks_clipboard_write(self.masked) {
            return;
        }
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
                        cx.theme().bg_selection,
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
    pub mssql_driver: Entity<TextField>,
    pub engine: Engine,
    pub read_only: bool,
    pub favourite: bool,
    pub ssh_enabled: bool,
    pub mssql_encrypt: bool,
    pub mssql_trust_cert: bool,
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
        // G15 T3: only carried for the Mssql engine — matches
        // `ConnectionConfig.mssql`'s "None means non-MSSQL or all
        // defaults" contract.
        let mssql = if self.engine == Engine::Mssql {
            let driver = self.mssql_driver.read(cx).text();
            Some(MssqlOptions {
                encrypt: self.mssql_encrypt,
                trust_server_certificate: self.mssql_trust_cert,
                driver: if driver.trim().is_empty() { None } else { Some(driver) },
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
            mssql,
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
    pub mssql: Option<MssqlOptions>,
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
            .field("mssql", &self.mssql)
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
            mssql: self.mssql.clone(),
        }
    }
}

#[cfg(test)]
mod form_data_mssql_tests {
    use super::*;

    fn base_form_data(engine: Engine) -> ConnectionFormData {
        ConnectionFormData {
            id: "c1".into(),
            name: "demo".into(),
            engine,
            host: "localhost".into(),
            port: None,
            database: "db".into(),
            user: "u".into(),
            password: String::new(),
            folder: vec![],
            read_only: false,
            favourite: false,
            timeout_secs: None,
            auto_limit: None,
            ssh: None,
            mssql: if engine == Engine::Mssql {
                Some(MssqlOptions { encrypt: false, trust_server_certificate: true, driver: Some("ODBC Driver 17 for SQL Server".into()) })
            } else {
                None
            },
        }
    }

    #[test]
    fn form_data_maps_mssql_options_only_for_mssql_engine() {
        let pg_cfg = base_form_data(Engine::Postgres).to_connection_config();
        assert_eq!(pg_cfg.mssql, None);

        let mssql_data = base_form_data(Engine::Mssql);
        let mssql_cfg = mssql_data.to_connection_config();
        assert_eq!(
            mssql_cfg.mssql,
            Some(MssqlOptions {
                encrypt: false,
                trust_server_certificate: true,
                driver: Some("ODBC Driver 17 for SQL Server".into()),
            })
        );
    }
}

/// What to do once the vault becomes available (unlocked or freshly created).
#[derive(Clone)]
pub enum PendingAfterUnlock {
    Connect(String),
    SaveConnection(Box<ConnectionFormData>),
    /// Security follow-up #6 (final-review.md): the Test button used to
    /// test WITHOUT the stored secret when the vault was locked (a
    /// confusing auth failure) instead of prompting for the master
    /// password like a normal connect. Carries the in-progress dialog's
    /// `ConnectionDialogUi` — NOT a frozen snapshot: its fields are
    /// `Entity<TextField>` handles, the SAME live entities the dialog was
    /// using, just cloned (cheaply — cloning an `Entity` clones the handle,
    /// not its content). `resume_pending`/`cancel_master_password_prompt`
    /// reopen the dialog from these handles and (on resume) re-read their
    /// CURRENT text via `to_form_data`, so whatever the user had typed
    /// stays intact across the detour through this prompt.
    TestConnection(Box<ConnectionDialogUi>),
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
    /// G6 Task 3: parametrized `:name` query values dialog — one row per
    /// distinct name (`names`/`inputs`/`null_flags` are parallel, same
    /// index), opened by `AppView::open_query_params_dialog` from
    /// `run_query`'s interception. `sql_template` is the ORIGINAL SQL (with
    /// live `:name` tokens, not yet substituted); `bypass_auto_limit` is
    /// the caller's original run intent (Ctrl+Enter vs Ctrl+Shift+Enter vs
    /// palette), carried through so confirming the dialog runs with the
    /// SAME guard behavior the user originally asked for. `error` is set by
    /// `confirm_query_params` when `build_param_sql` refuses (design §5's
    /// mandatory post-substitution rescan) — shown in the dialog, dialog
    /// stays open, nothing runs, nothing persists.
    QueryParams {
        names: Vec<String>,
        inputs: Vec<Entity<TextField>>,
        null_flags: Vec<bool>,
        sql_template: String,
        bypass_auto_limit: bool,
        error: Option<String>,
    },
    /// G9: confirmed-admin-action dialog for kill (design §6). Reuses the
    /// single-modal-at-a-time infrastructure deliberately — `run_query_with`
    /// already refuses to run while `modal.is_some()`, and the
    /// dropdown/palette refuse to open a second modal; a kill confirmation
    /// is exactly the blocking dialog that invariant exists for. `sql` is
    /// the LITERAL statement that will run (shown in a monospace block —
    /// same "show the exact generated SQL" principle as the Apply dialog).
    /// `error` is a failed kill's message: the dialog stays open with it
    /// (same "error stays in the modal" precedent as Apply's
    /// rollback-error UX).
    ///
    /// Review fix (MAJOR + MINOR, G9 T6 adversarial review): `pid`+`tab_id`
    /// are what let `on_monitor_view_event` (main.rs) tell THIS dialog's
    /// outcome apart from some other tab's/pid's in-flight kill — a
    /// `KillFinished` event whose `(pid, tab_id)` doesn't match the
    /// currently open dialog must never mutate it (misattribution: one
    /// kill's result landing in a DIFFERENT kill's dialog). `dispatched`
    /// guards the same class of bug on the send side: nothing previously
    /// stopped a double-click on "Ukončit proces" from dispatching two
    /// `Kill` commands for the same pid while the first was still in
    /// flight — `confirm_kill_confirm` sets it on the first click and
    /// becomes a no-op on any click after.
    KillConfirm {
        pid: i64,
        label: String, // "{user} · {application} · běží {n}s"
        sql: String,
        tab_id: u64,
        error: Option<String>,
        dispatched: bool,
    },
    /// G13 T6 (design §5 case 3 / §3-novela): confirm dialog for
    /// "Analyzovat" on a write statement over a WRITABLE connection — `sql`
    /// is the ORIGINAL (pre-`EXPLAIN ANALYZE`-wrap) editor text, shown
    /// verbatim so the user sees exactly what will actually run (and be
    /// rolled back). `engine` is needed at confirm time to rebuild the
    /// wrapped `EXPLAIN ANALYZE` SQL via `plan::explain_analyze_sql`.
    ///
    /// Review fix (MAJOR, adversarial review of commit 0bab655): `running`/
    /// `error` mirror `ApplyDialogState`'s exact shape — `self.modal` stays
    /// `Some(AnalyzeWriteConfirm { running: true, .. })` (mutated in place,
    /// never cleared) for the WHOLE duration of the analyze, so the
    /// existing `self.modal.is_some()` busy-guards elsewhere refuse a
    /// second dispatch, and Escape (which only allow-lists
    /// `ConnectionDialog`/`QueryParams` as closable — see
    /// `AppView::on_cancel_query`) is a structural no-op against it rather
    /// than the previous version's `self.cancel`-based guard, which was
    /// never actually wired to `QueryRunner::run_analyze_write` and so
    /// could be defeated by Escape mid-flight.
    AnalyzeWriteConfirm { sql: String, engine: Engine, running: bool, error: Option<String> },
    /// G7 T6: two-connection picker for the schema/data compare feature
    /// (design §3). `conn_a`/`conn_b` are `ConnectionConfig.id` values (or
    /// `None` while unpicked); "Spustit porovnání" (`confirm_compare_dialog`)
    /// is disabled until both are `Some`. The SAME connection on both sides
    /// is explicitly ALLOWED (design §3) — yields an all-Unchanged result,
    /// useful as a smoke test — so there is no equality guard here. `error`
    /// is currently unused by any handler (schema-pair fetch failures are
    /// surfaced on the resulting `CompareView` tab, T7, not back into this
    /// already-closed dialog) but kept for a uniform modal-state shape and
    /// possible future pre-dispatch validation.
    CompareDialog { conn_a: Option<String>, conn_b: Option<String>, error: Option<String> },
    /// G11 T6 (design §2/§3, §3-novela): backup/restore confirm/progress
    /// overlay — one panel per `BackupSession::status` transition
    /// (Confirming [Restore only] -> Running -> terminal), same
    /// single-modal-at-a-time shape every other arm here already
    /// establishes. See `crate::backup::BackupSession`'s own doc comment
    /// for why the session lives in `backup.rs` rather than as fields here.
    BackupRestore(crate::backup::BackupSession),
    /// G12 T3: script-runner confirm modal (design §3) — opened by
    /// `AppView::start_script_pick` once the file/folder picker resolves and
    /// the pre-scan has counted each file's statements. Confirmed via
    /// `AppView::confirm_script_run`, which RE-RESOLVES the connection spec
    /// fresh at confirm time (same `resolve_spec_for_explain` precedent
    /// `AnalyzeWriteConfirm` uses above) rather than storing one here — this
    /// modal only carries display data plus the user's tx-scope/error-policy
    /// choice; `conn_label`/`read_only`/`timeout_secs` are a snapshot taken
    /// at pick time purely for the confirm dialog's own display (the "jen
    /// pro čtení" badge + timeout line), not re-validated against a possible
    /// connection switch in between — the modal occludes the top bar, so a
    /// switch can't happen while it's open.
    ScriptRun {
        /// (path, pre-scanned statement count) per file, run order.
        files: Vec<std::path::PathBuf>,
        file_counts: Vec<usize>,
        tx_scope: crate::runner::TxScope,
        error_policy: crate::runner::ErrorPolicy,
        /// "{filename}" for a single file, or "{foldername}/ ({n} souborů)"
        /// for a folder run — drives the modal heading AND the progress
        /// tab's title.
        source_label: String,
        conn_label: String,
        read_only: bool,
        timeout_secs: Option<u64>,
        /// Review fix (MAJOR 1, same pattern as `CsvImport.conn_identity`
        /// below): the STABLE identity (`AppView::current_conn_identity`)
        /// captured at `start_script_pick` dispatch time, BEFORE the file
        /// picker + background pre-scan ever ran — both leave the
        /// connection dropdown clickable. `confirm_script_run` re-verifies
        /// this against the CURRENTLY active connection before dispatching
        /// anything; on mismatch it refuses rather than running a stale
        /// file/folder selection against a different, currently-active
        /// (writable) database.
        conn_identity: String,
    },
    /// G12 T4: CSV import mapping modal (design §5) — opened once the file
    /// picker + header/row pre-count pass resolve (`AppView::start_csv_import`).
    /// `targets` is the live mapping being edited
    /// (`AppView::cycle_csv_target`); `sample_sql` is recomputed on every
    /// mapping change (`AppView::recompute_csv_sample`) from the REAL first
    /// batch, never a synthetic example — an `Err` from
    /// `csv_import::generate_insert_batches` (duplicate target) fills
    /// `error` and disables "Spustit import". Same "re-resolve the spec
    /// fresh at confirm time" posture as `ScriptRun` above — this modal only
    /// carries display/editing data.
    ///
    /// Review fix (BLOCKER): `schema`/`table`/`columns` are a snapshot of
    /// the connection ACTIVE at `start_csv_import` time — the file picker +
    /// background pre-count pass do NOT block the UI, so the connection
    /// dropdown stays clickable while this modal is being built (before it
    /// even opens) and while it's open. `conn_identity` is the SAME stable
    /// identity value `ResultTab::conn_identity`/`current_conn_identity`
    /// use, captured at `start_csv_import` dispatch time — `confirm_csv_import`
    /// re-checks it against the CURRENT active connection before building
    /// anything, refusing on mismatch (same "connection changed out from
    /// under staged state" guard `on_open_apply_dialog`/`on_confirm_apply`
    /// already enforce for the Apply flow, main.rs). `conn_label` is
    /// `current_connection_label()`'s snapshot, shown in the modal so the
    /// target connection is visible (same convention `ScriptRun`'s
    /// `conn_label` sets above).
    CsvImport {
        path: std::path::PathBuf,
        schema: Option<String>,
        table: String,
        headers: Vec<String>,
        columns: Vec<crate::csv_import::TargetColumn>,
        targets: Vec<Option<usize>>,
        row_count: usize,
        first_rows: Vec<crate::csv_import::CsvRow>,
        sample_sql: Option<String>,
        error: Option<String>,
        conn_identity: String,
        conn_label: String,
    },
    /// G14 T10: app settings modal (theme row only, for now). Unit variant —
    /// all its state (`config.theme`) already lives on `AppView`, same
    /// "modal only carries display data" posture the other arms follow.
    /// Opened via the topbar gear or the palette's "Přepnout motiv"
    /// bypasses this entirely (direct `toggle_theme` dispatch, no modal).
    Settings,
    /// G14 T11: bar/line chart axis picker (design §2.1/§2.4) — opened by
    /// `AppView::open_chart_picker` (grid "Graf" button / palette "Graf z
    /// výsledku") or reopened editing-in-place by
    /// `AppView::on_chart_view_event` (a Chart tab's "Upravit…").
    ChartPicker {
        source_title: String,
        buffer: std::rc::Rc<std::cell::RefCell<ResultBuffer>>,
        /// (column name, is_numeric) per buffer column, display order.
        columns: Vec<(String, bool)>,
        kind: ChartKind,
        x_col: usize,
        /// One flag per column; only numeric columns are toggleable (design
        /// §2.1: Y list pre-filtered numeric, X unfiltered).
        y_selected: Vec<bool>,
        /// Some(tab_id): re-pick — reconfigure that tab's `ChartView` in
        /// place instead of opening a new tab.
        edit_tab: Option<u64>,
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
            .bg(cx.theme().bg_app)
            .text_color(cx.theme().text_primary)
            .cursor_pointer()
            .child(format!("Připojení: {label} ▾"))
            .on_click(cx.listener(|view, _, _, cx| {
                view.dropdown_open = !view.dropdown_open;
                if view.dropdown_open {
                    view.refresh_grouped_cache();
                }
                cx.notify();
            }))
            // Version label, right-aligned (spec: Versioning — bumped per
            // completed phase as part of the merge checklist).
            .child(
                div()
                    .ml_auto()
                    .text_color(cx.theme().text_faint)
                    .child(format!("dbc v{}", env!("CARGO_PKG_VERSION"))),
            )
            // G14 T10: settings gear — same `cx.stop_propagation()` pattern
            // as `dropdown_item`'s ★/✎ icon buttons so this click doesn't
            // also bubble to the row's dropdown-toggle handler above.
            .child(
                div()
                    .id("top-bar-settings")
                    .px_1()
                    .cursor_pointer()
                    .text_color(cx.theme().text_muted)
                    .hover(|s| s.text_color(cx.theme().text_primary))
                    .child("⚙")
                    .on_click(cx.listener(|view, _, _, cx| {
                        cx.stop_propagation();
                        view.open_settings(cx);
                    })),
            )
    }

    pub(crate) fn render_dropdown_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let grouped = self.grouped_cache.clone();
        let mut panel = div()
            .absolute()
            .top(px(32.))
            .left(px(4.))
            .w(px(340.))
            .bg(cx.theme().bg_panel)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .p_2()
            .flex()
            .flex_col()
            .gap_1()
            .text_color(cx.theme().text_primary)
            .occlude()
            .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                view.dropdown_open = false;
                cx.notify();
            }));

        if !grouped.favourites.is_empty() {
            panel = panel.child(div().text_color(cx.theme().warn).child("Oblíbené"));
            for c in &grouped.favourites {
                panel = panel.child(dropdown_item(c, 1, cx));
            }
        }
        for folder in &grouped.folders {
            let header = if folder.path.is_empty() { "Bez složky".to_string() } else { folder.path.join("/") };
            let depth = folder.path.len();
            panel = panel.child(
                div()
                    .text_color(cx.theme().accent)
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
                .text_color(cx.theme().success)
                .hover(|s| s.bg(cx.theme().bg_hover))
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
            ModalState::QueryParams { names, inputs, null_flags, sql_template, error, .. } => {
                render_query_params_panel(names, inputs, null_flags, sql_template, error, cx)
            }
            ModalState::KillConfirm { pid, label, sql, error, dispatched, .. } => {
                render_kill_confirm_panel(pid, &label, &sql, &error, dispatched, cx)
            }
            ModalState::AnalyzeWriteConfirm { sql, engine, running, error } => {
                render_analyze_write_confirm_panel(&sql, engine, running, &error, cx)
            }
            ModalState::CompareDialog { conn_a, conn_b, error } => {
                render_compare_dialog_panel(conn_a, conn_b, error, self.grouped_cache.clone(), cx)
            }
            ModalState::BackupRestore(session) => render_backup_restore_panel(&session, cx),
            ModalState::ScriptRun {
                files,
                file_counts,
                tx_scope,
                error_policy,
                source_label,
                conn_label,
                read_only,
                timeout_secs,
                ..
            } => render_script_run_confirm_panel(
                &files,
                &file_counts,
                tx_scope,
                error_policy,
                &source_label,
                &conn_label,
                read_only,
                timeout_secs,
                cx,
            ),
            ModalState::CsvImport {
                path,
                table,
                headers,
                columns,
                targets,
                row_count,
                sample_sql,
                error,
                conn_label,
                ..
            } => render_csv_import_panel(
                &path, &table, &headers, &columns, &targets, row_count, &sample_sql, &error,
                &conn_label, cx,
            ),
            ModalState::Settings => self.render_settings_panel(cx),
            ModalState::ChartPicker { source_title, columns, kind, x_col, y_selected, edit_tab, .. } => {
                render_chart_picker_panel(source_title, columns, kind, x_col, y_selected, edit_tab, cx)
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
                .bg(cx.theme().bg_backdrop)
                .occlude()
                .child(panel)
                .into_any_element(),
        )
    }

    /// G14 T10: theme row only, for now (design's minimal `ModalState::Settings`
    /// scope) — the two radios call `set_theme` directly, so the switch is
    /// visible immediately while the modal stays open (design §1.5: "the
    /// user sees the live switch"); "Zavřít" (or Esc — see
    /// `AppView::on_cancel_query`'s closable match) is the only way out.
    fn render_settings_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.config.theme;
        let radio = |id: &'static str,
                     label: &'static str,
                     m: dbc_state::ThemeMode,
                     current: dbc_state::ThemeMode,
                     cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .bg(if m == current { cx.theme().bg_selected } else { cx.theme().bg_hover })
                .child(if m == current { "●" } else { "○" })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_theme(m, cx)))
        };
        div()
            .id("settings-panel")
            .w(px(360.))
            .bg(cx.theme().bg_panel)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .text_color(cx.theme().text_primary)
            .child(div().text_size(px(16.)).child("Nastavení"))
            .child(div().text_color(cx.theme().text_muted).child("Motiv"))
            .child(radio("settings-theme-dark", "Tmavý", dbc_state::ThemeMode::Dark, mode, cx))
            .child(radio("settings-theme-light", "Světlý", dbc_state::ThemeMode::Light, mode, cx))
            .child(
                div()
                    .id("settings-close")
                    .mt_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(cx.theme().bg_hover)
                    .cursor_pointer()
                    .child("Zavřít")
                    .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
            )
            .into_any_element()
    }

    pub(crate) fn open_connection_dialog(
        &mut self,
        editing: Option<ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Review MINOR B fix: single-modal invariant, same guard every
        // other dialog opener in this file already applies (see
        // `on_monitor_view_event`'s `KillRequested` arm) — the G11 T6
        // teardown-path accounting for `ModalState::BackupRestore`
        // specifically depends on this invariant actually holding
        // everywhere, not just at ITS OWN two opener call sites.
        if self.modal.is_some() {
            return;
        }
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
        let mssql_driver = cx.new(|cx| TextField::new(cx, "ODBC Driver 18 for SQL Server", false));

        let (editing_id, engine, read_only, favourite, ssh_enabled, mssql_encrypt, mssql_trust_cert) = if let Some(c) = &editing {
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
            let mssql_opts = c.mssql.clone().unwrap_or_default();
            if let Some(driver) = &mssql_opts.driver {
                mssql_driver.update(cx, |f, cx| f.set_text(driver, cx));
            }
            (
                Some(c.id.clone()),
                c.engine,
                c.read_only,
                c.favourite,
                ssh_enabled,
                mssql_opts.encrypt,
                mssql_opts.trust_server_certificate,
            )
        } else {
            let defaults = MssqlOptions::default();
            (None, Engine::Postgres, false, false, false, defaults.encrypt, defaults.trust_server_certificate)
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
            mssql_driver,
            engine,
            read_only,
            favourite,
            ssh_enabled,
            mssql_encrypt,
            mssql_trust_cert,
            test_result: None,
            testing: false,
        };
        self.modal = Some(ModalState::ConnectionDialog(ui));
        self.dropdown_open = false;
        window.focus(&name_focus, cx);
        cx.notify();
    }

    pub(crate) fn close_modal(&mut self, cx: &mut Context<Self>) {
        // G11 T6 binding carry-forward: backstop for every path that closes
        // the modal — cancels a still-`Running` backup/restore's handle
        // before it can be abandoned. See `cancel_active_backup_if_running`'s
        // doc comment (main.rs) for the full teardown-path accounting.
        self.cancel_active_backup_if_running();
        self.modal = None;
        cx.notify();
    }

    /// G14 T10: topbar gear entry point. Same single-modal invariant every
    /// other opener in this file applies (see `open_connection_dialog`'s
    /// identical guard) — a no-op while any other modal is already open.
    pub(crate) fn open_settings(&mut self, cx: &mut Context<Self>) {
        if self.modal.is_some() {
            return;
        }
        self.modal = Some(ModalState::Settings);
        cx.notify();
    }

    /// G7 T6: opens the connection-pair picker. Reuses `self.grouped_cache`
    /// (the SAME folder/favourite grouping the top-bar dropdown shows) —
    /// refreshed here rather than trusting whatever it last held, since the
    /// dialog can be opened via the palette without the dropdown ever having
    /// been opened this session.
    pub(crate) fn open_compare_dialog(&mut self, cx: &mut Context<Self>) {
        // Review MINOR B fix: single-modal invariant — see
        // `open_connection_dialog`'s identical guard for why this matters
        // beyond just this dialog's own correctness.
        if self.modal.is_some() {
            return;
        }
        self.refresh_grouped_cache();
        self.modal = Some(ModalState::CompareDialog { conn_a: None, conn_b: None, error: None });
        cx.notify();
    }

    /// A picker-row click on side `side` — updates `conn_a`/`conn_b` on the
    /// open `CompareDialog`, a no-op if some other modal is open by the time
    /// this fires (defensive; the dialog's own overlay occludes clicks
    /// elsewhere while open).
    pub(crate) fn select_compare_side(&mut self, side: CompareSide, id: String, cx: &mut Context<Self>) {
        if let Some(ModalState::CompareDialog { conn_a, conn_b, .. }) = &mut self.modal {
            match side {
                CompareSide::A => *conn_a = Some(id),
                CompareSide::B => *conn_b = Some(id),
            }
        }
        cx.notify();
    }

    /// G14 T11: the picker's kind toggle ("Sloupcový"/"Čárový") — same
    /// in-place-mutate-open-modal idiom as `select_compare_side`.
    pub(crate) fn set_chart_kind(&mut self, kind: ChartKind, cx: &mut Context<Self>) {
        if let Some(ModalState::ChartPicker { kind: k, .. }) = &mut self.modal {
            *k = kind;
        }
        cx.notify();
    }

    /// G14 T11: the picker's X-column radio.
    pub(crate) fn set_chart_x_col(&mut self, col: usize, cx: &mut Context<Self>) {
        if let Some(ModalState::ChartPicker { x_col, .. }) = &mut self.modal {
            *x_col = col;
        }
        cx.notify();
    }

    /// G14 T11: a Y-column checkbox toggle (numeric columns only — the
    /// panel only ever renders a checkbox for a numeric `col`).
    pub(crate) fn toggle_chart_y_col(&mut self, col: usize, cx: &mut Context<Self>) {
        if let Some(ModalState::ChartPicker { y_selected, .. }) = &mut self.modal {
            if let Some(flag) = y_selected.get_mut(col) {
                *flag = !*flag;
            }
        }
        cx.notify();
    }

    /// "Spustit porovnání" — resolves both picked `ConnectionConfig`s +
    /// their vault secrets (EXACT `run_query_with`'s
    /// `self.vault.as_ref().and_then(|v| v.get_secret(&cfg.id))` pattern, no
    /// new vault API/unlock step), closes the dialog immediately (design §3:
    /// "the modal itself closes as soon as the request is dispatched"), and
    /// dispatches `QueryRunner::fetch_schema_pair` — fire-and-forget with a
    /// generation guard, mirroring `AppView::trigger_schema_fetch`'s exact
    /// shape. `on_compare_schema_pair_ready` (T7 fills in the real body)
    /// picks the result up and opens the Compare tab.
    pub(crate) fn confirm_compare_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(ModalState::CompareDialog { conn_a, conn_b, .. }) = self.modal.clone() else {
            return;
        };
        let (Some(id_a), Some(id_b)) = (conn_a, conn_b) else { return };
        let Some(cfg_a) = self.config.connections.iter().find(|c| c.id == id_a).cloned() else {
            return;
        };
        let Some(cfg_b) = self.config.connections.iter().find(|c| c.id == id_b).cloned() else {
            return;
        };
        let secret_a = self.vault.as_ref().and_then(|v| v.get_secret(&cfg_a.id));
        let secret_b = self.vault.as_ref().and_then(|v| v.get_secret(&cfg_b.id));

        self.modal = None; // design §3: closes as soon as the request is dispatched
        self.compare_fetch_generation += 1;
        let my_generation = self.compare_fetch_generation;
        let label_a = format!("{} ({})", cfg_a.name, engine_label(cfg_a.engine));
        let label_b = format!("{} ({})", cfg_b.name, engine_label(cfg_b.engine));
        let spec_a = ConnectSpec::Config { cfg: Box::new(cfg_a.clone()), secret: secret_a.clone() };
        let spec_b = ConnectSpec::Config { cfg: Box::new(cfg_b.clone()), secret: secret_b.clone() };
        let rx = self.runner.fetch_schema_pair(spec_a, spec_b);

        // design §3: the Compare tab opens IMMEDIATELY (`CompareLoadState::
        // Loading`, "Načítám schéma…") — `on_compare_schema_pair_ready`
        // (main.rs) updates this SAME entity in place once the fetch
        // resolves, rather than a second entity/tab being created then.
        let view = cx.new(|_| crate::compare::CompareView {
            label_a: label_a.clone(),
            label_b: label_b.clone(),
            conn_a: cfg_a,
            secret_a,
            conn_b: cfg_b,
            secret_b,
            state: crate::compare::CompareLoadState::Loading,
            selection: crate::compare::CompareSelection::None,
            show_unchanged: crate::compare::ShowUnchanged::default(),
            show_ddl_diff: false,
            data_where: String::new(),
            data_diff: crate::compare::DataDiffState::Idle,
            data_diff_generation: 0,
        });
        cx.subscribe(&view, AppView::on_compare_view_event).detach();
        self.tabs.open(crate::tabs::ResultTab {
            id: 0,
            title: crate::tabs::collapse_title(&format!("Porovnání: {label_a} ↔ {label_b}")),
            pinned: false,
            preview_key: None,
            conn_identity: self.current_conn_identity(),
            content: crate::tabs::TabContent::Compare { view: view.clone() },
        });
        let pending = crate::PendingCompare { view, generation: my_generation };

        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                if view.compare_fetch_generation != pending.generation {
                    return;
                }
                view.on_compare_schema_pair_ready(pending, result, cx);
            });
        })
        .detach();
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
    fn toggle_mssql_encrypt(&mut self, cx: &mut Context<Self>) {
        if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
            ui.mssql_encrypt = !ui.mssql_encrypt;
        }
        cx.notify();
    }
    fn toggle_mssql_trust(&mut self, cx: &mut Context<Self>) {
        if let Some(ModalState::ConnectionDialog(ui)) = &mut self.modal {
            ui.mssql_trust_cert = !ui.mssql_trust_cert;
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
    ///
    /// Security follow-up #6 (final-review.md): if the password field is
    /// empty (relying on the stored secret) and the vault is locked, this
    /// mirrors `on_dropdown_item_click`'s gate and prompts for the master
    /// password FIRST — same as a normal connect — instead of silently
    /// testing without the secret and surfacing a confusing auth failure.
    fn on_test_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ModalState::ConnectionDialog(ui)) = &self.modal else { return };
        if ui.testing {
            return;
        }
        let ui_snapshot = ui.clone();
        let data = ui_snapshot.to_form_data(cx);

        if test_needs_vault_prompt(
            data.password.is_empty(),
            data.engine,
            self.vault.is_some(),
            Vault::exists(&self.vault_path),
        ) {
            let input = cx.new(|cx| TextField::new(cx, "Heslo", true));
            let focus = input.focus_handle(cx);
            self.modal = Some(ModalState::MasterPasswordPrompt {
                input,
                error: None,
                pending: PendingAfterUnlock::TestConnection(Box::new(ui_snapshot)),
            });
            window.focus(&focus, cx);
            cx.notify();
            return;
        }

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

    /// final-review must-fix #2's corrupt-config guard, extracted out of
    /// `finish_save` (G3 Task 4) so every config-mutating save path — the
    /// connection dialog's save, a tree object's ★ toggle
    /// (`AppView::on_tree_event`'s `TreeEvent::ToggleFavourite` arm in
    /// main.rs), and a dropdown connection's ★ toggle
    /// (`toggle_connection_favourite` below) — shares the exact same
    /// backup-before-overwrite behaviour: never silently overwrite a
    /// config.toml that failed to parse at startup. Moves it aside to
    /// `config.toml.corrupt-bak` first; if that fails (permissions, file
    /// vanished, etc.), the whole save is aborted (`false`) rather than
    /// risk clobbering data the user may still want to recover by hand. A
    /// no-op returning `true` once there's no `config_load_error` left to
    /// guard against, so callers can call it unconditionally before every
    /// save.
    pub(crate) fn guard_corrupt_config(&mut self, cx: &mut Context<Self>) -> bool {
        if self.config_load_error.is_some() {
            let backup = self.config_path.with_extension("toml.corrupt-bak");
            match std::fs::rename(&self.config_path, &backup) {
                Ok(()) => self.config_load_error = None,
                Err(e) => {
                    self.status = format!(
                        "error: nelze zálohovat poškozený config.toml ({e}) – uložení zrušeno"
                    );
                    cx.notify();
                    return false;
                }
            }
        }
        true
    }

    fn finish_save(&mut self, data: ConnectionFormData, cx: &mut Context<Self>) {
        if !self.guard_corrupt_config(cx) {
            return;
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

    /// G3 Task 4: the dropdown row's ★ toggle (mirrors the tree's object ★
    /// toggle — see `main.rs`'s `on_tree_event` `ToggleFavourite` arm) —
    /// flips `ConnectionConfig::favourite`, saves through the same guarded
    /// path (`guard_corrupt_config`), and refreshes `grouped_cache` so the
    /// dropdown's favourites-first ordering (G1) picks up the change
    /// immediately rather than waiting for the next dropdown-open.
    pub(crate) fn toggle_connection_favourite(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.guard_corrupt_config(cx) {
            return;
        }
        let Some(c) = self.config.connections.iter_mut().find(|c| c.id == id) else {
            return; // connection vanished meanwhile — nothing to toggle/save
        };
        c.favourite = !c.favourite;
        self.status = match self.config.save(&self.config_path) {
            Ok(()) => "Uloženo".to_string(),
            Err(e) => format!("error saving config: {}", e.message),
        };
        self.refresh_grouped_cache();
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
    /// `pub(crate)` (rather than private) so the command palette's
    /// `Connection` item (G3 Task 5, main.rs) can route through this exact
    /// switch path — brief contract #4: "no new execution logic".
    pub(crate) fn switch_to_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        // G11 T6 binding carry-forward: defensive — see
        // `cancel_active_backup_if_running`'s doc comment (main.rs) for why
        // this path isn't reachable while a backup/restore modal is open
        // today, and why the call stays here anyway.
        self.cancel_active_backup_if_running();
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
                                view.active_connection_id = Some(target_id.clone());
                                view.conn_url = None;
                                // G6 T7 review round 3, MAJOR 1: close any
                                // open autocomplete popup RIGHT HERE, at the
                                // moment the active connection identity
                                // itself changes — don't wait for the
                                // (async) schema fetch below to land.
                                // `trigger_schema_fetch`'s own success arm
                                // closes it again once the NEW schema
                                // actually arrives, covering the window in
                                // between (and same-connection refreshes,
                                // which don't go through this switch path
                                // at all).
                                view.close_autocomplete(cx);
                                // G2 Task 6: re-fetch the schema tree for the
                                // newly active connection. Rebuilt from
                                // `view.config`/`view.vault` rather than
                                // reusing the (already-consumed) `spec` this
                                // test_connect dispatched with.
                                if let Some(spec) = view.active_conn_spec() {
                                    view.trigger_schema_fetch(spec, cx);
                                }
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

    fn resume_pending(&mut self, pending: PendingAfterUnlock, window: &mut Window, cx: &mut Context<Self>) {
        match pending {
            PendingAfterUnlock::Connect(id) => self.switch_to_connection(&id, cx),
            PendingAfterUnlock::SaveConnection(data) => self.finish_save(*data, cx),
            PendingAfterUnlock::TestConnection(ui) => {
                self.modal = Some(ModalState::ConnectionDialog(*ui));
                self.on_test_clicked(window, cx);
            }
        }
    }

    /// td-security fix round, MINOR M3: the master-password prompt's
    /// "Zrušit" used to call the generic `close_modal` unconditionally,
    /// which set `modal = None` no matter what was pending — for
    /// `PendingAfterUnlock::TestConnection` that silently threw away the
    /// connection dialog the user was typing into (host/port/user/password
    /// fields all vanish). Fixed by restoring the dialog from the pending's
    /// `ConnectionDialogUi` instead: it carries LIVE `Entity<TextField>`
    /// handles (not a frozen snapshot — see `PendingAfterUnlock::
    /// TestConnection`'s doc comment), so reopening it picks up exactly
    /// whatever's still typed in those fields, untouched by the detour
    /// through this prompt.
    ///
    /// `PendingAfterUnlock::SaveConnection` is deliberately NOT given the
    /// same treatment: it carries a plain-data `ConnectionFormData`
    /// snapshot, not live field entities, so "restoring" it would mean
    /// reconstructing a whole new `ConnectionDialogUi` (fresh `TextField`
    /// entities re-seeded from the snapshot) rather than a same-shaped
    /// swap — a bigger change than this pass covers. Cancelling a save's
    /// vault prompt still closes the dialog outright, as before.
    pub(crate) fn cancel_master_password_prompt(&mut self, cx: &mut Context<Self>) {
        self.cancel_active_backup_if_running();
        match self.modal.take() {
            Some(ModalState::MasterPasswordPrompt { pending: PendingAfterUnlock::TestConnection(ui), .. }) => {
                self.modal = Some(ModalState::ConnectionDialog(*ui));
            }
            _ => self.modal = None,
        }
        cx.notify();
    }

    fn on_master_password_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ModalState::MasterPasswordPrompt { input, pending, .. }) = self.modal.clone() else { return };
        let pwd = input.read(cx).text();
        match Vault::unlock(&self.vault_path, &pwd) {
            Ok(vault) => {
                self.vault = Some(vault);
                self.modal = None;
                self.resume_pending(pending, window, cx);
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
                self.resume_pending(pending, window, cx);
            }
            Err(e) => self.set_create_master_error(&e.message, cx),
        }
    }

    /// G6 Task 3: the QueryParams dialog's per-row "NULL" checkbox — same
    /// small-toggle shape as `toggle_read_only`/`toggle_favourite`/
    /// `toggle_ssh_enabled` above. Does not clear the row's `TextField`
    /// text (so unchecking NULL restores whatever was typed); the checked
    /// state alone decides NULL-vs-text at substitution time
    /// (`build_param_sql` in main.rs).
    fn toggle_query_param_null(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(ModalState::QueryParams { null_flags, .. }) = &mut self.modal {
            if let Some(flag) = null_flags.get_mut(ix) {
                *flag = !*flag;
            }
        }
        cx.notify();
    }

    /// G9 T5: "Zrušit" on the kill-confirm dialog — closes it, nothing was
    /// sent.
    pub(crate) fn cancel_kill_confirm(&mut self, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(ModalState::KillConfirm { .. })) {
            self.modal = None;
            cx.notify();
        }
    }

    /// G9 T5: "Ukončit proces" — dispatches `MonitorCmd::Kill` via the
    /// tab's `MonitorView`; the dialog STAYS OPEN until
    /// `MonitorViewEvent::KillFinished` resolves (T6's on_monitor_view_event
    /// closes it on Ok / fills `error` on Err).
    ///
    /// MINOR review fix: a double-click used to `try_send` two `Kill`
    /// commands for the same pid — `kill_confirm_dispatch_target` (below)
    /// returns `None` once `dispatched` is already `true`, making a second
    /// call a no-op.
    pub(crate) fn confirm_kill_confirm(&mut self, cx: &mut Context<Self>) {
        let Some((pid, tab_id)) = kill_confirm_dispatch_target(&self.modal) else {
            return;
        };
        let Some(view) = self.monitor_view_for_tab(tab_id) else {
            // Tab closed under the dialog — nothing to kill against.
            self.modal = None;
            self.status = "monitor tab už není otevřený — ukončení zrušeno".into();
            cx.notify();
            return;
        };
        // Mark in-flight BEFORE dispatching so a rapid double-click can't
        // slip a second Kill through between this check and the send.
        if let Some(ModalState::KillConfirm { dispatched, .. }) = &mut self.modal {
            *dispatched = true;
        }
        view.update(cx, |m, cx| m.dispatch_kill(pid, cx));
        // Deliberately no self.modal = None here: success/failure arrives
        // as MonitorViewEvent::KillFinished (T6), which closes the dialog
        // on Ok or writes `error` on Err — the failure-stays-in-dialog UX
        // (design §6).
        cx.notify();
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

/// Builds the `ConnectSpec` for a Test/switch validation. Every engine —
/// including MSSQL since G15 T3 — goes through the runner to
/// `connect::open_config` → `probe()`/handshake the same way; there is no
/// more client-side short-circuit for MSSQL (it used to hard-refuse here
/// before the driver was wired in).
///
/// Used by both `on_test_clicked` and `switch_to_connection` — Task 8's
/// review found the synchronous `pending_connect` this replaces froze the
/// whole window on an unreachable/firewalled host (no bound on the
/// UI-thread `block_on` call); both call sites now dispatch the returned
/// spec through `QueryRunner::test_connect`, which runs entirely off the UI
/// thread and is bounded by `connect::open_config`'s `connect_timeout`.
fn test_connect_spec(cfg: ConnectionConfig, secret: Option<String>) -> Result<ConnectSpec, String> {
    Ok(ConnectSpec::Config { cfg: Box::new(cfg), secret })
}

/// Whether `on_test_clicked` must prompt for the master password before
/// dispatching the test connect — security follow-up #6 (final-review.md):
/// editing a saved connection with an empty password field while the vault
/// is locked used to test WITHOUT the stored secret (a confusing auth
/// failure) instead of prompting like a normal connect. Mirrors
/// `on_dropdown_item_click`'s gate: only relevant when the dialog's
/// password field is empty (i.e. the secret, if any, would have to come
/// from the vault) AND the engine actually needs one AND the vault is
/// currently locked but exists on disk.
fn test_needs_vault_prompt(
    password_field_empty: bool,
    engine: Engine,
    vault_unlocked: bool,
    vault_file_exists: bool,
) -> bool {
    password_field_empty && engine != Engine::Sqlite && !vault_unlocked && vault_file_exists
}

#[cfg(test)]
mod test_vault_prompt_tests {
    use super::*;

    #[test]
    fn empty_password_locked_vault_needs_prompt() {
        assert!(test_needs_vault_prompt(true, Engine::Postgres, false, true));
    }

    #[test]
    fn typed_password_never_needs_prompt() {
        // password field non-empty -> test uses the typed value, vault is irrelevant.
        assert!(!test_needs_vault_prompt(false, Engine::Postgres, false, true));
    }

    #[test]
    fn sqlite_never_needs_prompt() {
        assert!(!test_needs_vault_prompt(true, Engine::Sqlite, false, true));
    }

    #[test]
    fn unlocked_vault_never_needs_prompt() {
        assert!(!test_needs_vault_prompt(true, Engine::Postgres, true, true));
    }

    #[test]
    fn no_vault_file_never_needs_prompt() {
        // no vault created yet -> nothing to unlock, secret is simply absent.
        assert!(!test_needs_vault_prompt(true, Engine::Postgres, false, false));
    }
}

fn field_row(label: &str, field: Entity<TextField>, theme: Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(div().w(px(130.)).text_color(theme.text_muted).child(label.to_string()))
        .child(div().flex_1().child(field))
}

fn styled_button(id: &'static str, label: &'static str, theme: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .bg(theme.bg_hover)
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(theme.bg_selected))
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
    let star_id = c.id.clone();
    let editing = c.clone();
    let backup_target = c.id.clone();
    let restore_target = c.id.clone();
    let restore_read_only = c.read_only;
    let label = format!("{}{} — {} {}", "  ".repeat(depth), c.name, engine_label(c.engine), c.host);
    let (star_glyph, star_color) =
        if c.favourite { ("★", cx.theme().warn) } else { ("☆", cx.theme().text_disabled) };
    div()
        .id(SharedString::from(format!("dropdown-item-row-{}", c.id)))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .hover(|s| s.bg(cx.theme().bg_hover))
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
            // G3 Task 4: ★ toggle, mirroring the ✎ edit affordance's
            // `cx.stop_propagation()` pattern below — without it, this click
            // would also bubble to the row's connect handler above.
            div()
                .id(SharedString::from(format!("dropdown-item-star-{}", c.id)))
                .px_1()
                .cursor_pointer()
                .text_color(star_color)
                .hover(|s| s.bg(cx.theme().bg_selected))
                .child(star_glyph)
                .on_click(cx.listener(move |view, _, _window, cx| {
                    cx.stop_propagation();
                    view.toggle_connection_favourite(&star_id, cx);
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
                .text_color(cx.theme().text_muted)
                .hover(|s| s.bg(cx.theme().bg_selected))
                .child("✎")
                .on_click(cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_connection_dialog(Some(editing.clone()), window, cx);
                })),
        )
        .child(
            // G11 T6: backup affordance — allowed on every connection
            // (backup is the one documented read-only exemption, design
            // CURATION item 2), same `cx.stop_propagation()` pattern as ★/✎
            // above so this click doesn't also bubble to the row's connect
            // handler.
            div()
                .id(SharedString::from(format!("dropdown-item-backup-{}", c.id)))
                .px_1()
                .cursor_pointer()
                .text_color(cx.theme().text_muted)
                .hover(|s| s.bg(cx.theme().bg_selected))
                .child("🗄")
                .on_click(cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_backup_dialog(backup_target.clone(), window, cx);
                })),
        )
        .child(
            // G11 T6: restore affordance — dimmed (still clickable; the
            // click itself surfaces the read-only refusal as a status line,
            // same "no tooltip component exists in this codebase" posture
            // this plan's Grounding documents) for a read-only connection.
            // Restore is NEVER exempt from the read-only gate (design
            // CURATION item 2) — `open_restore_dialog` enforces this for
            // real; the dim here is a visual hint only, not the guard.
            div()
                .id(SharedString::from(format!("dropdown-item-restore-{}", c.id)))
                .px_1()
                .cursor_pointer()
                .text_color(if restore_read_only { cx.theme().text_disabled } else { cx.theme().text_muted })
                .hover(|s| s.bg(cx.theme().bg_selected))
                .child("♻")
                .on_click(cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_restore_dialog(restore_target.clone(), window, cx);
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
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child(title))
        .child(field_row("Název", ui.name.clone(), *cx.theme()))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(130.)).text_color(cx.theme().text_muted).child("Engine"))
                .child(
                    div()
                        .id("engine-cycle")
                        .px_2()
                        .py_1()
                        .bg(cx.theme().bg_hover)
                        .rounded_md()
                        .cursor_pointer()
                        .child(engine_label(ui.engine))
                        .on_click(cx.listener(|view, _, _, cx| view.cycle_engine(cx))),
                ),
        )
        .child(field_row("Host", ui.host.clone(), *cx.theme()))
        .child(field_row("Port", ui.port.clone(), *cx.theme()))
        .child(field_row("Databáze", ui.database.clone(), *cx.theme()))
        .child(field_row("Uživatel", ui.user.clone(), *cx.theme()))
        .child(field_row("Heslo", ui.password.clone(), *cx.theme()))
        .child(field_row("Složka", ui.folder.clone(), *cx.theme()))
        .child(field_row("Timeout (s)", ui.timeout_secs.clone(), *cx.theme()))
        .child(field_row("Auto-limit řádků", ui.auto_limit.clone(), *cx.theme()))
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
            .child(field_row("SSH host", ui.ssh_host.clone(), *cx.theme()))
            .child(field_row("SSH port", ui.ssh_port.clone(), *cx.theme()))
            .child(field_row("SSH uživatel", ui.ssh_user.clone(), *cx.theme()))
            .child(field_row("SSH klíč (cesta)", ui.ssh_key_path.clone(), *cx.theme()));
    }

    if ui.engine == Engine::Mssql {
        panel = panel
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .child(
                        checkbox("chk-mssql-encrypt", "Šifrovat připojení (Encrypt)", ui.mssql_encrypt)
                            .on_click(cx.listener(|v, _, _, cx| v.toggle_mssql_encrypt(cx))),
                    )
                    .child(
                        checkbox(
                            "chk-mssql-trust",
                            "Důvěřovat certifikátu serveru (TrustServerCertificate)",
                            ui.mssql_trust_cert,
                        )
                        .on_click(cx.listener(|v, _, _, cx| v.toggle_mssql_trust(cx))),
                    ),
            )
            .child(field_row("ODBC driver (volitelné)", ui.mssql_driver.clone(), *cx.theme()))
            // Read-only honesty, UI half (§1a): rendered only for MSSQL.
            .child(
                div()
                    .text_color(cx.theme().text_muted)
                    .child("Pouze pro čtení: u MSSQL vynuceno pouze na straně klienta"),
            );
    }

    if let Some((text, ok)) = test_line {
        let color = match ok {
            Some(true) => cx.theme().success,
            Some(false) => cx.theme().danger,
            None => cx.theme().text_muted,
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
            .child(styled_button("dlg-test", test_label, *cx.theme()).on_click(cx.listener(|v, _, window, cx| v.on_test_clicked(window, cx))))
            .child(styled_button("dlg-save", "Uložit", *cx.theme()).on_click(cx.listener(|v, _, window, cx| v.on_save_clicked(window, cx))))
            .child(styled_button("dlg-cancel", "Zrušit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| v.close_modal(cx)))),
    );

    panel.into_any_element()
}

fn render_master_password_panel(input: Entity<TextField>, error: Option<String>, cx: &mut Context<AppView>) -> AnyElement {
    let mut panel: Div = div()
        .w(px(360.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Master heslo"))
        .child(field_row("Heslo", input, *cx.theme()));
    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(e));
    }
    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("mpp-cancel", "Zrušit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| v.cancel_master_password_prompt(cx))))
            .child(styled_button("mpp-submit", "Odemknout", *cx.theme()).on_click(cx.listener(|v, _, window, cx| v.on_master_password_submit(window, cx)))),
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
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Vytvořit master heslo"))
        .child(field_row("Nové heslo", input1, *cx.theme()))
        .child(field_row("Zopakujte heslo", input2, *cx.theme()));
    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(e));
    }
    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("cmp-cancel", "Zrušit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| v.close_modal(cx))))
            .child(styled_button("cmp-submit", "Vytvořit", *cx.theme()).on_click(cx.listener(|v, _, window, cx| v.on_create_master_password_submit(window, cx)))),
    );
    panel.into_any_element()
}

/// G6 Task 3: one row per distinct `:name` (label + `TextField` + a "NULL"
/// checkbox, same visual idiom as `grid.rs`'s cell editor's Uložit/NULL/
/// Zrušit), a live substituted-SQL preview line (recomputed read-only from
/// the inputs' CURRENT text on every render — cheap at interactive SQL
/// sizes, same posture the design doc calls for; it does not gate typing),
/// and an error line when `confirm_query_params` (main.rs) has set one.
/// `build_param_sql` (main.rs, crate-private but visible here — this
/// module is a child of the crate root) is the single source of truth for
/// both the preview and the actual Spustit dispatch, so the preview can
/// never show a different result than what running would actually do.
fn render_query_params_panel(
    names: Vec<String>,
    inputs: Vec<Entity<TextField>>,
    null_flags: Vec<bool>,
    sql_template: String,
    error: Option<String>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let values: Vec<(String, bool)> = inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            let text = input.read(cx).text();
            let is_null = null_flags.get(i).copied().unwrap_or(false);
            (text, is_null)
        })
        .collect();
    let preview = crate::build_param_sql(&sql_template, &names, &values);

    let mut panel: Div = div()
        .w(px(480.))
        .max_h(px(520.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Hodnoty parametrů"));

    for (i, name) in names.iter().enumerate() {
        let input = inputs[i].clone();
        let is_null = null_flags.get(i).copied().unwrap_or(false);
        let mark = if is_null { "☑" } else { "☐" };
        panel = panel.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(110.)).text_color(cx.theme().text_muted).child(format!(":{name}")))
                .child(div().flex_1().child(input))
                .child(
                    div()
                        .id(("qp-null", i))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .child(format!("{mark} NULL"))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.toggle_query_param_null(i, cx);
                        })),
                ),
        );
    }

    panel = panel.child(
        div()
            .id("qp-preview")
            .p_1()
            .bg(cx.theme().bg_app)
            .rounded_md()
            .text_color(cx.theme().text_muted)
            .whitespace_normal()
            .child(match &preview {
                Ok(sql) => sql.clone(),
                Err(e) => format!("náhled: {e}"),
            }),
    );

    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(e));
    }

    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("qp-cancel", "Zrušit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| v.cancel_query_params(cx))))
            .child(styled_button("qp-run", "Spustit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| v.confirm_query_params(cx)))),
    );
    panel.into_any_element()
}

/// Pure guard for `AppView::confirm_kill_confirm` (MINOR review fix):
/// `Some((pid, tab_id))` when an OPEN `KillConfirm` dialog has not yet
/// dispatched its kill; `None` when there's no `KillConfirm` open at all, OR
/// one is open but `dispatched` is already `true` (a second click while the
/// first kill is still in flight must be a no-op — nothing else stopped a
/// double-click from `try_send`-ing two `Kill` commands for the same pid).
/// Factored out as a free function, same "pure logic, thin cx-touching
/// wrapper" split `monitor_view::MonitorView::apply_event` uses, so it's
/// unit-testable without a GPUI entity/`Context`.
fn kill_confirm_dispatch_target(modal: &Option<ModalState>) -> Option<(i64, u64)> {
    match modal {
        Some(ModalState::KillConfirm { pid, tab_id, dispatched: false, .. }) => Some((*pid, *tab_id)),
        _ => None,
    }
}

/// Pure guard for `AppView::on_confirm_analyze_write` (MAJOR review fix on
/// commit 0bab655): `Some(sql)` when an OPEN `AnalyzeWriteConfirm` dialog
/// isn't already running; `None` when there's no `AnalyzeWriteConfirm` open
/// at all, OR one is open but `running` is already `true`. This is what
/// makes the busy-guard for the analyze-write sequence structural rather
/// than the earlier (buggy) `self.cancel`-token approach: `self.modal`
/// stays `Some(AnalyzeWriteConfirm { running: true, .. })` — mutated in
/// place, never cleared — for the whole duration of the analyze, so (a) a
/// second "Analyzovat" click routes through this SAME guard and is a
/// no-op, exactly like `kill_confirm_dispatch_target` above, and (b) Esc
/// can't defeat it either — `on_cancel_query`'s `closable` match only
/// allow-lists `ConnectionDialog`/`QueryParams`, so `AnalyzeWriteConfirm`
/// (like `KillConfirm`) falls into its `_ => false` arm and Esc is
/// structurally inert against it, whether or not the analyze is running.
/// Same "pure logic, thin cx-touching wrapper" testability rationale as
/// `kill_confirm_dispatch_target`.
pub(crate) fn analyze_write_dispatch_sql(modal: &Option<ModalState>) -> Option<String> {
    match modal {
        Some(ModalState::AnalyzeWriteConfirm { sql, running: false, .. }) => Some(sql.clone()),
        _ => None,
    }
}

/// Pure guard for `AppView::on_monitor_view_event` (MAJOR review fix): does
/// the resolving `KillFinished` event — from tab `event_tab_id`, for
/// `event_pid` — belong to the CURRENTLY open `KillConfirm` dialog? A
/// `false` here means some OTHER kill's outcome arrived (a different pid on
/// the same tab, or the same/different pid on a different monitor tab, or
/// the dialog was cancelled/closed already) — the caller must not mutate
/// `self.modal` in that case, or one kill's result gets misattributed into
/// an unrelated open dialog (wrong error text, or silently closing a dialog
/// nobody confirmed). Same testability rationale as
/// `kill_confirm_dispatch_target` above. `pub(crate)`: called from
/// `main.rs`'s `on_monitor_view_event`.
pub(crate) fn kill_confirm_matches(modal: &Option<ModalState>, event_tab_id: u64, event_pid: i64) -> bool {
    matches!(
        modal,
        Some(ModalState::KillConfirm { pid, tab_id, .. })
            if *pid == event_pid && *tab_id == event_tab_id
    )
}

/// Applies a FAILED kill's outcome to `modal`, if it's still open for THIS
/// pid/tab_id (`kill_confirm_matches`) — writes `error` for display AND
/// resets `dispatched` back to `false`. No-op (leaves `modal` untouched)
/// when the event doesn't match the currently open dialog.
///
/// Review fix (NEW MINOR, follow-up to the MINOR double-dispatch guard):
/// leaving `dispatched` at `true` here permanently greyed "Ukončit proces"
/// out after the FIRST failed attempt — the Ok arm is the only other place
/// that ever clears it, and Ok closes the dialog outright instead, so nobody
/// ever reset it back to `false` for a genuine retry. `pub(crate)`: called
/// from `main.rs`'s `on_monitor_view_event`; factored out (same rationale as
/// `kill_confirm_matches`/`kill_confirm_dispatch_target` above) so the
/// retry-reset is unit-testable without a GPUI entity/`Context`.
pub(crate) fn apply_kill_error_to_modal(modal: &mut Option<ModalState>, tab_id: u64, pid: i64, msg: &str) {
    if !kill_confirm_matches(modal, tab_id, pid) {
        return;
    }
    if let Some(ModalState::KillConfirm { error, dispatched, .. }) = modal {
        *error = Some(msg.to_string());
        *dispatched = false;
    }
}

/// G9 T5: the kill-confirm dialog panel — same card tokens as every other
/// modal in this file. Shows the exact SQL that will run (design §6's "show
/// the exact generated SQL" principle) and, on a failed attempt, the error
/// text below it (dialog stays open). `dispatched` (MINOR review fix)
/// greys out "Ukončit proces" and drops its click handler once a kill is in
/// flight, so a second click can't fire a second `Kill`; "Zrušit" stays
/// active throughout — dismissing the dialog while a kill is in flight is
/// harmless (the eventual `KillFinished` just finds no matching dialog to
/// update, per `kill_confirm_matches`, and falls back to a status line).
fn render_kill_confirm_panel(
    pid: i64,
    label: &str,
    sql: &str,
    error: &Option<String>,
    dispatched: bool,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let mut panel: Div = div()
        .w(px(520.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Ukončit proces"))
        .child(format!("Opravdu ukončit proces {pid} ({label})?"))
        .child(
            div()
                .id("kill-sql-preview")
                .p_1()
                .bg(cx.theme().bg_app)
                .rounded_md()
                .text_color(cx.theme().text_muted)
                .whitespace_normal()
                .child(sql.to_string()),
        );

    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(format!("error: {e}")));
    }

    let confirm_button = if dispatched {
        div()
            .id("kill-confirm")
            .px_3()
            .py_1()
            .rounded_md()
            .bg(cx.theme().bg_hover)
            .text_color(cx.theme().text_disabled)
            .child("Ukončuji…")
            .into_any_element()
    } else {
        styled_button("kill-confirm", "Ukončit proces", *cx.theme())
            .bg(cx.theme().diff_deleted_bg) // danger tint — DELETED_ROW_BG family
            .on_click(cx.listener(|v, _, _, cx| v.confirm_kill_confirm(cx)))
            .into_any_element()
    };

    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(
                styled_button("kill-cancel", "Zrušit", *cx.theme())
                    .on_click(cx.listener(|v, _, _, cx| v.cancel_kill_confirm(cx))),
            )
            .child(confirm_button),
    );
    panel.into_any_element()
}

/// G13 T6 (design §5 case 3 / §3-novela): confirm dialog for "Analyzovat"
/// on a write over a writable connection. Shows the LITERAL SQL that will
/// actually run — same "show the exact generated SQL" principle as the
/// Apply dialog and the kill-confirm panel above — plus an explicit warning
/// that side effects OUTSIDE the wrapping transaction (sequence/IDENTITY
/// advances, external function calls) are not undone by the ROLLBACK.
/// Confirming dispatches `AppView::on_confirm_analyze_write` (main.rs),
/// which resolves the spec, rebuilds the wrapped `EXPLAIN ANALYZE` SQL, and
/// calls `QueryRunner::run_analyze_write`.
/// `running`/`error` mirror `render_apply_dialog_overlay`'s exact shape
/// (`AppView::render_apply_dialog_overlay`, main.rs): both buttons disabled
/// (no `cursor_pointer`/`on_click`) while `running`, an "analyzuji…" note
/// shown while in flight, and any `error` from a failed/cancelled run shown
/// beneath it — the dialog stays open on error so a retry ("Analyzovat"
/// again) or an explicit "Zrušit" are both still available, same as Apply.
fn render_analyze_write_confirm_panel(
    sql: &str,
    engine: Engine,
    running: bool,
    error: &Option<String>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let sql = sql.to_string();
    let mut panel = div()
        .id("analyze-write-confirm")
        .w(px(520.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Analyzovat (EXPLAIN ANALYZE)"))
        .child(div().text_color(cx.theme().warn).child(
            "Toto SQL bude SKUTEČNĚ PROVEDENO, aby bylo možné změřit skutečný plán, a poté vráceno \
             zpět (ROLLBACK). Vedlejší efekty MIMO transakci (např. hodnoty sekvencí/IDENTITY, \
             volání externích funkcí) NEBUDOU vráceny zpět.",
        ))
        .child(
            div()
                .id("analyze-write-sql-preview")
                .p_1()
                .bg(cx.theme().bg_app)
                .rounded_md()
                .text_color(cx.theme().text_muted)
                .whitespace_normal()
                .child(sql),
        );

    if running {
        panel = panel.child(div().text_color(cx.theme().warn).child("analyzuji (BEGIN…ROLLBACK)…"));
    }
    if let Some(err) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(format!("error: {err}")));
    }

    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(
                div()
                    .id("analyze-write-cancel")
                    .when(!running, |d| {
                        d.cursor_pointer().on_click(cx.listener(|v, _, _, cx| {
                            v.modal = None;
                            cx.notify();
                        }))
                    })
                    .bg(cx.theme().bg_hover)
                    .text_color(if running { cx.theme().text_disabled } else { cx.theme().text_primary })
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .child("Zrušit"),
            )
            .child(
                div()
                    .id("analyze-write-confirm-btn")
                    .when(!running, |d| {
                        d.cursor_pointer().on_click(cx.listener(move |v, _, window, cx| {
                            v.on_confirm_analyze_write(engine, window, cx);
                        }))
                    })
                    // danger tint — DELETED_ROW_BG family, matches kill-confirm — dimmed while running.
                    .bg(if running { cx.theme().bg_hover } else { cx.theme().diff_deleted_bg })
                    .text_color(if running { cx.theme().text_disabled } else { cx.theme().text_primary })
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .child(if running { "Analyzuji…" } else { "Analyzovat" }),
            ),
    );
    panel.into_any_element()
}

/// G7 T6: which column of the `CompareDialog` picker a row click targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareSide {
    A,
    B,
}

/// Render contract (design §3): heading, two labeled columns ("Databáze A" /
/// "Databáze B") each a list of `grouped`'s rows (folder/favourite sections
/// — the SAME grouping data the top-bar dropdown shows), a single-select
/// click handler per row, an `error` line (if any) below both columns, and
/// "Spustit porovnání" disabled until both `conn_a`/`conn_b` are `Some`
/// (same connection on both sides explicitly allowed — no equality guard).
fn render_compare_dialog_panel(
    conn_a: Option<String>,
    conn_b: Option<String>,
    error: Option<String>,
    grouped: GroupedConnections,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let both_picked = conn_a.is_some() && conn_b.is_some();
    let mut panel = div()
        .id("compare-dialog")
        .w(px(680.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child("Porovnat databáze…"))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_3()
                .child(render_compare_picker_column(
                    "compare-col-a",
                    "Databáze A",
                    CompareSide::A,
                    &conn_a,
                    &grouped,
                    cx,
                ))
                .child(render_compare_picker_column(
                    "compare-col-b",
                    "Databáze B",
                    CompareSide::B,
                    &conn_b,
                    &grouped,
                    cx,
                )),
        );

    if let Some(e) = &error {
        panel = panel.child(div().text_color(cx.theme().danger).child(format!("error: {e}")));
    }

    let confirm_button = if both_picked {
        styled_button("compare-confirm", "Spustit porovnání", *cx.theme())
            .on_click(cx.listener(|v, _, _, cx| v.confirm_compare_dialog(cx)))
            .into_any_element()
    } else {
        div()
            .id("compare-confirm")
            .px_3()
            .py_1()
            .rounded_md()
            .bg(cx.theme().bg_hover)
            .text_color(cx.theme().text_disabled)
            .child("Spustit porovnání")
            .into_any_element()
    };

    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("compare-cancel", "Zrušit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| {
                v.modal = None;
                cx.notify();
            })))
            .child(confirm_button),
    );
    panel.into_any_element()
}

fn render_compare_picker_column(
    id: &'static str,
    label: &'static str,
    side: CompareSide,
    selected: &Option<String>,
    grouped: &GroupedConnections,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    let mut list = div()
        .id(id)
        .flex()
        .flex_col()
        .flex_1()
        .gap_1()
        .p_1()
        .h(px(240.))
        .overflow_hidden()
        .border_1()
        .border_color(cx.theme().border_subtle)
        .rounded_md();

    if !grouped.favourites.is_empty() {
        list = list.child(div().text_color(cx.theme().warn).child("Oblíbené"));
        for c in &grouped.favourites {
            list = list.child(compare_picker_row(c, side, selected, cx));
        }
    }
    for folder in &grouped.folders {
        let header = if folder.path.is_empty() { "Bez složky".to_string() } else { folder.path.join("/") };
        list = list.child(div().text_color(cx.theme().text_disabled).child(header));
        for c in &folder.connections {
            list = list.child(compare_picker_row(c, side, selected, cx));
        }
    }

    div().flex().flex_col().flex_1().gap_1().child(div().text_color(cx.theme().accent).child(label)).child(list)
}

fn compare_picker_row(
    c: &ConnectionConfig,
    side: CompareSide,
    selected: &Option<String>,
    cx: &mut Context<AppView>,
) -> impl IntoElement {
    let id = c.id.clone();
    let is_selected = selected.as_deref() == Some(c.id.as_str());
    let side_tag = match side {
        CompareSide::A => "a",
        CompareSide::B => "b",
    };
    let label = format!("{} — {} {}", c.name, engine_label(c.engine), c.host);
    div()
        .id(SharedString::from(format!("compare-row-{side_tag}-{}", c.id)))
        .px_1()
        .cursor_pointer()
        .rounded_md()
        .when(is_selected, |d| d.bg(cx.theme().bg_hover).text_color(cx.theme().success))
        .hover(|s| s.bg(cx.theme().bg_hover))
        .child(label)
        .on_click(cx.listener(move |view, _, _, cx| {
            view.select_compare_side(side, id.clone(), cx);
        }))
}

/// G11 T6: backup/restore confirm/progress panel — one render per
/// `BackupSession::status`, mirroring `render_kill_confirm_panel`'s
/// running/error/terminal shape. §3-novela: `session.command_line` (the
/// redacted command/SQL text — never containing the raw secret, see
/// `backup::display_command_line`) is always shown so the user sees exactly
/// what will run/ran before and during dispatch.
fn render_backup_restore_panel(session: &crate::backup::BackupSession, cx: &mut Context<AppView>) -> AnyElement {
    use crate::backup::{BackupKind, BackupStatus};

    let title = match session.kind {
        BackupKind::Backup => "Zálohovat databázi",
        BackupKind::Restore => "Obnovit databázi ze zálohy",
    };
    let status = session.status.borrow().clone();

    let mut panel = div()
        .id("backup-restore-panel")
        .w(px(560.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child(title))
        .child(format!(
            "{} — {} ({})",
            session.connection_name,
            engine_label(session.engine),
            session.database
        ))
        .child(div().text_color(cx.theme().text_muted).child(session.target_path.clone()));

    if !session.command_line.is_empty() {
        panel = panel.child(
            div()
                .id("backup-restore-command-preview")
                .p_1()
                .bg(cx.theme().bg_app)
                .rounded_md()
                .text_color(cx.theme().text_muted)
                .whitespace_normal()
                .child(session.command_line.clone()),
        );
    }

    match &status {
        BackupStatus::Confirming => {
            // Restore only (design §3, GitHub-delete-repo pattern) — Backup
            // never reaches this state (`open_backup_dialog` starts it
            // straight in `Running`).
            if let Some(input) = &session.confirm_input {
                panel = panel
                    .child(div().text_color(cx.theme().warn).child(format!(
                        "Pro potvrzení napište přesný název databáze: {}",
                        session.expected_name
                    )))
                    .child(field_row("Název databáze", input.clone(), *cx.theme()));
            }
            let typed = session.confirm_input.as_ref().map(|f| f.read(cx).text()).unwrap_or_default();
            let allowed = crate::backup::confirm_matches(&typed, &session.expected_name);
            let confirm_button = if allowed {
                styled_button("backup-restore-confirm", "Obnovit", *cx.theme())
                    .bg(cx.theme().diff_deleted_bg)
                    .on_click(cx.listener(|v, _, _, cx| v.confirm_restore(cx)))
                    .into_any_element()
            } else {
                div()
                    .id("backup-restore-confirm")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(cx.theme().bg_hover)
                    .text_color(cx.theme().text_disabled)
                    .child("Obnovit")
                    .into_any_element()
            };
            panel = panel.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .justify_end()
                    .mt_2()
                    .child(
                        styled_button("backup-restore-cancel", "Zrušit", *cx.theme())
                            .on_click(cx.listener(|v, _, _, cx| v.cancel_backup_restore(cx))),
                    )
                    .child(confirm_button),
            );
        }
        BackupStatus::Running => {
            let elapsed = session.started_at.elapsed().as_secs();
            panel = panel.child(div().text_color(cx.theme().warn).child(format!("probíhá… ({elapsed} s)")));

            let log = session.log.borrow();
            let log_len = log.lines.len();
            // Review MINOR 2 fix: `session.log` retains at most
            // `backup::BACKUP_LOG_CAP` lines (evicted oldest-first at the
            // push site, `backup::push_backup_log`) — a `pg_dump -v` run
            // against a huge schema emits one line per object, and cloning
            // an unbounded Vec every render frame would be O(n²) cumulative
            // over the run's lifetime. `truncated` tells the user their
            // scrollback isn't the whole story.
            if log.truncated {
                panel = panel.child(
                    div()
                        .text_size(px(11.))
                        .text_color(cx.theme().text_disabled)
                        .child("… (starší řádky zahozeny)"),
                );
            }
            if log_len > 0 {
                let lines: Vec<String> = log.lines.iter().cloned().collect();
                let list = uniform_list(
                    "backup-restore-log",
                    log_len,
                    move |range: std::ops::Range<usize>, _window, _cx| {
                        range.map(|ix| div().text_size(px(11.)).child(lines[ix].clone())).collect::<Vec<_>>()
                    },
                )
                .h(px(160.));
                panel = panel.child(list);
            }
            drop(log);

            // Review MAJOR fix: MSSQL/SQLite have no real cancel hook
            // (`session.can_cancel() == false` — no OS child process, only
            // a `tokio` task driving `Connection::execute`/`fs::copy`) —
            // rendering a clickable "Zrušit" there would let the user
            // believe a click actually stops the in-flight write, when in
            // fact nothing would happen but the UI lying about it (see
            // `backup::should_cancel_on_teardown`'s doc comment for the
            // full consequence chain). Render it non-interactive instead,
            // same "dimmed div, no `.on_click`" pattern this file already
            // uses for a disabled Confirming-state "Obnovit".
            let cancel_button = if session.can_cancel() {
                styled_button("backup-restore-cancel-running", "Zrušit", *cx.theme())
                    .on_click(cx.listener(|v, _, _, cx| v.cancel_backup_restore(cx)))
                    .into_any_element()
            } else {
                div()
                    .id("backup-restore-cancel-running")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(cx.theme().bg_hover)
                    .text_color(cx.theme().text_disabled)
                    .child("nelze přerušit — čeká se na dokončení")
                    .into_any_element()
            };
            panel = panel.child(div().flex().flex_row().justify_end().mt_2().child(cancel_button));
        }
        BackupStatus::Succeeded | BackupStatus::Failed(_) | BackupStatus::Cancelled => {
            let (line, color) = match &status {
                BackupStatus::Succeeded => ("hotovo".to_string(), cx.theme().success),
                BackupStatus::Failed(e) => (format!("error: {e}"), cx.theme().danger),
                BackupStatus::Cancelled => ("přerušeno uživatelem".to_string(), cx.theme().warn),
                _ => unreachable!(),
            };
            panel = panel.child(div().text_color(color).child(line));
            panel = panel.child(
                div().flex().flex_row().justify_end().mt_2().child(
                    styled_button("backup-restore-close", "Zavřít", *cx.theme())
                        .on_click(cx.listener(|v, _, _, cx| v.close_modal(cx))),
                ),
            );
        }
    }

    panel.into_any_element()
}

#[cfg(test)]
mod compare_dialog_tests {
    use super::*;

    #[test]
    fn compare_dialog_starts_with_both_sides_unpicked() {
        let modal = ModalState::CompareDialog { conn_a: None, conn_b: None, error: None };
        assert!(matches!(modal, ModalState::CompareDialog { conn_a: None, conn_b: None, .. }));
    }

    #[test]
    fn confirm_is_a_noop_until_both_sides_are_picked() {
        // Pure precondition check mirrored from `confirm_compare_dialog`'s
        // early-return guard — proven directly on the enum shape rather than
        // through a full `AppView`/window harness, same precedent as
        // `Tabs`' own plain-data tests (tabs.rs's module doc comment).
        let one_picked = ModalState::CompareDialog { conn_a: Some("x".into()), conn_b: None, error: None };
        let (a, b) = match one_picked {
            ModalState::CompareDialog { conn_a, conn_b, .. } => (conn_a, conn_b),
            _ => unreachable!(),
        };
        assert!(!(a.is_some() && b.is_some()));
    }

    #[test]
    fn same_connection_on_both_sides_is_a_valid_pick() {
        // design §3: explicitly allowed, not a validation error.
        let both_same = ModalState::CompareDialog { conn_a: Some("x".into()), conn_b: Some("x".into()), error: None };
        let (a, b) = match both_same {
            ModalState::CompareDialog { conn_a, conn_b, .. } => (conn_a, conn_b),
            _ => unreachable!(),
        };
        assert!(a.is_some() && b.is_some());
    }
}

/// G12 T3: `ModalState::ScriptRun`'s confirm panel — file list (with the
/// pre-scanned per-file statement counts), the connection's name + a "jen
/// pro čtení" badge when read-only, the tx-scope/error-policy radios (a
/// click on a combination `script_options_valid` forbids is a no-op, dimmed
/// rather than hidden — `AppView::set_script_tx_scope`/
/// `set_script_error_policy` enforce the same rule server-side of the
/// click), the fixed per-statement timeout (read from config, not
/// editable), and "Spustit"/"Zrušit".
#[allow(clippy::too_many_arguments)]
fn render_script_run_confirm_panel(
    files: &[std::path::PathBuf],
    file_counts: &[usize],
    tx_scope: crate::runner::TxScope,
    error_policy: crate::runner::ErrorPolicy,
    source_label: &str,
    conn_label: &str,
    read_only: bool,
    timeout_secs: Option<u64>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    use crate::runner::{ErrorPolicy, TxScope};

    let total: usize = file_counts.iter().sum();
    let mut file_list = div().flex().flex_col().gap_1().max_h(px(160.)).overflow_hidden();
    for (path, count) in files.iter().zip(file_counts.iter()) {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
        file_list = file_list.child(
            div().text_color(cx.theme().text_muted).child(format!("{name} — {count} příkazů")),
        );
    }

    let mut conn_line = div().flex().flex_row().gap_2().text_color(cx.theme().text_muted).child(conn_label.to_string());
    if read_only {
        conn_line = conn_line.child(div().text_color(cx.theme().warn).child("jen pro čtení"));
    }

    let tx_option = |id: &'static str, label: &'static str, value: TxScope, current: TxScope| {
        let valid = crate::script_options_valid(value, error_policy);
        let selected = value == current;
        let base = div().id(id).px_2().py_1().rounded_md().child(label);
        if !valid {
            base.text_color(cx.theme().border)
        } else if selected {
            base.cursor_pointer().bg(cx.theme().bg_selected).text_color(cx.theme().text_primary).on_click(cx.listener(move |v, _, _, cx| {
                v.set_script_tx_scope(value, cx);
            }))
        } else {
            base.cursor_pointer().bg(cx.theme().bg_hover).text_color(cx.theme().text_muted).on_click(cx.listener(move |v, _, _, cx| {
                v.set_script_tx_scope(value, cx);
            }))
        }
    };
    let policy_option = |id: &'static str, label: &'static str, value: ErrorPolicy, current: ErrorPolicy| {
        let valid = crate::script_options_valid(tx_scope, value);
        let selected = value == current;
        let base = div().id(id).px_2().py_1().rounded_md().child(label);
        if !valid {
            base.text_color(cx.theme().border)
        } else if selected {
            base.cursor_pointer().bg(cx.theme().bg_selected).text_color(cx.theme().text_primary).on_click(cx.listener(move |v, _, _, cx| {
                v.set_script_error_policy(value, cx);
            }))
        } else {
            base.cursor_pointer().bg(cx.theme().bg_hover).text_color(cx.theme().text_muted).on_click(cx.listener(move |v, _, _, cx| {
                v.set_script_error_policy(value, cx);
            }))
        }
    };

    let timeout_line = match timeout_secs {
        Some(t) => format!("timeout na příkaz: {t}s"),
        None => "bez timeoutu".to_string(),
    };

    let panel = div()
        .id("script-run-confirm")
        .w(px(560.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child(format!("Spustit skript: {source_label}")))
        .child(conn_line)
        .child(file_list)
        .child(div().text_color(cx.theme().text_muted).child(format!("celkem: {total} příkazů")))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(cx.theme().text_muted).child("Transakce"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(tx_option("script-tx-none", "žádná transakce", TxScope::None, tx_scope))
                        .child(tx_option("script-tx-perfile", "transakce na soubor", TxScope::PerFile, tx_scope))
                        .child(tx_option("script-tx-whole", "jedna transakce na celý běh", TxScope::WholeRun, tx_scope)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(cx.theme().text_muted).child("Při chybě"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(policy_option("script-err-stop", "zastavit", ErrorPolicy::Stop, error_policy))
                        .child(policy_option("script-err-continue", "pokračovat", ErrorPolicy::Continue, error_policy)),
                ),
        )
        .child(div().text_color(cx.theme().text_muted).child(timeout_line))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .justify_end()
                .mt_2()
                .child(styled_button("script-run-cancel-modal", "Zrušit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| v.close_modal(cx))))
                .child(
                    styled_button("script-run-confirm-btn", "Spustit", *cx.theme())
                        .on_click(cx.listener(|v, _, _, cx| v.confirm_script_run(cx))),
                ),
        );
    panel.into_any_element()
}

/// G12 T4: `ModalState::CsvImport`'s mapping panel — file path, target
/// table, per-header mapping row (a lightweight cycle-button through
/// "(přeskočit)" -> each target column, same idiom as the grid's "Export ▾"
/// menu rather than a real dropdown — `AppView::cycle_csv_target`), exact
/// row count, the fixed batch size, and the REAL first batch's `INSERT`
/// verbatim (recomputed on every mapping change by
/// `AppView::recompute_csv_sample`) in a scrollable monospace box. A
/// duplicate-target `error` disables "Spustit import". `conn_label` (review
/// fix, BLOCKER) is shown so the target connection is visible before
/// confirming — same convention `render_script_run_confirm_panel`'s
/// `conn_label` sets; `confirm_csv_import` re-verifies the STABLE identity
/// (not the label) is unchanged before dispatching.
#[allow(clippy::too_many_arguments)]
fn render_csv_import_panel(
    path: &std::path::Path,
    table: &str,
    headers: &[String],
    columns: &[crate::csv_import::TargetColumn],
    targets: &[Option<usize>],
    row_count: usize,
    sample_sql: &Option<String>,
    error: &Option<String>,
    conn_label: &str,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let mut mapping_rows = div().flex().flex_col().gap_1().max_h(px(220.)).overflow_hidden();
    for (ix, header) in headers.iter().enumerate() {
        let target_label = targets
            .get(ix)
            .copied()
            .flatten()
            .and_then(|t| columns.get(t))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "(přeskočit)".to_string());
        mapping_rows = mapping_rows.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(160.)).text_color(cx.theme().text_muted).child(header.clone()))
                .child(
                    div()
                        .id(("csv-target-cycle", ix))
                        .px_2()
                        .py_1()
                        .bg(cx.theme().bg_hover)
                        .rounded_md()
                        .cursor_pointer()
                        .child(format!("→ {target_label}"))
                        .on_click(cx.listener(move |v, _, _, cx| v.cycle_csv_target(ix, cx))),
                ),
        );
    }

    let mut panel = div()
        .id("csv-import-modal")
        .w(px(600.))
        .bg(cx.theme().bg_panel)
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(cx.theme().text_primary)
        .child(div().text_size(px(16.)).child(format!("Import CSV do {table}")))
        .child(div().text_color(cx.theme().text_muted).child(format!("připojení: {conn_label}")))
        .child(div().text_color(cx.theme().text_muted).child(path.display().to_string()))
        .child(mapping_rows)
        .child(div().text_color(cx.theme().text_muted).child(format!(
            "{row_count} řádků · dávka: {} řádků",
            crate::csv_import::CSV_IMPORT_BATCH_SIZE
        )))
        .child(
            div()
                .text_color(cx.theme().text_muted)
                .child("prázdné pole → NULL; hlavičkový řádek je povinný"),
        );

    if let Some(sql) = sample_sql {
        panel = panel.child(
            div()
                .id("csv-sample-sql")
                .max_h(px(140.))
                .overflow_hidden()
                .p_1()
                .bg(cx.theme().bg_app)
                .rounded_md()
                .text_color(cx.theme().text_muted)
                .font_family("Consolas")
                .whitespace_normal()
                .child(sql.clone()),
        );
    }
    if let Some(e) = error {
        panel = panel.child(div().text_color(cx.theme().danger).child(format!("error: {e}")));
    }

    let can_run = error.is_none() && sample_sql.is_some();
    let confirm_btn = if can_run {
        styled_button("csv-import-confirm-btn", "Spustit import", *cx.theme())
            .on_click(cx.listener(|v, _, _, cx| v.confirm_csv_import(cx)))
            .into_any_element()
    } else {
        div()
            .id("csv-import-confirm-btn")
            .px_3()
            .py_1()
            .rounded_md()
            .bg(cx.theme().bg_hover)
            .text_color(cx.theme().text_disabled)
            .child("Spustit import")
            .into_any_element()
    };

    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .justify_end()
            .mt_2()
            .child(styled_button("csv-import-cancel", "Zrušit", *cx.theme()).on_click(cx.listener(|v, _, _, cx| v.close_modal(cx))))
            .child(confirm_btn),
    );
    panel.into_any_element()
}

/// G14 T11 (design §2.1/§2.4): bar/line axis picker — same panel skeleton
/// as `render_settings_panel`. `columns` is the FULL column list (display
/// order); the X list shows every column (radio, `x_col` index), the Y list
/// shows ONLY `numeric == true` columns (checkboxes, `y_selected[i]`) — the
/// real bound on series count (`chart_view::MAX_SERIES` is only a belt).
/// Heading/footer label switch on `edit_tab.is_some()` (re-pick vs. create).
fn render_chart_picker_panel(
    source_title: String,
    columns: Vec<(String, bool)>,
    kind: ChartKind,
    x_col: usize,
    y_selected: Vec<bool>,
    edit_tab: Option<u64>,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let theme = *cx.theme();
    let is_edit = edit_tab.is_some();
    let numeric_count = columns.iter().filter(|(_, numeric)| *numeric).count();

    let kind_button = |id: &'static str, label: &'static str, this_kind: ChartKind, cx: &mut Context<AppView>| {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .bg(if this_kind == kind { cx.theme().bg_selected } else { cx.theme().bg_hover })
            .child(label)
            .on_click(cx.listener(move |v, _, _, cx| v.set_chart_kind(this_kind, cx)))
    };

    let mut panel = div()
        .id("chart-picker-panel")
        .w(px(460.))
        .max_h(px(560.))
        .bg(theme.bg_panel)
        .border_1()
        .border_color(theme.border)
        .rounded_md()
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(theme.text_primary)
        .child(div().text_size(px(16.)).child(format!("Graf: {source_title}")))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(kind_button("chart-picker-kind-bar", "Sloupcový", ChartKind::Bar, cx))
                .child(kind_button("chart-picker-kind-line", "Čárový", ChartKind::Line, cx)),
        )
        .child(div().text_color(theme.text_muted).child("Osa X"));

    let mut x_list = div().id("chart-picker-x-list").flex().flex_col().gap_1().max_h(px(120.)).overflow_hidden();
    for (i, (name, _numeric)) in columns.iter().enumerate() {
        let selected = i == x_col;
        x_list = x_list.child(
            div()
                .id(SharedString::from(format!("chart-picker-x-{i}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .bg(if selected { theme.bg_selected } else { theme.bg_hover })
                .child(if selected { "●" } else { "○" })
                .child(name.clone())
                .on_click(cx.listener(move |v, _, _, cx| v.set_chart_x_col(i, cx))),
        );
    }
    panel = panel.child(x_list).child(div().text_color(theme.text_muted).child("Osa Y (číselné sloupce)"));

    if numeric_count == 0 {
        panel = panel.child(div().text_color(theme.danger).child("výsledek nemá žádný číselný sloupec"));
    }
    let mut y_list = div().id("chart-picker-y-list").flex().flex_col().gap_1().max_h(px(160.)).overflow_hidden();
    for (i, (name, numeric)) in columns.iter().enumerate() {
        if !*numeric {
            continue;
        }
        let checked = y_selected.get(i).copied().unwrap_or(false);
        let mark = if checked { "☑" } else { "☐" };
        y_list = y_list.child(
            div()
                .id(SharedString::from(format!("chart-picker-y-{i}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .child(format!("{mark} {name}"))
                .on_click(cx.listener(move |v, _, _, cx| v.toggle_chart_y_col(i, cx))),
        );
    }
    panel = panel.child(y_list);

    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .justify_end()
            .gap_2()
            .mt_2()
            .child(
                styled_button("chart-picker-cancel", "Zrušit", theme)
                    .on_click(cx.listener(|v, _, _, cx| v.close_modal(cx))),
            )
            .child(
                styled_button("chart-picker-confirm", if is_edit { "Použít" } else { "Vytvořit graf" }, theme)
                    .on_click(cx.listener(|v, _, _, cx| v.confirm_chart_picker(cx))),
            ),
    );

    panel.into_any_element()
}

#[cfg(test)]
mod kill_confirm_tests {
    use super::*;

    fn kill_confirm(pid: i64, tab_id: u64, dispatched: bool) -> Option<ModalState> {
        Some(ModalState::KillConfirm {
            pid,
            label: "u · app · běží 5s".into(),
            sql: format!("SELECT pg_terminate_backend({pid})"),
            tab_id,
            error: None,
            dispatched,
        })
    }

    // --- kill_confirm_dispatch_target (MINOR: double-dispatch guard) ---

    #[test]
    fn dispatch_target_none_when_no_dialog_open() {
        assert_eq!(kill_confirm_dispatch_target(&None), None);
    }

    #[test]
    fn dispatch_target_some_when_not_yet_dispatched() {
        let modal = kill_confirm(42, 7, false);
        assert_eq!(kill_confirm_dispatch_target(&modal), Some((42, 7)));
    }

    #[test]
    fn dispatch_target_none_once_already_dispatched() {
        // Regression for the MINOR finding: a second confirm click must be
        // a no-op while the first kill is still in flight.
        let modal = kill_confirm(42, 7, true);
        assert_eq!(kill_confirm_dispatch_target(&modal), None);
    }

    // --- kill_confirm_matches (MAJOR: misattribution guard) ---

    #[test]
    fn matches_true_for_same_pid_and_tab() {
        let modal = kill_confirm(1, 100, false);
        assert!(kill_confirm_matches(&modal, 100, 1));
    }

    #[test]
    fn matches_false_for_different_pid_same_tab() {
        // The MAJOR repro: pid 1's dialog cancelled, pid 2's dialog open on
        // the same tab, pid 1's stale KillResult arrives.
        let modal = kill_confirm(2, 100, false);
        assert!(!kill_confirm_matches(&modal, 100, 1));
    }

    #[test]
    fn matches_false_for_same_pid_different_tab() {
        // The cross-tab variant: two monitor tabs, same pid coincidentally.
        let modal = kill_confirm(1, 200, false);
        assert!(!kill_confirm_matches(&modal, 100, 1));
    }

    #[test]
    fn matches_false_when_no_dialog_open() {
        assert!(!kill_confirm_matches(&None, 100, 1));
    }

    #[test]
    fn matches_ignores_dispatched_flag() {
        // A dispatched-but-still-open matching dialog must still resolve —
        // `dispatched` only gates the SEND side (dispatch_target), not the
        // RESOLVE side (matches).
        let modal = kill_confirm(1, 100, true);
        assert!(kill_confirm_matches(&modal, 100, 1));
    }

    // --- apply_kill_error_to_modal (NEW MINOR: retry-after-failure) ---

    #[test]
    fn matching_err_sets_error_and_resets_dispatched_for_retry() {
        // Regression for the NEW MINOR finding: before this fix, a genuine
        // failed kill left `dispatched` at `true` forever, permanently
        // greying out "Ukončit proces" — no way to retry.
        let mut modal = kill_confirm(42, 7, true); // in flight
        apply_kill_error_to_modal(&mut modal, 7, 42, "boom");
        let Some(ModalState::KillConfirm { error, dispatched, .. }) = &modal else {
            panic!("dialog must stay open on a matching failed kill");
        };
        assert_eq!(error.as_deref(), Some("boom"));
        assert!(!dispatched, "dispatched must reset so a retry click can dispatch again");
        // The retry itself must now be dispatchable.
        assert_eq!(kill_confirm_dispatch_target(&modal), Some((42, 7)));
    }

    #[test]
    fn non_matching_err_leaves_modal_untouched() {
        // Same misattribution class as the MAJOR fix: pid 1's stale error
        // must not touch pid 2's currently open, still-in-flight dialog.
        let mut modal = kill_confirm(2, 7, true);
        apply_kill_error_to_modal(&mut modal, 7, 1, "boom");
        let Some(ModalState::KillConfirm { pid, error, dispatched, .. }) = &modal else {
            panic!("unrelated dialog must remain open");
        };
        assert_eq!(*pid, 2);
        assert_eq!(*error, None);
        assert!(*dispatched, "unrelated dialog's in-flight state must be untouched");
    }
}

/// G13 T6 MAJOR review fix (adversarial review of commit 0bab655): pins
/// `analyze_write_dispatch_sql`'s busy-guard — the mechanism that replaced
/// the earlier `self.cancel`-token approach, which could be defeated by
/// Escape (see that function's doc comment for the full story).
#[cfg(test)]
mod analyze_write_confirm_tests {
    use super::*;

    fn analyze_confirm(sql: &str, running: bool) -> Option<ModalState> {
        Some(ModalState::AnalyzeWriteConfirm {
            sql: sql.to_string(),
            engine: Engine::Postgres,
            running,
            error: None,
        })
    }

    #[test]
    fn dispatch_sql_none_when_no_dialog_open() {
        assert_eq!(analyze_write_dispatch_sql(&None), None);
    }

    #[test]
    fn dispatch_sql_none_for_an_unrelated_modal() {
        let modal = Some(ModalState::QueryParams {
            names: Vec::new(),
            inputs: Vec::new(),
            null_flags: Vec::new(),
            sql_template: "SELECT 1".into(),
            bypass_auto_limit: false,
            error: None,
        });
        assert_eq!(analyze_write_dispatch_sql(&modal), None);
    }

    #[test]
    fn dispatch_sql_some_when_not_yet_running() {
        let modal = analyze_confirm("UPDATE t SET x = 1", false);
        assert_eq!(analyze_write_dispatch_sql(&modal), Some("UPDATE t SET x = 1".to_string()));
    }

    #[test]
    fn dispatch_sql_none_once_already_running() {
        // The core regression pin for the MAJOR fix: a second confirm
        // click (or, before the fix, a false Escape-triggered re-enable
        // via the old `self.cancel`-based guard) must be a no-op while the
        // first analyze is still in flight — `self.modal` stays `Some(..)`
        // with `running: true` for the whole duration, so this guard alone
        // is what makes a second dispatch impossible now.
        let modal = analyze_confirm("UPDATE t SET x = 1", true);
        assert_eq!(analyze_write_dispatch_sql(&modal), None);
    }
}
