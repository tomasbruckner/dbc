use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub struct MultilineBuffer {
    text: String,
    cursor: usize,
    selection: Option<Range<usize>>,
    goal_column: Option<usize>,
}

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
        Self {
            text: s.to_string(),
            cursor: s.len(),
            selection: None,
            goal_column: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = s.len();
        self.selection = None;
        self.goal_column = None;
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }

    pub fn insert(&mut self, s: &str) {
        if let Some(sel) = self.selection.take() {
            let start = sel.start.min(sel.end);
            let end = sel.start.max(sel.end);
            self.text.drain(start..end);
            self.cursor = start;
        }
        self.text.insert_str(self.cursor, s);
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

    pub fn move_left(&mut self, extend_selection: bool) {
        if self.cursor > 0 {
            // Find the previous grapheme boundary
            let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
            for i in (0..graphemes.len()).rev() {
                if graphemes[i].0 < self.cursor {
                    if extend_selection {
                        if let Some(sel) = &mut self.selection {
                            sel.end = graphemes[i].0;
                        } else {
                            self.selection = Some(self.cursor..graphemes[i].0);
                        }
                    } else {
                        self.selection = None;
                    }
                    self.cursor = graphemes[i].0;
                    break;
                }
            }
        }
        self.goal_column = None;
    }

    pub fn move_right(&mut self, extend_selection: bool) {
        // If cursor is past end of text, move backward to last valid position
        if self.cursor > self.text.len() {
            // Move backward
            if self.cursor > 0 {
                let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
                for i in (0..graphemes.len()).rev() {
                    if graphemes[i].0 < self.cursor {
                        if extend_selection {
                            if let Some(sel) = &mut self.selection {
                                sel.end = graphemes[i].0;
                            } else {
                                self.selection = Some(self.cursor..graphemes[i].0);
                            }
                        } else {
                            self.selection = None;
                        }
                        self.cursor = graphemes[i].0;
                        break;
                    }
                }
            }
        } else if self.cursor < self.text.len() {
            // Move forward normally
            let graphemes: Vec<(usize, &str)> = self.text.grapheme_indices(true).collect();
            for i in 0..graphemes.len() {
                if graphemes[i].0 == self.cursor && i + 1 < graphemes.len() {
                    let next_pos = graphemes[i + 1].0;
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
                col += 1;
            }
        }

        (line, col)
    }

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
        let clamped_col = byte_col.min(line_text.len());
        offset + clamped_col
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
        // cursor at end of first line (col 6)
        for _ in 0..10 { b.move_right(false); }
        assert_eq!(b.cursor_position(), (0, 6));
        b.move_down(false);              // line "xy" only has 2 cols → clamp
        assert_eq!(b.cursor_position(), (1, 2));
        b.move_down(false);              // back to a long line → goal col 6 restored
        assert_eq!(b.cursor_position(), (2, 6));
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
        for _ in 0..6 { b.move_right(true); } // selects "aa\nbbb"... byte-wise 1..7
        b.delete();
        assert_eq!(b.text(), "ab\nccc".replace("bb\n", "b\n")); // = "ab\nccc"? — compute: "aaa\nbbb\nccc" minus 1..7 = "a" + "b\nccc" = "ab\nccc"
        assert_eq!(b.text(), "ab\nccc");
    }
}
