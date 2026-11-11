//! Keeps the Kimono folder here in step with Drive, both ways. Drive's changes come from its
//! change feed (asked every few seconds, and at once when nudged); changes made here come from
//! the OS backend, which calls the methods below to do them on Drive first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use kimono_core::tus::{self, Source};
use kimono_core::{Client, Node, Problem};
use parking_lot::Mutex;
use tokio::sync::{watch, Notify};

use crate::mirror::{local_parent, Action, Entry, Mirror, SHARED};

/// Does each action on this computer's disk. Called off the async threads.
pub trait Backend: Send + Sync + 'static {
    fn apply(&self, root: &Path, action: &Action) -> std::io::Result<()>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Starting,
    UpToDate,
    Syncing { left: usize },
    Offline,
    SignedOut,
    Failed(String),
}

pub struct Engine {
    pub client: Client,
    pub root: PathBuf,
    state: PathBuf,
    pub mirror: Mutex<Mirror>,
    backend: Mutex<Option<Arc<dyn Backend>>>,
    status: watch::Sender<Status>,
    wake: Notify,
}

const POLL: Duration = Duration::from_secs(15);

impl Engine {
    /// `root` is the Kimono folder; `state` the file where what's here is remembered.
    pub fn new(client: Client, root: PathBuf, state: PathBuf) -> Arc<Self> {
        let mirror = std::fs::read(&state).map(|b| Mirror::load(&b)).unwrap_or_default();
        Arc::new(Self { client, root, state, mirror: Mutex::new(mirror), backend: Mutex::new(None), status: watch::channel(Status::Starting).0, wake: Notify::new() })
    }

    pub fn set_backend(&self, backend: Arc<dyn Backend>) {
        *self.backend.lock() = Some(backend);
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }

    /// Looks for changes now rather than at the next poll.
    pub fn nudge(&self) {
        self.wake.notify_one();
    }

    /// Forgets what's here, so the next round lists everything again.
    pub fn forget(&self) {
        *self.mirror.lock() = Mirror::default();
        let _ = std::fs::remove_file(&self.state);
    }

    fn save(&self) {
        let bytes = self.mirror.lock().save();
        if let Some(dir) = self.state.parent() { let _ = std::fs::create_dir_all(dir); }
        let temp = self.state.with_extension("tmp");
        if std::fs::write(&temp, bytes).is_ok() { let _ = std::fs::rename(&temp, &self.state); }
    }

    /// Runs until the task is dropped.
    pub async fn run(self: Arc<Self>) {
        loop {
            match self.round().await {
                Ok(()) => { self.status.send_replace(Status::UpToDate); }
                Err(p) if p.status == 401 => { self.status.send_replace(Status::SignedOut); }
                Err(p) if p.offline() => { self.status.send_replace(Status::Offline); }
                Err(p) => { self.status.send_replace(Status::Failed(p.message)); }
            }
            let _ = tokio::time::timeout(POLL, self.wake.notified()).await;
        }
    }

    /// One round: what changed since last time, or everything when that's unknown.
    pub async fn round(&self) -> Result<(), Problem> {
        let Some(mut cursor) = self.mirror.lock().cursor else { return self.relist().await };
        loop {
            let changes = match self.client.changes(cursor).await {
                Err(p) if p.status == 410 => return self.relist().await,
                other => other?,
            };
            let actions = self.mirror.lock().apply_changes(&changes.changed, &changes.removed, &changes.spaces);
            self.execute(actions).await;
            cursor = changes.cursor;
            self.mirror.lock().cursor = Some(cursor);
            self.save();
            if !changes.more { return Ok(()); }
        }
    }

    /// Lists everything on Drive and brings the folder here to match.
    pub async fn relist(&self) -> Result<(), Problem> {
        self.status.send_replace(Status::Syncing { left: 0 });
        let cursor = self.client.latest_cursor().await?;
        let spaces = self.client.spaces().await?;
        let shared = self.client.shared_with_you().await?;
        let mut nodes: Vec<Node> = shared.clone();
        let mut folders: Vec<String> = spaces.iter().map(|s| s.root_id.clone()).chain(shared.iter().filter(|n| n.is_folder()).map(|n| n.id.clone())).collect();
        /* Many folders at once: a big Drive lists in seconds, not minutes. */
        while !folders.is_empty() {
            let listed: Vec<_> = stream::iter(folders.drain(..)).map(|id| { let c = self.client.clone(); async move { c.folder(&id).await } }).buffer_unordered(12).collect().await;
            for listing in listed {
                let listing = match listing { Ok(l) => l, Err(p) if p.status == 404 => continue, Err(p) => return Err(p) };
                for child in listing.children {
                    if child.is_folder() { folders.push(child.id.clone()); }
                    nodes.push(child);
                }
            }
        }
        let actions = self.mirror.lock().reconcile(&spaces, &nodes, !shared.is_empty());
        self.execute(actions).await;
        self.mirror.lock().cursor = Some(cursor);
        self.save();
        Ok(())
    }

    async fn execute(&self, actions: Vec<Action>) {
        let Some(backend) = self.backend.lock().clone() else { return };
        let total = actions.len();
        for (i, action) in actions.into_iter().enumerate() {
            if total > 20 && i % 50 == 0 { self.status.send_replace(Status::Syncing { left: total - i }); }
            let (backend, root) = (backend.clone(), self.root.clone());
            let result = tokio::task::spawn_blocking(move || backend.apply(&root, &action).map_err(|e| (action, e))).await;
            if let Ok(Err((action, e))) = result { tracing::warn!("couldn't {action:?}: {e}"); }
        }
    }

    // ---- Changes made here ----

    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.root).ok().filter(|p| !p.as_os_str().is_empty())
    }

    /// The Drive folder a path here goes into: the entry for its parent folder.
    fn folder_for(&self, path: &Path) -> Result<Entry, Problem> {
        let relative = self.relative(path).ok_or_else(|| Problem::say("Put files inside one of your spaces."))?;
        let parent = relative.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| Problem::say("Put files inside one of your spaces."))?;
        let entry = self.mirror.lock().at(parent).cloned().ok_or_else(|| Problem::say("That folder isn't on Kimono yet."))?;
        if entry.id == SHARED { return Err(Problem::say("Put files inside one of the folders shared with you.")); }
        Ok(entry)
    }

    /// The entry for a path here, if Kimono has it.
    pub fn entry_at(&self, path: &Path) -> Option<Entry> {
        self.mirror.lock().at(self.relative(path)?).cloned()
    }

    /// A folder or file made here goes up to Drive. Returns its entry, already remembered.
    pub async fn added(&self, path: &Path) -> Result<Entry, Problem> {
        let parent = self.folder_for(path)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| Problem::say("That has no name."))?;
        let node = if path.is_dir() {
            match self.client.create_folder(&parent.id, &name).await {
                Ok(n) => n,
                Err(p) if p.status == 409 => {
                    let listing = self.client.folder(&parent.id).await?;
                    listing.children.into_iter().find(|c| c.is_folder() && c.name.eq_ignore_ascii_case(&name)).ok_or(p)?
                }
                Err(p) => return Err(p),
            }
        } else {
            let size = std::fs::metadata(path).map_err(Problem::from)?.len();
            let up = tus::send(&self.client, Source::file(path), size, &tus::new_file(&parent.id, &name), None, None).await?;
            self.client.node(&up.id).await?
        };
        /* Kept under the name it has here, even if Drive numbered it to keep names apart. */
        let mut entry = Entry::from_node(&node, local_parent(&node).or(Some(parent.id.clone())));
        entry.name = name;
        self.mirror.lock().added_here(entry.clone());
        self.save();
        Ok(entry)
    }

    /// A file edited here: its new bytes go up. If someone else's edit landed first, Drive keeps
    /// these as a copy beside it, and the copy here becomes that copy.
    pub async fn edited(&self, path: &Path) -> Result<(Entry, bool), Problem> {
        let entry = self.entry_at(path).ok_or_else(|| Problem::say("That file isn't on Kimono yet."))?;
        let size = std::fs::metadata(path).map_err(Problem::from)?.len();
        let up = tus::send(&self.client, Source::file(path), size, &tus::replace(&entry.id, entry.content_version), None, None).await?;
        let node = self.client.node(&up.id).await?;
        if up.conflict {
            let copy = Entry::from_node(&node, entry.parent.clone());
            /* The original comes back on the next round, with the other edit. */
            self.mirror.lock().removed_here(&entry.id);
            self.mirror.lock().added_here(copy.clone());
            self.save();
            self.nudge();
            return Ok((copy, true));
        }
        self.mirror.lock().saved_here(&entry.id, node.size, node.content_version);
        self.save();
        Ok((self.mirror.lock().get(&entry.id).cloned().unwrap_or(entry), false))
    }

    /// Something here was renamed or moved, within the Kimono folder.
    pub async fn moved(&self, from: &Path, to: &Path) -> Result<(), Problem> {
        let entry = self.entry_at(from).ok_or_else(|| Problem::say("That isn't on Kimono yet."))?;
        if entry.parent.is_none() { return Err(Problem::new("Rename spaces on the Kimono website.", 403)); }
        let target = self.folder_for(to)?;
        let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let new_parent = (Some(&target.id) != entry.parent.as_ref()).then_some(target.id.as_str());
        let new_name = (name != entry.name).then_some(name.as_str());
        if new_parent.is_none() && new_name.is_none() { return Ok(()); }
        self.client.update(&entry.id, new_name, new_parent).await?;
        self.mirror.lock().moved_here(&entry.id, Some(target.id.clone()), &name);
        self.save();
        Ok(())
    }

    /// Something here was deleted: it goes to Drive's trash, for 30 days.
    pub async fn deleted(&self, path: &Path) -> Result<(), Problem> {
        let Some(entry) = self.entry_at(path) else { return Ok(()) };
        if entry.parent.is_none() { return Err(Problem::new("Leave or delete spaces on the Kimono website.", 403)); }
        match self.client.trash(&entry.id).await {
            Ok(()) => {}
            Err(p) if p.status == 404 => {}
            Err(p) => return Err(p),
        }
        self.mirror.lock().removed_here(&entry.id);
        self.save();
        Ok(())
    }
}
