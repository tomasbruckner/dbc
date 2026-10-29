//! The rail that keeps the English UI complete (2026-10-27).
//!
//! Every string literal with Czech diacritics in the crates whose text
//! reaches the screen must be the FIRST argument of `tr!`/`trf!` — the
//! Czech half of a pair whose English half sits right after it. A new
//! Czech-only message fails this test instead of quietly showing up in
//! Czech for someone who picked English.
//!
//! What it cannot see: Czech written without diacritics („Analyzovat",
//! „Hledat"). Those are caught by review, not by this test — the
//! diacritics are what make the check cheap and free of false alarms.
//!
//! An intentional exception (a literal that is data, not UI: a parser
//! keyword, a test fixture outside `#[cfg(test)]`, a bilingual label that
//! is the same in both languages) carries `i18n-ok: <why>` in a comment on
//! the same line or the line right above it.

#![cfg(test)]

use std::path::{Path, PathBuf};

const CZ: &str = "ěščřžýáíéůúťďňĚŠČŘŽÝÁÍÉŮÚŤĎŇ";

fn has_czech(s: &str) -> bool {
    s.chars().any(|c| CZ.contains(c))
}

/// One string literal: its content, its byte offset in the source, and the
/// 1-based line it starts on.
#[derive(Debug)]
struct Lit {
    text: String,
    start: usize,
    line: usize,
}

/// A tiny Rust lexer — just enough to tell code, comments and string
/// literals apart. Returns the source with comments AND string contents
/// blanked to spaces (so brace matching and `tr!(` lookbehind run on code
/// only), the literals, and the comment text per line (for `i18n-ok`).
fn lex(src: &str) -> (Vec<u8>, Vec<Lit>, Vec<String>) {
    let b = src.as_bytes();
    let mut code = b.to_vec();
    let mut lits = Vec::new();
    let nlines = src.lines().count() + 2;
    let mut comments = vec![String::new(); nlines];
    let mut line = 1usize;
    let mut i = 0usize;
    let blank = |code: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut code[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            i += 1;
        } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
            let end = src[i..].find('\n').map(|n| i + n).unwrap_or(b.len());
            comments[line].push_str(&src[i..end]);
            blank(&mut code, i, end);
            i = end;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            let start = i;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if b[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            comments[line].push_str(&src[start..i]);
            blank(&mut code, start, i);
        } else if c == b'r' && matches!(b.get(i + 1), Some(b'"') | Some(b'#')) && !is_ident(b, i) {
            // Raw string r"…" / r#"…"#.
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) != Some(&b'"') {
                i += 1;
                continue;
            }
            let body = j + 1;
            let close: String = std::iter::once('"').chain(std::iter::repeat('#').take(hashes)).collect();
            let end = src[body..].find(&close).map(|n| body + n).unwrap_or(b.len());
            lits.push(Lit { text: src[body..end].to_string(), start: i, line });
            line += src[body..end].matches('\n').count();
            blank(&mut code, i, (end + close.len()).min(b.len()));
            i = end + close.len();
        } else if c == b'"' {
            let start = i;
            let start_line = line;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            lits.push(Lit { text: src[start + 1..i.min(b.len())].to_string(), start, line: start_line });
            blank(&mut code, start, (i + 1).min(b.len()));
            i += 1;
        } else if c == b'\'' {
            // Char literal ('x', '\n', '"', '↵') or a lifetime ('a).
            if b.get(i + 1) == Some(&b'\\') {
                let end = src[i + 2..].find('\'').map(|n| i + 2 + n + 1).unwrap_or(b.len());
                blank(&mut code, i, end);
                i = end;
            } else {
                let ch_len = src[i + 1..].chars().next().map(char::len_utf8).unwrap_or(1);
                if b.get(i + 1 + ch_len) == Some(&b'\'') {
                    blank(&mut code, i, i + 2 + ch_len);
                    i += 2 + ch_len;
                } else {
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    (code, lits, comments)
}

fn is_ident(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
}

/// Byte ranges of `#[cfg(test)]` items, found on the blanked code.
fn test_ranges(code: &[u8]) -> Vec<(usize, usize)> {
    let s = String::from_utf8_lossy(code);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(n) = s[from..].find("#[cfg(test)]") {
        let start = from + n;
        let mut j = start + "#[cfg(test)]".len();
        let bytes = s.as_bytes();
        while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'{' {
            let mut depth = 0i32;
            while j < bytes.len() {
                match bytes[j] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        out.push((start, j));
        from = j.max(start + 1);
    }
    out
}

/// Is the literal at `start` the first argument of `tr!(` / `trf!(`?
fn is_tr_first_arg(code: &[u8], start: usize) -> bool {
    let before = String::from_utf8_lossy(&code[..start]);
    let t = before.trim_end();
    t.ends_with("tr!(") || t.ends_with("trf!(")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The audited crates: everything whose text can end up on screen in the
/// app. The CLI and the MCP server are not the app's UI.
fn audited_files() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let mut files = Vec::new();
    for c in ["dbc-ui", "dbc-state", "dbc-connect"] {
        rs_files(&crates.join(c).join("src"), &mut files);
    }
    files.sort();
    files
}

/// `(file:line, literal)` for every Czech literal outside `tr!`/`trf!`.
fn offenders() -> (Vec<String>, usize) {
    let mut bad = Vec::new();
    let mut wrapped = 0;
    for f in audited_files() {
        if f.ends_with("i18n_audit.rs") {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        let (code, lits, comments) = lex(&src);
        let tests = test_ranges(&code);
        for lit in lits {
            if !has_czech(&lit.text) || tests.iter().any(|&(a, b)| lit.start >= a && lit.start < b) {
                continue;
            }
            if is_tr_first_arg(&code, lit.start) {
                wrapped += 1;
                continue;
            }
            let ok = |l: usize| comments.get(l).is_some_and(|c| c.contains("i18n-ok"));
            if ok(lit.line) || ok(lit.line.saturating_sub(1)) {
                continue;
            }
            let short: String = lit.text.chars().take(70).collect();
            bad.push(format!("{}:{}: \"{}\"", f.display(), lit.line, short));
        }
    }
    (bad, wrapped)
}

#[test]
fn every_czech_ui_text_has_an_english_twin() {
    let (bad, _) = offenders();
    assert!(
        bad.is_empty(),
        "{} Czech literal(s) outside tr!/trf! — wrap them (or mark `i18n-ok: <why>`):\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// Non-vacuity: the scan must actually be seeing the wrapped texts, or a
/// broken lexer would pass the test above by finding nothing at all.
#[test]
fn the_audit_sees_the_translated_texts() {
    let (_, wrapped) = offenders();
    assert!(wrapped > 500, "only {wrapped} tr!/trf! literals found — the lexer is broken");
}

#[test]
fn the_lexer_tells_code_comments_and_strings_apart() {
    let src = r##"
fn a() {
    let x = tr!("Uložit", "Save"); // komentář „Uložit"
    let y = "Zrušit"; // i18n-ok: test
    let z = "Smazat";
    let c = '"';
    let r = r#"Přidat"#;
    /* "Blok" s „č" */
}
#[cfg(test)]
mod tests {
    fn t() { let _ = "Testovací"; }
}
"##;
    let (code, lits, comments) = lex(src);
    let tests = test_ranges(&code);
    let texts: Vec<&str> = lits.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["Uložit", "Save", "Zrušit", "Smazat", "Přidat", "Testovací"]);
    assert!(is_tr_first_arg(&code, lits[0].start));
    assert!(!is_tr_first_arg(&code, lits[1].start), "the English half is not the first arg");
    assert!(!is_tr_first_arg(&code, lits[3].start));
    assert!(comments[4].contains("i18n-ok"));
    assert!(tests.iter().any(|&(a, b)| lits[5].start >= a && lits[5].start < b));
    assert!(!tests.iter().any(|&(a, b)| lits[3].start >= a && lits[3].start < b));
}
