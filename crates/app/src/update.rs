//! Keeping Kimono up to date, quietly: a look at the releases now and every few hours; a new
//! version downloads in the background and installs when Kimono next quits or restarts.

use std::sync::Mutex;
use std::time::Duration;

use gpui_kit::App;
use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateManager};

/// Where releases are published: the workspace's repository, so moving it means changing one line.
const RELEASES: &str = env!("CARGO_PKG_REPOSITORY");
static READY: Mutex<Option<String>> = Mutex::new(None);

/// The version waiting to install, if one is.
pub fn ready() -> Option<String> {
    READY.lock().ok()?.clone()
}

fn manager() -> Option<UpdateManager> {
    /* Not installed (run from a build folder): nothing to update. */
    UpdateManager::new(GithubSource::new(RELEASES, None, false), None, None).ok()
}

pub fn start(cx: &mut App) {
    std::thread::Builder::new().name("kimono-update".into()).spawn(|| loop {
        if let Some(manager) = manager() {
            if let Ok(UpdateCheck::UpdateAvailable(update)) = manager.check_for_updates() {
                if manager.download_updates(&update, None).is_ok() {
                    let version = update.TargetFullRelease.Version.clone();
                    tracing::info!("Kimono {version} downloaded");
                    /* Installs as soon as this process exits, whenever that is, and starts Kimono again. */
                    let _ = manager.wait_exit_then_apply_updates(&*update, true, true, ["--background"]);
                    *READY.lock().unwrap() = Some(version);
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(4 * 3600));
    }).ok();
    let _ = cx;
}
