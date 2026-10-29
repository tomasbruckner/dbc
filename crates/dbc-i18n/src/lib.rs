//! The UI language: Czech or English, picked once at startup (and again
//! when the user switches it in Settings), read on every render.
//!
//! Every user-visible text is written at its call site in BOTH languages:
//!
//! ```
//! use dbc_i18n::{tr, trf};
//! let n = 3;
//! let title: &'static str = tr!("Uložit", "Save");
//! let status: String = trf!("smazáno {n} řádků", "deleted {n} rows");
//! ```
//!
//! No keys, no catalogue files. A text and its translation sit on the same
//! two lines, so nobody has to invent a name for each of ~1 500 strings,
//! a missing translation is a compile error rather than a key printed on
//! screen, and changing a message changes both languages in one diff.
//! What this gives up — adding a third language without touching every call
//! site — is not a goal: dbc is Czech plus English.
//!
//! The language is a process-wide atomic, not a GPUI global, because the
//! texts also live in crates that know nothing of GPUI (`dbc-state`'s and
//! `dbc-connect`'s error messages end up in the status line).
//!
//! The default is Czech. That keeps every existing test that asserts on a
//! Czech message valid without each one having to set the language, and the
//! CLI — which never calls `set_lang` — unchanged.

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Cs,
    En,
}

static LANG: AtomicU8 = AtomicU8::new(0);

/// The current UI language.
pub fn lang() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::Cs,
    }
}

/// Switches the UI language for the whole process. Texts already built
/// into state (a status line, a dialog that is open) keep the language they
/// were built in; everything rendered afterwards uses the new one.
pub fn set_lang(lang: Lang) {
    LANG.store(
        match lang {
            Lang::Cs => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

/// The language Windows shows its own UI in, mapped to ours: Czech (and
/// Slovak, whose readers read Czech without effort) → Czech, anything else
/// → English. Used when the user never picked a language in Settings.
///
/// `GetUserDefaultUILanguage` is the DISPLAY language, which is the one that
/// matters here — not the regional format (`LocaleName`), which a Czech
/// developer often sets to English for dates while reading Czech, or vice
/// versa.
pub fn system_lang() -> Lang {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        // SAFETY: no arguments, no pointers; returns a LANGID by value.
        let langid = unsafe { GetUserDefaultUILanguage() };
        lang_from_langid(langid)
    }
    #[cfg(not(windows))]
    {
        Lang::En
    }
}

/// LANGID → `Lang`, by the primary language (low 10 bits).
/// `LANG_CZECH` = 0x05, `LANG_SLOVAK` = 0x1B.
pub fn lang_from_langid(langid: u16) -> Lang {
    match langid & 0x3ff {
        0x05 | 0x1b => Lang::Cs,
        _ => Lang::En,
    }
}

/// `tr!("česky", "English")` → `&'static str` in the current language.
/// Both arguments must be string literals, so the audit test in `dbc-ui`
/// can see that every Czech literal has an English twin next to it.
#[macro_export]
macro_rules! tr {
    ($cs:literal, $en:literal $(,)?) => {
        match $crate::lang() {
            $crate::Lang::Cs => $cs,
            $crate::Lang::En => $en,
        }
    };
}

/// `trf!("smazáno {n} řádků", "deleted {n} rows")` → `String`: `format!`
/// in the current language. Takes the same extra arguments as `format!`,
/// shared by both texts; inline captures (`{n}`) work as in `format!`.
/// Only the chosen branch evaluates its arguments.
#[macro_export]
macro_rules! trf {
    ($cs:literal, $en:literal $(,)?) => {
        match $crate::lang() {
            $crate::Lang::Cs => format!($cs),
            $crate::Lang::En => format!($en),
        }
    };
    ($cs:literal, $en:literal, $($arg:tt)*) => {
        match $crate::lang() {
            $crate::Lang::Cs => format!($cs, $($arg)*),
            $crate::Lang::En => format!($en, $($arg)*),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // ONE test touches the global, so parallel tests in this crate cannot
    // observe each other's switch.
    #[test]
    fn switching_changes_what_tr_and_trf_return() {
        let n = 3;
        assert_eq!(lang(), Lang::Cs, "Czech until someone switches");
        assert_eq!(tr!("Uložit", "Save"), "Uložit");
        assert_eq!(trf!("smazáno {n} řádků", "deleted {n} rows"), "smazáno 3 řádků");
        assert_eq!(trf!("{} z {}", "{} of {}", 1, 2), "1 z 2");

        set_lang(Lang::En);
        assert_eq!(tr!("Uložit", "Save"), "Save");
        assert_eq!(trf!("smazáno {n} řádků", "deleted {n} rows"), "deleted 3 rows");
        assert_eq!(trf!("{} z {}", "{} of {}", 1, 2), "1 of 2");

        set_lang(Lang::Cs);
        assert_eq!(tr!("Uložit", "Save"), "Uložit");
    }

    #[test]
    fn windows_display_languages_map_by_primary_language() {
        assert_eq!(lang_from_langid(0x0405), Lang::Cs, "cs-CZ");
        assert_eq!(lang_from_langid(0x041b), Lang::Cs, "sk-SK");
        assert_eq!(lang_from_langid(0x0409), Lang::En, "en-US");
        assert_eq!(lang_from_langid(0x0809), Lang::En, "en-GB");
        assert_eq!(lang_from_langid(0x0407), Lang::En, "de-DE: English, not Czech");
        assert_eq!(lang_from_langid(0), Lang::En);
    }
}
