//! Walks folders fast, like TreeSize: many folders at once, never following links or junctions,
//! skipping the system's own clutter, with every folder's total size and file count.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

#[derive(Debug, Clone)]
pub struct FoundFile {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FoundFolder {
    pub name: String,
    pub path: PathBuf,
    /// Biggest first.
    pub folders: Vec<FoundFolder>,
    pub files: Vec<FoundFile>,
    /// Everything under it, all the way down.
    pub size: u64,
    pub file_count: u64,
    pub folder_count: u64,
    /// Folders that couldn't be read (no permission, gone while scanning).
    pub unreadable: u64,
}

/// Running totals while a scan goes, for showing it count up.
#[derive(Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub folders: AtomicU64,
    pub stop: AtomicBool,
}

const CLUTTER: &[&str] = &["desktop.ini", "thumbs.db", ".ds_store", "$recycle.bin", "system volume information", ".spotlight-v100", ".trashes", ".fseventsd"];

pub fn is_clutter(name: &str) -> bool {
    name.starts_with("~$") || CLUTTER.contains(&name.to_ascii_lowercase().as_str())
}

#[cfg(windows)]
fn skipped(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const SYSTEM: u32 = 0x4;
    const TEMPORARY: u32 = 0x100;
    const REPARSE: u32 = 0x400;
    meta.file_attributes() & (SYSTEM | TEMPORARY | REPARSE) != 0
}

#[cfg(not(windows))]
fn skipped(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Scans a folder and everything in it. Runs on the calling thread and rayon's; call it off the UI thread.
pub fn scan(path: &Path, progress: &ScanProgress) -> FoundFolder {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned());
    let mut root = FoundFolder { name, path: path.to_path_buf(), ..Default::default() };
    walk(&mut root, progress);
    root
}

fn walk(folder: &mut FoundFolder, progress: &ScanProgress) {
    if progress.stop.load(Ordering::Relaxed) { return; }
    match std::fs::read_dir(&folder.path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_clutter(&name) { continue; }
                /* The kind comes with the listing; links and junctions are never followed. */
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_symlink() { continue; }
                /* On Windows this comes from the listing too, with no extra look at the disk. */
                let Ok(meta) = entry.metadata() else { continue };
                if skipped(&meta) { continue; }
                if kind.is_dir() {
                    folder.folders.push(FoundFolder { name, path: entry.path(), ..Default::default() });
                } else if kind.is_file() {
                    folder.files.push(FoundFile { name, size: meta.len() });
                }
            }
        }
        Err(_) => folder.unreadable += 1,
    }
    let bytes: u64 = folder.files.iter().map(|f| f.size).sum();
    progress.files.fetch_add(folder.files.len() as u64, Ordering::Relaxed);
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    progress.folders.fetch_add(1, Ordering::Relaxed);

    /* Disks answer many questions at once faster than one at a time, SSDs especially. */
    folder.folders.par_iter_mut().for_each(|sub| walk(sub, progress));

    folder.folders.sort_by(|a, b| b.size.cmp(&a.size));
    folder.size = bytes + folder.folders.iter().map(|f| f.size).sum::<u64>();
    folder.file_count = folder.files.len() as u64 + folder.folders.iter().map(|f| f.file_count).sum::<u64>();
    folder.folder_count = folder.folders.len() as u64 + folder.folders.iter().map(|f| f.folder_count).sum::<u64>();
    folder.unreadable += folder.folders.iter().map(|f| f.unreadable).sum::<u64>();
}
