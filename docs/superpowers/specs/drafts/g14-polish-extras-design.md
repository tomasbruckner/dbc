# G14 — Polish & Extras: Theme System + Charts (design)

Date: 2026-08-23
Status: designed autonomously under the standing mandate (per §4 of the
2026-08-22 GUI target design); decisions recorded here for later user review.
Scope (spec row G14, lowest priority): theme system with light/dark toggle;
charts from a result tab (bar/line over selected columns).

## 0. Survey (basis for every decision below)

- **Color hardcoding, honest count:** `rgb(0x......)` literals in
  `crates/dbc-ui/src`: 125 call sites — `grid.rs` 53, `connections_ui.rs` 34,
  `main.rs` 20, `schema_tree.rs` 11, `history_panel.rs` 7. Plus 6 more sites
  using named `u32` consts (`STAGED_CELL_BG`, `DELETED_ROW_BG`,
  `INSERTED_ROW_BG`, declared `grid.rs:26-28`) and 4 `rgba(0x00000099)` modal
  backdrops (`main.rs`, `grid.rs` x2, `connections_ui.rs`) and 1 editor
  selection `rgba(0x3311ff30)` (`sql_input.rs:849`). **Total: 136 literal
  color call sites across 6 files.** No existing indirection (no
  `Theme`/`Colors` module) — every site is a raw literal today.
- **Palette in use today** is Catppuccin Mocha, unlabeled: `0x181825`
  (crust — topbars/tab strip), `0x1e1e2e` (base — panel/dialog bg),
  `0x232334` (alt grid row, undocumented one-off close to base),
  `0x313244` (surface0 — hover/inactive-selected bg), `0x45475a`
  (surface1 — borders + selected-row/cell bg + deeper hover, reused for two
  different roles today), `0x585b70` (surface2 — find-match bg),
  `0x2a2a3d` (bespoke — FK-joined column tint, not part of Catppuccin),
  `0xcdd6f4` (text), `0xa6adc8` (subtext0 — muted text), `0x7f849c`
  (subtext1 — a second, slightly different muted tone used once),
  `0x6c7086` (overlay0 — disabled/icon-muted), `0x89b4fa` (blue — accent/
  links), `0xf9e2af` (yellow — star/favourite/warn), `0xf38ba8` (red —
  error/danger/delete), `0xa6e3a1` (green — success/new-row),
  `0x6b5d2e`/`0x5d2e2e`/`0x2e5d3a` (muted gold/red/green — G5 staged/
  deleted/inserted row tints, deliberately desaturated so text stays
  legible over them), `0x00000099` (modal backdrop alpha), `0x3311ff30`
  (editor selection alpha-blue).
- **`dbc-state`:** `AppConfig` (`crates/dbc-state/src/config.rs`) is the
  existing persisted-settings home (TOML, atomic write via tmp+rename,
  `#[serde(default)]` fields for forward compat) — theme choice slots in
  here as one more field, same pattern as `favourite_objects`.
- **GPUI `Global` (pinned rev 907ed09, `crates/gpui/src/global.rs`):** a
  plain marker trait, `impl<T: Global> ReadGlobal/UpdateGlobal for T`
  blanket-implemented. `App` has `global::<G>()`, `global_mut`,
  `try_global`, `default_global`, `set_global`, `observe_global` (confirmed
  at `crates/gpui/src/app.rs:2006-2069`). This is a first-class, supported
  mechanism at the pinned rev — no borrow needed.
- **Custom painting precedent:** `connections_ui.rs` and `sql_input.rs`
  already implement a custom GPUI `Element` with `paint()` calling
  `window.paint_quad(..)` for selection/cursor rects. `window.paint_path`
  (`crates/gpui/src/window.rs:4174`) takes a `Path<Pixels>` built via
  `PathBuilder`/`Path::move_to`/`line_to`/`curve_to`
  (`crates/gpui/src/scene.rs:840-859`, `crates/gpui/src/path_builder.rs`).
  Quads + straight-line paths are enough for bar/line charts — **no
  external chart crate needed** (decision, §2).
- **Numeric detection today:** `main.rs:676-682` builds `numeric_cols:
  Vec<bool>` from `buf.borrow().schema().fields().iter().map(|f|
  f.data_type().is_numeric())` (Arrow `DataType::is_numeric`) — already
  reused once (by G5 sandbox for bare-vs-quoted SQL value emission). Charts
  reuse the identical call, not a new detector.
- **Tab mechanism (`crates/dbc-ui/src/tabs.rs`):** `TabContent` is a plain
  enum (`Grid { grid: Entity<ResultGrid>, buffer: Rc<RefCell<ResultBuffer>>
  }`, `Text { text, scroll_lines }`) held in `ResultTab` inside `Tabs`
  (GPUI-free plain data, deliberately unit-testable without a window).
  Adding `TabContent::Chart` is additive, same shape.
- **`ResultBuffer` (`crates/dbc-buffer/src`):** exposes `schema()`,
  `row_count()`, `column_count()`, `cell_text(row, col) -> String`,
  `cell_is_null`. No typed f64 accessor — chart data-prep parses
  `cell_text` the same way `sandbox::sql_value` already treats
  numeric-typed cells (strict parse; failure → treat as gap, see §2).

## 1. Theme system

### 1.1 `Theme` struct — field list (semantic, derived from §0 survey)

New module `crates/dbc-ui/src/theme.rs`. One struct, `Hsla` values (GPUI's
native color type; literals converted from the audited hex once during
authoring — `Hsla` interpolates correctly for future accent-shade math,
`Rgba` does not).

```rust
pub struct Theme {
    // surfaces (from bg_app.. down to bg_selected: replaces the 4-deep
    // hardcoded surface stack 0x181825/0x1e1e2e/0x313244/0x45475a)
    pub bg_app: Hsla,        // topbars, tab strip, gutter (was 0x181825)
    pub bg_panel: Hsla,      // dialog/panel/grid base bg (was 0x1e1e2e)
    pub bg_panel_alt: Hsla,  // alternating grid row (was 0x232334)
    pub bg_hover: Hsla,      // hover / inactive-selected bg (was 0x313244)
    pub bg_selected: Hsla,   // selected row/cell bg (was 0x45475a, role A)
    pub border: Hsla,        // panel/input borders (was 0x45475a, role B —
                              // split from bg_selected: same hex today,
                              // must diverge in light palette, see 1.4)
    pub bg_find_match: Hsla, // grid find-match cell bg (was 0x585b70)
    pub bg_joined_col: Hsla, // FK-joined column tint (was 0x2a2a3d)
    pub bg_backdrop: Hsla,   // modal backdrop, alpha (was 0x00000099)
    pub bg_selection: Hsla,  // editor text-selection, alpha (was 0x3311ff30)

    // text
    pub text_primary: Hsla,  // was 0xcdd6f4
    pub text_muted: Hsla,    // was 0xa6adc8
    pub text_faint: Hsla,    // was 0x7f849c (2nd muted tone, connections_ui)
    pub text_disabled: Hsla, // was 0x6c7086 (disabled/icon-muted)

    // semantic accents
    pub accent: Hsla,   // links/pointers/active-tab indicator (was 0x89b4fa)
    pub warn: Hsla,     // star/favourite/DDL-name highlight (was 0xf9e2af)
    pub danger: Hsla,   // errors, delete affordance (was 0xf38ba8)
    pub success: Hsla,  // boolean-true, success state (was 0xa6e3a1)

    // G5 sandbox diff tints (grid.rs consts)
    pub diff_staged_bg: Hsla,   // was 0x6b5d2e
    pub diff_deleted_bg: Hsla,  // was 0x5d2e2e
    pub diff_inserted_bg: Hsla, // was 0x2e5d3a

    // G6 hook — not consumed anywhere yet (editor is still plain-text);
    // reserved now so the eventual tree-sitter highlighter has a home
    // instead of inventing its own literals. Defaults derive from the
    // fields above so nothing needs a value until G6 lands.
    pub syntax: EditorSyntaxTheme,
}

pub struct EditorSyntaxTheme {
    pub keyword: Hsla,  // defaults to accent
    pub string: Hsla,   // defaults to success
    pub number: Hsla,   // defaults to warn
    pub comment: Hsla,  // defaults to text_disabled
    pub identifier: Hsla, // defaults to text_primary
}
```

- 19 top-level fields + 5 nested syntax fields = 24 named colors, vs. 136
  anonymous call sites today — confirms the literals are highly repeated,
  not 136 distinct values (survey above lists ~20 distinct hex values
  total, matching the field count almost 1:1).
- `bg_selected` vs `border` are listed as distinct fields despite sharing
  `0x45475a` today — collapsing them would re-hardcode a coincidence; the
  light palette (1.4) gives them different values on purpose, which is
  exactly the bug a "just rename the constant" approach would miss.

### 1.2 Distribution mechanism — decision: `cx.global::<Theme>()`

- Confirmed supported at the pinned rev (§0). `Theme: gpui::Global`,
  installed once in `main.rs` app setup via `cx.set_global(Theme::dark())`
  before the first window opens.
- Every render fn reads `cx.theme()` (a small extension trait,
  `fn theme(&self) -> &Theme { self.global::<Theme>() }`, added on `App`/
  `Context` — mirrors GPUI's own `ReadGlobal` blanket impl) instead of
  threading a `&Theme` parameter through every render function's
  signature. Rejected alternative: passing `&Theme` down through props —
  would touch every render function's signature across all 6 files for no
  behavioral gain over the global, and GPUI's own convention (confirmed in
  `global.rs` doc comments) is exactly this global-for-cross-cutting-
  concerns pattern.
- Toggling: `cx.set_global(Theme::light())` (or `dark()`), then
  `cx.refresh()` to force a full repaint — GPUI re-runs every `render()` on
  refresh, so no per-component subscription plumbing is needed. (Global
  *mutation* via `update_global` is also available if a future feature
  wants incremental theme edits; toggle is a full replace, simplest correct
  option for two fixed palettes.)

### 1.3 Refactor strategy — mechanical sweep, honest scale

- **Scale: 136 call sites, 6 files** (`grid.rs` 59, `connections_ui.rs` 35,
  `main.rs` 21, `schema_tree.rs` 11, `history_panel.rs` 7, `sql_input.rs`
  1 — counts include the const- and rgba-based sites folded in per §0).
  `grid.rs` alone is ~43% of the sweep and is also where the G5 diff tints
  live — highest-value and highest-risk file, done first and alone.
- **Mechanical rule per site:** `rgb(0x......)` → `cx.theme().<field>`
  (or, inside a non-`Context`-holding helper fn, a `&Theme` parameter
  threaded in locally — occurs only in a few free functions in `grid.rs`
  that build row/cell divs outside the main render closure). No new colors
  introduced; every replacement maps 1:1 to a §0/§1.1 audited value — this
  is a rename, not a redesign, which is what keeps the regression risk
  bounded (see §4).
  - Ambiguous sites (`0x45475a` used as both `border` and `bg_selected`)
    are resolved by *role at the call site*, not by the literal: a
    `.border_color(rgb(0x45475a))` call becomes `.border_color(cx.theme
    ().border)`; a `.bg(rgb(0x45475a))` on a selected row/cell becomes
    `.bg(cx.theme().bg_selected)`. Each such site is checked individually
    during the sweep (there are ~15 of these across the audit, all in
    `grid.rs`/`main.rs`/`connections_ui.rs`/`history_panel.rs`/
    `schema_tree.rs` per the `border_color(rgb(0x45475a))` / row-select
    `bg(rgb(0x45475a))` greps above).
- **Per-file task split — parallelizable**, since the 6 files don't share
  color-literal state, only the shared `Theme` type (which lands first,
  read-only after that point):
  - T-theme-1 (prereq, serial): add `theme.rs` (struct + `dark()` +
    `light()` constructors + `App`/`Context` extension trait), wire
    `cx.set_global` in `main.rs` startup. No call-site changes yet.
  - T-theme-2..6 (parallel, one PR/commit per file, in file-size order so
    the riskiest lands with the most eyes early):
    `grid.rs`, `connections_ui.rs`, `main.rs`, `schema_tree.rs`,
    `history_panel.rs`, `sql_input.rs` (last one is a single line, folded
    into whichever of the five lands last).
  - T-theme-7 (serial, after all sweeps land): grep-audit
    (`rgb(0x|rgba(0x`) over `crates/dbc-ui/src` returns zero hits outside
    `theme.rs` itself — the compile-time backstop (§3) makes this
    redundant long-term but is the merge gate for *this* phase.

### 1.4 Light palette — concrete values (tasteful defaults, contrast-minded)

Same 24-field shape, light values chosen so `text_primary` on `bg_panel`
and `bg_app` clears WCAG AA for normal text (>4.5:1); diff tints keep the
same hue identity (yellow/red/green) desaturated toward white instead of
black so staged text stays legible either way.

| field | dark (existing) | light (new) |
|---|---|---|
| bg_app | `#181825` | `#eef0f6` |
| bg_panel | `#1e1e2e` | `#ffffff` |
| bg_panel_alt | `#232334` | `#f6f7fb` |
| bg_hover | `#313244` | `#e4e7f0` |
| bg_selected | `#45475a` | `#cfd5e6` |
| border | `#45475a` | `#d3d7e3` |
| bg_find_match | `#585b70` | `#ffe58a` |
| bg_joined_col | `#2a2a3d` | `#eef1fb` |
| bg_backdrop | `#00000099` (60% black) | `#00000066` (40% black) |
| bg_selection | `#3311ff30` (19% blue) | `#3355ff33` (20% blue) |
| text_primary | `#cdd6f4` | `#1e2030` |
| text_muted | `#a6adc8` | `#4c5273` |
| text_faint | `#7f849c` | `#6b7094` |
| text_disabled | `#6c7086` | `#9498ad` |
| accent | `#89b4fa` | `#3b6fe0` |
| warn | `#f9e2af` | `#a8791a` |
| danger | `#f38ba8` | `#c2255c` |
| success | `#a6e3a1` | `#1f8a4c` |
| diff_staged_bg | `#6b5d2e` | `#fdf1c8` |
| diff_deleted_bg | `#5d2e2e` | `#fbdada` |
| diff_inserted_bg | `#2e5d3a` | `#d7f2df` |
| syntax.keyword | = accent | = accent |
| syntax.string | = success | = success |
| syntax.number | = warn | = warn |
| syntax.comment | = text_disabled | = text_disabled |
| syntax.identifier | = text_primary | = text_primary |

- `accent`/`warn`/`danger`/`success` are darkened relative to their dark-
  mode counterparts (not just the same hue on white) so they clear AA on a
  white `bg_panel` at normal text size — a straight hue-preserving
  lightness-only flip (as sometimes done) would fail contrast for
  `warn`/`success` specifically, which is why these are hand-picked rather
  than formula-derived.

### 1.5 Toggle UX + persistence + live switch

- **Persistence:** `AppConfig` (`crates/dbc-state/src/config.rs`) gains
  `#[serde(default)] pub theme: ThemeMode` where `enum ThemeMode { #[default]
  Dark, Light }` — same `#[serde(default)]` forward-compat pattern already
  used for `favourite_objects`/`read_only`, loads cleanly for existing
  config files with no `theme` key (mirrors the `old_config_without_
  favourites_loads` test already in `config.rs`).
  Config crate stays GPUI-free per the binding constraint — it stores the
  *choice* (`ThemeMode`), not a `Theme`/`Hsla` value; `dbc-ui` maps
  `ThemeMode` → `Theme::dark()`/`Theme::light()` at startup and on toggle.
- **Toggle UX — both, per the brief's "config dialog? palette command?
  both — decide":**
  1. A row in the existing settings/preferences surface (there isn't a
     dedicated "app settings" dialog yet distinct from the per-connection
     form — this adds the first one: a minimal modal, "Motiv: Tmavý /
     Světlý" radio, reachable from a new topbar icon next to the version
     string). Single decision, not deferred: this modal is the *only* new
     UI surface G14 needs to add for the toggle beyond the palette entry.
  2. A Ctrl+K palette action "Přepnout motiv" (toggles dark↔light
     directly, no submenu) — palette already generalizes over "app
     actions" (per the target-design spec's palette section), this is one
     more.
  Both write through the same path: `AppConfig.theme = new_mode;
  config.save(path)`, then `cx.set_global(Theme::from_mode(new_mode))`,
  then `cx.refresh()`.
- **Live switch:** no relaunch needed — `cx.refresh()` re-runs every
  mounted `render()`, and every render call now reads `cx.theme()` fresh
  (§1.2), so the whole window repaints in the new palette on the same
  frame the global is swapped.

## 2. Charts

### 2.1 Scope (v1)

- Bar and line charts only, over the **current result tab's** `ResultBuffer`
  — no cross-tab, no live-refresh-on-rerun (chart is a snapshot of the
  buffer at open time, consistent with "chart as a new tab tied to the
  source buffer snapshot" from the brief).
- Small dialog on open: pick one X column (any type — rendered as
  category/tick labels, not necessarily numeric) and one-or-more numeric Y
  columns (checkbox list, reusing the exact `data_type().is_numeric()`
  scan from `main.rs:676-682` — no new detector). Y-column list is
  pre-filtered to numeric columns only; X-column list is unfiltered.
- Multiple Y columns on a bar chart render as grouped bars per X tick;
  on a line chart as multiple polylines, one color per series drawn from
  a small fixed series palette (reuses `theme.accent`, `theme.success`,
  `theme.warn`, `theme.danger` in that order, wrapping if >4 series — v1
  accepts wrap-around reuse rather than adding a 5th+ hue, since result
  sets with >4 numeric Y columns charted at once are already a poor UX
  regardless of color).
- Row cap: charts render at most the first 500 rows of the buffer (bar/line
  over thousands of rows is illegible and slow to lay out); a status note
  ("zobrazeno prvních 500 z N řádků") appears when the buffer exceeds that,
  consistent with existing "never silently truncate without saying so"
  posture elsewhere in the app (e.g. auto-LIMIT guard).

### 2.2 Data prep (pure, testable — no GPUI)

New module `crates/dbc-ui/src/chart_data.rs` (GPUI-free, like `tabs.rs`/
`sandbox.rs`), consumed by a GPUI element that only paints its output:

```rust
pub struct ChartSeries { pub label: String, pub points: Vec<Option<f64>> }
pub struct ChartData { pub x_labels: Vec<String>, pub series: Vec<ChartSeries> }

pub fn prepare(
    x_labels: Vec<String>,          // one per row, already cell_text()'d
    y_columns: &[(String, Vec<String>)], // (column name, cell_text per row)
    row_cap: usize,
) -> ChartData
```

- Y-value parse: strict `f64::from_str` on the trimmed cell text (mirrors
  `sandbox::sql_value`'s "strict numeric-parse check, failure → treat as
  non-numeric" posture); a NULL cell or a parse failure becomes `None` — a
  **gap** in the series (line chart skips the segment, bar chart renders no
  bar for that tick), never silently coerced to 0 (0 is a real, different
  value).
- This is the only logic-bearing part of charts; the rest is layout math
  (scale rows→pixels) and painting, covered by the same module for the
  scale math (`pub fn scale_to(range: (f64,f64), value: f64, pixel_height:
  f32) -> f32`-shaped pure fns) so axis/scale bugs are unit-testable
  without a window too.

### 2.3 Rendering approach — decision: plain GPUI paint, no external crate

- Confirmed feasible from precedent (§0): a custom `Element` impl (same
  shape as `connections_ui.rs`'s selection-quad element)  whose `paint()`:
  - draws axis lines via `paint_quad` (1px-thick rects),
  - draws bars via `paint_quad` per (x-tick, series) pair, colored per
    §2.1's series palette,
  - draws line series via `PathBuilder`/`window.paint_path`: one
    `move_to` + `line_to` chain per series, stroked in the series color.
  - draws X tick labels via ordinary GPUI text layout (same text-shaping
    path already used for grid cell text), not custom-painted glyphs.
- **No external chart crate** — decision, not a placeholder: v1's shape
  (axis + bars/polylines, static, no legend interaction, no animation) is
  well within `paint_quad`/`paint_path`, and a crate dependency would (a)
  need its own GPUI integration shim anyway since no Rust chart crate
  targets GPUI natively, (b) pull in a rendering backend (e.g. plotters'
  raster backend) redundant with GPUI's own paint pipeline, (c) fight the
  theme system (§1) for color control. Revisit only if v2 wants
  interactive zoom/pan/tooltips at a fidelity plain paint can't cleanly
  give — not needed for v1's static bar/line.
- This mirrors the ER diagram's (G8, future) planned approach per the
  brief's own framing ("same canvas/paint investigation as the ER
  diagram") — one investigation, two consumers eventually.

### 2.4 Interaction — decision: static in v1

- No hover tooltip, no zoom/pan, no click-to-drill in v1. The brief flags
  this as a "maybe static — decide": deciding static, because the
  dialog-driven axis picker already covers the main use case (glance at a
  trend/comparison) without needing per-point inspection, and hover-value
  tracking would need mouse-position→data-index math plus a floating
  tooltip element — real scope, not a v1-blocking feature. Full cell-level
  detail is still one click away in the *grid* tab the chart was built
  from (chart tab and source grid tab coexist, per §2.6).
- Only interaction: X/Y column re-pick re-opens the same small dialog
  (edits the chart tab in place rather than spawning a new one), and the
  tab closes like any other result tab.

### 2.5 Export — decision: non-goal for v1

- No image/CSV export of the chart itself. The brief explicitly flags this
  as "non-goal v1 — decide": deciding non-goal, consistent with G4's export
  already covering the underlying data (CSV/TSV/JSON/INSERT) — exporting
  the *chart rendering* as an image is a distinct, separable feature
  (would need an off-screen paint-to-texture path) with no evidence of
  demand yet; revisit only if requested.

### 2.6 Where it opens

- **New result tab, tied to the source buffer snapshot** — matches the
  brief exactly. Extends `TabContent` (`tabs.rs:29-37`):
  ```rust
  pub enum TabContent {
      Grid { .. },
      Text { .. },
      Chart { chart: Entity<ChartView>, buffer: Rc<RefCell<ResultBuffer>>,
              source_tab_id: u64 },
  }
  ```
  `source_tab_id` is metadata only (shown in the chart tab's subtitle,
  e.g. "Graf: run3") — no live link back to the source tab; if the source
  tab closes, the chart tab keeps its own `Rc<RefCell<ResultBuffer>>`
  clone (same buffer-sharing pattern `Grid` already uses) and stays fully
  functional, consistent with "snapshot" in the brief.
  `ChartView` owns the `ChartData` (from §2.2, computed once at open time
  from the axis picker's selection) plus the picker-reopen state; it does
  not re-scan the buffer on every render.
- Title convention (matches existing `"Náhled: {table}"` / `"DDL: {name}"`
  pattern in `tabs.rs`): `"Graf: {source tab title}"`.
- Opened via a new toolbar/palette action "Graf z výsledku" available
  whenever the active tab is a `Grid` (disabled/absent otherwise) — same
  discoverability tier as existing grid-toolbar buttons (filters/export/
  columns menus already living in `grid.rs`'s toolbar row).

## 3. Task decomposition

Two independent workstreams (theme sweep touches only `dbc-ui`'s existing
render call sites; charts is new code) — **fully parallelizable against
each other**, and the theme sweep is *internally* parallelizable per-file
once T-theme-1 lands.

- **T-theme-1** (serial, prereq): `theme.rs` (`Theme`, `EditorSyntaxTheme`,
  `dark()`, `light()`, `ThemeMode`-mapping fn, `App`/`Context` ext trait),
  `dbc-state::config::ThemeMode` field + serde default + roundtrip test,
  wire `cx.set_global` at startup from loaded config.
- **T-theme-2..6** (parallel, one per file): sweep `grid.rs`,
  `connections_ui.rs`, `main.rs`, `schema_tree.rs`, `history_panel.rs` (+
  the single `sql_input.rs` site folded into whichever lands last) —
  each a standalone commit, each independently buildable/testable (theme
  is additive, doesn't remove the old literals' *values*, just their
  location).
- **T-theme-7** (serial, after 2-6): toggle UX — settings modal + palette
  action + `cx.refresh()` wiring; zero-hits grep audit as merge gate.
- **T-chart-1** (independent of all theme tasks): `chart_data.rs` — data
  prep + scale-math pure functions (§2.2).
- **T-chart-2** (after T-chart-1): axis-pick dialog (X/Y column checkbox
  list, numeric filter reusing `is_numeric()`).
- **T-chart-3** (after T-chart-1, parallel with T-chart-2): `ChartView`
  GPUI element — `paint_quad`/`paint_path` rendering, series palette
  (reads `cx.theme()` — soft-depends on T-theme-1 landing first for the
  color fields to exist, but not on T-theme-2..7; can be stubbed against a
  literal `Theme::dark()` instance and switched to the global trivially).
- **T-chart-4** (after T-chart-2/3): `TabContent::Chart` wiring + toolbar/
  palette "Graf z výsledku" action + tab title convention.

Suggested execution: T-theme-1 and T-chart-1 first (both quick, both
prereqs), then T-theme-2..6 fanned out to parallel workers alongside
T-chart-2/3 fanned out in the same wave, T-theme-7 and T-chart-4 as the
closing serial steps once their dependencies land.

## 4. Tests

- **Theme:** no runtime "does the UI look right" test is feasible without a
  window; the real backstop is **compile-time coverage** — once T-theme-2..6
  land, `Theme` fields are the *only* color source in `dbc-ui` render code,
  so a future accidental `rgb(0x...)` literal is caught by the T-theme-7
  grep audit (could additionally be a clippy-disallowed-method / custom
  lint if this recurs, but a merge-time grep is enough for a one-time
  sweep). `dbc-state::config`: extend the existing `old_config_without_
  favourites_loads`-style test with `old_config_without_theme_loads`
  (missing `theme` key defaults to `Dark`) plus a `theme_roundtrip` save/
  load test, both trivial additions to `config.rs`'s existing test module.
  `Theme::dark()`/`light()` get one test each asserting all 24 fields are
  distinct-from-default-initialized (catches a copy-paste field left at
  `Hsla::default()` = transparent black) — cheap, catches the most likely
  authoring mistake.
- **Charts:** `chart_data.rs` is pure data-in/data-out, tested the same way
  `tabs.rs`/`sandbox.rs` already are (no GPUI, no window):
  - `prepare()`: numeric parse success, NULL cell → `None`/gap, non-numeric
    garbage in a nominally-numeric column → `None`/gap (not a panic, not a
    silent 0), row cap truncates to `row_cap` and no further, multiple Y
    columns produce one `ChartSeries` each in input order.
  - scale-math fns: `scale_to` at range min/max/mid, degenerate range
    (min==max, i.e. a constant column) doesn't divide by zero.
  - `ChartView`'s picker-reopen path is thin enough to leave to a launch
    sanity check rather than a unit test (it's GPUI state mutation, same
    tier as other dialog-open/close paths in the codebase that aren't
    unit-tested today either).

## 5. Risks / needs-verification

- **Regression risk of the color sweep is the dominant risk in this phase**
  — 136 sites is enough that a mis-mapped field (e.g. swapping `border`/
  `bg_selected` at one of the ~15 ambiguous `0x45475a` sites, §1.3) would
  ship a subtly wrong dark-mode render that's easy to miss in review since
  the *value* stays identical, only the light-mode divergent behavior
  would expose it. Mitigation: (a) per-file commits (§1.3/§3) keep each
  diff small enough to line-by-line diff against the original literal
  table in §0; (b) a visual sanity launch (`run` skill / manual `cargo run
  -p dbc-ui`) after each file's sweep AND after the light palette lands,
  eyeballing the specific ambiguous-role screens (grid selection, dialog
  borders) since those are exactly where a role-swap would show; (c) the
  T-theme-7 grep audit as a hard merge gate, not a suggestion.
- **Light palette contrast is a "tasteful defaults" pick (§1.4), not a
  contrast-tool-verified one** — needs-verification: run the 4
  accent/warn/danger/success values against `text_primary`-on-`bg_panel`
  and against white text (for filled buttons, if any exist) through a
  proper contrast checker before shipping; the hand-darkening described in
  §1.4 is a reasonable eyeball pass, not a guarantee.
- **`bg_panel_alt` (`0x232334`) is currently undocumented/one-off** in the
  audit (used once, for alternating grid rows, and is suspiciously close
  to `bg_panel`) — needs-verification: confirm during the `grid.rs` sweep
  (T-theme-2) whether this was an intentional subtle zebra-stripe or a
  near-miss typo of `bg_panel`; if the latter, the light-palette value
  should collapse to equal `bg_panel` (no stripe) rather than invent a
  fake distinction.
- **Series-palette wraparound at >4 numeric Y columns (§2.1)** is an
  accepted v1 UX gap, not a bug — flagged here so it isn't rediscovered as
  a surprise; a legend (mapping series→color, trivial to add) is the
  natural v1.1 follow-up if it proves confusing in practice, deliberately
  left out of v1 scope per §2.4's "keep interaction/chrome minimal"
  posture.
- **Chart row cap (500) is a guess, not measured** — needs-verification:
  once `ChartView` paints real data, confirm 500 bars/points at typical
  result-tab widths doesn't already overplot before hitting the cap (if
  a typical panel is ~800px wide, 500 bars is ~1.6px/bar — likely too
  dense; the cap may need to be width-derived rather than fixed, revisit
  once T-chart-3 has something on screen to look at).
