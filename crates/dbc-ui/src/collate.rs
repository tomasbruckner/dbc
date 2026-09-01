//! Ordering names the way a reader expects them rather than the way their
//! bytes happen to fall.
//!
//! `str`'s own `Ord` compares UTF-8 bytes, so every capital letter sorts
//! before every lower-case one and every accented letter after all of them.
//! In the sidebar that turned a list the code had genuinely just sorted
//! into „PG-dev, PG-prod, demo, produkce", and the user reported it as not
//! sorted (2026-09-01). They were right: „sorted" is a claim about what the
//! reader sees, and by that measure `cmp` was wrong, not the report.
//!
//! ## What this deliberately is not
//!
//! It is not a Czech collation. Real Czech ordering treats `ch` as a single
//! letter between `h` and `i`, and `č`/`ř`/`š`/`ž` as letters in their own
//! right that follow their base letter rather than merging into it. Getting
//! that right needs a collation table — ICU — and a dependency this app
//! does not carry for the sake of ordering a dozen sidebar rows.
//!
//! What it does instead is fold case and strip diacritics for the PRIMARY
//! key, which fixes both of the ways byte order surprises a reader: `demo`
//! now sorts beside `DI dev`, and `Účetnictví` beside `ucet` instead of
//! after everything. Where real collation would disagree with this, it
//! disagrees about which of two ADJACENT rows comes first — `Čapek` before
//! or after `Cejn` — never about where in the list to look.
//!
//! The original string is kept as the tie-break, so the order is total and
//! stable: two names differing only in case or accents have a fixed order,
//! they are merely neighbours.

/// The primary sort key: lower-cased and stripped of diacritics.
fn key(name: &str) -> String {
    name.chars().flat_map(char::to_lowercase).map(fold_char).collect()
}

/// One character, folded to its unaccented base.
///
/// Czech first (it is the app's language), then the rest of the Latin-1 and
/// Latin Extended-A letters a name in this part of the world plausibly
/// contains. Anything unlisted — including every non-Latin script — passes
/// through untouched and simply orders by its own code point, which is the
/// right fallback: unchanged behaviour rather than a wrong guess.
fn fold_char(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'č' | 'ć' | 'ç' | 'ĉ' | 'ċ' => 'c',
        'ď' | 'đ' => 'd',
        'é' | 'ě' | 'è' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' => 'e',
        'ģ' | 'ğ' | 'ĝ' => 'g',
        'í' | 'ì' | 'î' | 'ï' | 'ī' | 'į' => 'i',
        'ĺ' | 'ľ' | 'ł' | 'ļ' => 'l',
        'ň' | 'ñ' | 'ń' | 'ņ' => 'n',
        'ó' | 'ô' | 'ò' | 'ö' | 'õ' | 'ø' | 'ō' | 'ő' => 'o',
        'ř' | 'ŕ' => 'r',
        'š' | 'ś' | 'ş' | 'ș' | 'ŝ' => 's',
        'ť' | 'ţ' | 'ț' => 't',
        'ú' | 'ů' | 'ù' | 'û' | 'ü' | 'ū' | 'ų' | 'ű' => 'u',
        'ý' | 'ÿ' => 'y',
        'ž' | 'ź' | 'ż' => 'z',
        other => other,
    }
}

/// Order two display names.
pub(crate) fn cmp_names(a: &str, b: &str) -> std::cmp::Ordering {
    key(a).cmp(&key(b)).then_with(|| a.cmp(b))
}

/// Order two folder paths, component by component.
///
/// Component-wise rather than on a joined string, and that matters: a
/// parent must always sort before its own children, because the sidebar
/// renders the list in order and indents by depth. Comparing the components
/// as sequences gives that for free — a prefix is less than anything that
/// extends it — whereas joining with a separator makes the answer depend on
/// where that separator falls in the alphabet.
pub(crate) fn cmp_paths(a: &[String], b: &[String]) -> std::cmp::Ordering {
    a.iter()
        .map(|s| key(s))
        .cmp(b.iter().map(|s| key(s)))
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report, verbatim: these are the names in the user's own „dw"
    /// folder, and byte order put `demo` after both `PG-*` entries.
    #[test]
    fn the_users_own_folder_comes_out_alphabetical() {
        let mut names = vec!["PG-dev", "produkce", "demo", "PG-prod", "DI prod", "dev"];
        names.sort_by(|a, b| cmp_names(a, b));
        assert_eq!(names, ["demo", "dev", "DI prod", "PG-dev", "PG-prod", "produkce"]);
    }

    /// Byte order is what this replaces — pin that the two really differ,
    /// so the test above cannot quietly start passing for the wrong reason.
    #[test]
    fn plain_byte_order_would_get_it_wrong() {
        let mut bytes = vec!["PG-dev", "demo", "DI prod"];
        bytes.sort();
        assert_eq!(bytes, ["DI prod", "PG-dev", "demo"], "byte order groups by case");
    }

    #[test]
    fn accents_sort_beside_their_base_letter_not_after_z() {
        let mut names = vec!["zebra", "Účetnictví", "ucet", "avion"];
        names.sort_by(|a, b| cmp_names(a, b));
        assert_eq!(names, ["avion", "ucet", "Účetnictví", "zebra"]);
    }

    /// Equal under folding ⇒ still a fixed order, never „either".
    #[test]
    fn the_order_is_total_even_when_the_keys_tie() {
        assert_eq!(cmp_names("abc", "ABC"), "abc".cmp("ABC"));
        assert_ne!(cmp_names("abc", "ABC"), std::cmp::Ordering::Equal);
        assert_eq!(cmp_names("abc", "abc"), std::cmp::Ordering::Equal);
    }

    /// The one structural promise: a folder precedes everything inside it,
    /// whatever the names are. `zebra` sorts after `alpha`, but
    /// `["zebra"]` must still precede `["zebra", "alpha"]`.
    #[test]
    fn a_parent_folder_always_precedes_its_children() {
        let mut paths: Vec<Vec<String>> = vec![
            vec!["zebra".into(), "alpha".into()],
            vec!["Zebra".into()],
            vec!["alpha".into()],
        ];
        paths.sort_by(|a, b| cmp_paths(a, b));
        assert_eq!(
            paths,
            vec![
                vec!["alpha".to_string()],
                vec!["Zebra".to_string()],
                vec!["zebra".to_string(), "alpha".to_string()],
            ]
        );
    }

    /// A separator-joined key would rank these by where `/` falls in the
    /// alphabet; component-wise comparison cannot.
    #[test]
    fn a_deep_path_does_not_jump_over_a_sibling() {
        let mut paths: Vec<Vec<String>> = vec![
            vec!["a b".into()],
            vec!["a".into(), "z".into()],
            vec!["a".into()],
        ];
        paths.sort_by(|a, b| cmp_paths(a, b));
        assert_eq!(
            paths,
            vec![
                vec!["a".to_string()],
                vec!["a".to_string(), "z".to_string()],
                vec!["a b".to_string()],
            ]
        );
    }
}
