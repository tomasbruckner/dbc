//! In-app updates, via Velopack (2026-09-03).
//!
//! The shipped app is a Velopack install (`dbc-win-Setup.exe`, or the
//! `dbc-win-Portable.zip` it also produces); both know their own version
//! and where to look for the next one. This module does three things:
//!
//! 1. [`startup`] — the very first statement of `main`. Velopack uses it
//!    to run its install/uninstall hooks and, when a newer package was
//!    downloaded last time but never applied, to apply it now. It may exit
//!    the process; that is by design. In a dev run or from a plain copy of
//!    the exe it is a no-op.
//! 2. [`check`] then [`FoundUpdate::download`] — two blocking calls for a
//!    background thread: ask the feed, then download what is newer while
//!    reporting progress. Two calls, not one, so the UI can say „a new
//!    version is coming" the moment it is known instead of only after a
//!    50 MB download it gave no sign of (2026-10-13). Neither shows
//!    anything; the caller decides what to do with the outcome.
//! 3. [`ReadyUpdate`] — a downloaded update the UI can apply: on click,
//!    restart into the new version; on quit, apply quietly so the next
//!    launch is the new one.
//!
//! **Not installed is not an error.** `cargo run`, a test, an exe copied
//! out of the install folder: all of those make [`Check::NotInstalled`],
//! and the UI shows nothing. The check is also entirely silent on network
//! failure — a laptop without internet must not start with a warning.
//!
//! **`DBC_UPDATE_SOURCE`**: a directory holding `releases.win.json` and
//! the packages `vpk pack` wrote. Set it to test the whole cycle on one
//! machine without publishing anything. Unset, the feed is the GitHub
//! Releases of [`REPO_URL`].

use velopack::sources::{FileSource, GithubSource, UpdateSource};
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp, VelopackAsset};

/// Where the shipped app looks for new versions. Public repo, no token:
/// GitHub allows 60 anonymous API calls an hour per address, and the app
/// makes one per launch.
pub const REPO_URL: &str = "https://github.com/tomasbruckner/dbc";

/// The environment variable that points the updater at a local folder
/// instead of GitHub. See the module docs.
pub const SOURCE_ENV: &str = "DBC_UPDATE_SOURCE";

/// Velopack's own startup work. Call it before anything else in `main`.
///
/// The hooks put the install folder on the user's PATH (see `user_path`).
/// The update hook runs it too, so installs older than that module get the
/// entry on their first update rather than never.
pub fn startup() {
    let app = VelopackApp::build();
    #[cfg(windows)]
    let app = app
        .on_after_install_fast_callback(|_| crate::user_path::add_install_dir())
        .on_after_update_fast_callback(|_| crate::user_path::add_install_dir())
        .on_before_uninstall_fast_callback(|_| crate::user_path::remove_install_dir());
    let mut app = app;
    app.run();
}

/// What [`check`] found.
pub enum Check {
    /// Not a Velopack install (dev run, copied exe). Show nothing.
    NotInstalled,
    /// The feed had nothing newer.
    UpToDate,
    /// The feed has a newer version; nothing is downloaded yet.
    Found(FoundUpdate),
    /// The feed could not be read. Worth a log line, not a dialog.
    Failed(String),
}

/// A newer version the feed announced, not downloaded yet.
pub struct FoundUpdate {
    /// The version on offer, e.g. `0.39.0`.
    pub version: String,
    manager: UpdateManager,
    info: Box<UpdateInfo>,
}

impl FoundUpdate {
    /// Blocking: download into the packages folder. `progress` gets 0–100
    /// as it goes — not always rising: a delta that fails to apply falls
    /// back to the full package, which starts from 0 again. The sender is
    /// dropped when this returns, so a receiver sees the channel close.
    pub fn download(self, progress: std::sync::mpsc::Sender<i16>) -> Result<ReadyUpdate, String> {
        self.manager.download_updates(&self.info, Some(progress)).map_err(|e| e.to_string())?;
        let asset = self.info.TargetFullRelease.clone();
        Ok(ReadyUpdate { version: asset.Version.clone(), manager: self.manager, asset })
    }
}

/// A downloaded update the UI may apply.
pub struct ReadyUpdate {
    /// The version we would restart into, e.g. `0.33.0`.
    pub version: String,
    manager: UpdateManager,
    asset: VelopackAsset,
}

impl ReadyUpdate {
    /// Hand over to Velopack's updater: it waits for this process to end,
    /// swaps the files, and starts the new version. The caller still has
    /// to end the process (save state first, then quit).
    pub fn apply_and_restart(&self) -> Result<(), String> {
        self.manager
            .wait_exit_then_apply_updates(&self.asset, false, true, Vec::<String>::new())
            .map_err(|e| e.to_string())
    }

    /// Same, but the new version is not started — for the quit path, so a
    /// user who closes the app finds the new version at the next launch.
    pub fn apply_on_exit(&self) -> Result<(), String> {
        self.manager
            .wait_exit_then_apply_updates(&self.asset, true, false, Vec::<String>::new())
            .map_err(|e| e.to_string())
    }
}

fn source() -> Box<dyn UpdateSource> {
    match std::env::var_os(SOURCE_ENV) {
        Some(dir) if !dir.is_empty() => Box::new(FileSource::new(dir)),
        _ => Box::new(GithubSource::new(REPO_URL, None, false)),
    }
}

/// Blocking: builds the manager and asks the feed. Run it on a background
/// thread — it is one HTTP request, but on a bad network a slow one.
pub fn check() -> Check {
    let manager = match UpdateManager::new_boxed(source(), None, None) {
        Ok(m) => m,
        Err(velopack::Error::NotInstalled(_)) => return Check::NotInstalled,
        Err(e) => return Check::Failed(e.to_string()),
    };
    match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => {
            let version = info.TargetFullRelease.Version.clone();
            Check::Found(FoundUpdate { version, manager, info })
        }
        Ok(UpdateCheck::NoUpdateAvailable) | Ok(UpdateCheck::RemoteIsEmpty) => Check::UpToDate,
        Err(e) => Check::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing a dev run must never do is show an update button or
    /// fail loudly. Nothing here is a Velopack install.
    #[test]
    fn a_dev_run_is_not_installed() {
        assert!(matches!(check(), Check::NotInstalled));
    }
}
