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
//! 2. [`check_and_download`] — one blocking call for a background thread:
//!    ask the feed, download what is newer, report. It never shows
//!    anything; the caller decides what to do with the outcome.
//! 3. [`ReadyUpdate`] — a downloaded update the UI can apply: on click,
//!    restart into the new version; on quit, apply quietly so the next
//!    launch is the new one.
//!
//! **Not installed is not an error.** `cargo run`, a test, an exe copied
//! out of the install folder: all of those make [`Outcome::NotInstalled`],
//! and the UI shows nothing. The check is also entirely silent on network
//! failure — a laptop without internet must not start with a warning.
//!
//! **`DBC_UPDATE_SOURCE`**: a directory holding `releases.win.json` and
//! the packages `vpk pack` wrote. Set it to test the whole cycle on one
//! machine without publishing anything. Unset, the feed is the GitHub
//! Releases of [`REPO_URL`].

use velopack::sources::{FileSource, GithubSource, UpdateSource};
use velopack::{UpdateCheck, UpdateManager, VelopackApp, VelopackAsset};

/// Where the shipped app looks for new versions. Public repo, no token:
/// GitHub allows 60 anonymous API calls an hour per address, and the app
/// makes one per launch.
pub const REPO_URL: &str = "https://github.com/tomasbruckner/dbc";

/// The environment variable that points the updater at a local folder
/// instead of GitHub. See the module docs.
pub const SOURCE_ENV: &str = "DBC_UPDATE_SOURCE";

/// Velopack's own startup work. Call it before anything else in `main`.
pub fn startup() {
    VelopackApp::build().run();
}

/// What [`check_and_download`] found.
pub enum Outcome {
    /// Not a Velopack install (dev run, copied exe). Show nothing.
    NotInstalled,
    /// The feed had nothing newer.
    UpToDate,
    /// A newer version is downloaded and waiting in the packages folder.
    Ready(ReadyUpdate),
    /// The feed or the download failed. Worth a log line, not a dialog.
    Failed(String),
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

/// Blocking: builds the manager, asks the feed, downloads. Run it on a
/// background thread; it can take as long as a 45 MB download.
pub fn check_and_download() -> Outcome {
    let manager = match UpdateManager::new_boxed(source(), None, None) {
        Ok(m) => m,
        Err(velopack::Error::NotInstalled(_)) => return Outcome::NotInstalled,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let info = match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => info,
        Ok(UpdateCheck::NoUpdateAvailable) | Ok(UpdateCheck::RemoteIsEmpty) => return Outcome::UpToDate,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    if let Err(e) = manager.download_updates(&info, None) {
        return Outcome::Failed(e.to_string());
    }
    let asset = info.TargetFullRelease.clone();
    Outcome::Ready(ReadyUpdate { version: asset.Version.clone(), manager, asset })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing a dev run must never do is show an update button or
    /// fail loudly. Nothing here is a Velopack install.
    #[test]
    fn a_dev_run_is_not_installed() {
        assert!(matches!(check_and_download(), Outcome::NotInstalled));
    }
}
