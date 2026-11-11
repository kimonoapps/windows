//! Kimono Drive in Explorer, through the Cloud Files API OneDrive uses: Kimono sits in the
//! navigation pane (and as a drive letter), every file shows at once without taking space, a
//! file downloads the moment something opens it, "Always keep on this device" keeps it, and
//! "Free up space" lets it go. Deleting or renaming here asks Kimono first, so what Kimono
//! refuses (a viewer's change) never happens here either.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures::StreamExt;
use parking_lot::Mutex;
use windows::core::{HSTRING, PCWSTR};
use windows::Storage::Provider::*;
use windows::Storage::StorageFolder;
use windows::Win32::Foundation::{CloseHandle, HANDLE, NTSTATUS};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::*;

use crate::engine::{Backend, Engine};
use crate::mirror::{Action, Entry};

const STATUS_SUCCESS: NTSTATUS = NTSTATUS(0);
const STATUS_UNSUCCESSFUL: NTSTATUS = NTSTATUS(0xC000_0001u32 as i32);
const STATUS_ACCESS_DENIED: NTSTATUS = NTSTATUS(0xC000_0022u32 as i32);
const STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE: NTSTATUS = NTSTATUS(0xC000_CF07u32 as i32);
const ERROR_NOT_A_CLOUD_FILE: i32 = 0x8007_0178u32 as i32;
const ATTRIBUTE_PINNED: u32 = 0x0008_0000;
const ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
const ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
const CHUNK: usize = 4 * 1024 * 1024;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(p: PCWSTR) -> String {
    if p.is_null() { return String::new(); }
    unsafe { String::from_utf16_lossy(p.as_wide()) }
}

fn io(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(e.message())
}

/// Windows file times: 100ns ticks since 1601.
fn file_time(ms: i64) -> i64 {
    if ms <= 0 { return 0; }
    (ms + 11_644_473_600_000) * 10_000
}

fn metadata(entry: &Entry) -> CF_FS_METADATA {
    let mut attributes = if entry.folder { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 };
    if !entry.folder && !entry.can_edit { attributes |= FILE_ATTRIBUTE_READONLY.0; }
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: file_time(entry.created),
            LastAccessTime: file_time(entry.updated),
            LastWriteTime: file_time(entry.updated),
            ChangeTime: file_time(entry.updated),
            FileAttributes: attributes,
        },
        FileSize: if entry.folder { 0 } else { entry.size as i64 },
    }
}

/// A handle opened just to look, never to read: looking doesn't download anything.
struct Handle(HANDLE);

impl Handle {
    fn look(path: &Path) -> std::io::Result<Self> {
        let w = wide(path);
        let h = unsafe {
            CreateFileW(PCWSTR(w.as_ptr()), FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, None)
        }.map_err(io)?;
        Ok(Self(h))
    }
    /// For changing a placeholder: exclusive, with write access, letting go if another app needs it.
    fn change(path: &Path) -> std::io::Result<Protected> {
        let w = wide(path);
        let h = unsafe { CfOpenFileWithOplock(PCWSTR(w.as_ptr()), CF_OPEN_FILE_FLAG_EXCLUSIVE | CF_OPEN_FILE_FLAG_WRITE_ACCESS) }.map_err(io)?;
        Ok(Protected(h))
    }
}

impl Drop for Handle {
    fn drop(&mut self) { unsafe { let _ = CloseHandle(self.0); } }
}

struct Protected(HANDLE);

impl Protected {
    fn win32(&self) -> HANDLE { unsafe { CfGetWin32HandleFromProtectedHandle(self.0) } }
}

impl Drop for Protected {
    fn drop(&mut self) { unsafe { CfCloseHandle(self.0) } }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// Not a placeholder: something made here that Kimono doesn't have yet.
    New,
    Placeholder { in_sync: bool, pinned: bool },
}

/// What a path here is to the Cloud Files platform.
pub fn kind(path: &Path) -> std::io::Result<Kind> {
    let handle = Handle::look(path)?;
    let mut buffer = [0u8; 512];
    let result = unsafe { CfGetPlaceholderInfo(handle.0, CF_PLACEHOLDER_INFO_BASIC, buffer.as_mut_ptr() as *mut c_void, buffer.len() as u32, None) };
    match result {
        Ok(()) => {
            let info = unsafe { &*(buffer.as_ptr() as *const CF_PLACEHOLDER_BASIC_INFO) };
            Ok(Kind::Placeholder { in_sync: info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC, pinned: info.PinState == CF_PIN_STATE_PINNED })
        }
        Err(e) if e.code().0 == ERROR_NOT_A_CLOUD_FILE => Ok(Kind::New),
        Err(e) => Err(io(e)),
    }
}

fn attributes(path: &Path) -> u32 {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path).map(|m| m.file_attributes()).unwrap_or(0)
}

fn on_disk(path: &Path) -> bool {
    attributes(path) & (ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE.0) == 0
}

/// The sync root's id: provider, the person's Windows account, and their Kimono.
fn sync_root_id(account: &str) -> windows::core::Result<HSTRING> {
    use windows::Win32::Foundation::HLOCAL;
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buffer = vec![0u8; needed as usize];
        GetTokenInformation(token, TokenUser, Some(buffer.as_mut_ptr() as *mut c_void), needed, &mut needed)?;
        let _ = CloseHandle(token);
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut sid = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
        let text = sid.to_string().unwrap_or_default();
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(sid.0 as *mut c_void)));
        let account: String = account.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        Ok(HSTRING::from(format!("Kimono!{text}!{account}")))
    }
}

/// Puts the folder in Explorer's navigation pane, as Kimono.
pub fn register(root: &Path, account: &str, icon: &Path) -> windows::core::Result<()> {
    std::fs::create_dir_all(root).map_err(|e| windows::core::Error::new(windows::core::HRESULT(0x8000_4005u32 as i32), e.to_string()))?;
    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(root.as_os_str()))?.join()?;
    let info = StorageProviderSyncRootInfo::new()?;
    info.SetId(&sync_root_id(account)?)?;
    info.SetPath(&folder)?;
    info.SetDisplayNameResource(&HSTRING::from("Kimono"))?;
    info.SetIconResource(&HSTRING::from(format!("{},0", icon.display())))?;
    info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full)?;
    info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::AutoDehydrationAllowed)?;
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::AlwaysFull)?;
    info.SetInSyncPolicy(StorageProviderInSyncPolicy::FileLastWriteTime)?;
    info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
    info.SetShowSiblingsAsGroup(false)?;
    info.SetVersion(&HSTRING::from(kimono_core::client::VERSION))?;
    info.SetAllowPinning(true)?;
    StorageProviderSyncRootManager::Register(&info)
}

/// Takes the folder out of Explorer (the files in it stay until removed).
pub fn unregister(account: &str) -> windows::core::Result<()> {
    StorageProviderSyncRootManager::Unregister(&sync_root_id(account)?)
}

/// Shows the folder as a drive letter too, like K:. Only for this sign-in to Windows; set again at each start.
pub fn map_letter(root: &Path, letter: char) -> windows::core::Result<()> {
    let device: Vec<u16> = format!("{letter}:\0").encode_utf16().collect();
    let target = wide(root);
    unsafe { DefineDosDeviceW(DEFINE_DOS_DEVICE_FLAGS(0), PCWSTR(device.as_ptr()), PCWSTR(target.as_ptr())) }?;
    /* Explorer shows it as "Kimono (K:)", with Kimono's mark. */
    set_drive_label(letter);
    Ok(())
}

pub fn unmap_letter(root: &Path, letter: char) {
    let device: Vec<u16> = format!("{letter}:\0").encode_utf16().collect();
    let target = wide(root);
    unsafe { let _ = DefineDosDeviceW(DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE | DDD_RAW_TARGET_PATH, PCWSTR(device.as_ptr()), PCWSTR(target.as_ptr())); }
}

/// The first free letter from K on.
pub fn free_letter() -> Option<char> {
    let used = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    ('K'..='Z').chain('D'..'K').find(|c| used & (1 << (*c as u32 - 'A' as u32)) == 0)
}

fn set_drive_label(letter: char) {
    let exe = std::env::current_exe().unwrap_or_default();
    let base = format!(r"Software\Classes\Applications\Explorer.exe\Drives\{letter}");
    let _ = registry_set(&format!(r"{base}\DefaultLabel"), "Kimono");
    let _ = registry_set(&format!(r"{base}\DefaultIcon"), &format!("{},0", exe.display()));
}

fn registry_set(key: &str, value: &str) -> windows::core::Result<()> {
    use windows::Win32::System::Registry::*;
    let key_w: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut opened = HKEY::default();
        RegCreateKeyExW(HKEY_CURRENT_USER, PCWSTR(key_w.as_ptr()), None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut opened, None).ok()?;
        let bytes = std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2);
        let result = RegSetValueExW(opened, PCWSTR::null(), None, REG_SZ, Some(bytes)).ok();
        let _ = RegCloseKey(opened);
        result
    }
}

// ---- The provider ----

/// Answers the platform's calls for the Kimono folder. One per process.
pub struct Provider {
    engine: Arc<Engine>,
    runtime: tokio::runtime::Handle,
    key: Mutex<Option<CF_CONNECTION_KEY>>,
    cancelled: Mutex<HashSet<i64>>,
    /// Paths here that changed, waiting for things to settle before going up.
    pending: Mutex<HashMap<PathBuf, Instant>>,
    stop: AtomicBool,
}

static PROVIDER: OnceLock<Arc<Provider>> = OnceLock::new();

impl Provider {
    /// Connects to the platform, takes over the engine's disk work, and starts watching for changes here.
    pub fn start(engine: Arc<Engine>, runtime: tokio::runtime::Handle) -> windows::core::Result<Arc<Self>> {
        let provider = PROVIDER.get_or_init(|| Arc::new(Self {
            engine: engine.clone(), runtime, key: Mutex::new(None), cancelled: Default::default(), pending: Default::default(), stop: AtomicBool::new(false),
        })).clone();
        let table = [
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_FETCH_DATA, Callback: Some(on_fetch) },
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA, Callback: Some(on_cancel) },
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NOTIFY_DELETE, Callback: Some(on_delete) },
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NOTIFY_RENAME, Callback: Some(on_rename) },
            CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NONE, Callback: None },
        ];
        let root = wide(&engine.root);
        let key = unsafe { CfConnectSyncRoot(PCWSTR(root.as_ptr()), table.as_ptr(), None, CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH) }?;
        *provider.key.lock() = Some(key);
        provider.stop.store(false, Ordering::Relaxed);
        engine.set_backend(Arc::new(Disk));
        provider.clone().watch();
        provider.clone().sweep();
        Ok(provider)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(key) = self.key.lock().take() { unsafe { let _ = CfDisconnectSyncRoot(key); } }
    }

    // ---- Changes made here ----

    fn watch(self: Arc<Self>) {
        use notify::{RecursiveMode, Watcher};
        let me = self.clone();
        let root = self.engine.root.clone();
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let Ok(mut watcher) = notify::recommended_watcher(tx) else { return };
            if watcher.watch(&root, RecursiveMode::Recursive).is_err() { return; }
            for event in rx {
                if me.stop.load(Ordering::Relaxed) { break; }
                let Ok(event) = event else { continue };
                if matches!(event.kind, notify::EventKind::Remove(_) | notify::EventKind::Access(_)) { continue; }
                let mut pending = me.pending.lock();
                for path in event.paths { pending.insert(path, Instant::now()); }
            }
        });
        let me = self.clone();
        self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(750)).await;
                if me.stop.load(Ordering::Relaxed) { break; }
                /* What's been quiet for two seconds: done being written, most likely. */
                let ready: Vec<PathBuf> = {
                    let mut pending = me.pending.lock();
                    let ready: Vec<PathBuf> = pending.iter().filter(|(_, at)| at.elapsed() > Duration::from_secs(2)).map(|(p, _)| p.clone()).collect();
                    for p in &ready { pending.remove(p); }
                    ready
                };
                let mut ready = ready;
                /* Folders before what's in them. */
                ready.sort_by_key(|p| p.components().count());
                for path in ready {
                    if let Err(e) = me.settle(&path).await { tracing::warn!("{}: {e}", path.display()); }
                }
            }
        });
    }

    /// At start: anything made or edited here while Kimono wasn't running goes up now.
    fn sweep(self: Arc<Self>) {
        let me = self.clone();
        std::thread::spawn(move || {
            let mut stack = vec![me.engine.root.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                for e in entries.flatten() {
                    let path = e.path();
                    match kind(&path) {
                        Ok(Kind::Placeholder { in_sync: true, .. }) => { if path.is_dir() { stack.push(path); } }
                        Ok(_) => { me.pending.lock().insert(path, Instant::now() - Duration::from_secs(5)); }
                        Err(_) => {}
                    }
                }
            }
        });
    }

    fn ignored(path: &Path) -> bool {
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        name.starts_with("~$") || name.ends_with(".tmp") || name.ends_with(".crdownload") || name.ends_with(".partial") || kimono_core::scan::is_clutter(&name)
    }

    /// Brings one changed path here up to Kimono.
    async fn settle(&self, path: &Path) -> Result<(), kimono_core::Problem> {
        if !path.exists() || Self::ignored(path) || path == self.engine.root { return Ok(()); }
        let attributes = attributes(path);
        match kind(path).map_err(kimono_core::Problem::from)? {
            Kind::New => {
                let entry = self.engine.added(path).await?;
                let path = path.to_path_buf();
                let id = entry.id.clone();
                tokio::task::spawn_blocking(move || convert(&path, &id)).await.ok();
                if entry.folder {
                    /* What came in with it (a folder moved in from elsewhere) goes up too. */
                    if let Ok(children) = std::fs::read_dir(&path_of(&self.engine, &entry)) {
                        let mut pending = self.pending.lock();
                        for c in children.flatten() { pending.insert(c.path(), Instant::now() - Duration::from_secs(5)); }
                    }
                }
            }
            Kind::Placeholder { in_sync: false, .. } if path.is_dir() => mark_in_sync(path),
            Kind::Placeholder { in_sync: false, .. } if on_disk(path) => {
                let (entry, conflict) = self.engine.edited(path).await?;
                let path = path.to_path_buf();
                let root = self.engine.root.clone();
                tokio::task::spawn_blocking(move || {
                    if conflict {
                        /* Ours is the copy now; the other edit comes back under the original name. */
                        let to = path.with_file_name(&entry.name);
                        if std::fs::rename(&path, &to).is_ok() { rebind(&to, &entry); }
                    } else {
                        rebind(&path, &entry);
                    }
                    let _ = root;
                }).await.ok();
            }
            Kind::Placeholder { pinned, .. } => {
                let path = path.to_path_buf();
                tokio::task::spawn_blocking(move || {
                    if pinned || attributes & ATTRIBUTE_PINNED != 0 { hydrate(&path) }
                    else if attributes & ATTRIBUTE_UNPINNED != 0 { dehydrate(&path) }
                }).await.ok();
            }
        }
        Ok(())
    }
}

fn path_of(engine: &Engine, entry: &Entry) -> PathBuf {
    engine.root.join(engine.mirror.lock().path(&entry.id).unwrap_or_default())
}

fn convert(path: &Path, id: &str) {
    let Ok(handle) = Handle::change(path) else { return };
    let flags = if path.is_dir() { CF_CONVERT_FLAG_MARK_IN_SYNC } else { CF_CONVERT_FLAG_MARK_IN_SYNC };
    let r = unsafe { CfConvertToPlaceholder(handle.win32(), Some(id.as_ptr() as *const c_void), id.len() as u32, flags, None, None) };
    if let Err(e) = r { tracing::warn!("couldn't make {} a Kimono file: {}", path.display(), e.message()); }
}

/// After an upload: the placeholder takes the entry's identity and size, in step again.
fn rebind(path: &Path, entry: &Entry) {
    let Ok(handle) = Handle::change(path) else { return };
    let meta = metadata(entry);
    let r = unsafe {
        CfUpdatePlaceholder(handle.win32(), Some(&meta), Some(entry.id.as_ptr() as *const c_void), entry.id.len() as u32, None,
            CF_UPDATE_FLAG_MARK_IN_SYNC | CF_UPDATE_FLAG_PASSTHROUGH_FS_METADATA, None, None)
    };
    if let Err(e) = r { tracing::warn!("couldn't update {}: {}", path.display(), e.message()); }
}

fn mark_in_sync(path: &Path) {
    if let Ok(handle) = Handle::change(path) { unsafe { let _ = CfSetInSyncState(handle.win32(), CF_IN_SYNC_STATE_IN_SYNC, CF_SET_IN_SYNC_FLAG_NONE, None); } }
}

fn hydrate(path: &Path) {
    if path.is_dir() {
        for e in std::fs::read_dir(path).into_iter().flatten().flatten() { hydrate(&e.path()); }
        return;
    }
    if on_disk(path) { return; }
    /* Exclusive would stop the platform reading it in; a plain handle is enough to ask. */
    let Ok(handle) = Handle::look(path) else { return };
    unsafe { let _ = CfHydratePlaceholder(handle.0, 0, -1, CF_HYDRATE_FLAG_NONE, None); }
}

fn dehydrate(path: &Path) {
    if path.is_dir() {
        for e in std::fs::read_dir(path).into_iter().flatten().flatten() { dehydrate(&e.path()); }
        return;
    }
    if !on_disk(path) || !matches!(kind(path), Ok(Kind::Placeholder { in_sync: true, .. })) { return; }
    let Ok(handle) = Handle::change(path) else { return };
    unsafe { let _ = CfDehydratePlaceholder(handle.win32(), 0, -1, CF_DEHYDRATE_FLAG_NONE, None); }
}

// ---- The engine's disk work ----

struct Disk;

impl Backend for Disk {
    fn apply(&self, root: &Path, action: &Action) -> std::io::Result<()> {
        match action {
            Action::Create { path, entry } => create(&root.join(path), entry),
            Action::Move { from, to, .. } => {
                let (from, to) = (root.join(from), root.join(to));
                if !from.exists() || from == to { return Ok(()); }
                if let Some(dir) = to.parent() { std::fs::create_dir_all(dir)?; }
                std::fs::rename(from, to)
            }
            Action::Replace { path, entry } => {
                let path = root.join(path);
                /* Edited here too and not sent yet: that edit goes up and is kept beside this one. */
                if !matches!(kind(&path)?, Kind::Placeholder { in_sync: true, .. }) { return Ok(()); }
                let pinned = attributes(&path) & ATTRIBUTE_PINNED != 0;
                let handle = Handle::change(&path)?;
                let meta = metadata(entry);
                unsafe { CfUpdatePlaceholder(handle.win32(), Some(&meta), None, 0, None, CF_UPDATE_FLAG_MARK_IN_SYNC | CF_UPDATE_FLAG_DEHYDRATE, None, None) }.map_err(io)?;
                drop(handle);
                if pinned { hydrate(&path); }
                Ok(())
            }
            Action::Remove { path, folder } => {
                let path = root.join(path);
                if !path.exists() { return Ok(()); }
                if *folder { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }
            }
        }
    }
}

fn create(path: &Path, entry: &Entry) -> std::io::Result<()> {
    if path.exists() {
        /* Already here from before: make sure it says what Kimono says. */
        if let Ok(Kind::Placeholder { in_sync: true, .. }) = kind(path) {
            if !entry.folder {
                let handle = Handle::change(path)?;
                let meta = metadata(entry);
                unsafe { let _ = CfUpdatePlaceholder(handle.win32(), Some(&meta), Some(entry.id.as_ptr() as *const c_void), entry.id.len() as u32, None, CF_UPDATE_FLAG_MARK_IN_SYNC, None, None); }
            }
        }
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    std::fs::create_dir_all(parent)?;
    let name: Vec<u16> = path.file_name().unwrap().encode_wide().chain(std::iter::once(0)).collect();
    let mut flags = CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;
    if entry.folder { flags |= CF_PLACEHOLDER_CREATE_FLAG_DISABLE_ON_DEMAND_POPULATION; }
    let mut info = [CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(name.as_ptr()),
        FsMetadata: metadata(entry),
        FileIdentity: entry.id.as_ptr() as *const c_void,
        FileIdentityLength: entry.id.len() as u32,
        Flags: flags,
        ..Default::default()
    }];
    let base = wide(parent);
    unsafe { CfCreatePlaceholders(PCWSTR(base.as_ptr()), &mut info, CF_CREATE_FLAG_NONE, None) }.map_err(io)?;
    info[0].Result.ok().map_err(io)
}

// ---- The platform's calls ----

/// What a callback needs once it has returned: the info it pointed at is gone by then.
#[derive(Clone, Copy)]
struct Keys {
    connection: CF_CONNECTION_KEY,
    transfer: i64,
    request: i64,
}

unsafe impl Send for Keys {}

struct Call {
    keys: Keys,
    path: PathBuf,
    identity: String,
    file_size: i64,
    ours: bool,
}

unsafe fn call(info: *const CF_CALLBACK_INFO) -> Call {
    let info = unsafe { &*info };
    let volume = from_wide(info.VolumeDosName);
    let path = PathBuf::from(format!("{volume}{}", from_wide(info.NormalizedPath)));
    let identity = if info.FileIdentity.is_null() { String::new() } else {
        let bytes = unsafe { std::slice::from_raw_parts(info.FileIdentity as *const u8, info.FileIdentityLength as usize) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    let ours = !info.ProcessInfo.is_null() && unsafe { (*info.ProcessInfo).ProcessId } == std::process::id();
    Call { keys: Keys { connection: info.ConnectionKey, transfer: info.TransferKey, request: info.RequestKey }, path, identity, file_size: info.FileSize, ours }
}

fn operation(keys: Keys, kind: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
    CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: kind,
        ConnectionKey: keys.connection,
        TransferKey: keys.transfer,
        RequestKey: keys.request,
        ..Default::default()
    }
}

fn transfer(keys: Keys, status: NTSTATUS, buffer: *const c_void, offset: i64, length: i64) -> windows::core::Result<()> {
    let info = operation(keys, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (8 + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_0>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 { TransferData: CF_OPERATION_PARAMETERS_0_0 { Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE, CompletionStatus: status, Buffer: buffer, Offset: offset, Length: length } },
    };
    unsafe { CfExecute(&info, &mut params) }
}

fn ack_delete(keys: Keys, status: NTSTATUS) {
    let info = operation(keys, CF_OPERATION_TYPE_ACK_DELETE);
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (8 + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_7>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 { AckDelete: CF_OPERATION_PARAMETERS_0_7 { Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE, CompletionStatus: status } },
    };
    unsafe { let _ = CfExecute(&info, &mut params); }
}

fn ack_rename(keys: Keys, status: NTSTATUS) {
    let info = operation(keys, CF_OPERATION_TYPE_ACK_RENAME);
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: (8 + std::mem::size_of::<CF_OPERATION_PARAMETERS_0_6>()) as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 { AckRename: CF_OPERATION_PARAMETERS_0_6 { Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE, CompletionStatus: status } },
    };
    unsafe { let _ = CfExecute(&info, &mut params); }
}

fn refusal(p: &kimono_core::Problem) -> NTSTATUS {
    if p.offline() { STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE } else if p.status == 403 || p.status == 400 { STATUS_ACCESS_DENIED } else { STATUS_UNSUCCESSFUL }
}

unsafe extern "system" fn on_fetch(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    let Some(provider) = PROVIDER.get().cloned() else { return };
    let call = unsafe { call(info) };
    let fetch = unsafe { (*params).Anonymous.FetchData };
    let start = fetch.RequiredFileOffset;
    /* To the end of the 4 KB block the platform asked about, or the end of the file. */
    let end = ((fetch.RequiredFileOffset + fetch.RequiredLength + 4095) / 4096 * 4096).min(call.file_size);
    provider.runtime.clone().spawn(async move {
        let keys = call.keys;
        let result = provider.fetch(&call, start, end).await;
        if result.is_err() {
            let _ = transfer(keys, STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE, std::ptr::null(), start, end - start);
        }
        provider.cancelled.lock().remove(&keys.transfer);
    });
}

impl Provider {
    async fn fetch(&self, call: &Call, start: i64, end: i64) -> Result<(), ()> {
        if end <= start { return Ok(()); }
        let response = self.engine.client.content(&call.identity, Some((start as u64, Some(end as u64 - 1)))).await.map_err(|_| ())?;
        let mut body = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::with_capacity(CHUNK + 65536);
        let mut at = start;
        loop {
            let next = body.next().await;
            if let Some(chunk) = &next {
                let chunk = chunk.as_ref().map_err(|_| ())?;
                buffer.extend_from_slice(chunk);
            }
            let done = next.is_none();
            /* Whole 4 KB blocks as they come; the last piece may end anywhere, at the end of the file. */
            let ready = if done { buffer.len() } else { buffer.len() / CHUNK * CHUNK };
            if ready > 0 {
                if self.cancelled.lock().contains(&call.keys.transfer) { return Ok(()); }
                transfer(call.keys, STATUS_SUCCESS, buffer.as_ptr() as *const c_void, at, ready as i64).map_err(|_| ())?;
                at += ready as i64;
                buffer.drain(..ready);
                unsafe { let _ = CfReportProviderProgress(call.keys.connection, call.keys.transfer, call.file_size, at); }
            }
            if done { break; }
        }
        if at < end { return Err(()); }
        Ok(())
    }
}

unsafe extern "system" fn on_cancel(info: *const CF_CALLBACK_INFO, _params: *const CF_CALLBACK_PARAMETERS) {
    let Some(provider) = PROVIDER.get() else { return };
    provider.cancelled.lock().insert(unsafe { (*info).TransferKey });
}

unsafe extern "system" fn on_delete(info: *const CF_CALLBACK_INFO, _params: *const CF_CALLBACK_PARAMETERS) {
    let Some(provider) = PROVIDER.get().cloned() else { return };
    let call = unsafe { call(info) };
    /* Kimono's own work (following a change from elsewhere): already done there. */
    if call.ours { return ack_delete(call.keys, STATUS_SUCCESS); }
    provider.runtime.clone().spawn(async move {
        let status = match provider.engine.deleted(&call.path).await { Ok(()) => STATUS_SUCCESS, Err(p) => refusal(&p) };
        ack_delete(call.keys, status);
    });
}

unsafe extern "system" fn on_rename(info: *const CF_CALLBACK_INFO, params: *const CF_CALLBACK_PARAMETERS) {
    let Some(provider) = PROVIDER.get().cloned() else { return };
    let call = unsafe { call(info) };
    if call.ours { return ack_rename(call.keys, STATUS_SUCCESS); }
    let rename = unsafe { (*params).Anonymous.Rename };
    let volume = from_wide(unsafe { (*info).VolumeDosName });
    let target = PathBuf::from(format!("{volume}{}", from_wide(rename.TargetPath)));
    let inside = rename.Flags.0 & CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE.0 != 0;
    provider.runtime.clone().spawn(async move {
        /* Moved out of the Kimono folder: off Kimono, to its trash. */
        let result = if inside { provider.engine.moved(&call.path, &target).await } else { provider.engine.deleted(&call.path).await };
        ack_rename(call.keys, match result { Ok(()) => STATUS_SUCCESS, Err(p) => refusal(&p) });
    });
}

/// The platform's own word on what this computer runs, for the about box.
pub fn platform() -> String {
    let info = unsafe { CfGetPlatformInfo() }.unwrap_or_default();
    format!("{}.{}", info.BuildNumber, info.RevisionNumber)
}

#[allow(dead_code)]
fn os(s: &std::ffi::OsStr) -> String { std::ffi::OsString::from_wide(&s.encode_wide().collect::<Vec<_>>()).to_string_lossy().into_owned() }
