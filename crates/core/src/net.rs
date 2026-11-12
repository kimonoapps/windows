//! The connection as uploads see it, shared across the app: whether anything is waiting for a
//! network, a nudge for when the network comes back (so waiting uploads go at once, not at their
//! next try), and a pause held while on a metered connection such as a phone's hotspot.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::LazyLock;

use tokio::sync::{watch, Notify};

static WAITING: AtomicUsize = AtomicUsize::new(0);
static BACK: LazyLock<Notify> = LazyLock::new(Notify::new);
static PAUSED: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);
static METERED: AtomicBool = AtomicBool::new(false);

/// Whether anything is waiting for a connection right now.
pub fn waiting() -> bool {
    WAITING.load(Ordering::Relaxed) > 0
}

/// The network is back: everything waiting for it tries again now.
pub fn back() {
    BACK.notify_waiters();
}

/// Holds uploads back (on a metered connection) or lets them go.
pub fn set_paused(paused: bool) {
    PAUSED.send_if_modified(|p| { let changed = *p != paused; *p = paused; changed });
    if !paused { back(); }
}

pub fn paused() -> bool {
    *PAUSED.borrow()
}

/// Remembers whether the connection is metered, for showing; pausing is the app's choice.
pub fn set_metered(metered: bool) {
    METERED.store(metered, Ordering::Relaxed);
}

pub fn metered() -> bool {
    METERED.load(Ordering::Relaxed)
}

/// Waits while uploads are paused. Call before sending anything.
pub async fn go_ahead() {
    let mut rx = PAUSED.subscribe();
    while *rx.borrow_and_update() {
        WAITING.fetch_add(1, Ordering::Relaxed);
        let _ = rx.changed().await;
        WAITING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Sleeps for `pause`, or less if the network comes back meanwhile; counted as waiting.
pub(crate) async fn wait_for_network(pause: std::time::Duration) {
    WAITING.fetch_add(1, Ordering::Relaxed);
    let notified = BACK.notified();
    tokio::select! {
        _ = tokio::time::sleep(pause) => {}
        _ = notified => {}
    }
    WAITING.fetch_sub(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn the_network_coming_back_ends_the_wait() {
        let started = Instant::now();
        let waiter = tokio::spawn(wait_for_network(Duration::from_secs(60)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(waiting());
        back();
        waiter.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_pause_holds_uploads_until_lifted() {
        set_paused(true);
        let held = tokio::spawn(go_ahead());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!held.is_finished());
        set_paused(false);
        tokio::time::timeout(Duration::from_secs(2), held).await.unwrap().unwrap();
    }
}
