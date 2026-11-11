//! Keeping chosen folders and files backed up to Kimono. Each folder goes to
//! `Backups/<this computer>/<its full path>` in a space you pick, so two folders with the same name
//! never meet. A folder is watched: a minute after changes settle it goes up again, and every
//! hour regardless. Only what's new or changed is sent; changed files replace their Kimono copy
//! (keeping its history), and nothing is ever deleted on Kimono because it was deleted here.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kimono_core::bulk::{BulkUpload, Options};
use kimono_core::scan::{self, ScanProgress};
use kimono_core::tus::FileResume;
use kimono_core::Client;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};

const QUIET: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// The top folder of the space backups go to, and its name for showing.
    pub space_root: Option<String>,
    pub space_name: Option<String>,
    pub chosen: Vec<PathBuf>,
    /// When the last full round finished, in milliseconds since 1970.
    pub last_done: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Nothing chosen yet.
    Off,
    Idle { last_done: Option<i64>, failed: u64 },
    Looking { folder: String },
    Sending { done: u64, total: u64, files_left: u64 },
    Failed(String),
}

pub struct Backup {
    client: Client,
    file: PathBuf,
    resume: Arc<FileResume>,
    pub config: Mutex<Config>,
    status: watch::Sender<Status>,
    wake: Notify,
    /// When the last change was seen in a backed-up folder: a round waits for things to settle.
    changed: Mutex<Option<Instant>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// This computer's name, as backups are filed under it.
pub fn computer_name() -> String {
    gethostname::gethostname().to_string_lossy().trim_end_matches(".local").to_string()
}

/// Where a folder's parent goes under `Backups/<computer>`: `C:\Users\akram` becomes `C/Users/akram`.
pub fn parents_on_kimono(folder: &Path) -> Vec<String> {
    let parent = folder.parent().unwrap_or(Path::new(""));
    parent.components().filter_map(|c| match c {
        Component::Prefix(p) => Some(p.as_os_str().to_string_lossy().trim_end_matches(':').trim_start_matches(r"\\?\").to_string()),
        Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
        _ => None,
    }).filter(|s| !s.is_empty()).collect()
}

impl Backup {
    pub fn new(client: Client, data: &Path) -> Arc<Self> {
        let file = data.join("backup.json");
        let config: Config = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let status = if config.chosen.is_empty() { Status::Off } else { Status::Idle { last_done: config.last_done, failed: 0 } };
        Arc::new(Self {
            client,
            file,
            resume: Arc::new(FileResume::open(data.join("backup-uploads.json"))),
            config: Mutex::new(config),
            status: watch::channel(status).0,
            wake: Notify::new(),
            changed: Mutex::new(None),
            watcher: Mutex::new(None),
        })
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    fn save(&self) {
        let bytes = serde_json::to_vec_pretty(&*self.config.lock()).unwrap_or_default();
        let temp = self.file.with_extension("tmp");
        if std::fs::write(&temp, bytes).is_ok() { let _ = std::fs::rename(temp, &self.file); }
    }

    /// Whether a folder is backed up, on its own or as part of one above it.
    pub fn covers(&self, path: &Path) -> Option<PathBuf> {
        self.config.lock().chosen.iter().find(|f| path.starts_with(f)).cloned()
    }

    /// Turns a folder's backup on or off. Turning on a folder takes in any chosen inside it.
    pub fn set(self: &Arc<Self>, path: &Path, on: bool) {
        {
            let mut config = self.config.lock();
            config.chosen.retain(|f| f != path && !(on && f.starts_with(path)));
            if on { config.chosen.push(path.to_path_buf()); }
            config.chosen.sort();
        }
        self.save();
        self.rewatch();
        if on { self.now(); } else if self.config.lock().chosen.is_empty() { self.status.send_replace(Status::Off); }
    }

    pub fn set_destination(self: &Arc<Self>, root: String, name: String) {
        {
            let mut config = self.config.lock();
            config.space_root = Some(root);
            config.space_name = Some(name);
        }
        self.save();
        self.now();
    }

    /// Backs up now, rather than when changes settle or the hour comes round.
    pub fn now(&self) {
        *self.changed.lock() = None;
        self.wake.notify_one();
    }

    /// Watches every chosen folder for changes.
    fn rewatch(self: &Arc<Self>) {
        use notify::{RecursiveMode, Watcher};
        let me = Arc::downgrade(self);
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Some(me) = me.upgrade() else { return };
            if let Ok(e) = event {
                if matches!(e.kind, notify::EventKind::Access(_)) { return; }
                *me.changed.lock() = Some(Instant::now());
                me.wake.notify_one();
            }
        });
        let Ok(mut watcher) = watcher else { return };
        for folder in self.config.lock().chosen.clone() {
            let mode = if folder.is_dir() { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            if let Err(e) = watcher.watch(&folder, mode) { tracing::warn!("can't watch {}: {e}", folder.display()); }
        }
        *self.watcher.lock() = Some(watcher);
    }

    /// Runs until the task is dropped.
    pub async fn run(self: Arc<Self>) {
        self.rewatch();
        let mut last_round: Option<Instant> = None;
        loop {
            let due = last_round.is_none_or(|t| t.elapsed() >= EVERY);
            let settled = self.changed.lock().is_none_or(|t| t.elapsed() >= QUIET);
            let changed = self.changed.lock().is_some();
            if !self.config.lock().chosen.is_empty() && (due || changed) && settled {
                *self.changed.lock() = None;
                self.round().await;
                last_round = Some(Instant::now());
            }
            /* Woken by a change, a choice, or "now"; otherwise look again shortly. */
            let _ = tokio::time::timeout(Duration::from_secs(15), self.wake.notified()).await;
        }
    }

    /// One round: every chosen folder, one after another.
    async fn round(&self) {
        let (folders, root) = {
            let config = self.config.lock();
            (config.chosen.clone(), config.space_root.clone())
        };
        let root = match root {
            Some(r) => r,
            None => match self.client.spaces().await {
                Ok(spaces) => match spaces.into_iter().find(|s| s.can_edit()) {
                    Some(s) => {
                        let mut config = self.config.lock();
                        config.space_root = Some(s.root_id.clone());
                        config.space_name = Some(s.name.clone());
                        drop(config);
                        self.save();
                        s.root_id
                    }
                    None => { self.status.send_replace(Status::Failed("There's no space you can add to.".into())); return; }
                },
                Err(p) => { self.status.send_replace(Status::Failed(p.message)); return; }
            },
        };
        let computer = computer_name();
        let mut failed = 0u64;
        /* Stay awake until this round is done: people start a backup and walk away. */
        let _awake = crate::awake::hold();
        for folder in folders {
            /* Folders and single files alike; anything gone from this computer is left as it is on Kimono. */
            let is_file = folder.is_file();
            if !is_file && !folder.is_dir() { continue; }
            let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.status.send_replace(Status::Looking { folder: name.clone() });
            let mut path = vec!["Backups".to_string(), computer.clone()];
            path.extend(parents_on_kimono(&folder));
            let destination = match self.client.ensure_path(&root, &path).await {
                Ok(id) => id,
                Err(p) => { self.status.send_replace(Status::Failed(p.message)); return; }
            };
            let (folders, files) = if is_file { (vec![], vec![folder.clone()]) } else {
                let scanned = { let f = folder.clone(); tokio::task::spawn_blocking(move || scan::scan(&f, &ScanProgress::default())).await };
                let Ok(found) = scanned else { continue };
                (vec![found], vec![])
            };
            let options = Options { replace_changed: true, small_lanes: 8, big_lanes: 3, ..Default::default() };
            let upload = BulkUpload::new(self.client.clone(), destination, folders, files, options, Some(self.resume.clone()));
            let running = tokio::spawn({ let u = upload.clone(); async move { u.run().await } });
            while !running.is_finished() {
                let s = &upload.stats;
                let (total, done) = (s.total_bytes.load(Ordering::Relaxed), s.done_bytes.load(Ordering::Relaxed).max(0) as u64);
                let files_left = s.total_files.load(Ordering::Relaxed).saturating_sub(s.done_files.load(Ordering::Relaxed));
                /* Only worth showing when something is actually going up. */
                if s.skipped_files.load(Ordering::Relaxed) < s.done_files.load(Ordering::Relaxed) || !upload.active().is_empty() {
                    self.status.send_replace(Status::Sending { done, total, files_left });
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            failed += upload.stats.failed_files.load(Ordering::Relaxed);
            for f in upload.failures().iter().take(20) { tracing::warn!("backup: {} — {}", f.path.display(), f.reason); }
        }
        let done = now_ms();
        self.config.lock().last_done = Some(done);
        self.save();
        self.status.send_replace(Status::Idle { last_done: Some(done), failed });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_keep_their_whole_path() {
        #[cfg(windows)]
        assert_eq!(parents_on_kimono(Path::new(r"C:\Users\akram\Pictures")), vec!["C", "Users", "akram"]);
        #[cfg(not(windows))]
        assert_eq!(parents_on_kimono(Path::new("/Users/akram/Pictures")), vec!["Users", "akram"]);
    }
}
