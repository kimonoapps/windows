//! What the whole app shares: the sign-in, the Kimono folder's sync, and where things are kept.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::Global;
use kimono_core::{Client, SecretStore};
use kimono_drive::{Engine, Status};
use parking_lot::Mutex;
use tokio::sync::watch;

/// Sign-in kept in the system keychain: Credential Manager, Keychain, or the Secret Service.
pub struct Keychain;

impl SecretStore for Keychain {
    fn get(&self, key: &str) -> Option<String> {
        keyring::Entry::new("Kimono", key).ok()?.get_password().ok()
    }
    fn set(&self, key: &str, value: Option<&str>) {
        let Ok(entry) = keyring::Entry::new("Kimono", key) else { return };
        match value {
            Some(v) => { let _ = entry.set_password(v); }
            None => { let _ = entry.delete_credential(); }
        }
    }
}

pub struct Paths {
    /// Where the app keeps its own state.
    pub data: PathBuf,
    /// The Kimono folder: Drive, as files.
    pub folder: PathBuf,
}

impl Paths {
    pub fn new() -> Self {
        /* On Windows the installer owns %LOCALAPPDATA%\Kimono (and empties it when installing), so what
           Kimono keeps lives beside it. Elsewhere the app is installed somewhere else entirely. */
        let data = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join(if cfg!(windows) { "Kimono Data" } else { "Kimono" });
        let folder = dirs::home_dir().unwrap_or_else(std::env::temp_dir).join("Kimono");
        let _ = std::fs::create_dir_all(&data);
        Self { data, folder }
    }
}

pub struct Drive {
    pub engine: Arc<Engine>,
    pub task: tokio::task::JoinHandle<()>,
    #[cfg(windows)]
    pub provider: Arc<kimono_drive::windows::Provider>,
    #[cfg(windows)]
    pub letter: Option<char>,
}

pub struct Kimono {
    pub runtime: tokio::runtime::Handle,
    pub client: Client,
    pub paths: Paths,
    pub drive: Mutex<Option<Drive>>,
    pub status: Mutex<watch::Receiver<Status>>,
    /// Folders and files handed over from Explorer ("Upload to Kimono") or a second launch.
    pub handed: Mutex<Vec<PathBuf>>,
    /// The folders kept backed up.
    pub backup: Arc<crate::backup::Backup>,
    backup_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Global for Kimono {}

impl Kimono {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        let client = Client::new(Arc::new(Keychain));
        let paths = Paths::new();
        let backup = crate::backup::Backup::new(client.clone(), &paths.data);
        Self { runtime, client, paths, drive: Mutex::new(None), status: Mutex::new(watch::channel(Status::Starting).1), handed: Mutex::new(vec![]), backup, backup_task: Mutex::new(None) }
    }

    /// The person and Kimono this computer is signed in to, as the folder's account name.
    fn account(&self) -> String {
        let host = self.client.server().and_then(|s| s.split("://").nth(1).map(|h| h.trim_end_matches('/').to_string())).unwrap_or_default();
        host.replace([':', '/'], "-")
    }

    /// Starts keeping the Kimono folder in step with Drive, and the chosen folders backed up.
    pub fn start_drive(&self) {
        {
            let mut task = self.backup_task.lock();
            if task.is_none() { *task = Some(self.runtime.spawn(self.backup.clone().run())); }
        }
        let mut slot = self.drive.lock();
        if slot.is_some() { return; }
        /* Debug builds: KIMONO_NO_DRIVE=1 leaves the Kimono folder alone, for narrowing problems down. */
        #[cfg(debug_assertions)]
        if std::env::var("KIMONO_NO_DRIVE").is_ok() { return; }
        let engine = Engine::new(self.client.clone(), self.paths.folder.clone(), self.paths.data.join(format!("drive-{}.json", self.account())));
        *self.status.lock() = engine.status();

        #[cfg(windows)]
        {
            let icon = std::env::current_exe().unwrap_or_default();
            if let Err(e) = kimono_drive::windows::register(&self.paths.folder, &self.account(), &icon) {
                tracing::error!("couldn't add Kimono to Explorer: {}", e.message());
            }
            let provider = match kimono_drive::windows::Provider::start(engine.clone(), self.runtime.clone()) {
                Ok(p) => p,
                Err(e) => { tracing::error!("couldn't connect the Kimono folder: {}", e.message()); return; }
            };
            let letter = kimono_drive::windows::free_letter();
            if let Some(l) = letter { let _ = kimono_drive::windows::map_letter(&self.paths.folder, l); }
            let task = self.runtime.spawn(engine.clone().run());
            *slot = Some(Drive { engine, task, provider, letter });
        }
        #[cfg(not(windows))]
        {
            /* The folder needs the OS's own file provider; until then, uploads work everywhere. */
            let _ = engine;
        }
    }

    pub fn stop_drive(&self, remove_files: bool) {
        if let Some(task) = self.backup_task.lock().take() { task.abort(); }
        let Some(drive) = self.drive.lock().take() else { return };
        drive.task.abort();
        #[cfg(windows)]
        {
            drive.provider.stop();
            if let Some(l) = drive.letter { kimono_drive::windows::unmap_letter(&self.paths.folder, l); }
            if remove_files {
                let _ = kimono_drive::windows::unregister(&self.account());
                drive.engine.forget();
                let _ = std::fs::remove_dir_all(&self.paths.folder);
            }
        }
        let _ = remove_files;
    }

    pub fn status(&self) -> Status {
        self.status.lock().borrow().clone()
    }

    pub fn open_folder(&self) {
        #[cfg(windows)]
        let target = self.drive.lock().as_ref().and_then(|d| d.letter).map(|l| PathBuf::from(format!("{l}:\\"))).unwrap_or(self.paths.folder.clone());
        #[cfg(not(windows))]
        let target = self.paths.folder.clone();
        crate::system::reveal(&target);
    }
}
