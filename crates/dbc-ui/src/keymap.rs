//! The one list of keyboard shortcuts the app admits to having.
//!
//! # Why this exists
//!
//! Shortcut help rots. It is written once against whatever the bindings
//! were that day, and every later change to a `KeyBinding::new` call leaves
//! it a little more wrong — until it is worse than no help at all, because
//! a wrong shortcut list costs you the time you spend trusting it.
//!
//! So this table is not a copy of the bindings, it is the thing the help is
//! rendered from, and two audits in `main.rs` keep it honest against the
//! real `KeyBinding::new` calls:
//!
//!   * **no lies** — every chord listed here is actually bound somewhere in
//!     the crate;
//!   * **no secrets** — every chord bound GLOBALLY (context `None`, so it
//!     works everywhere) is either listed here or named in
//!     [`UNDOCUMENTED_GLOBALS`] with a reason.
//!
//! Neither is a compiler rail — they read the source text, the way this
//! codebase's other audits do. What they buy is that the list cannot drift
//! silently: it drifts loudly, in CI, with the chord named.
//!
//! # Why there are no modes
//!
//! zellij is the model for the always-visible hint strip, and deliberately
//! NOT for its modes. zellij can afford a modal layer because its default
//! mode passes keystrokes through to a terminal; the moment a mode swallows
//! a keystroke in a SQL editor, the user is typing into a void and does not
//! know it. The hints here change with FOCUS instead — same „the app tells
//! you what the keys do right now" property, no state to get stuck in.

/// Where a shortcut applies. Drives both the grouping in the cheat sheet
/// and which hints show in the strip at the bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Works anywhere in the window.
    Global,
    /// The SQL editor.
    Editor,
    /// The schema tree.
    Tree,
    /// The results grid.
    Results,
    /// The command palette, while it is open.
    Palette,
}

impl Scope {
    pub fn title(self) -> &'static str {
        match self {
            Scope::Global => "Kdekoliv",
            Scope::Editor => "Editor SQL",
            Scope::Tree => "Strom schémat",
            Scope::Results => "Výsledky",
            Scope::Palette => "Paleta příkazů",
        }
    }
}

pub struct Shortcut {
    /// EXACTLY as passed to `KeyBinding::new` — the audit compares strings,
    /// so a prettier spelling here would read as an unbound chord.
    pub chord: &'static str,
    pub label: &'static str,
    pub scope: Scope,
    /// Shown in the always-visible strip. Reserved for the handful worth
    /// permanent screen space; everything else lives in F1.
    pub in_strip: bool,
}

const fn s(chord: &'static str, label: &'static str, scope: Scope, in_strip: bool) -> Shortcut {
    Shortcut { chord, label, scope, in_strip }
}

pub const SHORTCUTS: &[Shortcut] = &[
    // --- Global ---
    s("ctrl-enter", "Spustit dotaz", Scope::Global, true),
    s("ctrl-shift-enter", "Spustit bez limitu řádků", Scope::Global, false),
    s("escape", "Zrušit běžící dotaz", Scope::Global, false),
    s("ctrl-k", "Paleta příkazů", Scope::Global, true),
    s("ctrl-p", "Paleta příkazů", Scope::Global, false),
    s("ctrl-shift-p", "Paleta příkazů", Scope::Global, false),
    s("ctrl-space", "Napovídání", Scope::Global, true),
    s("ctrl-shift-f", "Naformátovat SQL", Scope::Global, true),
    s("ctrl-s", "Uložit skript", Scope::Global, false),
    s("ctrl-b", "Zobrazit/skrýt strom", Scope::Global, false),
    s("ctrl-h", "Zobrazit/skrýt historii", Scope::Global, false),
    s("f1", "Přehled zkratek", Scope::Global, true),
    s("ctrl-1", "Přejít do editoru", Scope::Global, false),
    s("ctrl-2", "Přejít do stromu", Scope::Global, false),
    s("ctrl-3", "Přejít do výsledků", Scope::Global, false),
    // --- Editor ---
    s("ctrl-a", "Vybrat vše", Scope::Editor, false),
    s("ctrl-left", "O slovo vlevo", Scope::Editor, false),
    s("ctrl-right", "O slovo vpravo", Scope::Editor, false),
    s("ctrl-shift-left", "Vybrat slovo vlevo", Scope::Editor, false),
    s("ctrl-shift-right", "Vybrat slovo vpravo", Scope::Editor, false),
    s("home", "Na začátek řádku", Scope::Editor, false),
    s("end", "Na konec řádku", Scope::Editor, false),
    s("shift-home", "Vybrat k začátku řádku", Scope::Editor, false),
    s("shift-end", "Vybrat ke konci řádku", Scope::Editor, false),
    s("ctrl-home", "Na začátek dotazu", Scope::Editor, false),
    s("ctrl-end", "Na konec dotazu", Scope::Editor, false),
    s("ctrl-shift-home", "Vybrat k začátku dotazu", Scope::Editor, false),
    s("ctrl-shift-end", "Vybrat ke konci dotazu", Scope::Editor, false),
    s("ctrl-backspace", "Smazat slovo vlevo", Scope::Editor, false),
    s("ctrl-delete", "Smazat slovo vpravo", Scope::Editor, false),
    s("ctrl-c", "Kopírovat", Scope::Editor, false),
    s("ctrl-x", "Vyjmout", Scope::Editor, false),
    s("ctrl-v", "Vložit", Scope::Editor, false),
    // --- Results ---
    s("ctrl-c", "Kopírovat výběr", Scope::Results, true),
    s("ctrl-f", "Hledat ve výsledku", Scope::Results, true),
    s("enter", "Další nález", Scope::Results, false),
    s("shift-enter", "Předchozí nález", Scope::Results, false),
    s("delete", "Smazat řádek (staged)", Scope::Results, false),
    // --- Tree ---
    s("escape", "Zrušit hledání ve stromu", Scope::Tree, false),
    // --- Palette ---
    s("up", "O položku nahoru", Scope::Palette, false),
    s("down", "O položku dolů", Scope::Palette, false),
    s("enter", "Potvrdit", Scope::Palette, true),
    s("escape", "Zavřít", Scope::Palette, true),
];

/// Chords bound with context `None` that the cheat sheet deliberately does
/// NOT list, each because it does the single thing its label on the key
/// says. Listing „šipka vlevo — o znak vlevo" is not help, it is noise.
///
/// This is an allowlist, not a filter: a NEW global chord that is not here
/// and not in [`SHORTCUTS`] fails the audit, so the choice to hide one is
/// always a choice somebody made on purpose.
/// `#[cfg(test)]` because the audit is its only reader: this is a policy
/// statement about what may stay undocumented, and policy is enforced at
/// test time, not carried in the shipped binary.
#[cfg(test)]
pub const UNDOCUMENTED_GLOBALS: &[&str] = &[
    "left", "right", "up", "down", "backspace", "delete", "enter", "tab", "shift-left",
    "shift-right", "shift-up", "shift-down", "shift-tab",
    // macOS spellings of the clipboard/select-all chords, bound beside
    // their ctrl- twins. Every one of them IS documented, under its
    // Windows spelling, which is the one this app ships on.
    "cmd-a", "cmd-c", "cmd-v", "cmd-x",
    // macOS system character palette. Found by this very audit, which is
    // the point of it: it was bound and written down nowhere. Not
    // documented because it does nothing on the platform this ships on —
    // listing a dead key in the cheat sheet would be its own kind of lie.
    "ctrl-cmd-space",
];

/// The shortcuts to show in the always-visible strip for `focus`.
///
/// Global ones first — they are true no matter where you are — then the
/// focused area's own. Ordering is stable so the strip does not reshuffle
/// under the eye as focus moves.
pub fn strip_for(focus: Scope) -> Vec<&'static Shortcut> {
    SHORTCUTS
        .iter()
        .filter(|sc| sc.in_strip && (sc.scope == Scope::Global || sc.scope == focus))
        .collect()
}

/// Every scope that has at least one shortcut, in cheat-sheet order.
pub fn scopes() -> Vec<Scope> {
    let all = [Scope::Global, Scope::Editor, Scope::Tree, Scope::Results, Scope::Palette];
    all.into_iter().filter(|sc| SHORTCUTS.iter().any(|s| s.scope == *sc)).collect()
}

/// Windows-style display: `ctrl-shift-f` → `Ctrl+Shift+F`.
pub fn pretty(chord: &str) -> String {
    chord
        .split('-')
        .map(|part| match part {
            "ctrl" => "Ctrl".to_string(),
            "shift" => "Shift".to_string(),
            "alt" => "Alt".to_string(),
            "cmd" => "Cmd".to_string(),
            "enter" => "Enter".to_string(),
            "escape" => "Esc".to_string(),
            "space" => "Space".to_string(),
            "backspace" => "Backspace".to_string(),
            "delete" => "Delete".to_string(),
            "home" => "Home".to_string(),
            "end" => "End".to_string(),
            "left" => "←".to_string(),
            "right" => "→".to_string(),
            "up" => "↑".to_string(),
            "down" => "↓".to_string(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_reads_like_windows_spells_it() {
        assert_eq!(pretty("ctrl-shift-f"), "Ctrl+Shift+F");
        assert_eq!(pretty("f1"), "F1");
        assert_eq!(pretty("escape"), "Esc");
        assert_eq!(pretty("ctrl-left"), "Ctrl+←");
    }

    /// The strip is the one piece of permanent screen space this feature
    /// takes, so it has to stay small enough to read at a glance.
    #[test]
    fn the_strip_stays_short() {
        for scope in scopes() {
            let n = strip_for(scope).len();
            assert!(n <= 8, "{scope:?} would show {n} hints");
        }
    }

    /// A hint that only makes sense somewhere else is worse than no hint.
    #[test]
    fn the_strip_only_shows_global_and_focused_shortcuts() {
        for sc in strip_for(Scope::Results) {
            assert!(matches!(sc.scope, Scope::Global | Scope::Results), "{}", sc.chord);
        }
    }

    /// Two entries with the same chord AND scope would render twice.
    #[test]
    fn no_scope_lists_the_same_chord_twice() {
        for scope in scopes() {
            let mut chords: Vec<&str> =
                SHORTCUTS.iter().filter(|s| s.scope == scope).map(|s| s.chord).collect();
            chords.sort_unstable();
            let before = chords.len();
            chords.dedup();
            assert_eq!(before, chords.len(), "{scope:?} lists a chord twice");
        }
    }

    /// Chords are compared against source text by the audits in `main.rs`,
    /// so a stray space or capital here would read as „not bound".
    #[test]
    fn chords_are_written_the_way_gpui_spells_them() {
        for sc in SHORTCUTS {
            assert_eq!(sc.chord, sc.chord.to_lowercase(), "{}", sc.chord);
            assert!(!sc.chord.contains(' '), "{}", sc.chord);
            assert!(!sc.chord.contains('+'), "{} — gpui uses '-'", sc.chord);
            assert!(!sc.label.is_empty());
        }
    }
}
