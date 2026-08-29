use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// Replaces "\r\n" with "\n" so that every internal `\n`-based line-splitting
/// operation (`lines`, `line_count`, `offset_at`, `cursor_position`, ...) can
/// safely assume LF-only line endings. Applied at every text-entry point
/// (`from_text`, `set_text`, `insert`), so CRLF input is normalized before it
/// ever reaches the buffer.
fn normalize_newlines(s: &str) -> String {
    if s.contains('\r') {
        s.replace("\r\n", "\n")
    } else {
        s.to_string()
    }
}

/// Snaps `byte_col` (clamped to `line.len()`) down to the nearest grapheme
/// cluster boundary at or before it, so callers (in particular `offset_at`,
/// which feeds `move_up`/`move_down`'s goal-column tracking) never produce a
/// cursor position that lands mid-character on a multi-byte line.
fn snap_to_grapheme_boundary(line: &str, byte_col: usize) -> usize {
    let clamped = byte_col.min(line.len());
    let mut boundaries: Vec<usize> = line.grapheme_indices(true).map(|(i, _)| i).collect();
    boundaries.push(line.len());
    boundaries
        .into_iter()
        .filter(|&b| b <= clamped)
        .max()
        .unwrap_or(0)
}

/// Like `snap_to_grapheme_boundary`, but snaps two byte offsets (each
/// already `<= text.len()`) down to their nearest grapheme boundaries in a
/// single scan over `text`'s grapheme boundaries, instead of two
/// independent scans. Used by `select_range` so placing a selection at an
/// arbitrary offset pair stays a single O(n) pass over the buffer rather
/// than O(n) per endpoint.
fn snap_two_to_grapheme_boundary(text: &str, a: usize, b: usize) -> (usize, usize) {
    let mut snapped_a = 0;
    let mut snapped_b = 0;
    let boundaries = text
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()));
    for i in boundaries {
        if i <= a {
            snapped_a = i;
        }
        if i <= b {
            snapped_b = i;
        }
    }
    (snapped_a, snapped_b)
}

pub struct MultilineBuffer {
    text: String,
    cursor: usize,
    selection: Option<Range<usize>>,
    goal_column: Option<usize>,
}

#[allow(dead_code)] // full surface lands in the editor element (G1 Task 4)
#[allow(dead_code)] // full surface lands in the editor element (G1 Task 4)
/// Character classes for word motion: a word run, a whitespace run, or a
/// run of anything else (operators, punctuation). Grouping "everything
/// else" means `a.b` stops at each part rather than skipping the whole
/// expression, which is what Windows editors do.
#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Word,
    Space,
    Other,
}

fn classify(c: char, is_word: &dyn Fn(char) -> bool) -> CharClass {
    if is_word(c) {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

#[allow(dead_code)] // full surface lands in the editor element (G1 Task 4)
impl MultilineBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            selection: None,
            goal_column: None,
        }
    }

    pub fn from_text(s: &str) -> Self {
        let text = normalize_newlines(s);
        let cursor = text.len();
        Self {
            text,
            cursor,
            selection: None,
            goal_column: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, s: &str) {
        self.text = normalize_newlines(s);
        self.cursor = self.text.len();
        self.selection = None;
        self.goal_column = None;
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Always returns an ordered range (`min..max`), regardless of which
    /// direction the selection was extended in. Internal storage may be
    /// anchor/active (unordered); callers must be able to rely on this
    /// accessor for slicing/rendering.
    pub fn selection(&self) -> Option<Range<usize>> {
        self.selection
            .clone()
            .map(|r| r.start.min(r.end)..r.start.max(r.end))
    }

    pub fn insert(&mut self, s: &str) {
        let s = normalize_newlines(s);
        if let Some(sel) = self.selection.take() {
            let start = sel.start.min(sel.end);
            let end = sel.start.max(sel.end);
            self.text.drain(start..end);
            self.cursor = start;
        }
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
        self.goal_column = None;
    }

    pub fn backspace(&mut self) {
        if let Some(sel) = self.selection.take() {
            let start = sel.start.min(sel.end);
            let end = sel.start.max(sel.end);
            self.text.drain(start..end);
            self.cursor = start;
        } else if self.cursor > 0 {
            // Find the previous grapheme boundary
            let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
            for i in (0..graphemes.len()).rev() {
                if graphemes[i].0 < self.cursor {
                    let prev_start = graphemes[i].0;
                    self.text.drain(prev_start..self.cursor);
                    self.cursor = prev_start;
                    break;
                }
            }
        }
        self.goal_column = None;
    }

    pub fn delete(&mut self) {
        if let Some(sel) = self.selection.take() {
            let start = sel.start.min(sel.end);
            let end = sel.start.max(sel.end);
            self.text.drain(start..end);
            self.cursor = start;
        } else if self.cursor < self.text.len() {
            // Find the next grapheme boundary
            let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
            for i in 0..graphemes.len() {
                if graphemes[i].0 == self.cursor {
                    let next_end = if i + 1 < graphemes.len() {
                        graphemes[i + 1].0
                    } else {
                        self.text.len()
                    };
                    self.text.drain(self.cursor..next_end);
                    break;
                }
            }
        }
        self.goal_column = None;
    }

    /// Byte offset of the word boundary `dir` from `self.cursor`.
    ///
    /// Windows/VS Code semantics, which is what the user asked for
    /// (2026-08-29: „klasické windows zkratky pro práci s textem"): moving
    /// LEFT skips whitespace and then consumes the word before it; moving
    /// RIGHT consumes the word under/after the cursor and then the
    /// whitespace after it. That asymmetry is deliberate and is why this is
    /// ONE function with a direction rather than two that could drift.
    ///
    /// A "word" is a run of alphanumerics and `_` — SQL identifiers.
    /// Everything else (operators, punctuation) is consumed one run at a
    /// time, so `a.b` is three stops, not one.
    fn word_boundary(&self, forward: bool) -> usize {
        let ch: Vec<(usize, char)> = self.text.char_indices().collect();
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        // Index into `ch` of the char at/after the cursor.
        let mut i = ch.partition_point(|(off, _)| *off < self.cursor);

        if forward {
            if i >= ch.len() {
                return self.text.len();
            }
            let kind = classify(ch[i].1, &is_word);
            while i < ch.len() && classify(ch[i].1, &is_word) == kind && kind != CharClass::Space {
                i += 1;
            }
            // Trailing whitespace joins the move, so one Ctrl+Right lands on
            // the next word rather than on the space before it.
            while i < ch.len() && ch[i].1.is_whitespace() && ch[i].1 != '\n' {
                i += 1;
            }
            ch.get(i).map(|(off, _)| *off).unwrap_or(self.text.len())
        } else {
            // Leading whitespace first, then the run before it.
            while i > 0 && ch[i - 1].1.is_whitespace() && ch[i - 1].1 != '\n' {
                i -= 1;
            }
            if i == 0 {
                return 0;
            }
            let kind = classify(ch[i - 1].1, &is_word);
            while i > 0 && classify(ch[i - 1].1, &is_word) == kind {
                i -= 1;
            }
            ch.get(i).map(|(off, _)| *off).unwrap_or(0)
        }
    }

    /// Ctrl+Left / Ctrl+Right, and with `extend_selection` their Shift
    /// variants. Shares [`Self::word_boundary`] with nothing else, so the
    /// four shortcuts can never disagree about where a word ends.
    pub fn move_word(&mut self, forward: bool, extend_selection: bool) {
        let target = self.word_boundary(forward);
        if extend_selection {
            match &mut self.selection {
                Some(sel) => sel.end = target,
                None => self.selection = Some(self.cursor..target),
            }
        } else {
            self.selection = None;
        }
        self.cursor = target;
        self.goal_column = None;
    }

    /// Ctrl+Home / Ctrl+End.
    pub fn move_document(&mut self, to_end: bool, extend_selection: bool) {
        let target = if to_end { self.text.len() } else { 0 };
        if extend_selection {
            match &mut self.selection {
                Some(sel) => sel.end = target,
                None => self.selection = Some(self.cursor..target),
            }
        } else {
            self.selection = None;
        }
        self.cursor = target;
        self.goal_column = None;
    }

    /// Ctrl+Backspace / Ctrl+Delete: delete to the word boundary. Reuses
    /// the same boundary the cursor motions use, so what a Ctrl+Shift+Arrow
    /// would have SELECTED is exactly what this deletes.
    pub fn delete_word(&mut self, forward: bool) {
        if self.selection.is_some() {
            self.backspace();
            return;
        }
        let target = self.word_boundary(forward);
        let (from, to) = if forward { (self.cursor, target) } else { (target, self.cursor) };
        if from < to {
            self.text.drain(from..to);
            self.cursor = from;
        }
        self.goal_column = None;
    }

    pub fn move_left(&mut self, extend_selection: bool) {
        // Standard editor behaviour: with an active selection and no shift
        // held, Left collapses the cursor to the selection's left edge
        // instead of stepping one grapheme from the active end.
        if !extend_selection {
            if let Some(sel) = self.selection.take() {
                self.cursor = sel.start.min(sel.end);
                self.goal_column = None;
                return;
            }
        }
        if self.cursor > 0 {
            // Find the previous grapheme boundary
            let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
            for i in (0..graphemes.len()).rev() {
                if graphemes[i].0 < self.cursor {
                    let new_pos = graphemes[i].0;
                    if extend_selection {
                        if let Some(sel) = &mut self.selection {
                            sel.end = new_pos;
                        } else {
                            self.selection = Some(self.cursor..new_pos);
                        }
                    } else {
                        self.selection = None;
                    }
                    self.cursor = new_pos;
                    break;
                }
            }
        }
        self.goal_column = None;
    }

    pub fn move_right(&mut self, extend_selection: bool) {
        // Standard editor behaviour: with an active selection and no shift
        // held, Right collapses the cursor to the selection's right edge
        // instead of stepping one grapheme from the active end.
        if !extend_selection {
            if let Some(sel) = self.selection.take() {
                self.cursor = sel.start.max(sel.end);
                self.goal_column = None;
                return;
            }
        }
        // Every code path that sets `self.cursor` keeps it <= text.len();
        // this should never fire via the public API. Surface the invariant
        // violation in debug builds instead of silently moving backward.
        debug_assert!(self.cursor <= self.text.len());
        if self.cursor < self.text.len() {
            let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
            for i in 0..graphemes.len() {
                if graphemes[i].0 == self.cursor {
                    let next_pos = if i + 1 < graphemes.len() {
                        graphemes[i + 1].0
                    } else {
                        self.text.len()
                    };
                    if extend_selection {
                        if let Some(sel) = &mut self.selection {
                            sel.end = next_pos;
                        } else {
                            self.selection = Some(self.cursor..next_pos);
                        }
                    } else {
                        self.selection = None;
                    }
                    self.cursor = next_pos;
                    break;
                }
            }
        }
        self.goal_column = None;
    }

    pub fn move_up(&mut self, extend_selection: bool) {
        let (line, col) = self.cursor_position();
        if line > 0 {
            // On first vertical move, remember the goal column
            if self.goal_column.is_none() {
                self.goal_column = Some(col);
            }

            let target_col = self.goal_column.unwrap();

            // Get the previous line
            let lines: Vec<&str> = self.text.split('\n').collect();
            let prev_line = lines[line - 1];

            // Move to the target column, clamped to line length
            let new_col = target_col.min(prev_line.len());
            let new_offset = self.offset_at(line - 1, new_col);

            if extend_selection {
                if let Some(sel) = &mut self.selection {
                    sel.end = new_offset;
                } else {
                    self.selection = Some(self.cursor..new_offset);
                }
            } else {
                self.selection = None;
            }

            self.cursor = new_offset;
        }
    }

    pub fn move_down(&mut self, extend_selection: bool) {
        let (line, col) = self.cursor_position();
        let lines: Vec<&str> = self.text.split('\n').collect();

        if line < lines.len() - 1 {
            // On first vertical move, remember the goal column
            if self.goal_column.is_none() {
                self.goal_column = Some(col);
            }

            let target_col = self.goal_column.unwrap();

            // Get the next line
            let next_line = lines[line + 1];

            // Move to the target column, clamped to line length
            let new_col = target_col.min(next_line.len());
            let new_offset = self.offset_at(line + 1, new_col);

            if extend_selection {
                if let Some(sel) = &mut self.selection {
                    sel.end = new_offset;
                } else {
                    self.selection = Some(self.cursor..new_offset);
                }
            } else {
                self.selection = None;
            }

            self.cursor = new_offset;
        }
    }

    pub fn move_home(&mut self, extend_selection: bool) {
        let (line, _) = self.cursor_position();
        let new_offset = self.offset_at(line, 0);

        if extend_selection {
            if let Some(sel) = &mut self.selection {
                sel.end = new_offset;
            } else {
                self.selection = Some(self.cursor..new_offset);
            }
        } else {
            self.selection = None;
        }

        self.cursor = new_offset;
        self.goal_column = None;
    }

    pub fn move_end(&mut self, extend_selection: bool) {
        let (line, _) = self.cursor_position();
        let lines: Vec<&str> = self.text.split('\n').collect();

        if line < lines.len() {
            let line_len = lines[line].len();
            let new_offset = self.offset_at(line, line_len);

            if extend_selection {
                if let Some(sel) = &mut self.selection {
                    sel.end = new_offset;
                } else {
                    self.selection = Some(self.cursor..new_offset);
                }
            } else {
                self.selection = None;
            }

            self.cursor = new_offset;
        }

        self.goal_column = None;
    }

    pub fn select_all(&mut self) {
        self.selection = Some(0..self.text.len());
        self.cursor = self.text.len();
        self.goal_column = None;
    }

    pub fn line_count(&self) -> usize {
        if self.text.is_empty() {
            1
        } else {
            self.text.matches('\n').count() + 1
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.split('\n')
    }

    /// Returns (line index, byte column within line) — the column is a raw
    /// byte offset into the line, consistent with `offset_at`'s `byte_col`
    /// parameter (NOT a character count).
    pub fn cursor_position(&self) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;

        for (i, ch) in self.text.char_indices() {
            if i >= self.cursor {
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

    /// Converts (line, byte_col) to an absolute byte offset. `line` is
    /// clamped to the last line, `byte_col` is clamped to the line length,
    /// and — defensively, since callers such as `move_up`/`move_down` may
    /// pass in a goal column computed against a *different* line's byte
    /// layout — snapped down to the nearest grapheme boundary so the result
    /// always lands on a valid, splittable position.
    pub fn offset_at(&self, line: usize, byte_col: usize) -> usize {
        let lines: Vec<&str> = self.text.split('\n').collect();

        if line >= lines.len() {
            return self.text.len();
        }

        let mut offset = 0;
        for i in 0..line {
            offset += lines[i].len() + 1; // +1 for newline
        }

        let line_text = lines[line];
        let clamped_col = snap_to_grapheme_boundary(line_text, byte_col);
        offset + clamped_col
    }

    /// Places the cursor at `offset`, clamped to `text.len()` and snapped
    /// down to the nearest grapheme boundary at or before it. Clears any
    /// active selection and the goal column. Single O(n) pass over `text`
    /// (via `snap_to_grapheme_boundary`) — unlike seeking an arbitrary
    /// offset via repeated `move_up`/`move_down`/`move_right` calls, which
    /// each independently re-scan the whole buffer (G1 Task 4 review,
    /// issue 2: O(line_count) such calls per seek).
    pub fn set_cursor(&mut self, offset: usize) {
        let clamped = offset.min(self.text.len());
        self.cursor = snap_to_grapheme_boundary(&self.text, clamped);
        self.selection = None;
        self.goal_column = None;
    }

    /// Sets the selection to `range`, with each end independently clamped
    /// to `text.len()` and snapped down to the nearest grapheme boundary at
    /// or before it (both ends resolved in one pass via
    /// `snap_two_to_grapheme_boundary`). `range.start` becomes the
    /// selection's anchor and `range.end` its active end (the cursor) —
    /// matching the anchor/active convention `move_left`/`move_right`/
    /// `move_up`/`move_down`/... already use internally, so a subsequent
    /// `move_*(extend_selection: true)` call extends from `range.end` while
    /// `range.start` stays fixed, exactly as if the selection had been
    /// built up via repeated extending moves.
    ///
    /// `range` is used verbatim, not pre-sorted: passing a "reversed" range
    /// (`range.start > range.end`) is a deliberate, supported way to select
    /// backward from an anchor — the anchor sits at `range.start` (here the
    /// larger offset) and the cursor lands at `range.end` (the smaller
    /// one). `selection()` always returns an ordered view regardless of
    /// which direction was used to build the selection.
    pub fn select_range(&mut self, range: Range<usize>) {
        let len = self.text.len();
        let (start, end) =
            snap_two_to_grapheme_boundary(&self.text, range.start.min(len), range.end.min(len));
        self.selection = Some(start..end);
        self.cursor = end;
        self.goal_column = None;
    }

    #[cfg(test)]
    pub(crate) fn set_cursor_for_test(&mut self, pos: usize) {
        // Clamp to char boundary
        let mut clamped = pos;
        while clamped > 0 && !self.text.is_char_boundary(clamped) {
            clamped -= 1;
        }
        self.cursor = clamped;
        self.selection = None;
        self.goal_column = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, cursor: usize) -> MultilineBuffer {
        let mut b = MultilineBuffer::from_text(text);
        b.cursor = cursor;
        b
    }

    /// Ctrl+Right takes the word AND the space after it; Ctrl+Left takes
    /// the space before a word AND the word. That asymmetry is what makes
    /// repeated presses land on word STARTS in both directions, and it is
    /// what Windows editors do.
    #[test]
    fn word_motion_is_asymmetric_the_way_windows_is() {
        let mut b = at("select from table", 0);
        b.move_word(true, false);
        assert_eq!(b.cursor(), 7, "Ctrl+Right lands on the next word, not on the space");
        b.move_word(true, false);
        assert_eq!(b.cursor(), 12);

        let mut b = at("select from table", 12);
        b.move_word(false, false);
        assert_eq!(b.cursor(), 7, "Ctrl+Left lands on the start of the previous word");
        b.move_word(false, false);
        assert_eq!(b.cursor(), 0);
    }

    /// `a.b` is three stops, not one: punctuation is its own run, so
    /// Ctrl+Left in a qualified name walks the parts.
    #[test]
    fn punctuation_is_its_own_run() {
        let mut b = at("dbo.orders", 10);
        b.move_word(false, false);
        assert_eq!(b.cursor(), 4, "stops after the dot");
        b.move_word(false, false);
        assert_eq!(b.cursor(), 3, "the dot itself");
        b.move_word(false, false);
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn word_motion_stops_at_the_ends_and_does_not_panic() {
        let mut b = at("abc", 3);
        b.move_word(true, false);
        assert_eq!(b.cursor(), 3);
        let mut b = at("abc", 0);
        b.move_word(false, false);
        assert_eq!(b.cursor(), 0);
        let mut b = at("", 0);
        b.move_word(true, false);
        b.move_word(false, false);
        assert_eq!(b.cursor(), 0);
    }

    /// The user's report: Ctrl+Shift+Arrow must SELECT, not just move.
    #[test]
    fn ctrl_shift_arrow_selects_the_word() {
        let mut b = at("select from", 0);
        b.move_word(true, true);
        assert_eq!(b.selection(), Some(0..7), "selection must cover the word and the space");
        // Extending again grows the same selection rather than starting a
        // new one.
        b.move_word(true, true);
        assert_eq!(b.selection(), Some(0..11));
    }

    #[test]
    fn ctrl_home_and_end_reach_the_document_bounds() {
        let mut b = at("a\nbb\nccc", 4);
        b.move_document(true, false);
        assert_eq!(b.cursor(), 8);
        b.move_document(false, false);
        assert_eq!(b.cursor(), 0);
        b.move_document(true, true);
        assert_eq!(b.selection(), Some(0..8), "Ctrl+Shift+End selects to the end");
    }

    /// Ctrl+Backspace deletes exactly what Ctrl+Shift+Left would have
    /// selected — they share `word_boundary`, and this pins that they
    /// agree.
    #[test]
    fn delete_word_matches_what_select_word_would_have_covered() {
        let mut sel = at("select from table", 17);
        sel.move_word(false, true);
        let covered = sel.selection().unwrap();

        let mut del = at("select from table", 17);
        del.delete_word(false);
        assert_eq!(del.text(), "select from ");
        assert_eq!(del.cursor(), covered.start);
    }

    #[test]
    fn delete_word_with_a_selection_deletes_the_selection() {
        let mut b = at("select from", 0);
        b.move_word(true, true);
        b.delete_word(false);
        assert_eq!(b.text(), "from");
    }

    /// Word motion is char-based; a multi-byte identifier must not be split
    /// mid-character (the same class of bug `walk_ident_prefix_start` was
    /// fixed for in `autocomplete.rs`).
    #[test]
    fn word_motion_handles_multi_byte_identifiers() {
        let mut b = at("čas období", "čas období".len());
        b.move_word(false, false);
        assert_eq!(b.cursor(), "čas ".len());
        b.move_word(false, false);
        assert_eq!(b.cursor(), 0);
        assert!(b.text().is_char_boundary(b.cursor()));
    }

    #[test]
    fn insert_and_newlines() {
        let mut b = MultilineBuffer::new();
        b.insert("select 1\nfrom t");
        assert_eq!(b.text(), "select 1\nfrom t");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.cursor(), b.text().len());
    }

    #[test]
    fn vertical_movement_keeps_goal_column() {
        let mut b = MultilineBuffer::from_text("abcdef\nxy\nabcdef");
        // from_text leaves the cursor at the end of the text = (2, 6)
        assert_eq!(b.cursor_position(), (2, 6));
        b.move_up(false);                // line "xy" only has 2 cols → clamp
        assert_eq!(b.cursor_position(), (1, 2));
        b.move_up(false);                // back to a long line → goal col 6 restored
        assert_eq!(b.cursor_position(), (0, 6));
    }

    #[test]
    fn selection_replace() {
        let mut b = MultilineBuffer::from_text("hello world");
        b.move_home(false);
        for _ in 0..5 { b.move_right(true); } // select "hello"
        assert_eq!(b.selection(), Some(0..5));
        b.insert("bye");
        assert_eq!(b.text(), "bye world");
        assert_eq!(b.selection(), None);
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn grapheme_aware_backspace() {
        let mut b = MultilineBuffer::from_text("ař🙂");
        b.backspace();
        assert_eq!(b.text(), "ař");
        b.backspace();
        assert_eq!(b.text(), "a");
    }

    #[test]
    fn home_end_are_line_scoped() {
        let mut b = MultilineBuffer::from_text("one\ntwo three");
        // put cursor on line 1 middle
        let off = b.offset_at(1, 3);
        assert_eq!(&b.text()[off..off+1], " ");
        b.set_cursor_for_test(off);
        b.move_home(false);
        assert_eq!(b.cursor_position(), (1, 0));
        b.move_end(false);
        assert_eq!(b.cursor_position(), (1, "two three".len()));
    }

    #[test]
    fn click_offset_clamps() {
        let b = MultilineBuffer::from_text("ab\ncd");
        assert_eq!(b.offset_at(0, 99), 2);   // end of line 0
        assert_eq!(b.offset_at(9, 0), 5);    // past last line → end of text
    }

    #[test]
    fn delete_selection_across_lines() {
        let mut b = MultilineBuffer::from_text("aaa\nbbb\nccc");
        b.set_cursor_for_test(1);
        for _ in 0..5 { b.move_right(true); } // selects "aa\nbb" = bytes 1..6
        assert_eq!(b.selection(), Some(1..6));
        b.delete();
        assert_eq!(b.text(), "ab\nccc");
    }

    // --- Fix round 1 regression tests -------------------------------------

    #[test]
    fn multibyte_vertical_move_then_backspace_does_not_panic() {
        // Reproduction from the review (B1): cursor_position() used to
        // return a char *count* instead of a byte *column*, so move_down's
        // goal-column tracking could hand offset_at() a column that split a
        // multi-byte character on the target line, panicking on backspace.
        let mut b = MultilineBuffer::from_text("\u{1F642}\u{1F642}x\n\u{0159}\u{1F642}z");
        b.set_cursor_for_test(9); // end of line 0 ("🙂🙂x" = 4+4+1 = 9 bytes)
        assert_eq!(b.cursor_position(), (0, 9)); // byte column, not char count (3)
        b.move_down(false);
        // Must not panic:
        b.backspace();
        // Sanity: text is still valid UTF-8 and shorter than before.
        assert!(b.text().len() < "\u{1F642}\u{1F642}x\n\u{0159}\u{1F642}z".len());
    }

    #[test]
    fn vertical_move_snaps_goal_column_to_char_boundary() {
        // B1: when the goal column lands strictly *inside* a multi-byte
        // character on the target line (not just past its end), the cursor
        // must snap to the nearest grapheme boundary at or before it rather
        // than landing mid-character.
        let mut b = MultilineBuffer::from_text("abc\na\u{1F642}bc");
        b.set_cursor_for_test(3); // end of line 0, byte col 3
        assert_eq!(b.cursor_position(), (0, 3));
        b.move_down(false);
        // Goal col 3 falls inside the emoji (bytes 1..5) on line 1 "a🙂bc";
        // must snap down to col 1 (right after 'a'), not panic.
        assert_eq!(b.cursor_position(), (1, 1));
        b.backspace();
        assert_eq!(b.text(), "abc\n\u{1F642}bc");
    }

    #[test]
    fn selection_is_always_ordered_when_extending_backward() {
        // B2: selection() must return an ordered range regardless of drag
        // direction, even though internal storage may be anchor/active.
        let mut b = MultilineBuffer::from_text("hello world");
        b.set_cursor_for_test(5);
        b.move_left(true);
        b.move_left(true);
        b.move_left(true);
        let sel = b.selection().expect("selection should be active");
        assert!(sel.start <= sel.end, "selection must be ordered: {:?}", sel);
        assert_eq!(sel, 2..5);
    }

    #[test]
    fn move_left_right_collapse_to_selection_edge() {
        // B3: with extend_selection == false and an active selection,
        // move_left/move_right must collapse to the selection's left/right
        // edge, not step one grapheme from the active end.
        let mut b = MultilineBuffer::from_text("hello world");
        b.move_home(false);
        for _ in 0..5 {
            b.move_right(true);
        } // select "hello", cursor at active end = 5 (right edge)
        assert_eq!(b.selection(), Some(0..5));

        let mut left = MultilineBuffer::from_text("hello world");
        left.move_home(false);
        for _ in 0..5 {
            left.move_right(true);
        }
        left.move_left(false);
        assert_eq!(left.cursor(), 0, "move_left should collapse to left edge");
        assert_eq!(left.selection(), None);

        let mut right = MultilineBuffer::from_text("hello world");
        right.set_cursor_for_test(5);
        for _ in 0..5 {
            right.move_left(true);
        } // select "hello" by dragging backward, cursor at active end = 0 (left edge)
        assert_eq!(right.selection(), Some(0..5));
        right.move_right(false);
        assert_eq!(right.cursor(), 5, "move_right should collapse to right edge");
        assert_eq!(right.selection(), None);
    }

    #[test]
    fn crlf_input_is_normalized_on_entry() {
        // Minor: CRLF is normalized to LF at every text-entry point so the
        // internal '\n'-based line model stays consistent.
        let b = MultilineBuffer::from_text("a\r\nb\r\nc");
        assert_eq!(b.text(), "a\nb\nc");
        assert_eq!(b.line_count(), 3);

        let mut b2 = MultilineBuffer::new();
        b2.set_text("x\r\ny");
        assert_eq!(b2.text(), "x\ny");

        let mut b3 = MultilineBuffer::new();
        b3.insert("p\r\nq");
        assert_eq!(b3.text(), "p\nq");
    }

    // --- Fix round 1: set_cursor / select_range ---------------------------

    #[test]
    fn set_cursor_snaps_mid_grapheme_offset_down() {
        // "🙂" occupies bytes 0..4; an offset landing inside it (2) must
        // snap down to 0 (the start of the grapheme), not panic or land
        // mid-character.
        let mut b = MultilineBuffer::from_text("\u{1F642}bc");
        b.set_cursor(2);
        assert_eq!(b.cursor(), 0);
        assert_eq!(b.selection(), None);
    }

    #[test]
    fn set_cursor_clamps_out_of_bounds_offset() {
        let mut b = MultilineBuffer::from_text("abc");
        b.set_cursor(9999);
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn select_range_snaps_mid_grapheme_offsets_down() {
        // Both ends of the range (1 and 3) land inside the same emoji
        // grapheme (bytes 0..4 of "🙂"); both must snap down to 0.
        let mut b = MultilineBuffer::from_text("\u{1F642}bc");
        b.select_range(1..3);
        assert_eq!(b.selection(), Some(0..0));
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn select_range_clamps_out_of_bounds_range() {
        let mut b = MultilineBuffer::from_text("abc");
        b.select_range(1..9999);
        assert_eq!(b.selection(), Some(1..3));
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    // The reversed range is a deliberate input value, not an iteration —
    // clippy::reversed_empty_ranges is a false positive here.
    #[allow(clippy::reversed_empty_ranges)]
    fn select_range_reversed_input_selects_backward_from_anchor() {
        // Documented/chosen behaviour: a "reversed" input range
        // (range.start > range.end) is a deliberate, supported way to
        // select backward. The anchor sits at range.start (here the larger
        // offset, 8) and the cursor lands at range.end (the smaller one,
        // 2). selection() still returns an ordered view; a subsequent
        // extend-move continues from the cursor (the active end) while the
        // anchor stays fixed.
        let mut b = MultilineBuffer::from_text("hello world");
        b.select_range(8..2);
        assert_eq!(b.selection(), Some(2..8), "selection() is always ordered");
        assert_eq!(b.cursor(), 2, "cursor sits at range.end, the active end");
        b.move_left(true); // extend further left from the active end
        assert_eq!(b.selection(), Some(1..8), "anchor (8) stays fixed");
    }
}
