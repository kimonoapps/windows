//! Keeps the computer from going to sleep while something is going up, so a backup started
//! before walking away finishes. The screen may still turn off, and closing a laptop's lid still
//! puts it to sleep; the moment nothing is sending, the computer is free to sleep again.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static HOLDERS: AtomicUsize = AtomicUsize::new(0);
static KEEPER: Mutex<Option<Keeper>> = Mutex::new(None);

/// Held while sending; dropping it lets the computer sleep again once nothing else holds one.
pub struct Awake(());

pub fn hold() -> Awake {
    if HOLDERS.fetch_add(1, Ordering::SeqCst) == 0 {
        if let Ok(mut k) = KEEPER.lock() { *k = Keeper::start(); }
    }
    Awake(())
}

impl Drop for Awake {
    fn drop(&mut self) {
        if HOLDERS.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Ok(mut k) = KEEPER.lock() { k.take(); }
        }
    }
}

#[cfg(windows)]
struct Keeper(std::sync::mpsc::Sender<()>);

#[cfg(windows)]
impl Keeper {
    /// Windows ties the request to a thread, so one thread holds it until told to let go.
    fn start() -> Option<Self> {
        use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new().name("kimono-awake".into()).spawn(move || {
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            let _ = rx.recv();
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }).ok()?;
        Some(Self(tx))
    }
}

#[cfg(windows)]
impl Drop for Keeper {
    fn drop(&mut self) { let _ = self.0.send(()); }
}

/// macOS and Linux: the system's own tool holds the request for as long as it runs.
#[cfg(not(windows))]
struct Keeper(std::process::Child);

#[cfg(not(windows))]
impl Keeper {
    fn start() -> Option<Self> {
        #[cfg(target_os = "macos")]
        let child = std::process::Command::new("caffeinate").args(["-i", "-w", &std::process::id().to_string()]).spawn().ok()?;
        #[cfg(not(target_os = "macos"))]
        let child = std::process::Command::new("systemd-inhibit")
            .args(["--what=sleep", "--who=Kimono", "--why=Backing up to Kimono", "--mode=block", "sleep", "infinity"]).spawn().ok()?;
        Some(Self(child))
    }
}

#[cfg(not(windows))]
impl Drop for Keeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
