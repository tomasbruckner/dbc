//! The app's component layer: the handful of shapes every screen is built
//! from, in one place instead of written out again at each call site.
//!
//! ## Why this exists
//!
//! It did not, and the cost was measurable. Before this module the modal
//! panel chrome — surface colour, border, radius, padding, column layout,
//! text colour — was spelled out longhand **22 times, character for
//! character**, plus 19 near-variants that differed only in which line the
//! author happened to leave out. There were **five** button helpers, two of
//! which (`styled_button` and `compare_button`) differed by four pixels of
//! padding and nothing else, and eleven more buttons written inline.
//!
//! That is not a tidiness complaint. It is where the „Databáze (výchozí)"
//! wrap came from (a label column sized once, in one of the copies) and it
//! is what makes „change how dialogs look" a 22-file-region edit that
//! nobody can verify. A component you cannot see on screen is a component
//! whose copies drift, and this app has no way to render a GPUI panel
//! headlessly — so the defence has to be that there is only one copy.
//!
//! ## What is here and what deliberately is not
//!
//! Here: the things that repeat with no meaningful variation — the
//! surface, the two panel flavours built on it, the three button
//! flavours, the checkbox, the labelled field row.
//!
//! Not here: [`crate::connections_ui`]'s caption buttons (window
//! minimise/maximise/close — those are platform chrome carrying
//! `window_control_area`, not app buttons) and the schema tree's rows
//! (a virtualised uniform list with its own hit-testing). Both LOOK like
//! candidates and neither is one; pulling them in would mean a component
//! with a flag for every caller, which is the shape this module exists to
//! replace.

use gpui::{
    div, px, Div, ElementId, IntoElement, ParentElement as _, SharedString, Stateful,
    Styled as _, InteractiveElement as _,
};

use crate::connections_ui::TextField;
use crate::theme::Theme;

/// The raised surface every floating thing sits on: panel colour, hairline
/// border, corner radius. Nothing else — no size, no padding, no layout.
///
/// Split out from [`panel`] because the third family of call sites (the
/// grid's dropdowns, the monitor's popups) wants the surface and its own
/// scroll body, and would otherwise have to un-set `panel`'s padding.
pub(crate) fn surface(theme: Theme) -> Div {
    div().bg(theme.bg_panel).border_1().border_color(theme.border).rounded_md()
}

/// A modal dialog panel of a fixed width.
///
/// Fixed, not fluid, and that is the design: a dialog that resizes with
/// the window makes its own line lengths unpredictable, and every one of
/// these is a form or a paragraph. The width is the ONE thing that varies
/// between dialogs, so it is the one argument.
///
/// Chain `.id(…)` for a dialog that needs to be interactive as a whole and
/// `.max_h(…)` for one whose body can grow past the window — both compose,
/// because this returns a plain `Div`.
pub(crate) fn panel(width: f32, theme: Theme) -> Div {
    surface(theme)
        .w(px(width))
        .p_4()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(theme.text_primary)
}

/// A small floating surface: context menus, dropdowns, hover cards.
///
/// Tighter than [`panel`] on purpose — a menu is a list of short lines and
/// `p_4` around it reads as a dialog that forgot its buttons.
pub(crate) fn popover(theme: Theme) -> Div {
    surface(theme).p_2().flex().flex_col().gap_1().text_color(theme.text_primary)
}

/// THE button: what a dialog's „Uložit" / „Zrušit" is made of.
///
/// `Stateful<Div>`, so a caller adds `.on_click(…)` and anything else it
/// needs — a fixed width, a focus ring — by chaining rather than by this
/// function growing a parameter for it. [`workspace_choice`] is that
/// pattern applied to the one button in the app that must be tabbable.
pub(crate) fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: Theme,
) -> Stateful<Div> {
    button_state(id, label, true, theme)
}

/// The same button, with its ENABLED state.
///
/// Disabled means three things together, and the eight sites that wrote
/// this out by hand each remembered two of them: muted text, no pointer
/// cursor, and no hover fill. A button that still lights up under the
/// mouse while refusing every click is worse than one that looks dead,
/// because the user tries twice.
///
/// A boolean, not a separate `button_disabled` function, because this is a
/// STATE the same button moves between — „Zrušit" is disabled only while
/// the job it would cancel is still starting. Two functions would put that
/// choice in an `if` at every call site, which is exactly where the three
/// forgotten details lived.
///
/// It is still cosmetic, and deliberately: every caller whose action can
/// refuse also refuses at click time. This is the app's existing posture
/// (see [`toolbar_button`]) — the dimming is a promise about what will
/// happen, not the mechanism that makes it happen.
pub(crate) fn button_state(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    theme: Theme,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .bg(theme.bg_hover)
        .rounded_md()
        .child(label.into());
    if enabled {
        base.cursor_pointer().hover(move |s| s.bg(theme.bg_selected))
    } else {
        base.text_color(theme.text_disabled)
    }
}

/// A button that sits in a column of settings rows rather than in a row of
/// dialog buttons — tighter padding, smaller radius, so a stack of them
/// reads as a list and not as a toolbar that fell over.
pub(crate) fn row_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(theme.bg_hover)
        .cursor_pointer()
        .hover(move |s| s.bg(theme.bg_selected))
        .child(label.into())
}

/// One top-bar control.
///
/// `active` lights it as an on/off state (the panel toggles); `enabled`
/// dims it and is otherwise cosmetic — every caller whose action can refuse
/// still refuses at click time, which is this codebase's existing posture
/// for the palette.
///
/// `occlude` is not optional: every top-bar button sits inside the window's
/// drag area, and without it the platform swallows the click as a titlebar
/// drag.
pub(crate) fn toolbar_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    enabled: bool,
    theme: Theme,
) -> Stateful<Div> {
    let fg = match (enabled, active) {
        (false, _) => theme.border,
        (true, true) => theme.text_primary,
        (true, false) => theme.text_muted,
    };
    div()
        .id(id)
        .occlude()
        .px_2()
        .py(px(3.))
        .rounded_md()
        .cursor_pointer()
        .text_color(fg)
        .bg(if active { theme.bg_hover } else { theme.bg_app })
        .hover(move |s| s.bg(theme.bg_hover).text_color(theme.text_primary))
        .child(label.into())
}

/// A hairline between groups of top-bar controls. Grouping is what makes a
/// row of buttons readable at a glance instead of a wall of words.
pub(crate) fn toolbar_separator(theme: Theme) -> Div {
    div().w(px(1.)).h(px(16.)).mx_1().bg(theme.border)
}

/// One option of a segmented control — the engine picker, the chart-kind
/// picker: a small closed set where a popup would cost a click and hide
/// the very thing being chosen.
///
/// The selected state is a FILLED accent chip with inverted text, not a
/// slightly different grey. The three call sites had three different
/// answers to „what does selected look like?" before this existed
/// (`accent` + inverted text, `bg_selected`, and a `●`/`○` glyph), which
/// is the drift this module is for.
///
/// NOT used for the theme radio, deliberately: `●`/`○` in a labelled
/// vertical list is a radio GROUP, a different idiom from a horizontal
/// segmented control, and flattening the two would be a component with a
/// flag rather than a component.
pub(crate) fn segmented_option(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .bg(if selected { theme.accent } else { theme.bg_hover })
        .text_color(if selected { theme.bg_panel } else { theme.text_muted })
        .child(label.into())
}

/// A row of checkboxes that WRAPS instead of running off the panel.
///
/// It exists because the obvious hand-written version does not work and
/// does not fail loudly either. A flex item will not shrink below its own
/// content, so two checkboxes whose labels together outrun the dialog are
/// not squeezed, ellipsised or clipped — the second label is simply painted
/// OUTSIDE the white surface, on top of whatever the modal is covering.
/// That is what „Důvěřovat certifikátu serveru (TrustServerCertificate)"
/// did beside „Šifrovat připojení (Encrypt)" in the MSSQL dialog, which the
/// user photographed on 2026-09-01.
///
/// `flex_wrap` rather than one checkbox per line, because the pair that
/// DOES fit („Pouze pro čtení" / „Oblíbené") should stay side by side.
/// Wrapping settles that per row at the width the row actually has, instead
/// of an author settling it once by eye and being right until someone
/// lengthens a label.
///
/// The horizontal gap is the wider of the two on purpose: inside a line it
/// is the only thing between one label and the next box, so it has to read
/// as a bigger break than the space between lines.
///
/// It does not rescue a row whose SINGLE label is wider than the panel —
/// nothing but a wider panel does. It does mean that no combination of
/// labels which individually fit can spill.
pub(crate) fn checkbox_row() -> Div {
    div().flex().flex_row().flex_wrap().gap_x_4().gap_y_1()
}

/// A checkbox drawn as a glyph rather than as a platform control, because
/// GPUI has no platform control and a hand-drawn box would need its own
/// hit-testing to gain nothing.
pub(crate) fn checkbox(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
) -> Stateful<Div> {
    let mark = if checked { "☑" } else { "☐" };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .cursor_pointer()
        .child(mark)
        .child(label.into())
}

/// Width of the label column in every dialog [`field_row`] builds.
///
/// It was 130 px, and „Databáze (výchozí)" did not fit: the text wrapped
/// and left the closing bracket alone on a second line, which is what the
/// user photographed on 2026-09-01. Sized here for the LONGEST label the
/// app passes — „ODBC driver (volitelné)", 23 characters — not for the one
/// that happened to be reported.
///
/// The two modifiers in [`field_row`] matter as much as the number.
/// `whitespace_nowrap` is the actual guarantee: a label too long for the
/// column overflows by a few pixels instead of folding, so misjudging this
/// width is cosmetic rather than the broken line above. `flex_none` stops a
/// long VALUE from squeezing the column back down and reintroducing the
/// wrap from the other side.
pub(crate) const FIELD_LABEL_W: f32 = 176.;

/// „A label column, then whatever this row is about."
///
/// The width is a parameter rather than a constant because the app has
/// four honest label columns, not one: dialog fields ([`FIELD_LABEL_W`]),
/// query parameters, CSV header mapping, and the key/value lists in the
/// admin panel and the about box. They hold different text and a single
/// width would be too wide for some and too narrow for others.
///
/// What is NOT a parameter is the part every one of them got wrong. The
/// five sites wrote the label cell out by hand and between them agreed on
/// nothing: widths of 110, 120, 130 and 160 px, one `flex_shrink_0`, no
/// `whitespace_nowrap` anywhere. So each of them could fold a label onto a
/// second line, which is the bug the user photographed in the connection
/// dialog on 2026-09-01 — and the connection dialog was the one site that
/// had been fixed. `flex_none` and `whitespace_nowrap` now come with the
/// column instead of being remembered.
pub(crate) fn labelled_row(
    label: impl Into<SharedString>,
    label_w: f32,
    theme: Theme,
) -> Div {
    div().flex().flex_row().items_center().gap_2().child(
        div()
            .w(px(label_w))
            .flex_none()
            .whitespace_nowrap()
            .text_color(theme.text_muted)
            .child(label.into()),
    )
}

/// A form row: a label of fixed width, then the field taking the rest.
pub(crate) fn field_row(
    label: impl Into<SharedString>,
    field: gpui::Entity<TextField>,
    theme: Theme,
) -> impl IntoElement {
    labelled_row(label, FIELD_LABEL_W, theme).child(div().flex_1().child(field))
}

#[cfg(test)]
mod tests {

    /// The one thing about this module that a test can actually reach:
    /// every component must be built from the THEME, never from a literal
    /// colour, or the light/dark switch stops covering it.
    ///
    /// A GPUI element cannot be rendered or inspected headlessly, so this
    /// reads the source instead — the same posture as
    /// `connections_ui`'s own panel pins.
    #[test]
    fn no_component_hardcodes_a_colour() {
        let src = include_str!("ui.rs");
        // Assembled so this test's own line cannot be the match.
        let needle = format!("{}(0x", "rgb");
        let body = src.split("mod tests").next().unwrap_or("");
        assert!(
            !body.contains(&needle),
            "a component hardcodes a colour — it will not follow the theme switch"
        );
        assert!(!body.contains("gpui::white()"), "same, via `white()`");
        assert!(!body.contains("gpui::black()"), "same, via `black()`");
    }

    /// `panel` and `popover` must both be built on `surface`, so „what a
    /// floating thing looks like" stays one decision. Two independent
    /// chains would drift exactly the way the 22 copies did.
    #[test]
    fn both_panel_flavours_are_built_on_the_shared_surface() {
        let src = include_str!("ui.rs");
        let body = src.split("mod tests").next().unwrap_or("");
        for f in ["fn panel(", "fn popover("] {
            let after = body.split(f).nth(1).expect(f);
            let end = after.find("\n}").unwrap_or(after.len());
            assert!(
                after[..end].contains("surface(theme)"),
                "`{f}` does not build on `surface` — the two would drift"
            );
        }
    }
}
