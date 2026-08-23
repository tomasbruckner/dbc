# G6 Research: Tree-sitter SQL Highlighting for `SqlInput`

> Research-pass output (2026-08-23, subagent). Feeds the G6 design pass. Not a spec — verify crate APIs at implementation time.

## Recommended approach

Add tree-sitter as a small, self-contained highlighting layer bolted onto the existing `TextElement` render path in `sql_input.rs` — do **not** pull in Zed's `language`/`syntax_map`/`editor` machinery. On every buffer mutation, re-parse the *whole* buffer text with `tree-sitter` + the `tree-sitter-sequel` SQL grammar in a background task (`cx.background_spawn`), run the grammar's bundled `highlights.scm` query with a `tree_sitter::QueryCursor` to get `(byte_range, capture_name)` spans, resolve each capture name to a color via a small static `capture_name -> Hsla` table (no full `SyntaxTheme`/`HighlightMap` machinery needed for one language), stash the resulting `Vec<(Range<usize>, Hsla)>` on `SqlInput`, and have `TextElement::prepaint` intersect those spans with each visible line's byte range to build multiple `TextRun`s per line — generalizing the existing `build_runs`/`marked_local` splitting logic that already exists for the IME-marked-range underline. Full re-parse (not incremental `tree.edit`) is fine at this buffer size; background-spawning the parse is cheap insurance against large pastes and keeps the pattern identical to Zed's own reparse pipeline.

## Dependencies

| Crate | Version | License | Why |
|---|---|---|---|
| `tree-sitter` | `~0.25` (crates.io release, **not** git-pinned — independent of GPUI's tree-sitter git rev since `gpui` itself has zero tree-sitter dependency) | MIT | Core parser/Query/QueryCursor API |
| `tree-sitter-sequel` | `0.3.11` (published Oct 2025, actively maintained by DerekStride) | MIT | crates.io-published name for `DerekStride/tree-sitter-sql` — the same permissive SQL grammar Zed's own SQL extension and Helix use. Ships `LANGUAGE: LanguageFn`, converted via `tree_sitter::Language::new(tree_sitter_sequel::LANGUAGE)`. Depends on `tree-sitter-language ^0.1` (ABI shim), `tree-sitter ~0.25` only as dev-dep — doesn't force our `tree-sitter` version, just needs ABI 14+ (0.22+). |
| (stale, avoid) `tree-sitter-sql` on crates.io | `0.0.2`, 2018-era | MIT | Abandoned — do not use. |

Confirmed via `crates\gpui\Cargo.toml` in the vendored checkout: **`gpui` has no tree-sitter dependency at all** — no version-lockstep risk with pinned rev `907ed09`.

## Zed pipeline reference (shape to imitate, not copy)

Vendored checkout: `C:\Users\tomas\.cargo\git\checkouts\zed-a70e2ad075855582\907ed09\`

- `crates\language_core\src\grammar.rs:449-474` — `Grammar::with_highlights_query`: builds `tree_sitter::Query::new(&language, source)` from a `highlights.scm` string.
- `crates\language_core\src\highlight_map.rs:1-40` — `HighlightMap`: flat `Arc<[Option<HighlightId>]>` indexed by capture index. Our minimal version: `Vec<Option<Hsla>>`.
- `crates\language\src\language.rs:1188-1195` — `build_highlight_map(capture_names, theme)`: capture name → theme color slot resolution.
- `crates\syntax_theme\src\syntax_theme.rs:64-93` — `style_for_name`/`highlight_id`: longest-dotted-prefix matching (`function.method.call` → `function`). Can be a simple `match` for our single palette.
- `crates\language\src\buffer.rs:1847-1922` — `Buffer::reparse`: **the exact async shape to reuse** — `cx.background_spawn` the parse, then `cx.spawn(async move |this, cx| { ... this.update(...) })` back to the entity + `cx.notify()`. Coalescing via `parse_again` flag at `buffer.rs:1911-1917`.
- `crates\editor\src\element.rs:7186-7274` — `HighlightStyle` merged into base `TextStyle` → `TextRun` → `window.text_system().shape_line(text, font_size, &runs, None)`. Confirms `TextRun` + `shape_line` is the minimal target — identical to what `sql_input.rs` already does per-line.

## Integration sketch for `sql_input.rs` (illustrative)

1. New module `crates/dbc-ui/src/sql_highlight.rs`:

```rust
// Illustrative
pub struct HighlightSpan { pub range: Range<usize>, pub color: Hsla }

pub fn highlight(text: &str) -> Vec<HighlightSpan> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter::Language::new(tree_sitter_sequel::LANGUAGE)).unwrap();
    let tree = parser.parse(text, None).unwrap();
    let query = tree_sitter::Query::new(&parser.language().unwrap(), HIGHLIGHTS_SCM).unwrap();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut spans = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let name = query.capture_names()[cap.index as usize];
            if let Some(color) = color_for_capture(name) {
                spans.push(HighlightSpan { range: cap.node.byte_range(), color });
            }
        }
    }
    spans
}
```

`HIGHLIGHTS_SCM` = the grammar's bundled query (`tree_sitter_sequel::HIGHLIGHTS_QUERY`) or a locally-vendored trimmed copy for control over capture names.

2. `SqlInput` gains `highlights: Vec<HighlightSpan>` + `highlight_task: Option<Task<()>>`. Every mutating op (insert/backspace/delete/paste/set_text/IME replace — same call sites that set `follow_cursor = true`) kicks:

```rust
// Illustrative, mirrors Buffer::reparse
let text = self.buffer.text().to_string();
self.highlight_task = Some(cx.spawn(async move |this, cx| {
    let spans = cx.background_spawn(async move { sql_highlight::highlight(&text) }).await;
    this.update(cx, |this, cx| {
        this.highlights = spans;
        this.highlight_task = None;
        cx.notify();
    }).ok();
}));
```

3. In `TextElement::prepaint` (`sql_input.rs:791-863`), generalize `build_runs` (`sql_input.rs:173-205`) from "0–1 marked sub-ranges" to "N colored sub-ranges plus the marked range" — each an own `TextRun { len, color, .. }` before the single `shape_line` call. Scroll/selection/cursor/hit-testing unchanged.

## Risks / open questions for the design pass

- **Grammar choice**: `tree-sitter-sequel` is third-party single-maintainer; verify its `highlights.scm` capture set before committing; may need a vendored trimmed query.
- **Dialect coverage**: general SQL, not T-SQL — `TOP`, `OUTER APPLY`, `[col]` may parse as ERROR nodes; must degrade to "no highlighting for that span", never a hard failure.
- **Debounce/coalescing**: per-keystroke background parse likely fine at few-KB buffers; copy `parse_again` coalescing if typing causes task pile-up.
- **Stale-highlight flicker**: async parse → a few frames may show highlights from previous text. Zed accepts this (optional small sync-block via `sync_parse_timeout`); decide in design pass.
- **Capture-name → color table** hand-maintained ("no theming system yet" simplification).
- **API drift**: `tree-sitter-sequel`'s exact `Language`-construction (`LanguageFn` vs `Language`) must be re-checked on docs.rs at implementation — ABI wrapper conventions changed across tree-sitter 0.22–0.25. No build was attempted in this research pass.
