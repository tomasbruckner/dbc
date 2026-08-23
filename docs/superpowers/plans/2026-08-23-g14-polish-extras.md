# G14 Polish & Extras Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A semantic theme system (dark = today's Catppuccin-Mocha values verbatim, light = a new contrast-checked palette, live toggle persisted in config.toml) replacing every hardcoded color literal in `dbc-ui`, plus static bar/line charts opened as a new result tab over the current grid's `ResultBuffer` snapshot — no external chart crate, painted with the same `paint_quad`/`PathBuilder`/`paint_path` primitives the ER diagram already uses.

**Architecture:** One new `crates/dbc-ui/src/theme.rs` module (a flat `Theme` struct of `Hsla` fields + nested `EditorSyntaxTheme`, installed once as a GPUI `Global`, read everywhere via a tiny `ActiveTheme` extension trait on `App`); a mechanical per-file sweep of all ~344 `rgb(0x…)`/`rgba(0x…)` literal occurrences across 11 files mapping each literal to its semantic field by role; `dbc_state::AppConfig` gains a `ThemeMode` field (`#[serde(default)]`, GPUI-free — the config stores the *choice*, never a color). Charts are two new files: `chart_data.rs` (pure, GPUI-free data prep + scale math, fully unit-tested) and `chart_view.rs` (a GPUI view painting bars/lines/axes/tick labels inside a `canvas()`, exact same idiom as `er_diagram_view.rs`), wired in as `TabContent::Chart`, a `ModalState::ChartPicker` axis-pick dialog, a grid-toolbar "Graf" button, and a gated palette action.

**Tech Stack:** Rust, GPUI (pinned rev `907ed09c9f4476caf250e6ce4bbffb23b4622f3b`), `dbc-state` (TOML config persistence, existing), `dbc-buffer::ResultBuffer` (existing), Arrow `DataType::is_numeric` (existing detector, reused). **No new crate dependency anywhere in this phase.**

**Spec:** `docs/superpowers/specs/drafts/g14-polish-extras-design.md` — the CURATION block (top of file, dated 2026-08-23) is binding. The four curation points and how this plan satisfies each:
1. **G6 shipped — `EditorSyntaxTheme` grounds in the shipped hex values.** `sql_highlight.rs::color_for_capture` (lines 108–118) ships keyword `0xcba6f7` (mauve), string `0xa6e3a1` (green), comment `0x6c7086` (overlay gray), type/type.builtin `0x94e2d5` (teal), number `0xfab387` (peach), function.call `0x89b4fa` (blue). `EditorSyntaxTheme` therefore has `function` and `type_` fields, its DARK defaults are those six hex values **verbatim** (enforced by a dedicated test in Task 1 — NOT "derives from accent"), Task 6 migrates `color_for_capture` to read the theme, and light-mode syntax values are hand-picked with a contrast check (encoded as a unit test, Task 1).
2. **§0's site count is a snapshot, treated as method.** Re-run at plan-writing time (2026-08-23, v0.10.0 main): ~344 literal occurrences on 317 lines across **11** files (`connections_ui.rs` 76 lines, `main.rs` 58, `grid.rs` 56, `compare.rs` 45, `monitor_view.rs` 28, `plan.rs` 15, `schema_tree.rs` 12, `er_diagram_view.rs` 8 + 6 named `u32` consts, `history_panel.rs` 7, `sql_input.rs` 6, `sql_highlight.rs` 6) — the design's 136/6 predates G6–G13. Three genuinely new roles found → three new `Theme` fields (`bg_warn_banner`, `bg_deep`, `accent_alt`); everything else maps onto the design's field set (see The Sweep Rulebook).
3. **Chart row cap is width-derived:** `visible_ticks(total, plot_width_px)` = `min(total, floor(plot_width_px / 3), 500)` — `MIN_PX_PER_TICK = 3.0`, `CHART_ROW_HARD_CAP = 500` (Task 7).
4. **Read-only phase: zero `execute()` surface.** Charts only read an already-materialized `ResultBuffer`; the theme only touches render code and config. `runner.rs` is NOT modified by any task in this plan; no confirm modal, no write path, no read-only-guard interaction anywhere.

## Global Constraints

- cargo is ALWAYS invoked as `%USERPROFILE%\.cargo\bin\cargo.exe` with `-p <crate>`; zero warnings allowed in both `build -p` and `test -p`.
- GPUI is git-pinned rev 907ed09 — no dependency bumps.
- Security rules: passwords only in the Argon2id vault, never plaintext on disk/logs; config.toml must never contain secrets; read-only flag blocks all write paths via the shared guard; history stores only SQL/connection-name/statistics, never result data or credentials. (This phase writes only a `theme = "dark"|"light"` string to config.toml — no secret can enter through any code path added here.)
- §3-novela write invariant: any write path is either a confirm modal showing exact SQL or an explicitly sanctioned runner-owned method with transactional discipline, AND must pass the shared read-only guard. (This phase adds NO write path — CURATION item 4; stated for completeness and so reviewers can reject any drift toward one.)
- Deep-recursion hazard class: any tree-like structure must use iterative build/drop/flatten; no derived Clone/Debug/PartialEq on deep node types; never assert_eq! full deep trees. (`Theme`/`ChartData` are flat — derives are fine and used; nothing in this phase builds a tree.)
- runner.rs and main.rs are single-writer files — tasks touching them must be marked SERIALIZED in the task ordering. (runner.rs: untouched this phase. main.rs: Tasks 1, 9, 10, 11, 12 — see the dependency table.)
- Tests: plain `#[test]` with `QueryRunner::handle().block_on(...)` for async (never `#[tokio::test]`); docker integration tests are `#[ignore]`-gated in-crate `mod X_pg_tests` using testcontainers-modules, `Postgres::default().with_tag("16.13")`, `open_spec`. (This phase needs no async and no docker tests — every new test is a plain synchronous `#[test]`.)
- UI strings are Czech (labels, statuses, tooltips) — English only in code/comments/tests (house convention, every G-phase plan restates it).
- Line references in this plan are against `main` at v0.10.0 (commit at plan-writing time). Other phases (G11/G12/G13 tails) may merge first — **re-locate by symbol name, not line number**, after any rebase.

## Grounding corrections (design assumptions vs. actual code — all verified on main at v0.10.0)

1. **`cx.refresh()` does not exist at the pinned rev.** The design's §1.2/§1.5 toggle mechanism says `cx.refresh()`; the actual API at rev 907ed09 is `App::refresh_windows()` (`crates/gpui/src/app.rs:1056` in the vendored checkout `C:\Users\tomas\.cargo\git\checkouts\zed-a70e2ad075855582\907ed09\`). Everything else the design claims about globals is confirmed: `pub trait Global` (global.rs:22), `App::global::<G>()` (app.rs:2006), `App::set_global` (app.rs:2044). All toggle code in this plan uses `cx.refresh_windows()`.
2. **`highlight()` runs on a background thread** (`sql_input.rs:439`: `cx.background_spawn(async move { sql_highlight::highlight(&text) })`) — a background task cannot read a GPUI global. The curation-mandated migration therefore changes the signature to `highlight(text: &str, syntax: &EditorSyntaxTheme)`; the caller captures `let syntax = cx.theme().syntax;` (the struct is `Copy` + `Send`) before the spawn and moves it in (Task 6).
3. **`TabContent` grew Entity-only variants since the design.** The shipped convention (tabs.rs:29–57) is `Monitor { view }`, `Plan { view }`, `Diagram { view }`, `Compare { view }` — a single typed `Entity` handle. This plan's `Chart { view: Entity<ChartView> }` follows that precedent instead of the design §2.6's three-field `Chart { chart, buffer, source_tab_id }`: the buffer `Rc` and the source-title metadata live inside `ChartView` (same information, shipped shape). `source_tab_id` as a live id is dropped — the design itself says it's "metadata only" for the subtitle, so `ChartView` stores the source tab's *title string* instead, which survives the source tab closing exactly as §2.6 requires.
4. **Three color roles exist that the design's audit predates** (all mapped in The Sweep Rulebook): a desaturated yellow "warn banner" background (`0x3a3a1e` at compare.rs:597 and main.rs:3910, `0x2a2a1e` at plan.rs:1636 — collapsed into ONE field `bg_warn_banner`, role-identical), a darker-than-app recessed well (`0x11111b`, monitor_view.rs:725) → `bg_deep`, and the ER diagram's pink selected-node accent (`ACCENT_COLOR = 0xf5c2e7`, er_diagram_view.rs:37) → `accent_alt`. Also: `compare.rs`'s `TINT_ADDED/TINT_REMOVED/TINT_CHANGED` consts (compare.rs:42–44) are the *same hex values* as grid.rs's G5 diff consts — they map to the same `diff_inserted_bg`/`diff_deleted_bg`/`diff_staged_bg` fields, not new ones.
5. **Two design light-palette values fail their own contrast requirement.** Design §1.4 claims the light accents clear WCAG AA on white, §5 flags this as needs-verification. Verified: `warn` `#a8791a` is ~3.9:1 and `success` `#1f8a4c` is ~4.4:1 against `#ffffff` — both under 4.5. This plan ships corrected values — `warn` light `#8a6210` (~5.4:1), `success` light `#187741` (~5.6:1) — and encodes the check as a unit test so it can never regress (Task 1). All other §1.4 values verified passing and kept verbatim.
6. **`ff0000`/`00ff00`/`0000ff` literals in `sql_input.rs` are `#[cfg(test)]`-only** (lines 1404–1421, arbitrary distinct colors for span-merge tests) — exempt from the sweep and from the final grep audit (test modules may use arbitrary colors; only *render* code must be literal-free).
7. **No app-settings dialog exists** (confirmed: `ModalState`, connections_ui.rs:892, has `ConnectionDialog`/`MasterPasswordPrompt`/`CreateMasterPassword`/`QueryParams`/`KillConfirm`/`AnalyzeWriteConfirm`/`CompareDialog` — nothing app-level). Task 10 adds the design's minimal `ModalState::Settings` arm; the topbar hook point is `render_top_bar` (connections_ui.rs:1011).
8. **`prepare()`'s Y-cell input is `Vec<Option<String>>`, not `Vec<String>`** (deviation from design §2.2's sketch): `ResultBuffer::cell_is_null` exists and is the honest NULL signal — encoding NULL as a magic string and re-parsing it would be exactly the "silently coerced" bug §2.2 forbids. `None` = NULL cell; `Some(text)` that fails strict `f64` parse also becomes a gap.

### Task dependency graph

| Task | Deliverable | Files | Depends on | Ordering class |
|---|---|---|---|---|
| 1 | Theme foundation: `theme.rs`, `ThemeMode` config, startup wiring | `crates/dbc-ui/src/theme.rs` (new), `crates/dbc-state/src/config.rs`, `crates/dbc-state/src/lib.rs`, `crates/dbc-ui/src/main.rs` | — | **SERIALIZED** (main.rs), lands FIRST |
| 2 | Color sweep: `grid.rs` | `crates/dbc-ui/src/grid.rs` | 1 | parallel |
| 3 | Color sweep: `connections_ui.rs` | `crates/dbc-ui/src/connections_ui.rs` | 1 | parallel |
| 4 | Color sweep: `compare.rs` + `monitor_view.rs` + `plan.rs` | those three files | 1 | parallel |
| 5 | Color sweep: `schema_tree.rs` + `history_panel.rs` + `er_diagram_view.rs` | those three files | 1 | parallel |
| 6 | Syntax-theme migration: `sql_highlight.rs` + `sql_input.rs` sweep | `crates/dbc-ui/src/sql_highlight.rs`, `crates/dbc-ui/src/sql_input.rs` | 1 | parallel |
| 7 | `chart_data.rs` pure module | `crates/dbc-ui/src/chart_data.rs` (new), `crates/dbc-ui/src/main.rs` (one `mod` line) | — | parallel (one-line main.rs edit, G11-T2 precedent) |
| 8 | `chart_view.rs` GPUI view | `crates/dbc-ui/src/chart_view.rs` (new), `crates/dbc-ui/src/main.rs` (one `mod` line) | 1, 7 | parallel |
| 9 | Color sweep: `main.rs` | `crates/dbc-ui/src/main.rs` | 1 | **SERIALIZED** |
| 10 | Theme toggle UX: settings modal, topbar gear, palette action | `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/palette.rs` | 1–6, 9 | **SERIALIZED** |
| 11 | Chart wiring: tab kind, picker modal, grid button, palette action | `crates/dbc-ui/src/main.rs`, `crates/dbc-ui/src/tabs.rs`, `crates/dbc-ui/src/grid.rs`, `crates/dbc-ui/src/connections_ui.rs`, `crates/dbc-ui/src/palette.rs` | 2, 7, 8, 10 | **SERIALIZED** |
| 12 | Final grep audit + full test pass + version bump | `crates/dbc-ui/Cargo.toml` | all | **SERIALIZED**, last |

**Execution order:** Task 1 first, alone (it is the only writer of `main.rs` in its window and every sweep needs the `Theme` type). Then fan out Tasks 2–8 to parallel worktrees (disjoint files; Task 8 additionally waits for Task 7 within its worktree — run 7→8 as one worker's sequence). Then the serialized `main.rs` tail in this exact order: Task 9 → Task 10 → Task 11 → Task 12. Tasks 10 and 11 both touch `palette.rs`/`connections_ui.rs`/`main.rs` and Task 11 touches `grid.rs` (also swept in Task 2) — hence both are tail-serialized, never parallel with the sweeps or each other. Cross-phase: if any other in-flight phase still has unmerged `main.rs` work, rebase before Task 9 and re-locate all `main.rs` references by symbol.

## The Sweep Rulebook (referenced by Tasks 2–6 and 9; authoritative hex→field mapping)

**Mechanical rule per site:** `rgb(0xHHHHHH)` / `rgba(0xHHHHHHAA)` → `cx.theme().<field>` where a `Context` (or `&mut App`) is in scope; inside a free helper function that has neither, thread a `&Theme` (or the individual `Hsla`) parameter in from the nearest caller that has `cx`. In GPUI `Element::paint`/`canvas` closures the `&mut App` parameter is in scope — `app.theme().<field>` works there. This is a rename, not a redesign: **no color VALUE changes anywhere in the sweeps** (values change only when the light palette is selected at runtime).

| dark hex (today) | `Theme` field | role notes |
|---|---|---|
| `0x181825` | `bg_app` | topbars, tab strip, gutters |
| `0x1e1e2e` | `bg_panel` | dialog/panel/grid base bg |
| `0x232334` | `bg_panel_alt` | alternating grid row (grid.rs:2654 — see Task 2 zebra check) |
| `0x313244` | `bg_hover` | hover / inactive-selected bg; also `NODE_FILL` (er_diagram_view.rs:32) |
| `0x45475a` | `bg_selected` **or** `border` | **AMBIGUOUS — resolve by role at each site**: `.border_color(rgb(0x45475a))` → `border`; `.bg(rgb(0x45475a))` on a selected row/cell/deeper hover → `bg_selected`. Also `NODE_BORDER` (er_diagram_view.rs:33) → `border`. |
| `0x585b70` | `bg_find_match` | grid find-match cell bg (grid.rs:2839) |
| `0x2a2a3d` | `bg_joined_col` | FK-joined column tint |
| `0x11111b` | `bg_deep` | recessed chart well (monitor_view.rs:725) — NEW field |
| `0x3a3a1e`, `0x2a2a1e` | `bg_warn_banner` | yellow notice-banner bg (compare.rs:597, main.rs:3910, plan.rs:1636) — NEW field; the two near-identical one-offs collapse into one |
| `rgba(0x00000099)` | `bg_backdrop` | modal backdrop (alpha) |
| `rgba(0x3311ff30)` | `bg_selection` | editor text-selection (alpha) |
| `0xcdd6f4` | `text_primary` | also `TEXT_COLOR` (er_diagram_view.rs:34) |
| `0xa6adc8` | `text_muted` | |
| `0x7f849c` | `text_faint` | one site, connections_ui.rs:1036 |
| `0x6c7086` | `text_disabled` | also `MUTED_COLOR` (er_diagram_view.rs:35) |
| `0x89b4fa` | `accent` | links/active indicators; also `EDGE_COLOR` (er_diagram_view.rs:36) |
| `0xf5c2e7` | `accent_alt` | ER selected-node border (`ACCENT_COLOR`, er_diagram_view.rs:37) — NEW field |
| `0xf9e2af` | `warn` | star/favourite/warn text |
| `0xf38ba8` | `danger` | errors, delete affordances |
| `0xa6e3a1` | `success` | boolean-true, success text |
| `0x6b5d2e` | `diff_staged_bg` | `STAGED_CELL_BG` (grid.rs:26) AND `TINT_CHANGED` (compare.rs:44) |
| `0x5d2e2e` | `diff_deleted_bg` | `DELETED_ROW_BG` (grid.rs:27), `TINT_REMOVED` (compare.rs:43), and the two direct connections_ui.rs sites (2303, 2415 — the comment there literally says "DELETED_ROW_BG family") |
| `0x2e5d3a` | `diff_inserted_bg` | `INSERTED_ROW_BG` (grid.rs:28) AND `TINT_ADDED` (compare.rs:42) |
| `0xcba6f7`/`0xa6e3a1`/`0xfab387`/`0x6c7086`/`0x89b4fa`/`0x94e2d5` in `sql_highlight.rs` only | `syntax.keyword`/`.string`/`.number`/`.comment`/`.function`/`.type_` | Task 6 only — signature change, not a call-site sweep |

**Named-const removal:** every `const *: u32` color const listed above is DELETED in its file's sweep task and its uses replaced by the theme field (or an `Hsla` parameter threaded into the paint helper). **Test-module exemption:** `#[cfg(test)]` code may keep arbitrary color literals (sql_input.rs:1404–1421).

**Per-sweep-task verification steps (identical for Tasks 2–6 and 9, written out in each):** build+test with zero warnings, a file-scoped grep proving zero production literals remain, and a visual sanity launch eyeballing the file's screens in dark mode (which must look pixel-identical to before).

---

### Task 1: Theme foundation — `theme.rs`, `ThemeMode` persistence, startup global

**Files:**
- Create: `crates/dbc-ui/src/theme.rs`
- Modify: `crates/dbc-state/src/config.rs` (add `ThemeMode` + `AppConfig.theme` field + tests)
- Modify: `crates/dbc-state/src/lib.rs` (export `ThemeMode` from the `pub use config::{…}` list at line 2)
- Modify: `crates/dbc-ui/src/main.rs` (add `mod theme;` to the module list; one `cx.set_global` call in the `application().run` closure, main.rs:4352)

**Interfaces:**
- Consumes: `gpui::{Global, Hsla, rgb, rgba, App}`, `serde` (dbc-state side only — `theme.rs` itself has no serde).
- Produces (relied on by every other task):

```rust
// dbc-state (GPUI-free — stores the CHOICE, never a color):
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode { #[default] Dark, Light }
// AppConfig gains: #[serde(default)] pub theme: ThemeMode,

// dbc-ui theme.rs:
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditorSyntaxTheme {
    pub keyword: Hsla, pub string: Hsla, pub number: Hsla, pub comment: Hsla,
    pub function: Hsla, pub type_: Hsla, pub identifier: Hsla,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme { /* 24 top-level fields, see Step 3 */ pub syntax: EditorSyntaxTheme, /* … */ }
impl gpui::Global for Theme {}
impl Theme {
    pub fn dark() -> Theme;
    pub fn light() -> Theme;
    pub fn from_mode(mode: dbc_state::ThemeMode) -> Theme;
}
pub trait ActiveTheme { fn theme(&self) -> &Theme; }
impl ActiveTheme for gpui::App { … } // so cx.theme() works in every render fn
                                     // (Context<T> derefs to App) and app.theme()
                                     // in Element::paint / canvas closures.
```

- [ ] **Step 1: Write the failing dbc-state tests** (append to the existing `mod tests` in `crates/dbc-state/src/config.rs`):

```rust
#[test]
fn old_config_without_theme_loads() {
    // Same forward-compat posture as old_config_without_favourites_loads:
    // a pre-G14 config.toml with no `theme` key defaults to Dark.
    let toml_str = r#"
[[connections]]
id = "c1"
name = "demo"
engine = "postgres"
host = "localhost"
database = "postgres"
user = "postgres"
"#;
    let config: AppConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.theme, ThemeMode::Dark);
}

#[test]
fn theme_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("config.toml");
    let mut config = sample();
    config.theme = ThemeMode::Light;
    config.save(&p).unwrap();
    let loaded = AppConfig::load(&p).unwrap();
    assert_eq!(loaded.theme, ThemeMode::Light);
}
```

Also update `sample()` (config.rs:110) — `AppConfig` is built as a full struct literal there, so it gains `theme: ThemeMode::Dark,`. If another phase's field (e.g. G11's `tool_paths`) has merged by execution time, add `theme` alongside whatever fields exist — re-locate by symbol.

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state`
Expected: compile error (`ThemeMode` not defined).

- [ ] **Step 3: Implement `ThemeMode` in dbc-state, then write `theme.rs` in full.** In `config.rs`, add the enum (exact derive list from Interfaces above — `Eq` matters, `AppConfig` derives `Eq`) directly above `AppConfig`, add the field with `#[serde(default)]`, and add `ThemeMode` to `lib.rs`'s `pub use config::{…}` list. Then create `crates/dbc-ui/src/theme.rs`:

```rust
//! G14 theme system. One flat struct of semantic Hsla colors, installed as
//! a GPUI Global at startup (main.rs) and swapped whole on toggle — every
//! render reads `cx.theme()` fresh, so `cx.refresh_windows()` after a swap
//! repaints the entire app in the new palette (design §1.2/§1.5; note the
//! pinned rev has `refresh_windows`, not the design's `cx.refresh()`).
//!
//! DARK values are the audited pre-G14 literals VERBATIM (The Sweep
//! Rulebook in the G14 plan) — the sweep is a rename, not a redesign.
//! LIGHT values are hand-picked; the contrast test below is the §1.4/§5
//! "contrast-minded" requirement made executable.

use gpui::{rgb, rgba, App, Hsla};

/// Editor syntax colors (G6's tree-sitter capture set + `identifier` as the
/// uncaptured-text default). DARK defaults are the shipped G6 hex values
/// verbatim (CURATION item 1 — binding; enforced by
/// `dark_syntax_is_shipped_g6_hex_verbatim` below).
///
/// `Copy` + `Send` on purpose: `sql_input::kick_highlight` captures this by
/// value BEFORE hopping to the background executor (a background task
/// cannot read a GPUI global).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditorSyntaxTheme {
    pub keyword: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub comment: Hsla,
    pub function: Hsla,
    pub type_: Hsla,
    /// Not produced by any current capture — the color of un-highlighted
    /// editor text; reserved so a future capture has a home.
    pub identifier: Hsla,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    // surfaces
    pub bg_app: Hsla,
    pub bg_panel: Hsla,
    pub bg_panel_alt: Hsla,
    pub bg_hover: Hsla,
    pub bg_selected: Hsla,
    pub border: Hsla,
    pub bg_find_match: Hsla,
    pub bg_joined_col: Hsla,
    pub bg_deep: Hsla,
    pub bg_warn_banner: Hsla,
    pub bg_backdrop: Hsla,
    pub bg_selection: Hsla,
    // text
    pub text_primary: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub text_disabled: Hsla,
    // semantic accents
    pub accent: Hsla,
    pub accent_alt: Hsla,
    pub warn: Hsla,
    pub danger: Hsla,
    pub success: Hsla,
    // G5 sandbox / G7 compare diff tints
    pub diff_staged_bg: Hsla,
    pub diff_deleted_bg: Hsla,
    pub diff_inserted_bg: Hsla,
    // editor syntax
    pub syntax: EditorSyntaxTheme,
}

impl gpui::Global for Theme {}

impl Theme {
    pub fn dark() -> Theme {
        Theme {
            bg_app: rgb(0x181825).into(),
            bg_panel: rgb(0x1e1e2e).into(),
            bg_panel_alt: rgb(0x232334).into(),
            bg_hover: rgb(0x313244).into(),
            bg_selected: rgb(0x45475a).into(),
            border: rgb(0x45475a).into(),
            bg_find_match: rgb(0x585b70).into(),
            bg_joined_col: rgb(0x2a2a3d).into(),
            bg_deep: rgb(0x11111b).into(),
            bg_warn_banner: rgb(0x3a3a1e).into(),
            bg_backdrop: rgba(0x00000099).into(),
            bg_selection: rgba(0x3311ff30).into(),
            text_primary: rgb(0xcdd6f4).into(),
            text_muted: rgb(0xa6adc8).into(),
            text_faint: rgb(0x7f849c).into(),
            text_disabled: rgb(0x6c7086).into(),
            accent: rgb(0x89b4fa).into(),
            accent_alt: rgb(0xf5c2e7).into(),
            warn: rgb(0xf9e2af).into(),
            danger: rgb(0xf38ba8).into(),
            success: rgb(0xa6e3a1).into(),
            diff_staged_bg: rgb(0x6b5d2e).into(),
            diff_deleted_bg: rgb(0x5d2e2e).into(),
            diff_inserted_bg: rgb(0x2e5d3a).into(),
            syntax: EditorSyntaxTheme {
                // Shipped G6 values VERBATIM (sql_highlight.rs:108-118).
                keyword: rgb(0xcba6f7).into(),  // mauve
                string: rgb(0xa6e3a1).into(),   // green
                number: rgb(0xfab387).into(),   // peach
                comment: rgb(0x6c7086).into(),  // overlay gray
                function: rgb(0x89b4fa).into(), // blue
                type_: rgb(0x94e2d5).into(),    // teal
                identifier: rgb(0xcdd6f4).into(),
            },
        }
    }

    pub fn light() -> Theme {
        Theme {
            bg_app: rgb(0xeef0f6).into(),
            bg_panel: rgb(0xffffff).into(),
            bg_panel_alt: rgb(0xf6f7fb).into(),
            bg_hover: rgb(0xe4e7f0).into(),
            bg_selected: rgb(0xcfd5e6).into(),
            border: rgb(0xd3d7e3).into(),
            bg_find_match: rgb(0xffe58a).into(),
            bg_joined_col: rgb(0xeef1fb).into(),
            bg_deep: rgb(0xe4e7f1).into(),
            bg_warn_banner: rgb(0xf7edc8).into(),
            bg_backdrop: rgba(0x00000066).into(),
            bg_selection: rgba(0x3355ff33).into(),
            text_primary: rgb(0x1e2030).into(),
            text_muted: rgb(0x4c5273).into(),
            text_faint: rgb(0x6b7094).into(),
            text_disabled: rgb(0x9498ad).into(),
            accent: rgb(0x3b6fe0).into(),
            accent_alt: rgb(0xb83280).into(),
            // Contrast-corrected vs. design §1.4 (grounding correction 5):
            // design's #a8791a / #1f8a4c were ~3.9:1 / ~4.4:1 on white.
            warn: rgb(0x8a6210).into(),
            danger: rgb(0xc2255c).into(),
            success: rgb(0x187741).into(),
            diff_staged_bg: rgb(0xfdf1c8).into(),
            diff_deleted_bg: rgb(0xfbdada).into(),
            diff_inserted_bg: rgb(0xd7f2df).into(),
            syntax: EditorSyntaxTheme {
                // Hand-picked, contrast-checked on #ffffff (CURATION 1d);
                // ratios asserted by light_palette_clears_wcag_aa below.
                keyword: rgb(0x8839ef).into(),  // ~5.4:1
                string: rgb(0x2e7d32).into(),   // ~5.1:1
                number: rgb(0xb45309).into(),   // ~5.0:1
                comment: rgb(0x6c6f85).into(),  // ~4.9:1
                function: rgb(0x1e66f5).into(), // ~4.9:1
                type_: rgb(0x0e7490).into(),    // ~5.4:1
                identifier: rgb(0x1e2030).into(),
            },
        }
    }

    pub fn from_mode(mode: dbc_state::ThemeMode) -> Theme {
        match mode {
            dbc_state::ThemeMode::Dark => Theme::dark(),
            dbc_state::ThemeMode::Light => Theme::light(),
        }
    }
}

/// `cx.theme()` everywhere a `Context` is in scope (it derefs to `App`);
/// `app.theme()` inside `Element::paint`/`canvas` closures which receive
/// `&mut App` directly. Mirrors GPUI's own ReadGlobal blanket-impl pattern.
pub trait ActiveTheme {
    fn theme(&self) -> &Theme;
}

impl ActiveTheme for App {
    fn theme(&self) -> &Theme {
        self.global::<Theme>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single place listing every field — update when adding a field, and
    /// every whole-struct test below stays exhaustive automatically.
    fn all_fields(t: &Theme) -> Vec<Hsla> {
        vec![
            t.bg_app, t.bg_panel, t.bg_panel_alt, t.bg_hover, t.bg_selected,
            t.border, t.bg_find_match, t.bg_joined_col, t.bg_deep,
            t.bg_warn_banner, t.bg_backdrop, t.bg_selection, t.text_primary,
            t.text_muted, t.text_faint, t.text_disabled, t.accent,
            t.accent_alt, t.warn, t.danger, t.success, t.diff_staged_bg,
            t.diff_deleted_bg, t.diff_inserted_bg, t.syntax.keyword,
            t.syntax.string, t.syntax.number, t.syntax.comment,
            t.syntax.function, t.syntax.type_, t.syntax.identifier,
        ]
    }

    /// CURATION item 1 (binding): dark syntax defaults are the shipped G6
    /// hex values VERBATIM — not derived from accents.
    #[test]
    fn dark_syntax_is_shipped_g6_hex_verbatim() {
        let s = Theme::dark().syntax;
        assert_eq!(s.keyword, rgb(0xcba6f7).into());
        assert_eq!(s.string, rgb(0xa6e3a1).into());
        assert_eq!(s.number, rgb(0xfab387).into());
        assert_eq!(s.comment, rgb(0x6c7086).into());
        assert_eq!(s.function, rgb(0x89b4fa).into());
        assert_eq!(s.type_, rgb(0x94e2d5).into());
    }

    /// Catches a copy-paste field left at Hsla::default() (transparent
    /// black) — the most likely authoring mistake (design §4).
    #[test]
    fn no_field_is_default_initialized() {
        for t in [Theme::dark(), Theme::light()] {
            for (i, f) in all_fields(&t).iter().enumerate() {
                assert_ne!(*f, Hsla::default(), "field #{i} left at default");
            }
        }
    }

    /// Every single field was deliberately given a DIFFERENT value in the
    /// two palettes — catches a light() line copy-pasted from dark().
    #[test]
    fn every_field_differs_between_dark_and_light() {
        let d = all_fields(&Theme::dark());
        let l = all_fields(&Theme::light());
        for (i, (a, b)) in d.iter().zip(l.iter()).enumerate() {
            assert_ne!(a, b, "field #{i} identical in dark and light");
        }
    }

    // --- WCAG contrast (design §1.4 requirement + §5 needs-verification,
    // made executable; grounding correction 5) ---

    fn luminance(c: Hsla) -> f64 {
        let rgba: gpui::Rgba = c.into();
        let lin = |v: f32| {
            let v = v as f64;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(rgba.r) + 0.7152 * lin(rgba.g) + 0.0722 * lin(rgba.b)
    }

    fn contrast(a: Hsla, b: Hsla) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn light_palette_clears_wcag_aa() {
        let t = Theme::light();
        for bg in [t.bg_panel, t.bg_app] {
            assert!(contrast(t.text_primary, bg) >= 4.5);
        }
        for fg in [t.accent, t.warn, t.danger, t.success] {
            assert!(contrast(fg, t.bg_panel) >= 4.5, "accent under AA on light bg_panel");
        }
        let s = t.syntax;
        for fg in [s.keyword, s.string, s.number, s.comment, s.function, s.type_, s.identifier] {
            assert!(contrast(fg, t.bg_panel) >= 4.5, "syntax color under AA on light bg_panel");
        }
    }

    #[test]
    fn dark_text_on_dark_panel_clears_wcag_aa() {
        let t = Theme::dark();
        assert!(contrast(t.text_primary, t.bg_panel) >= 4.5);
    }

    #[test]
    fn from_mode_maps_both_ways() {
        assert_eq!(Theme::from_mode(dbc_state::ThemeMode::Dark), Theme::dark());
        assert_eq!(Theme::from_mode(dbc_state::ThemeMode::Light), Theme::light());
    }
}
```

Then wire `main.rs`: add `mod theme;` next to the other `mod` declarations at the top of the file, and inside the `application().run` closure (main.rs:4352), immediately after the `*::bind_keys(cx);` block and before `cx.open_window`, add:

```rust
// G14 Task 1: theme global — installed before the first window opens so
// every render() can read cx.theme(). `config.theme` is Copy; `config`
// itself moves into the window closure below untouched.
cx.set_global(theme::Theme::from_mode(config.theme));
```

(No call site reads `cx.theme()` yet — that's Tasks 2–9. `theme.rs` items unused until then would warn; the module is immediately consumed by its own tests, but if `cargo build -p dbc-ui` still flags dead code at this intermediate point, silence it with an item-level `#[allow(dead_code)]` on `ActiveTheme` ONLY, with a `// removed in G14 Task 2..9` comment, and remove it in the first sweep task that lands.)

- [ ] **Step 4: Run to green**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-state` then `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all pass (old + new), zero warnings in both.

- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/theme.rs crates/dbc-ui/src/main.rs crates/dbc-state/src/config.rs crates/dbc-state/src/lib.rs
git commit -m "feat: theme system foundation (Theme global, ThemeMode config)"
```

---

### Task 2: Color sweep — `grid.rs`

**Files:**
- Modify: `crates/dbc-ui/src/grid.rs` (56 literal lines + the 3 diff consts at grid.rs:26–28)

**Interfaces:**
- Consumes: `crate::theme::{ActiveTheme, Theme}` (Task 1). No new produced interface — behavior-preserving refactor; existing `grid.rs` tests must stay green untouched.

- [ ] **Step 1: Sweep every production color literal per The Sweep Rulebook.** Specifics for this file:
  - Delete `STAGED_CELL_BG`/`DELETED_ROW_BG`/`INSERTED_ROW_BG` (grid.rs:26–28); their uses (e.g. grid.rs:2560, 2601, 2650, 2837, 2843) become `cx.theme().diff_staged_bg` / `.diff_deleted_bg` / `.diff_inserted_bg`.
  - `rgb(0x585b70)` (grid.rs:2839) → `cx.theme().bg_find_match`.
  - `rgb(0x232334)` (grid.rs:2654) → `cx.theme().bg_panel_alt`. **Zebra check (design §5 needs-verification):** confirm at this site that the alternating-row stripe is intentional (it renders on even/odd rows against `bg_panel`). It is deliberate — keep the distinct field; record one line in the commit message body: "bg_panel_alt zebra confirmed intentional".
  - `0x45475a` ambiguity: resolve per site by `.border_color(..)` → `border` vs `.bg(..)`-on-selected → `bg_selected` (Rulebook).
  - Free helper functions building row/cell divs without a `Context`: thread `theme: &Theme` as a parameter from the nearest render fn (design §1.3's anticipated case — occurs only in grid.rs helpers).
- [ ] **Step 2: Build + test, zero warnings**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`
Expected: all existing tests pass unmodified; zero warnings.

- [ ] **Step 3: File-scoped audit**

Run (Git Bash): `grep -n 'rgba\?(0x' crates/dbc-ui/src/grid.rs`
Expected: zero hits.

- [ ] **Step 4: Visual sanity launch** — `%USERPROFILE%\.cargo\bin\cargo.exe run -p dbc-ui`, run a query, eyeball: row selection, alt-row stripes, find-match highlight, a sandbox staged/deleted/inserted row (dark mode must look pixel-identical to pre-sweep).
- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/grid.rs
git commit -m "refactor: grid.rs color sweep to Theme fields"
```

---

### Task 3: Color sweep — `connections_ui.rs`

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (76 literal lines)

**Interfaces:**
- Consumes: `crate::theme::ActiveTheme` (Task 1). Behavior-preserving; no produced interface.

- [ ] **Step 1: Sweep per The Sweep Rulebook.** Specifics:
  - `rgb(0x7f849c)` (connections_ui.rs:1036) → `cx.theme().text_faint` (this file is the field's only consumer).
  - `rgb(0x5d2e2e)` at connections_ui.rs:2303 and 2415 → `cx.theme().diff_deleted_bg` (the in-code comment already names the family); the `if running { rgb(0x313244) } else { … }` at 2415 → `cx.theme().bg_hover` for the running arm.
  - The `rgba(0x00000099)` modal backdrop → `cx.theme().bg_backdrop`.
  - `0x45475a` ambiguity per Rulebook (dialog borders vs. selected rows — this file has both).
  - The custom-element selection/cursor `paint_quad`s (connections_ui.rs:694, 701): the paint fn receives `&mut App` — use `app.theme()`.
- [ ] **Step 2: Build + test** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings.
- [ ] **Step 3: File-scoped audit** — `grep -n 'rgba\?(0x' crates/dbc-ui/src/connections_ui.rs` → zero hits.
- [ ] **Step 4: Visual sanity launch** — open the connection dropdown, the connection dialog, a master-password prompt: borders, hovers, danger buttons identical.
- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs
git commit -m "refactor: connections_ui.rs color sweep to Theme fields"
```

---

### Task 4: Color sweep — `compare.rs`, `monitor_view.rs`, `plan.rs`

**Files:**
- Modify: `crates/dbc-ui/src/compare.rs` (45 lines + `TINT_*` consts at compare.rs:42–44)
- Modify: `crates/dbc-ui/src/monitor_view.rs` (28 lines)
- Modify: `crates/dbc-ui/src/plan.rs` (15 lines)

**Interfaces:**
- Consumes: `crate::theme::ActiveTheme` (Task 1). Behavior-preserving; no produced interface.

- [ ] **Step 1: Sweep per The Sweep Rulebook.** Specifics:
  - `compare.rs`: delete `TINT_ADDED`/`TINT_REMOVED`/`TINT_CHANGED` (42–44) → `cx.theme().diff_inserted_bg`/`.diff_deleted_bg`/`.diff_staged_bg` (same hex family as grid's G5 consts — mapping to the SAME fields is the point; a light-mode compare diff automatically matches a light-mode sandbox diff). The `rgb(0x3a3a1e)` banner (compare.rs:597) → `cx.theme().bg_warn_banner`.
  - `monitor_view.rs`: `rgb(0x11111b)` (monitor_view.rs:725) → `cx.theme().bg_deep` (the recessed chart well — its only consumer today).
  - `plan.rs`: `rgb(0x2a2a1e)` banner (plan.rs:1636) → `cx.theme().bg_warn_banner` (**collapses the 0x2a2a1e/0x3a3a1e near-duplicate pair into one field** — grounding correction 4; in dark mode plan.rs's banner gets 0x3a3a1e, an imperceptible tint change on a notice banner, the single deliberate value change in the whole sweep — call it out in the commit message).
- [ ] **Step 2: Build + test** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings.
- [ ] **Step 3: File-scoped audit** — `grep -n 'rgba\?(0x' crates/dbc-ui/src/compare.rs crates/dbc-ui/src/monitor_view.rs crates/dbc-ui/src/plan.rs` → zero hits.
- [ ] **Step 4: Visual sanity launch** — open a compare tab, a monitor tab, a plan tab (against the SQLite fixture where available); diff tints and banners identical.
- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/compare.rs crates/dbc-ui/src/monitor_view.rs crates/dbc-ui/src/plan.rs
git commit -m "refactor: compare/monitor/plan color sweep to Theme fields"
```

---

### Task 5: Color sweep — `schema_tree.rs`, `history_panel.rs`, `er_diagram_view.rs`

**Files:**
- Modify: `crates/dbc-ui/src/schema_tree.rs` (12 lines)
- Modify: `crates/dbc-ui/src/history_panel.rs` (7 lines)
- Modify: `crates/dbc-ui/src/er_diagram_view.rs` (8 lines + the 6 consts at er_diagram_view.rs:32–37)

**Interfaces:**
- Consumes: `crate::theme::{ActiveTheme, Theme}` (Task 1). Behavior-preserving; no produced interface.

- [ ] **Step 1: Sweep per The Sweep Rulebook.** Specifics for `er_diagram_view.rs` (the only structurally interesting one):
  - Delete the consts `NODE_FILL`/`NODE_BORDER`/`TEXT_COLOR`/`MUTED_COLOR`/`EDGE_COLOR`/`ACCENT_COLOR` (32–37) → `bg_hover`/`border`/`text_primary`/`text_disabled`/`accent`/`accent_alt`.
  - The paint helpers take `color: u32` today (`paint_text_line` er_diagram_view.rs:458, `paint_edge` :560, `paint_node` :495) and convert via `rgb(color).into()` internally — change those parameters to `color: Hsla` and drop the conversion; the callers (`paint_diagram`, which receives `&mut App`) resolve `app.theme().<field>` once and pass `Hsla` down. This is the Rulebook's "thread a parameter" case.
  - `schema_tree.rs` / `history_panel.rs`: straight Rulebook mapping, all sites have `cx`.
- [ ] **Step 2: Build + test** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings.
- [ ] **Step 3: File-scoped audit** — `grep -n 'rgba\?(0x' crates/dbc-ui/src/schema_tree.rs crates/dbc-ui/src/history_panel.rs crates/dbc-ui/src/er_diagram_view.rs` → zero hits.
- [ ] **Step 4: Visual sanity launch** — tree hover/selection, history rows, an ER diagram (node fill, pink selected border, blue edges) identical.
- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/schema_tree.rs crates/dbc-ui/src/history_panel.rs crates/dbc-ui/src/er_diagram_view.rs
git commit -m "refactor: tree/history/er-diagram color sweep to Theme fields"
```

---

### Task 6: Syntax-theme migration — `sql_highlight.rs` + `sql_input.rs` sweep

**Files:**
- Modify: `crates/dbc-ui/src/sql_highlight.rs` (signature change: theme-fed colors; CURATION item 1c)
- Modify: `crates/dbc-ui/src/sql_input.rs` (the `kick_highlight` call site + this file's ~2 production literals; `#[cfg(test)]` colors at sql_input.rs:1404–1421 stay)

**Interfaces:**
- Consumes: `crate::theme::{ActiveTheme, EditorSyntaxTheme, Theme}` (Task 1).
- Produces (breaking change, both callers fixed in this same task — `highlight()` is consumed only by `sql_input.rs` and `sql_highlight`'s own tests):

```rust
pub fn highlight(text: &str, syntax: &crate::theme::EditorSyntaxTheme) -> Vec<HighlightSpan>;
```

- [ ] **Step 1: Write the failing test** (append to `sql_highlight.rs`'s `mod tests`):

```rust
#[test]
fn colors_come_from_the_passed_syntax_theme() {
    // Dark theme reproduces the shipped G6 colors byte-for-byte (the
    // migration is a plumbing change, not a recolor)…
    let dark = crate::theme::Theme::dark().syntax;
    let spans = highlight("SELECT 1", &dark);
    assert_eq!(color_at(&spans, 0), Some(dark.keyword));
    // …and a different theme yields different colors from the same input.
    let light = crate::theme::Theme::light().syntax;
    let spans_l = highlight("SELECT 1", &light);
    assert_eq!(color_at(&spans_l, 0), Some(light.keyword));
    assert_ne!(dark.keyword, light.keyword);
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui colors_come_from_the_passed_syntax_theme`
Expected: compile error (`highlight` takes one argument).

- [ ] **Step 3: Implement.** In `sql_highlight.rs`:

```rust
/// (priority, color) — priority resolves same-range capture collisions
/// (unchanged); colors now come from the passed theme (G14 CURATION 1c)
/// instead of hardcoded literals.
fn color_for_capture(
    name: &str,
    syntax: &crate::theme::EditorSyntaxTheme,
) -> Option<(u8, gpui::Hsla)> {
    match name {
        "keyword" => Some((1, syntax.keyword)),
        "string" => Some((1, syntax.string)),
        "comment" => Some((1, syntax.comment)),
        "type" | "type.builtin" => Some((1, syntax.type_)),
        "number" => Some((2, syntax.number)),           // outranks "string"
        "function.call" => Some((2, syntax.function)),  // outranks "type"
        _ => None,
    }
}

pub fn highlight(text: &str, syntax: &crate::theme::EditorSyntaxTheme) -> Vec<HighlightSpan> {
    // body unchanged except the one call:
    //   color_for_capture(name)  →  color_for_capture(name, syntax)
    …
}
```

Update every existing test in the file mechanically: each `highlight(sql)` becomes `highlight(sql, &crate::theme::Theme::dark().syntax)` (add a file-local test helper `fn dark_syntax() -> crate::theme::EditorSyntaxTheme { crate::theme::Theme::dark().syntax }` to keep it readable); the `keyword_color()`/`number_color()` helpers become `color_for_capture("keyword", &dark_syntax()).unwrap().1` etc. No test ASSERTION changes — the dark theme reproduces the old literals exactly (Task 1's verbatim test guarantees it).

In `sql_input.rs`, `kick_highlight` (sql_input.rs:430–450):

```rust
fn kick_highlight(&mut self, cx: &mut Context<Self>) {
    self.highlight_generation += 1;
    let my_generation = self.highlight_generation;
    let text = self.buffer.text().to_string();
    // G14: the syntax palette is captured HERE, on the main thread — a
    // background task cannot read a GPUI global. EditorSyntaxTheme is
    // Copy + Send precisely for this hop (grounding correction 2).
    let syntax = cx.theme().syntax;
    cx.spawn(async move |this, cx| {
        cx.background_executor().timer(std::time::Duration::from_millis(60)).await;
        let spans = cx
            .background_spawn(async move { sql_highlight::highlight(&text, &syntax) })
            .await;
        …unchanged…
    })
    .detach();
}
```

(Stale-theme note, deliberate: spans computed just before a toggle keep old colors until the next keystroke's re-highlight. Task 10 closes this by calling `kick_highlight` from the toggle path — see Task 10 Step 3.)

Also sweep this file's production literals: `rgba(0x3311ff30)` (sql_input.rs:1092, the selection quad) → `bg_selection`, plus any cursor/other literal in the paint pass — the element's paint fn receives `&mut App`, so `app.theme().bg_selection`; if only `Window` is in scope at the exact site, resolve the `Hsla` in prepaint (where `App` is available) and carry it in the frame state. Leave the `#[cfg(test)]` colors.

- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; all sql_highlight/sql_input tests pass, zero warnings.
- [ ] **Step 5: File-scoped audit** — `grep -n 'rgba\?(0x' crates/dbc-ui/src/sql_highlight.rs crates/dbc-ui/src/sql_input.rs` → hits ONLY inside `sql_input.rs`'s `#[cfg(test)]` module (verify each remaining line number sits below the `#[cfg(test)]` marker).
- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/sql_highlight.rs crates/dbc-ui/src/sql_input.rs
git commit -m "refactor: syntax highlighting reads EditorSyntaxTheme (G14 curation 1c)"
```

---

### Task 7: `chart_data.rs` — pure data prep + scale math

**Files:**
- Create: `crates/dbc-ui/src/chart_data.rs`
- Modify: `crates/dbc-ui/src/main.rs` (ONE line: `mod chart_data;` in the module list — the G11-T2 "one-line mod decl is parallel-eligible" precedent; rebase-trivial)

**Interfaces:**
- Consumes: nothing project-internal (GPUI-free, like `tabs.rs`/`sandbox.rs`).
- Produces (consumed by Tasks 8 and 11):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind { Bar, Line }

#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries { pub label: String, pub points: Vec<Option<f64>> }

#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    pub x_labels: Vec<String>,
    pub series: Vec<ChartSeries>,
    /// Buffer row count BEFORE capping — drives the honest truncation note.
    pub total_rows: usize,
}

pub const CHART_ROW_HARD_CAP: usize = 500;   // curation item 3
pub const MIN_PX_PER_TICK: f32 = 3.0;        // curation item 3: w / 3

pub fn parse_y(cell: &str) -> Option<f64>;
pub fn prepare(x_labels: Vec<String>, y_columns: &[(String, Vec<Option<String>>)],
               row_cap: usize, total_rows: usize) -> ChartData;
pub fn value_range(series: &[ChartSeries]) -> Option<(f64, f64)>;
pub fn bar_range(range: (f64, f64)) -> (f64, f64); // bars always include 0
pub fn scale_to(range: (f64, f64), value: f64, pixel_height: f32) -> f32; // px from plot TOP
pub fn visible_ticks(total_ticks: usize, plot_width_px: f32) -> usize;
pub fn format_axis(v: f64) -> String;
```

- [ ] **Step 1: Write the failing tests** (in-file `#[cfg(test)] mod tests`):

```rust
#[test]
fn parse_y_strict() {
    assert_eq!(parse_y(" 42 "), Some(42.0));
    assert_eq!(parse_y("3.14"), Some(3.14));
    assert_eq!(parse_y("-1.5e3"), Some(-1500.0));
    assert_eq!(parse_y(""), None);
    assert_eq!(parse_y("abc"), None);
    assert_eq!(parse_y("1,5"), None);   // locale comma is NOT a number
    assert_eq!(parse_y("NaN"), None);   // non-finite is a gap, not a point
    assert_eq!(parse_y("inf"), None);
}

#[test]
fn prepare_null_and_garbage_become_gaps_never_zero() {
    let data = prepare(
        vec!["a".into(), "b".into(), "c".into()],
        &[("y".into(), vec![Some("1".into()), None, Some("x".into())])],
        500, 3,
    );
    assert_eq!(data.series[0].points, vec![Some(1.0), None, None]);
}

#[test]
fn prepare_caps_rows_and_keeps_total() {
    let x: Vec<String> = (0..10).map(|i| i.to_string()).collect();
    let cells: Vec<Option<String>> = (0..10).map(|i| Some(i.to_string())).collect();
    let data = prepare(x, &[("y".into(), cells)], 4, 10);
    assert_eq!(data.x_labels.len(), 4);
    assert_eq!(data.series[0].points.len(), 4);
    assert_eq!(data.total_rows, 10);
}

#[test]
fn prepare_multiple_y_columns_in_input_order() {
    let data = prepare(
        vec!["a".into()],
        &[("y1".into(), vec![Some("1".into())]), ("y2".into(), vec![Some("2".into())])],
        500, 1,
    );
    assert_eq!(data.series.len(), 2);
    assert_eq!(data.series[0].label, "y1");
    assert_eq!(data.series[1].label, "y2");
}

#[test]
fn value_range_ignores_gaps_and_is_none_when_empty() {
    let s = vec![ChartSeries { label: "y".into(), points: vec![None, Some(-2.0), Some(5.0)] }];
    assert_eq!(value_range(&s), Some((-2.0, 5.0)));
    let empty = vec![ChartSeries { label: "y".into(), points: vec![None, None] }];
    assert_eq!(value_range(&empty), None);
}

#[test]
fn bar_range_always_includes_zero() {
    assert_eq!(bar_range((2.0, 5.0)), (0.0, 5.0));
    assert_eq!(bar_range((-5.0, -2.0)), (-5.0, 0.0));
    assert_eq!(bar_range((-1.0, 1.0)), (-1.0, 1.0));
}

#[test]
fn scale_to_min_max_mid_and_degenerate() {
    // px from the plot TOP: max → 0.0, min → full height.
    assert_eq!(scale_to((0.0, 10.0), 10.0, 100.0), 0.0);
    assert_eq!(scale_to((0.0, 10.0), 0.0, 100.0), 100.0);
    assert_eq!(scale_to((0.0, 10.0), 5.0, 100.0), 50.0);
    // Constant column (min == max) must not divide by zero — midline.
    assert_eq!(scale_to((7.0, 7.0), 7.0, 100.0), 50.0);
}

#[test]
fn visible_ticks_width_derived_with_hard_cap() {
    // curation item 3: max_bars = plot_width_px / 3, hard cap 500.
    assert_eq!(visible_ticks(1000, 300.0), 100);
    assert_eq!(visible_ticks(50, 300.0), 50);       // fewer rows than room
    assert_eq!(visible_ticks(10_000, 9000.0), 500); // hard cap
    assert_eq!(visible_ticks(10, 1.0), 1);          // degenerate width
    assert_eq!(visible_ticks(0, 300.0), 0);
}

#[test]
fn format_axis_trims_noise() {
    assert_eq!(format_axis(1500.0), "1500");
    assert_eq!(format_axis(3.14), "3.14");
    assert_eq!(format_axis(0.5), "0.5");
    assert_eq!(format_axis(-2.0), "-2");
}
```

- [ ] **Step 2: Run to see it fail**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui chart_data::`
Expected: compile error (module doesn't exist).

- [ ] **Step 3: Implement** (`mod chart_data;` in main.rs + the file):

```rust
//! G14 charts — pure data prep + scale math (design §2.2). GPUI-free like
//! tabs.rs/sandbox.rs; chart_view.rs only paints this module's output.

pub const CHART_ROW_HARD_CAP: usize = 500;
pub const MIN_PX_PER_TICK: f32 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind { Bar, Line }

#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries { pub label: String, pub points: Vec<Option<f64>> }

#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    pub x_labels: Vec<String>,
    pub series: Vec<ChartSeries>,
    pub total_rows: usize,
}

/// Strict parse (mirrors sandbox::sql_value's posture): trimmed
/// `f64::from_str`, non-finite refused. A failure is a GAP (None), never a
/// silent 0 — 0 is a real, different value (design §2.2).
pub fn parse_y(cell: &str) -> Option<f64> {
    cell.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

pub fn prepare(
    x_labels: Vec<String>,
    y_columns: &[(String, Vec<Option<String>>)],
    row_cap: usize,
    total_rows: usize,
) -> ChartData {
    let cap = row_cap.min(x_labels.len());
    let x: Vec<String> = x_labels.into_iter().take(cap).collect();
    let series = y_columns
        .iter()
        .map(|(name, cells)| ChartSeries {
            label: name.clone(),
            points: cells.iter().take(cap).map(|c| c.as_deref().and_then(parse_y)).collect(),
        })
        .collect();
    ChartData { x_labels: x, series, total_rows }
}

pub fn value_range(series: &[ChartSeries]) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for s in series {
        for v in s.points.iter().flatten() {
            min = min.min(*v);
            max = max.max(*v);
        }
    }
    (min <= max).then_some((min, max))
}

/// Bars are drawn FROM zero — a bar chart whose axis starts at the data
/// minimum lies about magnitude.
pub fn bar_range(range: (f64, f64)) -> (f64, f64) {
    (range.0.min(0.0), range.1.max(0.0))
}

/// Pixel distance from the plot TOP for `value` (GPUI y grows downward).
/// Degenerate range (constant column) → midline, never a division by zero.
pub fn scale_to(range: (f64, f64), value: f64, pixel_height: f32) -> f32 {
    let span = range.1 - range.0;
    if !(span > 0.0) || !span.is_finite() {
        return pixel_height / 2.0;
    }
    let frac = ((value - range.0) / span).clamp(0.0, 1.0) as f32;
    pixel_height - frac * pixel_height
}

/// Curation item 3: width-derived cap — floor(w / 3px), 500 hard bound.
pub fn visible_ticks(total_ticks: usize, plot_width_px: f32) -> usize {
    if total_ticks == 0 {
        return 0;
    }
    let by_width = (plot_width_px / MIN_PX_PER_TICK).floor().max(1.0) as usize;
    total_ticks.min(by_width).min(CHART_ROW_HARD_CAP)
}

/// Axis label: integers without a decimal tail, everything else trimmed of
/// trailing zeros ("{v}" via Rust's shortest-roundtrip float Display).
pub fn format_axis(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}
```

- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings. (Until Task 8 consumes the module, silence any dead-code warning with a module-level `#![allow(dead_code)]` header comment `// consumed by chart_view.rs (G14 Task 8); allow removed there` — same freeze idiom G12's T6 used.)
- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/chart_data.rs crates/dbc-ui/src/main.rs
git commit -m "feat: chart_data pure prep and scale math (G14)"
```

---

### Task 8: `chart_view.rs` — the painted chart view

**Files:**
- Create: `crates/dbc-ui/src/chart_view.rs`
- Modify: `crates/dbc-ui/src/main.rs` (ONE line: `mod chart_view;` — same precedent as Task 7; remove `chart_data.rs`'s temporary `#![allow(dead_code)]` if present)

**Interfaces:**
- Consumes: `crate::chart_data::{self, ChartData, ChartKind}` (Task 7), `crate::theme::{ActiveTheme, Theme}` (Task 1), `dbc_buffer::ResultBuffer`, gpui `canvas`/`paint_quad`/`PathBuilder`/`paint_path`/`shape_line` (exact idioms: er_diagram_view.rs:220–241 for `canvas`, :458–492 for text, :560–573 for stroked paths).
- Produces (consumed by Task 11):

```rust
pub struct ChartView { /* private fields */ }

pub enum ChartViewEvent {
    /// "Upravit…" clicked — main.rs reopens ModalState::ChartPicker seeded
    /// from picker_seed(), edit-in-place (design §2.4's only interaction).
    ReopenPicker,
}
impl gpui::EventEmitter<ChartViewEvent> for ChartView {}

impl ChartView {
    pub fn new(
        buffer: std::rc::Rc<std::cell::RefCell<dbc_buffer::ResultBuffer>>,
        kind: chart_data::ChartKind,
        x_col: usize,
        y_cols: Vec<usize>,
        source_title: String,
    ) -> Self;
    /// Re-pick path: recompute ChartData in place (design §2.4 — edits the
    /// tab, doesn't spawn a new one).
    pub fn reconfigure(&mut self, kind: chart_data::ChartKind, x_col: usize,
                       y_cols: Vec<usize>, cx: &mut gpui::Context<Self>);
    /// (kind, x_col, y_cols) — prefills the reopened picker.
    pub fn picker_seed(&self) -> (chart_data::ChartKind, usize, Vec<usize>);
    pub fn source_title(&self) -> &str;
    /// Rc clone of the snapshot buffer — Task 11's picker-reopen path
    /// rebuilds the column list from it.
    pub fn buffer_handle(&self) -> std::rc::Rc<std::cell::RefCell<dbc_buffer::ResultBuffer>>;
}
impl gpui::Render for ChartView { … }
```

- [ ] **Step 1: Write the failing test** (data-extraction only — painting itself is covered by the visual sanity launch, same tier as the ER diagram's paint pass):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dbc_core::arrow::array::{Int64Array, RecordBatch, StringArray};
    use dbc_core::arrow::datatypes::{DataType, Field, Schema};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    /// ResultBuffer over a tiny in-memory Arrow batch — exact fixture idiom
    /// of dbc-buffer's own `batch()` test helper (dbc-buffer/src/lib.rs:224),
    /// with a NULLABLE Int64 y-column: rows 0..4, y NULL at row 2.
    fn test_buffer() -> Rc<RefCell<dbc_buffer::ResultBuffer>> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("label", DataType::Utf8, false),
            Field::new("y", DataType::Int64, true),
        ]));
        let labels = StringArray::from_iter_values((0..4).map(|i| format!("r{i}")));
        let ys = Int64Array::from_iter([Some(10), Some(20), None, Some(40)]);
        let b = RecordBatch::try_new(schema, vec![Arc::new(labels), Arc::new(ys)]).unwrap();
        let mut buf = dbc_buffer::ResultBuffer::new(b.schema());
        buf.push(b).unwrap();
        Rc::new(RefCell::new(buf))
    }

    #[test]
    fn compute_reads_nulls_as_gaps_and_respects_the_hard_cap() {
        let buffer = test_buffer();
        let data = ChartView::compute(&buffer, /*x_col*/ 0, &[1]);
        assert_eq!(data.total_rows, 4);
        assert_eq!(data.x_labels, vec!["r0", "r1", "r2", "r3"]);
        // the NULL cell surfaced as a gap, never 0 (design §2.2):
        assert_eq!(data.series[0].points, vec![Some(10.0), Some(20.0), None, Some(40.0)]);
        assert!(data.x_labels.len() <= chart_data::CHART_ROW_HARD_CAP);
    }

    #[test]
    fn compute_skips_out_of_range_y_columns_without_panicking() {
        let buffer = test_buffer();
        let data = ChartView::compute(&buffer, 0, &[1, 99]); // 99: belt only
        assert_eq!(data.series.len(), 1);
    }
}
```

- [ ] **Step 2: Run to see it fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui chart_view::` → compile error.
- [ ] **Step 3: Implement.** Data half:

```rust
impl ChartView {
    /// Associated fn (not method) so the unit test can call it without a
    /// GPUI Entity. Reads at most CHART_ROW_HARD_CAP rows; NULL → None.
    fn compute(
        buffer: &Rc<RefCell<ResultBuffer>>,
        x_col: usize,
        y_cols: &[usize],
    ) -> ChartData {
        let mut buf = buffer.borrow_mut();
        let total = buf.row_count();
        let rows = total.min(chart_data::CHART_ROW_HARD_CAP);
        // Belt: the picker only offers real columns, but never panic on an
        // out-of-range index — silently drop it (tested).
        let y_cols: Vec<usize> =
            y_cols.iter().copied().filter(|&c| c < buf.column_count()).collect();
        // names first — schema() borrows buf, cell_text needs &mut:
        let names: Vec<String> =
            y_cols.iter().map(|&c| buf.schema().field(c).name().clone()).collect();
        let x_labels: Vec<String> = (0..rows).map(|r| buf.cell_text(r, x_col)).collect();
        let y_columns: Vec<(String, Vec<Option<String>>)> = y_cols
            .iter()
            .zip(names)
            .map(|(&c, name)| {
                let cells = (0..rows)
                    .map(|r| (!buf.cell_is_null(r, c)).then(|| buf.cell_text(r, c)))
                    .collect();
                (name, cells)
            })
            .collect();
        chart_data::prepare(x_labels, &y_columns, chart_data::CHART_ROW_HARD_CAP, total)
    }
}
```

Render half — `render()` returns a column: a slim header row (`div().bg(cx.theme().bg_app)` with `"Graf: {source_title}"` in `text_muted`, the kind label, and an "Upravit…" button that `cx.emit(ChartViewEvent::ReopenPicker)`), then `canvas(prepaint, paint).size_full()` over `bg_deep` (the same recessed-well role monitor_view already uses). Paint fn (free function, all inputs cloned into the closure like `paint_diagram`'s — er_diagram_view.rs:229–240):

```rust
const PAD_LEFT: f32 = 48.0;   // y-axis label gutter
const PAD_RIGHT: f32 = 8.0;
const PAD_TOP: f32 = 8.0;
const PAD_BOTTOM: f32 = 22.0; // x tick labels
const LABEL_MIN_PX: f32 = 60.0; // label every Nth tick so labels don't collide

fn series_color(theme: &Theme, i: usize) -> Hsla {
    // design §2.1: fixed 4-color rotation, wrap-around accepted for v1.
    [theme.accent, theme.success, theme.warn, theme.danger][i % 4]
}

fn paint_chart(bounds: Bounds<Pixels>, data: &ChartData, kind: ChartKind,
               window: &mut Window, app: &mut App) {
    let theme = *app.theme(); // Theme is Copy — one read, then paint freely
    let plot = Bounds::new(
        bounds.origin + point(px(PAD_LEFT), px(PAD_TOP)),
        size(bounds.size.width - px(PAD_LEFT + PAD_RIGHT),
             bounds.size.height - px(PAD_TOP + PAD_BOTTOM)),
    );
    // axes: 1px quads (design §2.3)
    window.paint_quad(gpui::fill(
        Bounds::new(point(plot.left(), plot.bottom()), size(plot.size.width, px(1.))),
        theme.border));
    window.paint_quad(gpui::fill(
        Bounds::new(point(plot.left(), plot.top()), size(px(1.), plot.size.height)),
        theme.border));

    let Some(raw_range) = chart_data::value_range(&data.series) else {
        paint_label(plot.origin, "žádná číselná data k vykreslení",
                    theme.text_muted, window, app);
        return;
    };
    let range = match kind {
        ChartKind::Bar => chart_data::bar_range(raw_range),
        ChartKind::Line => raw_range,
    };
    let shown = chart_data::visible_ticks(data.x_labels.len(), f32::from(plot.size.width));
    let h = f32::from(plot.size.height);
    let w = f32::from(plot.size.width);
    let tick_w = w / shown.max(1) as f32;

    match kind {
        ChartKind::Bar => {
            let group_w = tick_w * 0.8;
            let bar_w = (group_w / data.series.len().max(1) as f32).max(1.0);
            let y0 = chart_data::scale_to(range, 0.0, h);
            for t in 0..shown {
                for (si, s) in data.series.iter().enumerate() {
                    let Some(v) = s.points[t] else { continue }; // gap: no bar
                    let y = chart_data::scale_to(range, v, h);
                    let (top, bottom) = if y < y0 { (y, y0) } else { (y0, y) };
                    let x = t as f32 * tick_w + tick_w * 0.1 + si as f32 * bar_w;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(plot.left() + px(x), plot.top() + px(top)),
                            size(px(bar_w.max(1.0)), px((bottom - top).max(1.0))),
                        ),
                        series_color(&theme, si)));
                }
            }
        }
        ChartKind::Line => {
            for (si, s) in data.series.iter().enumerate() {
                // one stroked path per maximal run of consecutive points; a
                // gap (None) breaks the run (design §2.2: skip the segment)
                let color = series_color(&theme, si);
                let mut run: Vec<Point<Pixels>> = Vec::new();
                for t in 0..shown {
                    match s.points[t] {
                        Some(v) => run.push(point(
                            plot.left() + px(t as f32 * tick_w + tick_w / 2.0),
                            plot.top() + px(chart_data::scale_to(range, v, h)),
                        )),
                        None => flush_run(&mut run, color, window),
                    }
                }
                flush_run(&mut run, color, window);
            }
        }
    }

    // x tick labels: every Nth so they don't collide (ordinary GPUI text
    // shaping, same shape_line+paint idiom as er_diagram's paint_text_line)
    let label_every = ((LABEL_MIN_PX / tick_w).ceil() as usize).max(1);
    for t in (0..shown).step_by(label_every) {
        paint_label(point(plot.left() + px(t as f32 * tick_w), plot.bottom() + px(4.)),
                    &data.x_labels[t], theme.text_muted, window, app);
    }
    // y axis: min + max
    paint_label(point(bounds.left() + px(2.), plot.top()),
                &chart_data::format_axis(range.1), theme.text_muted, window, app);
    paint_label(point(bounds.left() + px(2.), plot.bottom() - px(14.)),
                &chart_data::format_axis(range.0), theme.text_muted, window, app);
    // honest truncation note (design §2.1 / curation 3)
    if shown < data.total_rows {
        paint_label(point(plot.left(), bounds.top()),
                    &format!("zobrazeno prvních {shown} z {} řádků", data.total_rows),
                    theme.warn, window, app);
    }
}

/// ≥2 points: PathBuilder::stroke(px(1.5)) + move_to/line_to/build/paint_path
/// (er_diagram_view.rs:563-571 verbatim idiom). Exactly 1 point: a 3×3 dot
/// quad so an isolated value between gaps is still visible. Clears the run.
fn flush_run(run: &mut Vec<Point<Pixels>>, color: Hsla, window: &mut Window) { … }

/// Single-run shape_line + paint, control chars sanitized to spaces first —
/// small deliberate copy of er_diagram_view::paint_text_line (that fn is
/// private and takes u32; house precedent for small copies: collapse_sql).
fn paint_label(origin: Point<Pixels>, text: &str, color: Hsla,
               window: &mut Window, app: &mut App) { … }
```

`ChartView::new` stores the args and `data: Self::compute(&buffer, x_col, &y_cols)`; `reconfigure` overwrites kind/x_col/y_cols, recomputes, `cx.notify()`; index-safety: `compute` skips (with no panic) any `y_col >= column_count` by filtering indices up front — the picker only offers real columns, this is belt only.

- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings (until Task 11 constructs the view, keep a `// consumed by main.rs (G14 Task 11)` `#![allow(dead_code)]` header if needed, removed in Task 11).
- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/chart_view.rs crates/dbc-ui/src/main.rs
git commit -m "feat: ChartView paints bar/line charts via paint_quad/paint_path (G14)"
```

---

### Task 9: Color sweep — `main.rs` (SERIALIZED)

**Files:**
- Modify: `crates/dbc-ui/src/main.rs` (58 literal lines)

**Interfaces:**
- Consumes: `crate::theme::ActiveTheme` (Task 1). Behavior-preserving; no produced interface.

- [ ] **Step 1: Sweep per The Sweep Rulebook.** Specifics: the `rgb(0x3a3a1e)` banner (main.rs:3910) → `cx.theme().bg_warn_banner`; the modal backdrop `rgba(0x00000099)` → `bg_backdrop`; `0x45475a` ambiguity per Rulebook; autocomplete-popup and status-bar literals map by role (`bg_panel`/`bg_hover`/`text_muted`/`accent` etc. — every literal in this file is one of the Rulebook's audited values).
- [ ] **Step 2: Build + test** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings.
- [ ] **Step 3: File-scoped audit** — `grep -n 'rgba\?(0x' crates/dbc-ui/src/main.rs` → zero hits.
- [ ] **Step 4: Visual sanity launch** — tab strip, status bar, autocomplete popup, params modal, a warn banner; identical in dark mode.
- [ ] **Step 5: Commit**

```bash
git add crates/dbc-ui/src/main.rs
git commit -m "refactor: main.rs color sweep to Theme fields"
```

---

### Task 10: Theme toggle UX — settings modal, topbar gear, palette action (SERIALIZED)

**Files:**
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::Settings` arm + `render_settings_panel` + topbar gear in `render_top_bar`, connections_ui.rs:1011)
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteAction::ToggleTheme` + `fixed_actions` row + test)
- Modify: `crates/dbc-ui/src/main.rs` (`set_theme`/`toggle_theme` methods + palette dispatch arm, main.rs:2053)

**Interfaces:**
- Consumes: `theme::{Theme, ActiveTheme}` (Task 1), `dbc_state::ThemeMode` (Task 1), `App::refresh_windows` (grounding correction 1), the `self.config.save(&self.config_path)` persistence idiom (main.rs:3670).
- Produces:

```rust
// palette.rs:
pub enum PaletteAction { …existing…, /// G14: toggles dark<->light directly (no submenu).
                         ToggleTheme }
// fixed_actions gains one UNCONDITIONAL row: ("Přepnout motiv", ToggleTheme)
// — signature UNCHANGED in this task (Task 11 changes it; see there).

// connections_ui.rs:
// ModalState gains: /// G14: app settings modal (theme row only, for now).
//                   Settings,

// main.rs (AppView):
fn set_theme(&mut self, mode: dbc_state::ThemeMode, cx: &mut Context<Self>);
fn toggle_theme(&mut self, cx: &mut Context<Self>);
```

- [ ] **Step 1: Write the failing palette test** (append to palette.rs's `mod tests`):

```rust
#[test]
fn theme_toggle_action_is_always_present() {
    let items = rank_items("", &[], &[], &[], false, 30);
    assert!(items.iter().any(|i| matches!(
        i, PaletteItem::Action { action: PaletteAction::ToggleTheme, .. })));
    // and it fuzzy-matches by its Czech label:
    let items = rank_items("motiv", &[], &[], &[], false, 30);
    assert!(items.iter().any(|i| matches!(
        i, PaletteItem::Action { action: PaletteAction::ToggleTheme, .. })));
}
```

- [ ] **Step 2: Run to see it fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui theme_toggle_action_is_always_present` → compile error (`ToggleTheme` missing).
- [ ] **Step 3: Implement.**
  - `palette.rs`: add the variant and the unconditional `("Přepnout motiv".to_string(), PaletteAction::ToggleTheme)` row at the end of `fixed_actions`' base vec (palette.rs:146–154).
  - `main.rs` (AppView methods, next to `toggle_favourite`'s save idiom main.rs:3669):

```rust
/// G14: single write-through path for both toggle surfaces (design §1.5).
/// A config-save failure still switches the SESSION theme (the live switch
/// must never be hostage to a read-only disk) — the error is surfaced in
/// the status line instead.
fn set_theme(&mut self, mode: dbc_state::ThemeMode, cx: &mut Context<Self>) {
    if self.config.theme != mode {
        self.config.theme = mode;
        self.status = match self.config.save(&self.config_path) {
            Ok(()) => format!(
                "motiv: {}",
                match mode { dbc_state::ThemeMode::Dark => "tmavý",
                             dbc_state::ThemeMode::Light => "světlý" }),
            Err(e) => format!("error: motiv se nepodařilo uložit ({e})"),
        };
    }
    cx.set_global(theme::Theme::from_mode(mode));
    // Re-highlight the editor with the new syntax palette (Task 6's spans
    // were computed against the old one), then repaint everything:
    self.sql.update(cx, |sql, cx| sql.kick_highlight(cx));
    cx.refresh_windows(); // NOT cx.refresh() — doesn't exist at rev 907ed09
    cx.notify();
}

fn toggle_theme(&mut self, cx: &mut Context<Self>) {
    let next = match self.config.theme {
        dbc_state::ThemeMode::Dark => dbc_state::ThemeMode::Light,
        dbc_state::ThemeMode::Light => dbc_state::ThemeMode::Dark,
    };
    self.set_theme(next, cx);
}
```

  (`kick_highlight` is currently a private method on `SqlInput` — make it `pub(crate)` in this task; if the field holding the editor entity is named differently than `self.sql`, re-locate by symbol from the startup code main.rs:4383.)
  - Palette dispatch (main.rs:2053 match): `PaletteAction::ToggleTheme => self.toggle_theme(cx),`.
  - `connections_ui.rs`: add `ModalState::Settings` (unit variant) plus its arm in `render_modal_overlay` (connections_ui.rs:1096) rendering a minimal panel — same overlay/backdrop shape as the other arms:

```rust
// inside render_modal_overlay's match:
ModalState::Settings => Some(self.render_settings_panel(cx).into_any_element()),

fn render_settings_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
    let mode = self.config.theme;
    let radio = |label: &'static str, m: dbc_state::ThemeMode, current: dbc_state::ThemeMode,
                 cx: &mut Context<Self>| {
        div().flex().gap_2().px_2().py_1().rounded_sm().cursor_pointer()
            .bg(if m == current { cx.theme().bg_selected } else { cx.theme().bg_hover })
            .child(if m == current { "●" } else { "○" })
            .child(label)
            .on_mouse_down(gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.set_theme(m, cx)))
    };
    div().flex().flex_col().gap_2().p_4().rounded_md()
        .bg(cx.theme().bg_panel).border_1().border_color(cx.theme().border)
        .child(div().text_color(cx.theme().text_primary).child("Nastavení"))
        .child(div().text_color(cx.theme().text_muted).child("Motiv"))
        .child(radio("Tmavý", dbc_state::ThemeMode::Dark, mode, cx))
        .child(radio("Světlý", dbc_state::ThemeMode::Light, mode, cx))
        .child(div().px_2().py_1().rounded_sm().bg(cx.theme().bg_hover).cursor_pointer()
            .child("Zavřít")
            .on_mouse_down(gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| { this.modal = None; cx.notify(); })))
}
```

  (Adapt div-builder details to the file's existing modal-panel idiom — e.g. if the other arms use a shared backdrop wrapper, reuse it; the modal stays open after a theme click so the user sees the live switch, and closes via "Zavřít"/the overlay's existing Esc handling.)
  - Topbar gear: in `render_top_bar` (connections_ui.rs:1011), append after the version/right-side elements an icon button `"⚙"` (`text_color(cx.theme().text_muted)`, hover `text_primary`, `cursor_pointer`) whose `on_mouse_down` sets `self.modal = Some(ModalState::Settings); cx.notify();` — same click idiom as the dropdown's ★/✎ icon buttons.
- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings.
- [ ] **Step 5: Visual sanity launch (the light-mode gate — design §5 mitigation b).** Toggle via the palette AND via the gear modal. In light mode eyeball specifically the ambiguous-role screens: grid selected row vs. panel borders (the split `0x45475a` sites — a role swap is INVISIBLE in dark mode and glaring here), dialog borders, find-match, diff tints over light text, the editor (light syntax colors + selection), ER diagram, monitor well, compare banners. Confirm: toggle back to dark is pixel-identical to pre-G14; relaunch the app — the saved mode is restored.
- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/palette.rs crates/dbc-ui/src/main.rs
git commit -m "feat: theme toggle (settings modal, topbar gear, palette action)"
```

---

### Task 11: Chart wiring — tab kind, picker modal, grid button, palette action (SERIALIZED)

**Files:**
- Modify: `crates/dbc-ui/src/tabs.rs` (`TabContent::Chart` variant)
- Modify: `crates/dbc-ui/src/grid.rs` (`GridEvent::OpenChart` + toolbar "Graf" button)
- Modify: `crates/dbc-ui/src/connections_ui.rs` (`ModalState::ChartPicker` + `render_chart_picker_panel`)
- Modify: `crates/dbc-ui/src/palette.rs` (`PaletteAction::OpenChart`, `fixed_actions`/`rank_items` gain `chart_available: bool`, tests)
- Modify: `crates/dbc-ui/src/main.rs` (open/confirm/reopen handlers, `render_tab_content` arm, palette dispatch + `build_palette_items` threading)

**Interfaces:**
- Consumes: `ChartView`/`ChartViewEvent` (Task 8), `chart_data::ChartKind` (Task 7), `tabs::{Tabs, ResultTab, collapse_title}`, `Rc<RefCell<ResultBuffer>>` from `TabContent::Grid`, Arrow `DataType::is_numeric` (the main.rs:1148–1153 idiom — design §2.1: the identical scan, no new detector).
- Produces:

```rust
// tabs.rs (after Compare, same Entity-only shape as Monitor/Plan/Diagram/Compare):
/// G14: bar/line chart over a result-buffer snapshot — titled
/// "Graf: {source tab title}", stacked like an ad-hoc tab (no preview-key
/// dedup). Keeps its own buffer Rc inside ChartView: the source tab
/// closing never breaks it (design §2.6 snapshot semantics).
Chart { view: Entity<crate::chart_view::ChartView> },

// grid.rs:
/// G14: "Graf" toolbar button — main.rs opens the axis picker for the tab
/// owning this grid (resolved via the emitting Entity in on_grid_event).
OpenChart,   // new GridEvent variant (unit)

// connections_ui.rs — ModalState gains:
ChartPicker {
    source_title: String,
    buffer: std::rc::Rc<std::cell::RefCell<dbc_buffer::ResultBuffer>>,
    /// (column name, is_numeric) per buffer column, display order.
    columns: Vec<(String, bool)>,
    kind: crate::chart_data::ChartKind,
    x_col: usize,
    /// One flag per column; only numeric columns are toggleable (design
    /// §2.1: Y list pre-filtered numeric, X unfiltered).
    y_selected: Vec<bool>,
    /// Some(tab_id): re-pick — reconfigure that tab's ChartView in place.
    edit_tab: Option<u64>,
},

// palette.rs — SIGNATURE CHANGE (both fns, all callers + tests updated here):
pub fn fixed_actions(monitor_available: bool, chart_available: bool) -> Vec<(String, PaletteAction)>;
pub fn rank_items(query: &str, tables: &[TableSource], history: &[HistorySource],
                  connections: &[ConnectionSource], monitor_available: bool,
                  chart_available: bool, cap: usize) -> Vec<PaletteItem>;
pub enum PaletteAction { …, /// G14: axis picker for the active Grid tab.
                          OpenChart }

// main.rs (AppView):
fn open_chart_picker(&mut self, from_grid: Option<Entity<ResultGrid>>, cx: &mut Context<Self>);
fn confirm_chart_picker(&mut self, cx: &mut Context<Self>);
fn on_chart_view_event(&mut self, emitter: Entity<chart_view::ChartView>,
                       event: &chart_view::ChartViewEvent, cx: &mut Context<Self>);
```

- [ ] **Step 1: Write the failing palette test** (append to palette.rs `mod tests`; also mechanically add the new `false` argument at position 6 to every existing `rank_items(...)` call in the test module — same churn G9 caused with `monitor_available`):

```rust
#[test]
fn chart_entry_present_only_when_a_grid_tab_is_active() {
    let items = rank_items("", &[], &[], &[], false, true, 30);
    assert!(items.iter().any(|i| matches!(
        i, PaletteItem::Action { action: PaletteAction::OpenChart, .. })));
    let items = rank_items("", &[], &[], &[], false, false, 30);
    assert!(items.iter().all(|i| !matches!(
        i, PaletteItem::Action { action: PaletteAction::OpenChart, .. })));
}
```

- [ ] **Step 2: Run to see it fail** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui chart_entry` → compile error.
- [ ] **Step 3: Implement, in this order:**
  1. `palette.rs`: variant + `("Graf z výsledku".to_string(), PaletteAction::OpenChart)` pushed when `chart_available` (same absent-not-disabled posture as the monitor entry, palette.rs:155–157); thread the param through `rank_items`; fix every caller (main.rs `build_palette_items` passes `matches!(self.tabs.active_content(), Some(TabContent::Grid { .. }))` — re-locate the actual active-content accessor by symbol in tabs.rs).
  2. `tabs.rs`: the `Chart` variant (code above). tabs.rs stays GPUI-free beyond the Entity type name, per its module doc.
  3. `grid.rs`: `GridEvent::OpenChart`; toolbar button in the same row as "Export ▾"/"Sloupce ▾" (grid.rs:1175/1188 idiom):

```rust
.child(
    div().px_2().py_0p5().rounded_sm().cursor_pointer()
        .bg(cx.theme().bg_hover)
        .hover(|s| s.bg(cx.theme().bg_selected))
        .child("Graf")
        .on_mouse_down(gpui::MouseButton::Left,
            cx.listener(|_, _, _, cx| cx.emit(GridEvent::OpenChart))),
)
```

  4. `connections_ui.rs`: the `ModalState::ChartPicker` arm + `render_chart_picker_panel` (same panel skeleton as `render_settings_panel`): heading `"Graf: {source_title}"`; two kind buttons ("Sloupcový" = `ChartKind::Bar`, "Čárový" = `ChartKind::Line`, selected one on `bg_selected`); an X-column list (every column, radio-style, `x_col` index); a Y checkbox list showing ONLY columns with `numeric == true` (`"☑"`/`"☐"` prefix, toggling `y_selected[i]`); footer buttons "Zrušit" (closes modal) and "Vytvořit graf" / "Použít" (label by `edit_tab.is_none()`), the confirm calling `self.confirm_chart_picker(cx)`. All mutations edit the `ModalState` fields in place via `if let Some(ModalState::ChartPicker { .. }) = &mut self.modal` listeners (existing arms' idiom, e.g. CompareDialog's).
  5. `main.rs`:

```rust
// on_grid_event (main.rs:2799) gains:
GridEvent::OpenChart => self.open_chart_picker(Some(emitter.clone()), cx),

// palette dispatch (main.rs:2053) gains:
PaletteAction::OpenChart => self.open_chart_picker(None, cx),

fn open_chart_picker(&mut self, from_grid: Option<Entity<ResultGrid>>, cx: &mut Context<Self>) {
    if self.modal.is_some() {
        self.status = "zavřete nejprve otevřený dialog".into();
        cx.notify();
        return;
    }
    // Resolve the source tab: the one owning the emitting grid Entity, or
    // the active tab (palette path). Entity<T> is comparable by identity.
    let source = self.tabs.iter().find(|t| match (&t.content, &from_grid) {
        (TabContent::Grid { grid, .. }, Some(g)) => grid == g,
        (TabContent::Grid { .. }, None) => Some(t.id) == self.tabs.active_id(),
        _ => false,
    }).map(|t| (t.title.clone(), match &t.content {
        TabContent::Grid { buffer, .. } => buffer.clone(),
        _ => unreachable!("matched Grid above"),
    }));
    let Some((source_title, buffer)) = source else {
        self.status = "graf lze vytvořit jen z výsledkové mřížky".into();
        cx.notify();
        return;
    };
    // design §2.1: the EXACT is_numeric scan from main.rs:1148-1153.
    let columns: Vec<(String, bool)> = buffer.borrow().schema().fields().iter()
        .map(|f| (f.name().clone(), f.data_type().is_numeric()))
        .collect();
    if !columns.iter().any(|(_, numeric)| *numeric) {
        self.status = "výsledek nemá žádný číselný sloupec — graf nelze vytvořit".into();
        cx.notify();
        return;
    }
    let n = columns.len();
    // default: first numeric column pre-checked as Y, column 0 as X.
    let mut y_selected = vec![false; n];
    if let Some(i) = columns.iter().position(|(_, num)| *num) { y_selected[i] = true; }
    self.modal = Some(ModalState::ChartPicker {
        source_title, buffer, columns,
        kind: chart_data::ChartKind::Bar, x_col: 0, y_selected, edit_tab: None,
    });
    cx.notify();
}

fn confirm_chart_picker(&mut self, cx: &mut Context<Self>) {
    // Validate BEFORE taking the modal — an invalid pick leaves the dialog
    // open untouched with a status nudge, never a half-configured chart.
    let valid = matches!(&self.modal, Some(ModalState::ChartPicker { y_selected, .. })
        if y_selected.iter().any(|on| *on));
    if !valid {
        self.status = "vyberte alespoň jeden číselný sloupec pro osu Y".into();
        cx.notify();
        return;
    }
    let Some(ModalState::ChartPicker { source_title, buffer, columns: _,
        kind, x_col, y_selected, edit_tab }) = self.modal.take() else { return };
    let y_cols: Vec<usize> =
        y_selected.iter().enumerate().filter(|(_, on)| **on).map(|(i, _)| i).collect();
    match edit_tab {
        Some(id) => {
            // re-pick: reconfigure the existing tab's view in place (§2.4)
            if let Some(TabContent::Chart { view }) = self.tabs.content_of(id) {
                view.update(cx, |v, cx| v.reconfigure(kind, x_col, y_cols, cx));
            }
        }
        None => {
            let view = cx.new(|_| chart_view::ChartView::new(
                buffer, kind, x_col, y_cols, source_title.clone()));
            cx.subscribe(&view, Self::on_chart_view_event).detach();
            let conn_identity = self.current_conn_identity();
            self.tabs.open(ResultTab {
                id: 0, // Tabs::open assigns
                title: tabs::collapse_title(&format!("Graf: {source_title}")),
                pinned: false,
                preview_key: None, // stacked like ad-hoc tabs, Plan precedent
                conn_identity,
                content: TabContent::Chart { view },
            });
        }
    }
    cx.notify();
}

fn on_chart_view_event(&mut self, emitter: Entity<chart_view::ChartView>,
                       _event: &chart_view::ChartViewEvent, cx: &mut Context<Self>) {
    // ReopenPicker (only variant): reopen the picker seeded from the view,
    // editing that tab in place (design §2.4's only interaction).
    if self.modal.is_some() {
        self.status = "zavřete nejprve otevřený dialog".into();
        cx.notify();
        return;
    }
    let Some(tab_id) = self.tabs.iter().find(|t| matches!(
        &t.content, TabContent::Chart { view } if view == &emitter)).map(|t| t.id) else { return };
    let (kind, x_col, y_cols) = emitter.read(cx).picker_seed();
    let (source_title, buffer) = {
        let v = emitter.read(cx);
        (v.source_title().to_string(), v.buffer_handle()) // buffer_handle():
        // one-line accessor added to ChartView in this task — returns the
        // Rc<RefCell<ResultBuffer>> clone the picker needs.
    };
    let columns: Vec<(String, bool)> = buffer.borrow().schema().fields().iter()
        .map(|f| (f.name().clone(), f.data_type().is_numeric()))
        .collect();
    let mut y_selected = vec![false; columns.len()];
    for c in &y_cols {
        if let Some(flag) = y_selected.get_mut(*c) { *flag = true; }
    }
    self.modal = Some(ModalState::ChartPicker {
        source_title, buffer, columns, kind, x_col, y_selected,
        edit_tab: Some(tab_id),
    });
    cx.notify();
}

// render_tab_content (main.rs:3855 area) gains:
TabContent::Chart { view } => view.clone().into_any_element(),
```

  Adapt accessor names (`tabs.active_id()`, `tabs.content_of(id)`, `ResultTab` construction fields) to the real `tabs.rs` API by symbol — `ResultTab { id, title, pinned, preview_key, conn_identity, content }` is the shape at tabs.rs:59–86, and `Tabs::open` overwrites `id`. If `content_of` doesn't exist, iterate `self.tabs.iter()`... via whatever accessor the Monitor arm at main.rs:3875 already uses. Also handle `ResultBuffer`'s import in connections_ui.rs (`use dbc_buffer::ResultBuffer;` or fully-qualified). Remove any leftover `#![allow(dead_code)]` from Tasks 7/8.
- [ ] **Step 4: Run to green** — `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-ui`; zero warnings.
- [ ] **Step 5: Visual sanity launch** — run `SELECT` over the SQLite fixture: "Graf" button visible on the grid toolbar; picker lists numeric Y columns only; bar chart renders grouped bars per X tick; line chart renders polylines with gaps at NULLs; >4 Y columns wraps the series palette (accepted v1, design §5); a >500-row result shows "zobrazeno prvních … z … řádků"; "Upravit…" reopens the picker prefilled and edits in place; closing the SOURCE grid tab leaves the chart tab alive (snapshot, §2.6); palette shows "Graf z výsledku" only when a grid tab is active; both themes render the chart legibly.
- [ ] **Step 6: Commit**

```bash
git add crates/dbc-ui/src/tabs.rs crates/dbc-ui/src/grid.rs crates/dbc-ui/src/connections_ui.rs crates/dbc-ui/src/palette.rs crates/dbc-ui/src/main.rs
git commit -m "feat: chart tab wiring (picker modal, grid button, palette action)"
```

---

### Task 12: Final audit + full test pass + version bump (SERIALIZED, last)

**Files:**
- Modify: `crates/dbc-ui/Cargo.toml` (version)

- [ ] **Step 1: Whole-crate grep audit (the T-theme-7 merge gate — design §1.3, hard gate not suggestion).**

Run (Git Bash): `grep -n 'rgba\?(0x' crates/dbc-ui/src/*.rs`
Expected hits ONLY: (a) `theme.rs` (the two palette constructors + its tests), (b) `#[cfg(test)]` modules (sql_input.rs:1404–1421 and any test colors added since). ANY other hit is a sweep miss — fix it (map per The Sweep Rulebook) before proceeding.

- [ ] **Step 2: Full workspace-relevant test pass**

Run: `%USERPROFILE%\.cargo\bin\cargo.exe test -p dbc-core -p dbc-state -p dbc-buffer -p dbc-ui`
Expected: all green, zero warnings. Then `%USERPROFILE%\.cargo\bin\cargo.exe build -p dbc-ui` — zero warnings.

- [ ] **Step 3: Bump workspace version to the next minor per merge order** in `crates/dbc-ui/Cargo.toml` (line 3, `version = "0.10.0"` at plan-writing time — the actual number is assigned at merge time by the orchestrator based on what has merged before this phase; write the next minor). Satellite crates (`dbc-state`, `dbc-core`, `dbc-buffer`, drivers) stay `0.1.0` per repo convention.
- [ ] **Step 4: Commit**

```bash
git add crates/dbc-ui/Cargo.toml
git commit -m "chore: bump dbc-ui version for G14 polish & extras"
```

---

## Self-review notes (spec coverage, resolved deviations)

**Spec coverage check** — every design section maps to a task: §0 audit method → re-run recorded in CURATION point 2 rationale + The Sweep Rulebook; §1.1 Theme struct → Task 1 (24 top-level fields: design's 19 + `bg_deep`/`bg_warn_banner`/`accent_alt` from the re-audit; 7 syntax fields per curation 1a); §1.2 global distribution → Task 1 (`ActiveTheme`); §1.3 sweep → Tasks 2–6, 9 + Rulebook (per-file commits, ambiguous-`0x45475a` rule, grep gate); §1.4 light palette → Task 1 (§5's contrast verification executed — two values corrected, test added); §1.5 toggle/persistence → Tasks 1 (ThemeMode) + 10 (both surfaces, one write-through path); §2.1–2.6 charts → Tasks 7, 8, 11 (bar+line, numeric-only Y via the existing `is_numeric` scan, gaps never zero, width-derived cap w/ 500 hard bound, static v1, no export, snapshot tab); §3 decomposition → the dependency table (this plan splits the design's T-theme-2..6 by the CURRENT 11-file reality); §4 tests → Task 1 (config forward-compat + roundtrip, non-default-fields, verbatim-G6, contrast), Task 7 (all listed prepare/scale cases), compile-time backstop = Task 12's grep gate; §5 risks → zebra check (Task 2), contrast (Task 1), wraparound accepted (Task 11 sanity list), width-derived cap adopted (curation 3, Task 7).

**Deviations from the design, each grounded:** (1) `cx.refresh()` → `cx.refresh_windows()` — API fact at the pinned rev. (2) `TabContent::Chart` is Entity-only — matches the four variants shipped since the design; no information lost (buffer/title live in `ChartView`). (3) `prepare()` takes `Vec<Option<String>>` — honest NULLs via `cell_is_null` instead of string-typed NULL sniffing. (4) Light `warn`/`success` corrected to clear AA — executing §5's own needs-verification instruction; recorded in `light()`'s comment. (5) `highlight()` gains a theme parameter (not a global read) — background-thread constraint; the curation's "migrate to `cx.theme().syntax`" is honored at the *call site*, which is the only place a `cx` exists. (6) The two warn-banner one-offs (`0x3a3a1e`/`0x2a2a1e`) collapse to one field — the single deliberate value change in the sweep, flagged in Task 4's commit. (7) `identifier` kept in `EditorSyntaxTheme` although no capture produces it — the design listed it; it is the future-capture default and costs one field.

**Placeholder scan:** the `…unchanged…`/`…` ellipses appear only inside code whose surrounding text names the exact existing code to keep (kick_highlight's spawn tail, highlight's body-minus-one-call) — each is an instruction to preserve named, located code, not an unwritten design. `flush_run`/`paint_label` bodies are specified by their doc comments + the named verbatim idiom lines to copy (er_diagram_view.rs:563–571, :458–492). `on_chart_view_event` is written out in full. No TBD/TODO remains.

**Type consistency check:** `ThemeMode` (dbc_state) vs `Theme`/`EditorSyntaxTheme` (dbc-ui) — config never holds a color ✓; `ChartKind` lives in `chart_data` and is the type used by `ModalState::ChartPicker`, `ChartView::new`, `reconfigure`, `picker_seed` ✓; `visible_ticks`/`scale_to`/`bar_range` signatures match between Task 7 (definition), Task 8 (paint), and tests ✓; `fixed_actions`/`rank_items` signature change happens once, in Task 11, with all callers and tests updated in that same task (Task 10 deliberately does NOT change the signature — its row is unconditional) ✓; `GridEvent::OpenChart` (grid.rs) ↔ `on_grid_event` arm (main.rs) ✓; `ChartViewEvent::ReopenPicker` emitted in Task 8's header button, consumed in Task 11's `on_chart_view_event` ✓.
