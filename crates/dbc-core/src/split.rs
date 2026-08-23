//! Push-based incremental SQL statement splitter.
//!
//! Splits a stream of SQL text (fed in arbitrary byte-sized chunks) into
//! top-level, `;`-separated statements. This is a **parallel,
//! independently-implemented** state machine, not a reuse of
//! `guards::tokenize` -- that function is private, operates on a whole
//! `&str` in one pass, and has no notion of "pause mid-scan, resume on the
//! next chunk". This module deliberately mirrors `guards.rs`'s
//! escaping/comment discipline (single-quote `''`, double-quote `""`, `--`
//! to EOL, nestable `/* */`) so "is this read-only" and "where do statements
//! split" stay behaviorally consistent -- documented as a known duplication,
//! not unified (see the G12 design doc, §1 and §7).
//!
//! Fail-closed posture, matching `guards.rs`: any construct still open at
//! EOF (`finish()`) is reported as [`SplitError::UnterminatedAtEof`] rather
//! than guessed at.

/// SQL dialects the splitter understands. `Mssql` intentionally does not
/// exist yet -- the `GO` batch separator is a client-tool line convention,
/// not a token-nesting construct, and belongs in a separate line-based
/// pre-pass when the MSSQL driver phase lands (see the G12 design doc, §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Sqlite,
}

/// Which open construct EOF landed inside, for [`SplitError::UnterminatedAtEof`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnterminatedKind {
    StringLiteral,
    QuotedIdent,
    BlockComment,
    DollarQuote,
    TriggerBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitError {
    /// A chunk (or the trailing bytes at EOF) contained bytes that are not
    /// valid UTF-8. Non-UTF-8-encoded `.sql` files are an explicit
    /// non-goal -- this is a file-level error, not a per-byte guess.
    InvalidUtf8,
    /// Only produced by [`StatementSplitter::finish`]: EOF occurred inside
    /// an open construct. Fail closed, same posture as `guards::tokenize`
    /// returning `None`.
    UnterminatedAtEof(UnterminatedKind),
}

/// Lexer state. Cheap to copy (small integers only), carried in
/// [`StatementSplitter`] across `push` calls so a chunk boundary can land
/// anywhere -- mid-token, mid-string, mid `--`/`/*`/`$tag$` opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Top-level: not inside any string/ident/comment/dollar-quote, and not
    /// mid-word.
    Normal,
    /// Accumulating a bare word (alnum/underscore run).
    InWord,
    /// Saw a `-`; deciding whether the next char makes it `--`.
    SawDash,
    /// Saw a `/`; deciding whether the next char makes it `/*`.
    SawSlash,
    InSingleString,
    /// Saw a `'` while inside a single-quoted string; deciding whether the
    /// next char is an escaped `''` or the real close.
    SingleStringMaybeEnd,
    InDoubleIdent,
    /// Same ambiguity as `SingleStringMaybeEnd`, for `""`.
    DoubleIdentMaybeEnd,
    /// `--` comment, runs to end of line (or EOF -- see `finish`'s doc
    /// comment: EOF counts as an implicit EOL, matching every other SQL
    /// tool's behavior; there is deliberately no
    /// `UnterminatedKind::LineComment`).
    InLineComment,
    /// `/* ... */` comment, nestable. `depth` mirrors
    /// `guards.rs`'s block-comment depth counter exactly.
    InBlockComment(u32),
    /// Inside a block comment, saw `/`; deciding whether it opens a nested
    /// comment.
    BlockCommentMaybeOpen(u32),
    /// Inside a block comment, saw `*`; deciding whether it closes this
    /// comment.
    BlockCommentMaybeClose(u32),
    /// Postgres only: saw a bare `$`, buffering a candidate tag in
    /// `dollar_tag_buf` until a closing `$` confirms it, an illegal char (or
    /// the 64-char cap) abandons it.
    MaybeDollarOpen,
    /// Postgres only: inside a confirmed `$tag$ ... $tag$` body. No nested
    /// lexing -- literal text until the exact closing tag recurs.
    InDollarQuote,
}

/// SQLite trigger-body tracking: only active once the CURRENT pending
/// statement's leading bare words have matched `CREATE [TEMP|TEMPORARY]
/// TRIGGER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerLead {
    AwaitingCreate,
    AwaitingTempOrTrigger,
    AwaitingTrigger,
    Confirmed,
    /// Dead state for the rest of this pending statement -- Postgres always
    /// starts (and stays) here, since it never tracks trigger bodies.
    NotATrigger,
}

impl TriggerLead {
    fn initial(dialect: Dialect) -> Self {
        match dialect {
            Dialect::Sqlite => TriggerLead::AwaitingCreate,
            Dialect::Postgres => TriggerLead::NotATrigger,
        }
    }
}

/// Push-based incremental SQL statement splitter. See the module doc
/// comment and the G12 design doc §1 for the exact semantics.
#[derive(Debug)]
pub struct StatementSplitter {
    dialect: Dialect,
    mode: Mode,
    /// Exact source text of the statement currently being accumulated.
    stmt_buf: String,
    /// Whether `stmt_buf` contains at least one char that is neither
    /// whitespace nor comment content -- the "is this a phantom empty
    /// statement" signal (mirrors `guards::split_statements` dropping
    /// segments with no significant token).
    has_content: bool,
    /// Bare word currently being accumulated (uppercased at finalize time),
    /// for SQLite trigger-lead/BEGIN/END tracking. Carried across `push`
    /// calls so a keyword split mid-token still reconstructs correctly.
    word_buf: String,
    /// UTF-8 carry buffer for a multi-byte sequence split across chunks
    /// (<=3 bytes: the max continuation-byte run).
    carry: Vec<u8>,
    /// Postgres: candidate dollar-quote tag being scanned.
    dollar_tag_buf: String,
    /// Postgres: the confirmed closing sequence (`$tag$`) once a
    /// dollar-quote body is open.
    dollar_close_seq: Vec<char>,
    /// Postgres: how many chars of `dollar_close_seq` have been matched so
    /// far while scanning the body.
    dollar_match_len: usize,
    /// SQLite: has the pending statement's leading keywords matched
    /// `CREATE [TEMP|TEMPORARY] TRIGGER`?
    trigger_lead: TriggerLead,
    /// SQLite: trigger-body `BEGIN...END` nesting depth. A counter (not a
    /// bool), defensively mirroring `guards.rs`'s block-comment nesting fix,
    /// even though SQLite's own trigger grammar disallows nested bodies.
    trigger_depth: u32,
}

impl StatementSplitter {
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            mode: Mode::Normal,
            stmt_buf: String::new(),
            has_content: false,
            word_buf: String::new(),
            carry: Vec::new(),
            dollar_tag_buf: String::new(),
            dollar_close_seq: Vec::new(),
            dollar_match_len: 0,
            trigger_lead: TriggerLead::initial(dialect),
            trigger_depth: 0,
        }
    }

    /// Feed a chunk of bytes at any boundary. Returns statements that
    /// became complete as a result of this push, in order; an empty `Vec`
    /// means no statement completed yet.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, SplitError> {
        let mut bytes = std::mem::take(&mut self.carry);
        bytes.extend_from_slice(chunk);

        let (chars, carry): (Vec<char>, Vec<u8>) = match std::str::from_utf8(&bytes) {
            Ok(s) => (s.chars().collect(), Vec::new()),
            Err(e) => {
                if e.error_len().is_some() {
                    return Err(SplitError::InvalidUtf8);
                }
                let valid_up_to = e.valid_up_to();
                let tail_len = bytes.len() - valid_up_to;
                if tail_len > 3 {
                    return Err(SplitError::InvalidUtf8);
                }
                let s = std::str::from_utf8(&bytes[..valid_up_to])
                    .map_err(|_| SplitError::InvalidUtf8)?;
                (s.chars().collect(), bytes[valid_up_to..].to_vec())
            }
        };

        let mut out = Vec::new();
        for c in chars {
            match self.mode {
                Mode::Normal => self.feed_top_level(c, &mut out),
                Mode::InWord => self.handle_word_char(c, &mut out),
                Mode::SawDash => self.handle_saw_dash(c, &mut out),
                Mode::SawSlash => self.handle_saw_slash(c, &mut out),
                Mode::InSingleString => self.handle_in_single_string(c),
                Mode::SingleStringMaybeEnd => self.handle_single_string_maybe_end(c, &mut out),
                Mode::InDoubleIdent => self.handle_in_double_ident(c),
                Mode::DoubleIdentMaybeEnd => self.handle_double_ident_maybe_end(c, &mut out),
                Mode::InLineComment => self.handle_in_line_comment(c),
                Mode::InBlockComment(d) => self.handle_in_block_comment(c, d),
                Mode::BlockCommentMaybeOpen(d) => self.handle_block_comment_maybe_open(c, d),
                Mode::BlockCommentMaybeClose(d) => self.handle_block_comment_maybe_close(c, d),
                Mode::MaybeDollarOpen => self.handle_maybe_dollar_open(c, &mut out),
                Mode::InDollarQuote => self.handle_in_dollar_quote(c),
            }
        }

        self.carry = carry;
        Ok(out)
    }

    /// Call once after EOF. `Ok(Some(text))` = a final statement with no
    /// trailing `;` (legal). `Ok(None)` = nothing pending. `Err` = EOF
    /// occurred inside an open construct -- fail closed.
    pub fn finish(mut self) -> Result<Option<String>, SplitError> {
        if !self.carry.is_empty() {
            return Err(SplitError::InvalidUtf8);
        }

        // A word with no trailing delimiter never got finalized (e.g. a
        // trigger body's closing `END` with no trailing `;`) -- process it
        // now so trigger-depth bookkeeping reflects the whole input.
        if self.mode == Mode::InWord {
            self.finalize_word();
        }

        match self.mode {
            Mode::InSingleString => {
                return Err(SplitError::UnterminatedAtEof(UnterminatedKind::StringLiteral));
            }
            Mode::InDoubleIdent => {
                return Err(SplitError::UnterminatedAtEof(UnterminatedKind::QuotedIdent));
            }
            Mode::InBlockComment(_)
            | Mode::BlockCommentMaybeOpen(_)
            | Mode::BlockCommentMaybeClose(_) => {
                return Err(SplitError::UnterminatedAtEof(UnterminatedKind::BlockComment));
            }
            Mode::InDollarQuote => {
                return Err(SplitError::UnterminatedAtEof(UnterminatedKind::DollarQuote));
            }
            _ => {}
        }

        if self.trigger_depth > 0 {
            return Err(SplitError::UnterminatedAtEof(UnterminatedKind::TriggerBody));
        }

        // A dangling `-` or `/` at EOF never got to resolve into a comment
        // (that needs a second char) -- it's real content, e.g. `SELECT 1 -`.
        if self.mode == Mode::SawDash || self.mode == Mode::SawSlash {
            self.has_content = true;
        }

        if !self.has_content {
            return Ok(None);
        }
        let trimmed = self.stmt_buf.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    // -- top-level dispatch -------------------------------------------

    /// Handles one char in a "fresh" top-level context: not mid-word, not
    /// inside any string/ident/comment/dollar-quote. Appends `c` to
    /// `stmt_buf` (except a genuine statement-terminating `;`, which is
    /// consumed as a delimiter) and transitions `mode` accordingly.
    fn feed_top_level(&mut self, c: char, out: &mut Vec<String>) {
        if c == ';' && self.trigger_depth == 0 {
            self.emit_statement(out);
            return;
        }
        match c {
            '\'' => {
                self.stmt_buf.push(c);
                self.has_content = true;
                self.mode = Mode::InSingleString;
            }
            '"' => {
                self.stmt_buf.push(c);
                self.has_content = true;
                self.mode = Mode::InDoubleIdent;
            }
            '-' => {
                self.stmt_buf.push(c);
                self.mode = Mode::SawDash;
            }
            '/' => {
                self.stmt_buf.push(c);
                self.mode = Mode::SawSlash;
            }
            '$' if self.dialect == Dialect::Postgres => {
                self.stmt_buf.push(c);
                self.has_content = true;
                self.dollar_tag_buf.clear();
                self.mode = Mode::MaybeDollarOpen;
            }
            c if c.is_alphanumeric() || c == '_' => {
                self.stmt_buf.push(c);
                self.has_content = true;
                self.word_buf.clear();
                self.word_buf.push(c);
                self.mode = Mode::InWord;
            }
            c if c.is_whitespace() => {
                self.stmt_buf.push(c);
                self.mode = Mode::Normal;
            }
            _ => {
                self.stmt_buf.push(c);
                self.has_content = true;
                self.mode = Mode::Normal;
            }
        }
    }

    fn handle_word_char(&mut self, c: char, out: &mut Vec<String>) {
        if c.is_alphanumeric() || c == '_' {
            self.stmt_buf.push(c);
            self.word_buf.push(c);
        } else {
            self.finalize_word();
            self.feed_top_level(c, out);
        }
    }

    fn handle_saw_dash(&mut self, c: char, out: &mut Vec<String>) {
        if c == '-' {
            self.stmt_buf.push(c);
            self.mode = Mode::InLineComment;
        } else {
            self.has_content = true; // the earlier `-` was real content
            self.mode = Mode::Normal;
            self.feed_top_level(c, out);
        }
    }

    fn handle_saw_slash(&mut self, c: char, out: &mut Vec<String>) {
        if c == '*' {
            self.stmt_buf.push(c);
            self.mode = Mode::InBlockComment(1);
        } else {
            self.has_content = true; // the earlier `/` was real content
            self.mode = Mode::Normal;
            self.feed_top_level(c, out);
        }
    }

    fn handle_in_single_string(&mut self, c: char) {
        self.stmt_buf.push(c);
        if c == '\'' {
            self.mode = Mode::SingleStringMaybeEnd;
        }
    }

    fn handle_single_string_maybe_end(&mut self, c: char, out: &mut Vec<String>) {
        if c == '\'' {
            self.stmt_buf.push(c);
            self.mode = Mode::InSingleString;
        } else {
            self.mode = Mode::Normal;
            self.feed_top_level(c, out);
        }
    }

    fn handle_in_double_ident(&mut self, c: char) {
        self.stmt_buf.push(c);
        if c == '"' {
            self.mode = Mode::DoubleIdentMaybeEnd;
        }
    }

    fn handle_double_ident_maybe_end(&mut self, c: char, out: &mut Vec<String>) {
        if c == '"' {
            self.stmt_buf.push(c);
            self.mode = Mode::InDoubleIdent;
        } else {
            self.mode = Mode::Normal;
            self.feed_top_level(c, out);
        }
    }

    fn handle_in_line_comment(&mut self, c: char) {
        self.stmt_buf.push(c);
        if c == '\n' {
            self.mode = Mode::Normal;
        }
    }

    fn handle_in_block_comment(&mut self, c: char, depth: u32) {
        self.stmt_buf.push(c);
        self.mode = match c {
            '/' => Mode::BlockCommentMaybeOpen(depth),
            '*' => Mode::BlockCommentMaybeClose(depth),
            _ => Mode::InBlockComment(depth),
        };
    }

    fn handle_block_comment_maybe_open(&mut self, c: char, depth: u32) {
        self.stmt_buf.push(c);
        self.mode = match c {
            '*' => Mode::InBlockComment(depth + 1),
            '/' => Mode::BlockCommentMaybeOpen(depth),
            _ => Mode::InBlockComment(depth),
        };
    }

    fn handle_block_comment_maybe_close(&mut self, c: char, depth: u32) {
        self.stmt_buf.push(c);
        self.mode = match c {
            '/' => {
                let new_depth = depth.saturating_sub(1);
                if new_depth == 0 {
                    Mode::Normal
                } else {
                    Mode::InBlockComment(new_depth)
                }
            }
            '*' => Mode::BlockCommentMaybeClose(depth),
            _ => Mode::InBlockComment(depth),
        };
    }

    fn handle_maybe_dollar_open(&mut self, c: char, out: &mut Vec<String>) {
        if c == '$' {
            self.stmt_buf.push(c);
            self.dollar_close_seq = format!("${}$", self.dollar_tag_buf).chars().collect();
            self.dollar_tag_buf.clear();
            self.dollar_match_len = 0;
            self.mode = Mode::InDollarQuote;
            return;
        }
        let is_legal = if self.dollar_tag_buf.is_empty() {
            c.is_alphabetic() || c == '_'
        } else {
            c.is_alphanumeric() || c == '_'
        };
        if is_legal && self.dollar_tag_buf.chars().count() < 64 {
            self.stmt_buf.push(c);
            self.dollar_tag_buf.push(c);
        } else {
            // Abandon: the buffered `$` + tag chars are already in
            // stmt_buf as ordinary text. Resume fresh scanning from `c`.
            self.dollar_tag_buf.clear();
            self.mode = Mode::Normal;
            self.feed_top_level(c, out);
        }
    }

    fn handle_in_dollar_quote(&mut self, c: char) {
        self.stmt_buf.push(c);
        let seq = &self.dollar_close_seq;
        let new_len = if seq.get(self.dollar_match_len) == Some(&c) {
            self.dollar_match_len + 1
        } else if c == '$' {
            // Only `$` can ever restart a match: the tag itself can't
            // contain `$`, so it's the only char that appears at both the
            // start and end of the close sequence.
            1
        } else {
            0
        };
        if new_len == seq.len() {
            self.mode = Mode::Normal;
            self.dollar_match_len = 0;
            self.dollar_close_seq.clear();
        } else {
            self.dollar_match_len = new_len;
        }
    }

    // -- statement/word bookkeeping -------------------------------------

    fn emit_statement(&mut self, out: &mut Vec<String>) {
        let text = std::mem::take(&mut self.stmt_buf);
        if self.has_content {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        self.has_content = false;
        self.word_buf.clear();
        self.mode = Mode::Normal;
        self.trigger_lead = TriggerLead::initial(self.dialect);
        self.trigger_depth = 0;
    }

    fn finalize_word(&mut self) {
        if self.dialect == Dialect::Sqlite {
            let w = self.word_buf.to_uppercase();
            self.apply_trigger_word(&w);
        }
        self.word_buf.clear();
        self.mode = Mode::Normal;
    }

    fn apply_trigger_word(&mut self, w: &str) {
        match self.trigger_lead {
            TriggerLead::AwaitingCreate => {
                self.trigger_lead = if w == "CREATE" {
                    TriggerLead::AwaitingTempOrTrigger
                } else {
                    TriggerLead::NotATrigger
                };
            }
            TriggerLead::AwaitingTempOrTrigger => {
                self.trigger_lead = if w == "TEMP" || w == "TEMPORARY" {
                    TriggerLead::AwaitingTrigger
                } else if w == "TRIGGER" {
                    TriggerLead::Confirmed
                } else {
                    TriggerLead::NotATrigger
                };
            }
            TriggerLead::AwaitingTrigger => {
                self.trigger_lead = if w == "TRIGGER" {
                    TriggerLead::Confirmed
                } else {
                    TriggerLead::NotATrigger
                };
            }
            TriggerLead::Confirmed => {
                if w == "BEGIN" {
                    self.trigger_depth += 1;
                } else if w == "END" && self.trigger_depth > 0 {
                    self.trigger_depth -= 1;
                }
            }
            TriggerLead::NotATrigger => {}
        }
    }
}

/// One-shot convenience over an in-memory string: internally just `push` +
/// `finish`.
pub fn split_sql(sql: &str, dialect: Dialect) -> Result<Vec<String>, SplitError> {
    let mut splitter = StatementSplitter::new(dialect);
    let mut out = splitter.push(sql.as_bytes())?;
    if let Some(last) = splitter.finish()? {
        out.push(last);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_bytes_one_at_a_time(sql: &str, dialect: Dialect) -> Result<Vec<String>, SplitError> {
        let mut s = StatementSplitter::new(dialect);
        let mut out = Vec::new();
        for b in sql.as_bytes() {
            out.extend(s.push(&[*b])?);
        }
        if let Some(last) = s.finish()? {
            out.push(last);
        }
        Ok(out)
    }

    // ---------- Basic splitting ----------

    #[test]
    fn two_statements_with_trailing_semicolon() {
        assert_eq!(
            split_sql("SELECT 1; SELECT 2;", Dialect::Postgres).unwrap(),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn two_statements_without_trailing_semicolon() {
        assert_eq!(
            split_sql("SELECT 1; SELECT 2", Dialect::Postgres).unwrap(),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn consecutive_semicolons_collapse_to_nothing_extra() {
        assert_eq!(
            split_sql("SELECT 1;;;SELECT 2;", Dialect::Postgres).unwrap(),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn whitespace_and_comment_only_input_is_none() {
        assert_eq!(split_sql("   \n\t  ", Dialect::Postgres).unwrap(), Vec::<String>::new());
        assert_eq!(
            split_sql("   -- just a comment\n  ", Dialect::Postgres).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            split_sql("/* only a block comment */", Dialect::Postgres).unwrap(),
            Vec::<String>::new()
        );
    }

    // ---------- Strings/idents/comments (shared discipline with guards.rs) ----------

    #[test]
    fn semicolon_inside_single_quoted_string_is_not_a_split() {
        assert_eq!(
            split_sql("SELECT ';' AS x;", Dialect::Postgres).unwrap(),
            vec!["SELECT ';' AS x"]
        );
    }

    #[test]
    fn escaped_quote_inside_string_containing_semicolon() {
        assert_eq!(
            split_sql("SELECT 'it''s; still one' AS x;", Dialect::Postgres).unwrap(),
            vec!["SELECT 'it''s; still one' AS x"]
        );
    }

    #[test]
    fn semicolon_inside_double_quoted_ident() {
        assert_eq!(
            split_sql(r#"SELECT "a;b" FROM t;"#, Dialect::Postgres).unwrap(),
            vec![r#"SELECT "a;b" FROM t"#]
        );
    }

    #[test]
    fn semicolon_inside_line_comment_then_real_semicolon_splits() {
        assert_eq!(
            split_sql("SELECT 1 -- has a ; in it\n; SELECT 2;", Dialect::Postgres).unwrap(),
            vec!["SELECT 1 -- has a ; in it", "SELECT 2"]
        );
    }

    #[test]
    fn semicolon_inside_block_comment() {
        assert_eq!(
            split_sql("SELECT 1 /* has a ; in it */;", Dialect::Postgres).unwrap(),
            vec!["SELECT 1 /* has a ; in it */"]
        );
    }

    #[test]
    fn nested_block_comment_matches_guards_semantics() {
        // The real leading statement is `SELECT 1`, matching guards.rs's
        // own nested_block_comment_bypass_fails_closed test shape: the
        // whole `/* /* */ SELECT 1 */` is one comment.
        assert_eq!(
            split_sql("/* /* */ SELECT 1 */ UPDATE t SET a=1;", Dialect::Postgres).unwrap(),
            vec!["/* /* */ SELECT 1 */ UPDATE t SET a=1"]
        );
    }

    // ---------- Chunk-boundary safety ----------

    #[test]
    fn keyword_split_across_two_pushes() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        let mut out = s.push(b"SEL").unwrap();
        out.extend(s.push(b"ECT 1;").unwrap());
        assert_eq!(out, vec!["SELECT 1"]);
    }

    #[test]
    fn line_comment_marker_split_across_boundary() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        let mut out = s.push(b"SELECT 1 -").unwrap();
        out.extend(s.push(b"- comment\n;SELECT 2;").unwrap());
        assert_eq!(out, vec!["SELECT 1 -- comment", "SELECT 2"]);
    }

    #[test]
    fn block_comment_markers_split_across_boundary() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        let mut out = s.push(b"SELECT 1 /").unwrap();
        out.extend(s.push(b"* c1 *").unwrap());
        out.extend(s.push(b"/;").unwrap());
        assert_eq!(out, vec!["SELECT 1 /* c1 */"]);
    }

    #[test]
    fn escaped_quote_pair_split_across_boundary_does_not_falsely_close() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        let mut out = s.push(b"SELECT 'it'").unwrap();
        out.extend(s.push(b"'s';").unwrap());
        assert_eq!(out, vec!["SELECT 'it''s'"]);
    }

    #[test]
    fn multibyte_utf8_char_split_across_boundary() {
        let sql = "SELECT 'café';";
        let bytes = sql.as_bytes();
        // "café" -> 'é' is 2 bytes (0xC3 0xA9); split right in the middle of it.
        let split_at = sql.find('é').unwrap() + 1; // lands mid-char
        let mut s = StatementSplitter::new(Dialect::Postgres);
        let mut out = s.push(&bytes[..split_at]).unwrap();
        out.extend(s.push(&bytes[split_at..]).unwrap());
        assert_eq!(out, vec!["SELECT 'café'"]);
    }

    #[test]
    fn round_trip_one_push_vs_byte_by_byte_postgres() {
        let corpus = "SELECT 1; SELECT 'it''s; a café' AS x FROM \"weird;ident\" -- trailing ; comment\n; \
                      /* nested /* block */ comment */ SELECT 2;;; \
                      SELECT $$a;b$$ AS y; \
                      SELECT $tag$has 'quotes' and ; and BEGIN/END$tag$; \
                      SELECT $1, $2;";
        let one_shot = split_sql(corpus, Dialect::Postgres).unwrap();
        let bytewise = split_bytes_one_at_a_time(corpus, Dialect::Postgres).unwrap();
        assert_eq!(one_shot, bytewise);
        assert!(!one_shot.is_empty());
    }

    #[test]
    fn round_trip_one_push_vs_byte_by_byte_sqlite() {
        let corpus = "SELECT 1; SELECT 'it''s; a café' AS x FROM \"weird;ident\" -- trailing ; comment\n; \
                      /* nested /* block */ comment */ SELECT 2;;; \
                      CREATE TRIGGER trg AFTER INSERT ON t WHEN NEW.x > 1 BEGIN \
                          UPDATE t SET y = 1; DELETE FROM t WHERE y = 0; \
                      END; \
                      BEGIN; SELECT 3; COMMIT; \
                      SELECT $$literal$$;";
        let one_shot = split_sql(corpus, Dialect::Sqlite).unwrap();
        let bytewise = split_bytes_one_at_a_time(corpus, Dialect::Sqlite).unwrap();
        assert_eq!(one_shot, bytewise);
        assert!(!one_shot.is_empty());
    }

    // ---------- Postgres dollar-quoting ----------

    #[test]
    fn simple_dollar_quote_with_internal_semicolon() {
        assert_eq!(
            split_sql("SELECT $$a; b$$ AS x;", Dialect::Postgres).unwrap(),
            vec!["SELECT $$a; b$$ AS x"]
        );
    }

    #[test]
    fn tagged_dollar_quote_with_semicolons_begin_end_unbalanced_quotes() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $body$ \
                   BEGIN a := 'unbalanced; RETURN 1; END $body$ LANGUAGE plpgsql;";
        assert_eq!(split_sql(sql, Dialect::Postgres).unwrap(), vec![sql.trim_end_matches(';')]);
    }

    #[test]
    fn mismatched_tag_does_not_close_body() {
        let sql = "SELECT $foo$ contains a $bar$ marker $foo$ AS x;";
        assert_eq!(split_sql(sql, Dialect::Postgres).unwrap(), vec![sql.trim_end_matches(';')]);
    }

    #[test]
    fn two_independent_dollar_quoted_bodies_in_one_statement() {
        assert_eq!(
            split_sql("SELECT $$a$$, $$b$$;", Dialect::Postgres).unwrap(),
            vec!["SELECT $$a$$, $$b$$"]
        );
    }

    #[test]
    fn positional_params_are_not_mistaken_for_dollar_quotes() {
        assert_eq!(
            split_sql("SELECT $1, $2 FROM t WHERE a = $1;", Dialect::Postgres).unwrap(),
            vec!["SELECT $1, $2 FROM t WHERE a = $1"]
        );
    }

    #[test]
    fn unterminated_dollar_quote_at_eof() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        s.push(b"SELECT $$abc").unwrap();
        assert_eq!(
            s.finish(),
            Err(SplitError::UnterminatedAtEof(UnterminatedKind::DollarQuote))
        );
    }

    // ---------- SQLite triggers ----------

    #[test]
    fn trigger_body_with_one_interior_semicolon() {
        let sql = "CREATE TRIGGER trg AFTER INSERT ON t BEGIN UPDATE t SET x = 1; END;";
        assert_eq!(split_sql(sql, Dialect::Sqlite).unwrap(), vec![sql.trim_end_matches(';')]);
    }

    #[test]
    fn trigger_body_with_multiple_interior_statements() {
        let sql = "CREATE TRIGGER trg AFTER INSERT ON t BEGIN \
                   UPDATE t SET x = 1; DELETE FROM t WHERE x = 0; INSERT INTO log VALUES (1); \
                   END; SELECT 1;";
        let out = split_sql(sql, Dialect::Sqlite).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("CREATE TRIGGER"));
        assert!(out[0].trim_end().ends_with("END"));
        assert_eq!(out[1], "SELECT 1");
    }

    #[test]
    fn trigger_with_when_condition_does_not_confuse_tracking() {
        let sql = "CREATE TRIGGER trg AFTER UPDATE ON t WHEN NEW.x <> OLD.x BEGIN \
                   INSERT INTO log VALUES (NEW.x); END; SELECT 2;";
        let out = split_sql(sql, Dialect::Sqlite).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], "SELECT 2");
    }

    #[test]
    fn lowercase_create_trigger_is_case_insensitive() {
        let sql = "create trigger trg after insert on t begin update t set x = 1; end; select 1;";
        let out = split_sql(sql, Dialect::Sqlite).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], "select 1");
    }

    #[test]
    fn standalone_begin_commit_splits_normally() {
        assert_eq!(
            split_sql("BEGIN; SELECT 1; COMMIT;", Dialect::Sqlite).unwrap(),
            vec!["BEGIN", "SELECT 1", "COMMIT"]
        );
    }

    #[test]
    fn unterminated_trigger_body_at_eof() {
        let mut s = StatementSplitter::new(Dialect::Sqlite);
        s.push(b"CREATE TRIGGER trg AFTER INSERT ON t BEGIN UPDATE t SET x = 1;")
            .unwrap();
        assert_eq!(
            s.finish(),
            Err(SplitError::UnterminatedAtEof(UnterminatedKind::TriggerBody))
        );
    }

    // ---------- Dialect isolation ----------

    #[test]
    fn sqlite_treats_dollar_dollar_as_ordinary_text() {
        assert_eq!(
            split_sql("SELECT $$foo$$;", Dialect::Sqlite).unwrap(),
            vec!["SELECT $$foo$$"]
        );
    }

    #[test]
    fn postgres_applies_no_trigger_body_tracking() {
        // Without trigger-body tracking, the `;` after `SELECT 1` splits
        // normally instead of being absorbed into one BEGIN...END body.
        let out = split_sql(
            "CREATE TRIGGER trg BEFORE INSERT ON t BEGIN SELECT 1; END;",
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], "CREATE TRIGGER trg BEFORE INSERT ON t BEGIN SELECT 1");
        assert_eq!(out[1], "END");
    }

    // ---------- Invalid input ----------

    #[test]
    fn unterminated_string_literal_at_eof() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        s.push(b"SELECT 'abc").unwrap();
        assert_eq!(
            s.finish(),
            Err(SplitError::UnterminatedAtEof(UnterminatedKind::StringLiteral))
        );
    }

    #[test]
    fn unterminated_quoted_ident_at_eof() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        s.push(b"SELECT \"abc FROM t").unwrap();
        assert_eq!(
            s.finish(),
            Err(SplitError::UnterminatedAtEof(UnterminatedKind::QuotedIdent))
        );
    }

    #[test]
    fn unterminated_block_comment_at_eof() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        s.push(b"SELECT 1 /* unterminated").unwrap();
        assert_eq!(
            s.finish(),
            Err(SplitError::UnterminatedAtEof(UnterminatedKind::BlockComment))
        );
    }

    #[test]
    fn chunk_with_invalid_utf8_errors() {
        let mut s = StatementSplitter::new(Dialect::Postgres);
        let bad: &[u8] = &[b'S', b'E', b'L', 0xFF, 0x28];
        assert_eq!(s.push(bad), Err(SplitError::InvalidUtf8));
    }
}
