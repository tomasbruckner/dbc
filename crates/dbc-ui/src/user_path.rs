//! The install folder on the user's PATH, so `dbc` works in a terminal
//! (2026-10-12).
//!
//! winget installs dbc through the Velopack `Setup.exe`, and for an `exe`
//! installer winget adds no command alias; Velopack does not touch PATH
//! either. So the app does it itself, from the Velopack hooks:
//!
//! - after install and after EVERY update — the update hook is what fixes
//!   the installs that predate this module, on their first update;
//! - before uninstall it takes the entry out again.
//!
//! The folder is the one the hook's own exe runs from: Velopack's
//! `…\dbc\current`, which stays the same across updates, so one entry
//! serves every future version. Only the USER `Path` (`HKCU\Environment`)
//! is touched — no admin, and nothing another user sees.
//!
//! Everything that decides what the new value is lives in [`with_dir`] and
//! [`without_dir`], pure and tested; the registry part only reads, writes
//! and announces.

/// `path` with `dir` appended, or `None` when it is already there.
pub fn with_dir(path: &str, dir: &str) -> Option<String> {
    if path.split(';').any(|e| same_dir(e, dir)) {
        return None;
    }
    // A trailing `;` stays trailing, so `without_dir` gives back exactly
    // what was there before.
    Some(match path {
        "" => dir.to_string(),
        p if p.ends_with(';') => format!("{p}{dir};"),
        p => format!("{p};{dir}"),
    })
}

/// `path` without any entry naming `dir`, or `None` when there was none.
/// Every other entry stays exactly as written, empty ones included.
pub fn without_dir(path: &str, dir: &str) -> Option<String> {
    if !path.split(';').any(|e| same_dir(e, dir)) {
        return None;
    }
    Some(path.split(';').filter(|e| !same_dir(e, dir)).collect::<Vec<_>>().join(";"))
}

/// Windows paths: case does not matter, a trailing backslash does not
/// either, and an empty entry names nothing.
fn same_dir(entry: &str, dir: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_lowercase();
    let e = norm(entry);
    !e.is_empty() && e == norm(dir)
}

/// Add the folder of the running exe to the user's PATH. Errors are only
/// logged: a failed PATH edit must never fail an install or an update.
///
/// Not for the portable zip: it can be unpacked anywhere, several times,
/// and its update hook runs too. Velopack marks it with a `.portable` file
/// in the root above `current`.
#[cfg(windows)]
pub fn add_install_dir() {
    let portable = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.join(".portable")))
        .is_some_and(|p| p.exists());
    if !portable {
        edit(with_dir);
    }
}

/// Take it out again (uninstall).
#[cfg(windows)]
pub fn remove_install_dir() {
    edit(without_dir);
}

#[cfg(windows)]
fn edit(change: fn(&str, &str) -> Option<String>) {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_owned()))
    else {
        return;
    };
    if let Err(e) = registry::edit(&dir.to_string_lossy(), change) {
        eprintln!("dbc: PATH nejde upravit: {e}");
    }
}

#[cfg(windows)]
mod registry {
    use std::io;

    use winreg::enums::{RegType, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::{RegKey, RegValue};

    pub fn edit(dir: &str, change: fn(&str, &str) -> Option<String>) -> io::Result<()> {
        let env = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)?;
        // Raw, not `get_value::<String>`: the user Path is usually
        // REG_EXPAND_SZ (`%USERPROFILE%\…` entries), and writing it back as
        // REG_SZ would freeze those variables into literal text.
        let (current, vtype) = match env.get_raw_value("Path") {
            Ok(v) => (decode(&v.bytes), v.vtype),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                (String::new(), RegType::REG_EXPAND_SZ)
            }
            Err(e) => return Err(e),
        };
        let Some(next) = change(&current, dir) else { return Ok(()) };
        let vtype = match vtype {
            RegType::REG_SZ | RegType::REG_EXPAND_SZ => vtype,
            _ => RegType::REG_EXPAND_SZ,
        };
        env.set_raw_value("Path", &RegValue { bytes: encode(&next), vtype })?;
        announce();
        Ok(())
    }

    fn decode(bytes: &[u8]) -> String {
        let wide: Vec<u16> =
            bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        String::from_utf16_lossy(&wide[..end])
    }

    fn encode(s: &str) -> Vec<u8> {
        s.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect()
    }

    /// Tell Explorer the environment changed. Without it a terminal opened
    /// from the Start menu would not see the new PATH until the next
    /// sign-in — the very thing this module is for.
    ///
    /// The broadcast is `SendMessageTimeoutW(WM_SETTINGCHANGE)`, which
    /// would be this crate's only `unsafe` (main.rs forbids it). .NET sends
    /// the same message whenever it sets a user variable, so PowerShell
    /// sets a throwaway one and removes it again. PATH itself is still
    /// written above, raw — .NET would rewrite it as REG_SZ.
    fn announce() {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = concat!(
            "[Environment]::SetEnvironmentVariable('DBC_PATH_REFRESH','1','User');",
            "[Environment]::SetEnvironmentVariable('DBC_PATH_REFRESH',$null,'User')",
        );
        let _ = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = r"C:\Users\x\AppData\Local\dbc\current";

    #[test]
    fn appends_once() {
        let p = r"C:\a;%USERPROFILE%\bin";
        let added = with_dir(p, DIR).unwrap();
        assert_eq!(added, format!(r"C:\a;%USERPROFILE%\bin;{DIR}"));
        assert_eq!(with_dir(&added, DIR), None);
    }

    #[test]
    fn already_there_in_another_spelling_is_there() {
        let p = r"C:\a;c:\users\x\appdata\local\DBC\current\;C:\b";
        assert_eq!(with_dir(p, DIR), None);
    }

    #[test]
    fn empty_or_trailing_semicolon_path() {
        assert_eq!(with_dir("", DIR).unwrap(), DIR);
        assert_eq!(with_dir(r"C:\a;", DIR).unwrap(), format!(r"C:\a;{DIR};"));
    }

    #[test]
    fn removes_every_spelling_and_keeps_the_rest_as_written() {
        let p = format!(r"C:\a;{DIR};;C:\b;{}\", DIR.to_uppercase());
        assert_eq!(without_dir(&p, DIR).unwrap(), r"C:\a;;C:\b");
        assert_eq!(without_dir(r"C:\a;C:\b", DIR), None);
    }

    #[test]
    fn a_round_trip_restores_the_original() {
        for p in ["", r"C:\a;%USERPROFILE%\bin", r"C:\a;%USERPROFILE%\bin;"] {
            assert_eq!(without_dir(&with_dir(p, DIR).unwrap(), DIR).unwrap(), p, "{p:?}");
        }
    }
}
