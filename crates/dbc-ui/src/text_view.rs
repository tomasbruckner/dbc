//! Mouse text selection for read-only text tabs — the pure half.
//!
//! The Log tab, the DDL tabs and every other `TabContent::Text` used to be
//! a stack of one `div` per line, and a `div` in GPUI has no notion of
//! selection: the only way to get anything out was the „Kopírovat" button,
//! which copies ALL of it. A user looking at a 64 KiB log tail to pull out
//! one ODBC error had to paste the whole file somewhere else first (user
//! report, 2026-08-30: „ten log nejde označit a zkopírovat").
//!
//! The renderer now draws the visible text as a single `StyledText`, which
//! gives two things a `div` cannot: `TextLayout::index_for_position`, so a
//! mouse position becomes a byte offset, and per-range `background_color`
//! highlights, so a selection can be painted. Everything BETWEEN those two
//! — which offsets are selected, which part of that is on screen, and what
//! ends up on the clipboard — is arithmetic, and lives here, unit-tested,
//! because GPUI has no headless harness in this repo and anything decided
//! inside a `render` can only be checked by hand.
//!
//! # The invariant that matters
//!
//! Every offset this module hands back is a CHAR BOUNDARY. `with_highlights`
//! carries a `debug_assert!(text.is_char_boundary(..))`, and `&text[a..b]`
//! panics outright — so a selection dragged across a Czech diacritic or an
//! emoji in a log line would take the window down. Snapping happens here,
//! once, rather than at each of the four call sites.

use std::ops::Range;

/// Normalise line endings for a text tab's body.
///
/// The previous renderer went through `str::lines()`, which quietly drops a
/// trailing `\r`. A single `StyledText` does not: server-supplied DDL with
/// CRLF would shape the carriage return as a visible control glyph at the
/// end of every line. Doing it once when the tab is built (rather than per
/// frame) also means every byte offset in this module indexes the same
/// string the user sees and copies.
pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Round `offset` down to the nearest char boundary, clamped to `text`.
pub fn snap(text: &str, offset: usize) -> usize {
    let mut o = offset.min(text.len());
    while o > 0 && !text.is_char_boundary(o) {
        o -= 1;
    }
    o
}

/// The selected range, low..high, whichever way the drag went.
pub fn ordered(text: &str, anchor: usize, head: usize) -> Range<usize> {
    let (a, h) = (snap(text, anchor), snap(text, head));
    if a <= h {
        a..h
    } else {
        h..a
    }
}

/// Byte range of the part of `text` that starts at line `scroll_lines`.
///
/// Scanning for `b'\n'` byte-wise is UTF-8 safe: a byte below 0x80 can
/// never occur inside a multi-byte sequence, so this cannot land mid-char.
pub fn visible_span(text: &str, scroll_lines: usize) -> Range<usize> {
    if scroll_lines == 0 {
        return 0..text.len();
    }
    let mut seen = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == scroll_lines {
                return (i + 1)..text.len();
            }
        }
    }
    // Scrolled past the end — nothing left to draw.
    text.len()..text.len()
}

/// `selection` expressed relative to the visible slice, or `None` when the
/// two do not overlap (the selection is scrolled off screen).
///
/// An EMPTY intersection is `None` on purpose: a zero-width highlight paints
/// nothing, and handing `with_highlights` a `4..4` run is a debug-assert
/// waiting to fire for no gain.
pub fn clip_to_visible(selection: &Range<usize>, visible: &Range<usize>) -> Option<Range<usize>> {
    let start = selection.start.max(visible.start);
    let end = selection.end.min(visible.end);
    if start >= end {
        return None;
    }
    Some((start - visible.start)..(end - visible.start))
}

/// What Ctrl+C puts on the clipboard.
///
/// An empty or collapsed selection means „the user has not chosen anything",
/// and the answer there is the whole text — the same thing the „Kopírovat"
/// button has always done, so the key and the button never disagree.
pub fn copy_text(text: &str, selection: Option<&Range<usize>>) -> String {
    match selection {
        Some(r) if r.start < r.end => {
            let start = snap(text, r.start);
            let end = snap(text, r.end);
            text[start..end].to_string()
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CZ: &str = "první řádek\ndruhý řádek\ntřetí";

    #[test]
    fn normalize_removes_carriage_returns_both_ways() {
        assert_eq!(normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize("nothing to do"), "nothing to do");
    }

    /// The whole reason this module exists as a separate, tested unit: a
    /// drag that lands mid-character must not be able to panic the app.
    #[test]
    fn snap_never_lands_inside_a_multibyte_char() {
        let s = "ř"; // 2 bytes
        assert_eq!(snap(s, 0), 0);
        assert_eq!(snap(s, 1), 0, "mid-char rounds down");
        assert_eq!(snap(s, 2), 2);
        assert_eq!(snap(s, 99), 2, "clamped to the end");
        let emoji = "a\u{1F600}b"; // 1 + 4 + 1
        for o in 0..=emoji.len() {
            assert!(emoji.is_char_boundary(snap(emoji, o)), "offset {o} did not snap");
        }
    }

    #[test]
    fn ordered_is_direction_free_and_snapped() {
        assert_eq!(ordered(CZ, 3, 9), 3..9);
        assert_eq!(ordered(CZ, 9, 3), 3..9, "dragged right-to-left");
        assert_eq!(ordered(CZ, 3, 3), 3..3, "a click is an empty range");
        // Byte 5 is inside 'í' (4..6) and byte 20 inside 'ý' (19..21) —
        // BOTH ends snap, in whichever order they arrive.
        assert_eq!(ordered(CZ, 5, 20), 4..19);
        assert_eq!(ordered(CZ, 20, 5), 4..19);
    }

    #[test]
    fn visible_span_skips_whole_lines() {
        assert_eq!(visible_span(CZ, 0), 0..CZ.len());
        let s = visible_span(CZ, 1);
        assert_eq!(&CZ[s.clone()], "druhý řádek\ntřetí");
        let s = visible_span(CZ, 2);
        assert_eq!(&CZ[s], "třetí");
    }

    #[test]
    fn visible_span_past_the_end_is_empty_not_a_panic() {
        let s = visible_span(CZ, 99);
        assert!(s.is_empty());
        assert_eq!(&CZ[s], "");
    }

    #[test]
    fn clip_returns_slice_local_coordinates() {
        let visible = 10..20;
        assert_eq!(clip_to_visible(&(12..15), &visible), Some(2..5));
        assert_eq!(clip_to_visible(&(5..15), &visible), Some(0..5), "clipped at the top");
        assert_eq!(clip_to_visible(&(15..99), &visible), Some(5..10), "clipped at the bottom");
        assert_eq!(clip_to_visible(&(0..30), &visible), Some(0..10), "spans the whole view");
    }

    #[test]
    fn clip_is_none_when_scrolled_away_or_collapsed() {
        let visible = 10..20;
        assert_eq!(clip_to_visible(&(0..5), &visible), None, "above");
        assert_eq!(clip_to_visible(&(25..30), &visible), None, "below");
        assert_eq!(clip_to_visible(&(0..10), &visible), None, "touching, not overlapping");
        assert_eq!(clip_to_visible(&(14..14), &visible), None, "empty selection paints nothing");
    }

    #[test]
    fn copy_takes_the_selection_when_there_is_one() {
        assert_eq!(copy_text(CZ, Some(&(0..6))), "první");
        let d = CZ.find("druhý").unwrap();
        assert_eq!(copy_text(CZ, Some(&(d..d + 6))), "druhý");
    }

    /// Ctrl+C with nothing selected must agree with the „Kopírovat" button.
    #[test]
    fn copy_falls_back_to_the_whole_text() {
        assert_eq!(copy_text(CZ, None), CZ);
        assert_eq!(copy_text(CZ, Some(&(7..7))), CZ, "a bare click selects nothing");
    }

    #[test]
    fn copy_snaps_a_mid_character_range() {
        // Byte 5 is inside the two-byte 'í'; slicing there would panic.
        assert_eq!(copy_text(CZ, Some(&(0..5))), "prvn");
    }
}
