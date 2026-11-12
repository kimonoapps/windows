//! Watching the connection for uploads: when the network comes back, waiting uploads go at once;
//! on a metered connection (a phone's hotspot, mobile data, roaming), they pause until back on
//! ordinary Wi-Fi or a cable, unless that's switched off.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kimono_core::net;
use serde::{Deserialize, Serialize};

static PAUSE_ON_METERED: AtomicBool = AtomicBool::new(true);

#[derive(Serialize, Deserialize)]
struct Settings {
    pause_on_metered: bool,
}

fn settings_file(data: &std::path::Path) -> PathBuf {
    data.join("settings.json")
}

pub fn pause_on_metered() -> bool {
    PAUSE_ON_METERED.load(Ordering::Relaxed)
}

pub fn set_pause_on_metered(data: &std::path::Path, on: bool) {
    PAUSE_ON_METERED.store(on, Ordering::Relaxed);
    let _ = std::fs::write(settings_file(data), serde_json::to_vec(&Settings { pause_on_metered: on }).unwrap_or_default());
    net::set_paused(on && net::metered());
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Seen {
    connected: bool,
    metered: bool,
}

#[cfg(windows)]
fn look(_data: &std::path::Path) -> Option<Seen> {
    use windows::Win32::NetworkManagement::IpHelper::GetNetworkConnectivityHint;
    use windows::Win32::Networking::WinSock::*;
    let mut hint = NL_NETWORK_CONNECTIVITY_HINT::default();
    let result = unsafe { GetNetworkConnectivityHint(&mut hint) };
    if result.0 != 0 {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("network: can't read the connection's state (error {})", result.0));
        return None;
    }
    let connected = !matches!(hint.ConnectivityLevel, NetworkConnectivityLevelHintNone | NetworkConnectivityLevelHintUnknown);
    let mut metered = matches!(hint.ConnectivityCost, NetworkConnectivityCostHintFixed | NetworkConnectivityCostHintVariable)
        || hint.Roaming || hint.OverDataLimit;
    /* Debug builds: a `pretend-metered` file in the data folder acts as a metered connection, for testing. */
    #[cfg(debug_assertions)]
    { metered |= _data.join("pretend-metered").exists(); }
    Some(Seen { connected, metered })
}

#[cfg(not(windows))]
fn look(_data: &std::path::Path) -> Option<Seen> {
    /* No cheap, reliable signal here yet: uploads still wait out drops on their own. */
    None
}

/// Starts watching, for as long as the app runs.
pub fn watch(data: PathBuf) {
    if let Some(s) = std::fs::read(settings_file(&data)).ok().and_then(|b| serde_json::from_slice::<Settings>(&b).ok()) {
        PAUSE_ON_METERED.store(s.pause_on_metered, Ordering::Relaxed);
    }
    std::thread::Builder::new().name("kimono-network".into()).spawn(move || {
        let mut last: Option<Seen> = None;
        loop {
            if let Some(now) = look(&data) {
                if last != Some(now) {
                    tracing::info!("network: {}{}", if now.connected { "connected" } else { "offline" }, if now.metered { ", metered" } else { "" });
                    net::set_metered(now.metered);
                    net::set_paused(now.metered && pause_on_metered());
                    /* Back (or onto another network): whatever is waiting tries now. */
                    if now.connected && last.is_none_or(|l| !l.connected || l.metered != now.metered) { net::back(); }
                    last = Some(now);
                }
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }).ok();
}

/// A few words for the window and tray when uploads aren't moving because of the connection.
pub fn holdup() -> Option<&'static str> {
    if net::paused() { Some("Paused on mobile data") }
    else if net::waiting() { Some("Waiting for a connection") }
    else { None }
}
