//! „Přenos na jiný počítač" — the Settings block and the import confirm
//! dialog, as far as either can be decided WITHOUT a `Window` or a `cx`.
//!
//! Everything here is a pure function over `dbc_state::bundle::Summary`, so
//! the sentences the user reads before replacing their whole configuration
//! are unit-testable. `AppView` supplies only the file dialogs and the
//! reload; see `main.rs`'s `start_settings_export` / `confirm_settings_import`.
//!
//! The two flows are deliberately asymmetric, and the asymmetry is the
//! design: **export changes nothing**, so it is a picker and a write, with
//! no gate and no confirmation. **Import replaces the connection list and
//! the vault**, so it goes through the same `context_switch_blocked` gate
//! as a workspace switch and then through a dialog that states, in advance,
//! exactly what will be true afterwards.

use dbc_state::bundle::Summary;

/// Settings block heading.
pub(crate) const TRANSFER_SETTINGS_HEADING: &str = "Přenos na jiný počítač";
/// Opens the save dialog. Writes a file, touches nothing in the app.
pub(crate) const TRANSFER_SETTINGS_EXPORT: &str = "Vyvézt nastavení…";
/// Opens the open dialog, then the confirm below.
pub(crate) const TRANSFER_SETTINGS_IMPORT: &str = "Načíst nastavení…";

/// The standing explanation, rendered in Settings BEFORE either dialog
/// opens — the same posture as [`crate::connections_ui::WORKSPACE_GIT_WARNING`]:
/// the thing a person needs in order to decide is on screen before the
/// picker takes over the window.
pub(crate) const TRANSFER_SETTINGS_NOTE: &str =
    "Vyveze se seznam připojení a trezor tak, jak je — pořád zašifrovaný, \
     takže se export na master heslo neptá a v souboru žádné čitelné heslo není. \
     Na druhém počítači ho otevřeš tím samým master heslem jako tady.";

/// Suggested filename in the save dialog. The pinned GPUI rev's
/// `prompt_for_new_path` has no extension filter (grounded in
/// `start_script_pick`'s note), so the extension is carried by the
/// suggestion rather than enforced by the dialog.
pub(crate) fn export_suggested_name() -> String {
    format!("dbc-nastaveni.{}", dbc_state::bundle::EXT)
}

pub(crate) const IMPORT_CONFIRM_TITLE: &str = "Načíst nastavení";
pub(crate) const IMPORT_CONFIRM_OK: &str = "Nahradit nastavení";
pub(crate) const IMPORT_CONFIRM_CANCEL: &str = "Zrušit";

/// How many connection names the dialog lists before it stops naming them.
///
/// A dialog that grows past the window is a dialog whose buttons cannot be
/// reached. Ten is enough to recognise „yes, this is my export"; past that
/// the count is what matters, and the count is always shown.
const NAME_CAP: usize = 10;

/// The body of the confirm dialog, one line per element.
///
/// `replacing` is whether this context ALREADY has a `config.toml` — the
/// difference between „set up this machine" and „replace what is here",
/// which is the single most important thing on the screen and so gets its
/// own line rather than an adjective in someone else's.
///
/// Note what is NOT claimed: how many passwords are inside. That would
/// require unsealing the vault with a master password nobody has typed yet
/// (see `bundle::Summary`), so the dialog says the vault is there and stops.
pub(crate) fn import_confirm_lines(summary: &Summary, replacing: bool) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(match summary.connections.len() {
        0 => "Soubor neobsahuje žádná připojení.".to_string(),
        n => format!("Soubor obsahuje {n} připojení:"),
    });
    for name in summary.connections.iter().take(NAME_CAP) {
        lines.push(format!("  {name}"));
    }
    if summary.connections.len() > NAME_CAP {
        lines.push(format!("  … a další ({} celkem)", summary.connections.len()));
    }

    if summary.has_vault {
        lines.push(
            "Trezor uvnitř zůstal zašifrovaný — po načtení se otevírá master heslem \
             z původního počítače, ne tím zdejším."
                .to_string(),
        );
    } else {
        lines.push(
            "V souboru není trezor, takže žádná hesla nepřijdou — u každého připojení \
             je budeš muset zadat znovu."
                .to_string(),
        );
    }

    if replacing {
        lines.push(
            "Tvoje současná připojení a trezor budou nahrazené. Původní soubory se \
             nemažou — odloží se vedle pod jménem *.pred-importem-<čas>."
                .to_string(),
        );
    } else {
        lines.push("Tenhle profil je zatím prázdný, takže se nic nepřepíše.".to_string());
    }

    lines.push("Historie, hodnoty parametrů ani cesty k psql/sqlcmd se nepřenášejí.".to_string());
    lines
}

/// The status-bar line after a successful export. The path, because the
/// save dialog's directory is not always the one the user thought they
/// picked, and this is the only place it is ever shown.
pub(crate) fn export_done_status(path: &std::path::Path, connections: usize) -> String {
    format!("nastavení vyvezeno: {} ({connections} připojení)", path.display())
}

/// The status-bar line after a successful import.
///
/// Carries both of the facts the user needs NEXT: where the previous files
/// went (the only undo there is), and that the vault behind those new
/// connections now wants a different master password than it did five
/// seconds ago. The reload deliberately LOCKS the vault, so the very next
/// action on a connection prompts — and a prompt that arrives unexplained
/// reads as a bug rather than as the design.
pub(crate) fn import_done_status(connections: usize, backed_up: usize, has_vault: bool) -> String {
    let mut s = format!("nastavení načteno: {connections} připojení");
    if backed_up > 0 {
        s.push_str(&format!(" — původní odloženo jako *.pred-importem ({backed_up} soubory)"));
    }
    if has_vault {
        s.push_str(" — trezor je zamčený, odemkne se master heslem z původního počítače");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(names: &[&str], has_vault: bool) -> Summary {
        Summary {
            connections: names.iter().map(|s| s.to_string()).collect(),
            has_vault,
            has_views: true,
            created_unix: 1_780_000_000,
            app_version: "0.27.0".to_string(),
        }
    }

    /// The three facts a person needs in order to answer „should I click
    /// this?", each of which was learned the hard way somewhere else in
    /// this app: what am I getting, what happens to what I have, and which
    /// password will it want afterwards.
    #[test]
    fn the_confirm_states_what_arrives_what_is_replaced_and_which_password() {
        let lines = import_confirm_lines(&summary(&["prodej", "sklad"], true), true);
        let text = lines.join("\n");
        assert!(text.contains("2 připojení"), "{text}");
        assert!(text.contains("prodej") && text.contains("sklad"), "{text}");
        assert!(text.contains("z původního počítače"), "{text}");
        assert!(text.contains("nahrazené"), "{text}");
        assert!(text.contains("pred-importem"), "{text}");
        assert!(text.contains("Historie"), "{text}");
    }

    /// An empty target must not be described as a replacement — that is a
    /// scary sentence with nothing behind it, and it would teach the user
    /// to ignore the real one.
    #[test]
    fn an_empty_profile_is_not_described_as_a_replacement() {
        let lines = import_confirm_lines(&summary(&["prodej"], true), false);
        let text = lines.join("\n");
        assert!(text.contains("prázdný"), "{text}");
        assert!(!text.contains("nahrazené"), "{text}");
        assert!(!text.contains("pred-importem"), "{text}");
    }

    /// A bundle with no vault is the case where the user WILL be asked for
    /// every password again, so it must say so instead of quietly omitting
    /// the vault sentence.
    #[test]
    fn a_bundle_without_a_vault_says_the_passwords_are_not_coming() {
        let text = import_confirm_lines(&summary(&["prodej"], false), true).join("\n");
        assert!(text.contains("není trezor"), "{text}");
        assert!(text.contains("zadat znovu"), "{text}");
        assert!(!text.contains("z původního počítače"), "{text}");
    }

    /// A long list must not push the buttons off the bottom of the window,
    /// and the count must survive the truncation — „… a další" alone tells
    /// you nothing.
    #[test]
    fn a_long_list_is_capped_but_still_reports_the_true_count() {
        let names: Vec<String> = (0..25).map(|i| format!("conn{i}")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let lines = import_confirm_lines(&summary(&refs, true), true);
        let listed = lines.iter().filter(|l| l.starts_with("  conn")).count();
        assert_eq!(listed, NAME_CAP);
        let text = lines.join("\n");
        assert!(text.contains("25 celkem"), "{text}");
        assert!(text.contains("25 připojení"), "{text}");
    }

    #[test]
    fn an_empty_bundle_does_not_claim_connections_it_does_not_have() {
        let text = import_confirm_lines(&summary(&[], true), true).join("\n");
        assert!(text.contains("žádná připojení"), "{text}");
    }

    #[test]
    fn the_suggested_name_carries_the_extension_the_dialog_cannot_enforce() {
        let name = export_suggested_name();
        assert!(name.ends_with(&format!(".{}", dbc_state::bundle::EXT)), "{name}");
    }

    #[test]
    fn the_status_lines_name_the_path_and_the_backups() {
        let s = export_done_status(std::path::Path::new("C:/tmp/a.dbcx"), 3);
        assert!(s.contains("a.dbcx") && s.contains('3'), "{s}");
        let s = import_done_status(3, 0, false);
        assert!(s.contains("3 připojení"), "{s}");
        assert!(!s.contains("pred-importem"), "{s}");
        assert!(!s.contains("trezor"), "{s}");
        let s = import_done_status(3, 2, true);
        assert!(s.contains("pred-importem") && s.contains('2'), "{s}");
        // The one fact that makes the next master-password prompt make
        // sense — see `import_done_status`.
        assert!(s.contains("z původního počítače"), "{s}");
    }
}
