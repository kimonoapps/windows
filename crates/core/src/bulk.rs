//! Uploads whole folder trees to a Drive folder as fast as the link allows: folders are made
//! while files are already going up, small files go many at once and big ones a few at once,
//! each upload resumes after a dropped connection, and anything already there is skipped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::{join_all, BoxFuture, FutureExt};
use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::client::Client;
use crate::models::Node;
use crate::problem::{Problem, Result};
use crate::scan::FoundFolder;
use crate::tus::{self, ResumeStore, Source};

#[derive(Debug, Clone)]
pub struct Options {
    /// Uploads at once for small files, where the time goes on round trips, not bytes.
    pub small_lanes: usize,
    /// Uploads at once for big files, where a few streams fill the link.
    pub big_lanes: usize,
    pub small_below: u64,
    /// A file already in its folder on Kimono with the same name and size is left alone, so running again carries on.
    pub skip_existing: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { small_lanes: 16, big_lanes: 4, small_below: 8 * 1024 * 1024, skip_existing: true }
    }
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub path: PathBuf,
    pub reason: String,
}

/// A file on its way up, for showing what's happening now.
#[derive(Debug)]
pub struct ActiveFile {
    pub name: String,
    pub size: u64,
    pub sent: AtomicI64,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub total_files: AtomicU64,
    pub total_bytes: AtomicU64,
    pub done_files: AtomicU64,
    /// Bytes sent, or found already there. Can dip when a dropped piece is sent again.
    pub done_bytes: AtomicI64,
    pub skipped_files: AtomicU64,
    pub failed_files: AtomicU64,
    pub folders_made: AtomicU64,
}

struct Work {
    path: PathBuf,
    name: String,
    size: u64,
    folder: String,
}

pub struct BulkUpload {
    client: Client,
    destination: String,
    folders: Vec<FoundFolder>,
    files: Vec<PathBuf>,
    options: Options,
    resume: Option<Arc<dyn ResumeStore>>,
    pub stats: Arc<Stats>,
    active: Arc<Mutex<HashMap<u64, Arc<ActiveFile>>>>,
    failures: Arc<Mutex<Vec<Failure>>>,
}

type Existing = Option<Arc<HashMap<String, Node>>>;

impl BulkUpload {
    pub fn new(client: Client, destination: impl Into<String>, folders: Vec<FoundFolder>, files: Vec<PathBuf>, options: Options, resume: Option<Arc<dyn ResumeStore>>) -> Arc<Self> {
        let stats = Stats::default();
        for f in &folders {
            stats.total_files.fetch_add(f.file_count, Ordering::Relaxed);
            stats.total_bytes.fetch_add(f.size, Ordering::Relaxed);
        }
        for p in &files {
            stats.total_files.fetch_add(1, Ordering::Relaxed);
            stats.total_bytes.fetch_add(std::fs::metadata(p).map(|m| m.len()).unwrap_or(0), Ordering::Relaxed);
        }
        Arc::new(Self {
            client, destination: destination.into(), folders, files, options, resume,
            stats: Arc::new(stats), active: Default::default(), failures: Default::default(),
        })
    }

    pub fn active(&self) -> Vec<Arc<ActiveFile>> {
        let mut list: Vec<_> = self.active.lock().values().cloned().collect();
        list.sort_by(|a, b| b.size.cmp(&a.size));
        list
    }

    pub fn failures(&self) -> Vec<Failure> {
        self.failures.lock().clone()
    }

    /// Runs to the end. Drop the future (or abort its task) to stop; running again carries on.
    pub async fn run(self: &Arc<Self>) {
        let (small_tx, small_rx) = async_channel::unbounded::<Work>();
        let (big_tx, big_rx) = async_channel::unbounded::<Work>();
        let mut lanes = Vec::new();
        for (rx, n) in [(small_rx, self.options.small_lanes), (big_rx, self.options.big_lanes)] {
            for _ in 0..n {
                let (me, rx) = (self.clone(), rx.clone());
                lanes.push(tokio::spawn(async move { me.lane(rx).await }));
            }
        }
        self.plan(&small_tx, &big_tx).await;
        drop((small_tx, big_tx));
        for lane in lanes { let _ = lane.await; }
    }

    /// Makes the folders, top down, handing each folder's files to the lanes as soon as it exists.
    async fn plan(self: &Arc<Self>, small: &async_channel::Sender<Work>, big: &async_channel::Sender<Work>) {
        let destination: Existing = if self.options.skip_existing {
            match self.existing(&self.destination).await {
                Ok(e) => Some(Arc::new(e)),
                Err(p) => { self.fail_everything(&p); return; }
            }
        } else { None };
        for path in &self.files {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            self.queue(Work { path: path.clone(), name, size, folder: self.destination.clone() }, &destination, small, big).await;
        }
        let gate = Arc::new(Semaphore::new(6));
        let roots = self.folders.iter().map(|f| self.folder(f, self.destination.clone(), destination.clone(), gate.clone(), small, big));
        join_all(roots).await;
    }

    fn folder<'a>(self: &'a Arc<Self>, folder: &'a FoundFolder, parent: String, parent_has: Existing, gate: Arc<Semaphore>,
        small: &'a async_channel::Sender<Work>, big: &'a async_channel::Sender<Work>) -> BoxFuture<'a, ()> {
        async move {
            let ensured = {
                let _turn = gate.acquire().await;
                self.ensure(&parent, &folder.name, &parent_has).await
            };
            let (id, has) = match ensured {
                Ok(e) => e,
                Err(p) => {
                    self.fail(folder.path.clone(), p.message, folder.file_count, folder.size);
                    return;
                }
            };
            for file in &folder.files {
                self.queue(Work { path: folder.path.join(&file.name), name: file.name.clone(), size: file.size, folder: id.clone() }, &has, small, big).await;
            }
            join_all(folder.folders.iter().map(|sub| self.folder(sub, id.clone(), has.clone(), gate.clone(), small, big))).await;
        }.boxed()
    }

    async fn queue(&self, work: Work, has: &Existing, small: &async_channel::Sender<Work>, big: &async_channel::Sender<Work>) {
        if let Some(there) = has.as_ref().and_then(|h| h.get(&work.name.to_lowercase())) {
            if !there.is_folder() && there.size == work.size {
                self.stats.skipped_files.fetch_add(1, Ordering::Relaxed);
                self.stats.done_files.fetch_add(1, Ordering::Relaxed);
                self.stats.done_bytes.fetch_add(work.size as i64, Ordering::Relaxed);
                return;
            }
        }
        let lane = if work.size < self.options.small_below { small } else { big };
        let _ = lane.send(work).await;
    }

    /// The folder called `name` in a folder, made if it isn't there, with what's already in it.
    async fn ensure(&self, parent: &str, name: &str, parent_has: &Existing) -> Result<(String, Existing)> {
        let listing = |id: String| async move {
            if self.options.skip_existing { self.existing(&id).await.map(|e| Some(Arc::new(e))) } else { Ok(None) }
        };
        if let Some(there) = parent_has.as_ref().and_then(|h| h.get(&name.to_lowercase())).filter(|n| n.is_folder()) {
            return Ok((there.id.clone(), listing(there.id.clone()).await?));
        }
        let mut attempt = 0u32;
        loop {
            match self.client.create_folder(parent, name).await {
                Ok(made) => {
                    self.stats.folders_made.fetch_add(1, Ordering::Relaxed);
                    /* New and empty: nothing to compare against. */
                    return Ok((made.id, Some(Arc::new(HashMap::new()))));
                }
                Err(p) if p.status == 409 => {
                    /* Made meanwhile (or a file has the name): look again. */
                    let now = self.existing(parent).await?;
                    return match now.get(&name.to_lowercase()).filter(|n| n.is_folder()) {
                        Some(found) => Ok((found.id.clone(), listing(found.id.clone()).await?)),
                        None => Err(Problem::say(format!("A file called {name} is already there."))),
                    };
                }
                Err(p) if (p.offline() || p.busy()) && attempt < 8 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(1 << attempt.min(6))).await;
                }
                Err(p) => return Err(p),
            }
        }
    }

    async fn existing(&self, folder: &str) -> Result<HashMap<String, Node>> {
        let mut attempt = 0u32;
        loop {
            match self.client.folder(folder).await {
                Ok(listing) => {
                    let mut by_name = HashMap::new();
                    for child in listing.children { by_name.entry(child.name.to_lowercase()).or_insert(child); }
                    return Ok(by_name);
                }
                Err(p) if (p.offline() || p.busy()) && attempt < 8 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(1 << attempt.min(6))).await;
                }
                Err(p) => return Err(p),
            }
        }
    }

    async fn lane(self: Arc<Self>, queue: async_channel::Receiver<Work>) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        while let Ok(work) = queue.recv().await {
            let slot = NEXT.fetch_add(1, Ordering::Relaxed);
            let shown = Arc::new(ActiveFile { name: work.name.clone(), size: work.size, sent: AtomicI64::new(0) });
            self.active.lock().insert(slot, shown.clone());
            let counted = Arc::new(AtomicI64::new(0));
            let progress: tus::Progress = {
                let (stats, shown, counted) = (self.stats.clone(), shown.clone(), counted.clone());
                Arc::new(move |n| {
                    counted.fetch_add(n, Ordering::Relaxed);
                    shown.sent.fetch_add(n, Ordering::Relaxed);
                    stats.done_bytes.fetch_add(n, Ordering::Relaxed);
                })
            };
            let sent = tus::send(&self.client, Source::file(&work.path), work.size, &tus::new_file(&work.folder, &work.name), Some(progress), self.resume.as_deref()).await;
            match sent {
                Ok(_) => { self.stats.done_files.fetch_add(1, Ordering::Relaxed); }
                Err(p) => {
                    /* The file's bytes stop counting as sent; it counts as failed instead. */
                    self.stats.done_bytes.fetch_sub(counted.load(Ordering::Relaxed), Ordering::Relaxed);
                    self.fail(work.path, p.message, 1, work.size);
                }
            }
            self.active.lock().remove(&slot);
        }
    }

    fn fail(&self, path: PathBuf, reason: String, files: u64, bytes: u64) {
        self.failures.lock().push(Failure { path, reason });
        self.stats.failed_files.fetch_add(files, Ordering::Relaxed);
        self.stats.done_files.fetch_add(files, Ordering::Relaxed);
        self.stats.done_bytes.fetch_add(bytes as i64, Ordering::Relaxed);
    }

    fn fail_everything(&self, p: &Problem) {
        let (files, bytes) = (self.stats.total_files.load(Ordering::Relaxed), self.stats.total_bytes.load(Ordering::Relaxed));
        self.fail(PathBuf::new(), p.message.clone(), files, bytes);
    }
}
