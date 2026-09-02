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
    /// Subtle divider/outline border for elements that used `bg_hover` as a
    /// border color pre-G14 (a dark-mode value-faithful pun that happened to
    /// work because `bg_hover` was already a low-contrast near-background
    /// gray there) — light mode's `bg_hover` (0xe4e7f0) is nearly invisible
    /// against `bg_panel`'s white, so those sites need their own field
    /// (G14 final review NIT-1). DARK is `bg_hover`'s value VERBATIM
    /// (pixel-identical dark mode — hard requirement); LIGHT is a
    /// deliberately visible mid-gray, picked distinct from both `border`
    /// (0xd3d7e3, the stronger panel-edge outline) and `bg_hover`
    /// (0xe4e7f0, near-invisible on white).
    pub border_subtle: Hsla,
    pub bg_find_match: Hsla,
    pub bg_joined_col: Hsla,
    pub bg_deep: Hsla,
    pub bg_warn_banner: Hsla,
    pub bg_backdrop: Hsla,
    pub bg_selection: Hsla,
    /// What a text input paints itself — the SQL editor and every
    /// `TextField`. Was `gpui::white()` in BOTH themes (user, 2026-09-02:
    /// „ten input pro sql v dark modu je nějaký moc světlý … obecně každý
    /// input"). Darker than `bg_panel` in the dark theme so a field still
    /// reads as a field.
    pub bg_input: Hsla,
    /// Selection inside an input. OPAQUE on purpose — `bg_selection` is a
    /// translucent tint that vanishes over a light field (that was the
    /// „Ctrl+A nefunguje" report of 2026-08-31), so inputs carry their own.
    pub bg_input_selection: Hsla,
    /// Overlay scrollbars (2026-09-02, `scrollbar.rs`): the faint track and
    /// the thumb. Translucent track so it reads on any surface it floats
    /// over; opaque thumb so it is always grabbable by eye.
    pub scrollbar_track: Hsla,
    pub scrollbar_thumb: Hsla,
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
            border_subtle: rgb(0x313244).into(), // = bg_hover, VERBATIM (pixel-identical dark mode)
            bg_find_match: rgb(0x585b70).into(),
            bg_joined_col: rgb(0x2a2a3d).into(),
            bg_deep: rgb(0x11111b).into(),
            bg_warn_banner: rgb(0x3a3a1e).into(),
            bg_backdrop: rgba(0x00000099).into(),
            bg_selection: rgba(0x3311ff30).into(),
            bg_input: rgb(0x11111b).into(),
            bg_input_selection: rgb(0x3b5a9a).into(),
            scrollbar_track: rgba(0xffffff10).into(),
            scrollbar_thumb: rgb(0x585b70).into(),
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
            // Visible subtle border (G14 final review NIT-1) — ~1.54:1
            // luminance contrast against bg_panel's #ffffff, clearly
            // distinguishable from it (unlike bg_hover's ~1.24:1); a
            // decorative container-outline border, not text, so this isn't
            // held to the WCAG AA 4.5:1 text threshold.
            border_subtle: rgb(0xccd0da).into(),
            bg_find_match: rgb(0xffe58a).into(),
            bg_joined_col: rgb(0xeef1fb).into(),
            bg_deep: rgb(0xe4e7f1).into(),
            bg_warn_banner: rgb(0xf7edc8).into(),
            bg_backdrop: rgba(0x00000066).into(),
            bg_selection: rgba(0x3355ff33).into(),
            bg_input: rgb(0xffffff).into(),
            bg_input_selection: rgb(0xa8d0ff).into(),
            scrollbar_track: rgba(0x00000012).into(),
            scrollbar_thumb: rgb(0xb4b8c8).into(),
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
            t.border, t.border_subtle, t.bg_find_match, t.bg_joined_col, t.bg_deep,
            t.bg_warn_banner, t.bg_backdrop, t.bg_selection, t.bg_input,
            t.bg_input_selection, t.scrollbar_track, t.scrollbar_thumb, t.text_primary,
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
        for bg in [t.bg_panel, t.bg_app, t.bg_input] {
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
        assert!(contrast(t.text_primary, t.bg_input) >= 4.5);
        let s = t.syntax;
        for fg in [s.keyword, s.string, s.number, s.function, s.type_, s.identifier] {
            assert!(contrast(fg, t.bg_input) >= 4.5, "syntax color under AA on dark bg_input");
        }
        // Comments are the shipped „overlay gray" (G6 verbatim, muted on
        // purpose): 3.84:1 on `bg_input` — clears AA for large text /
        // UI components (3:1), not for body text. Pinned at what it IS so
        // a darker input cannot quietly push it below legible.
        assert!(contrast(s.comment, t.bg_input) >= 3.0, "comment gray unreadable on dark bg_input");
    }

    /// An input's selection has to be SEEN on that input, in both themes:
    /// opaque (a tint washes out over a light field) and clearly lighter
    /// or darker than the field it sits on. Both halves of the 2026-08-31
    /// „Ctrl+A nefunguje" fix, now per theme.
    #[test]
    fn input_selection_is_opaque_and_distinct_from_the_input_in_both_themes() {
        for t in [Theme::dark(), Theme::light()] {
            assert_eq!(t.bg_input_selection.a, 1.0);
            assert!((t.bg_input_selection.l - t.bg_input.l).abs() > 0.1);
            assert!(contrast(t.text_primary, t.bg_input_selection) >= 3.0, "selected text still legible");
        }
    }

    #[test]
    fn from_mode_maps_both_ways() {
        assert_eq!(Theme::from_mode(dbc_state::ThemeMode::Dark), Theme::dark());
        assert_eq!(Theme::from_mode(dbc_state::ThemeMode::Light), Theme::light());
    }
}
