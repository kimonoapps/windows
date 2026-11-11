//! The bits of each OS the app touches: opening folders, starting at sign-in, Explorer's menu,
//! and making sure only one Kimono runs.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

pub fn reveal(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

pub fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// One Kimono per person: a second launch hands its paths to the first and leaves.
/// Returns the paths that arrive from later launches, one batch per line.
pub fn single_instance(data: &Path, paths: &[PathBuf]) -> Option<std::sync::mpsc::Receiver<Vec<PathBuf>>> {
    let port_file = data.join("instance.port");
    if let Some(port) = std::fs::read_to_string(&port_file).ok().and_then(|p| p.trim().parse::<u16>().ok()) {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let line = serde_json::to_string(&paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>()).unwrap_or_default();
            let _ = writeln!(stream, "kimono {line}");
            return None;
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let _ = std::fs::write(&port_file, listener.local_addr().ok()?.port().to_string());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(stream).read_line(&mut line).is_err() { continue; }
            let Some(json) = line.trim().strip_prefix("kimono ") else { continue };
            let list: Vec<String> = serde_json::from_str(json).unwrap_or_default();
            let _ = tx.send(list.into_iter().map(PathBuf::from).collect());
        }
    });
    Some(rx)
}

/// The program to start: the installer's launcher when installed (it stays put across updates,
/// while the app itself lives in a folder the updater replaces), else this program.
pub fn launcher() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    #[cfg(windows)]
    if let Some(root) = exe.parent().filter(|p| p.file_name().is_some_and(|n| n == "current")).and_then(|p| p.parent()) {
        let stub = root.join("Kimono.exe");
        if stub.exists() { return stub; }
    }
    exe
}

/// Keeps the start-at-login entry, if there is one, pointing at the launcher.
pub fn refresh_start_at_login() {
    if starts_at_login() { set_start_at_login(true); }
}

/// Starts with the computer, quietly, in the tray.
pub fn set_start_at_login(on: bool) {
    let exe = launcher();
    #[cfg(windows)]
    {
        let value = format!("\"{}\" --background", exe.display());
        let _ = registry(r"Software\Microsoft\Windows\CurrentVersion\Run", Some("Kimono"), on.then_some(value.as_str()));
    }
    #[cfg(target_os = "macos")]
    {
        let agent = dirs::home_dir().unwrap_or_default().join("Library/LaunchAgents/dev.kimonolabs.kimono.plist");
        if on {
            let plist = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>Label</key><string>dev.kimonolabs.kimono</string>
<key>ProgramArguments</key><array><string>{}</string><string>--background</string></array>
<key>RunAtLoad</key><true/></dict></plist>"#, exe.display());
            let _ = std::fs::create_dir_all(agent.parent().unwrap());
            let _ = std::fs::write(agent, plist);
        } else {
            let _ = std::fs::remove_file(agent);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let entry = dirs::config_dir().unwrap_or_default().join("autostart/kimono.desktop");
        if on {
            let _ = std::fs::create_dir_all(entry.parent().unwrap());
            let _ = std::fs::write(entry, format!("[Desktop Entry]\nType=Application\nName=Kimono\nExec=\"{}\" --background\nX-GNOME-Autostart-enabled=true\n", exe.display()));
        } else {
            let _ = std::fs::remove_file(entry);
        }
    }
}

pub fn starts_at_login() -> bool {
    #[cfg(windows)]
    { registry_get(r"Software\Microsoft\Windows\CurrentVersion\Run", "Kimono").is_some() }
    #[cfg(target_os = "macos")]
    { dirs::home_dir().unwrap_or_default().join("Library/LaunchAgents/dev.kimonolabs.kimono.plist").exists() }
    #[cfg(target_os = "linux")]
    { dirs::config_dir().unwrap_or_default().join("autostart/kimono.desktop").exists() }
}

/// "Upload to Kimono" on folders and files in Explorer.
pub fn set_explorer_menu(on: bool) {
    #[cfg(windows)]
    {
        let exe = launcher();
        for kind in ["Directory", "*"] {
            let base = format!(r"Software\Classes\{kind}\shell\KimonoUpload");
            if on {
                let _ = registry(&base, None, Some("Upload to Kimono"));
                let _ = registry(&base, Some("Icon"), Some(&format!("{},0", exe.display())));
                let _ = registry(&base, Some("MultiSelectModel"), Some("Player"));
                let _ = registry(&format!(r"{base}\command"), None, Some(&format!("\"{}\" --upload \"%1\"", exe.display())));
            } else {
                let _ = registry_delete(&base);
            }
        }
    }
    let _ = on;
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn registry(key: &str, name: Option<&str>, value: Option<&str>) -> windows::core::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::*;
    let key_w = wide(key);
    let name_w = name.map(wide);
    let name_p = name_w.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null());
    unsafe {
        let mut opened = HKEY::default();
        RegCreateKeyExW(HKEY_CURRENT_USER, PCWSTR(key_w.as_ptr()), None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut opened, None).ok()?;
        let result = match value {
            Some(v) => {
                let v = wide(v);
                RegSetValueExW(opened, name_p, None, REG_SZ, Some(std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2))).ok()
            }
            None => RegDeleteValueW(opened, name_p).ok(),
        };
        let _ = RegCloseKey(opened);
        result
    }
}

#[cfg(windows)]
fn registry_get(key: &str, name: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::*;
    let (key_w, name_w) = (wide(key), wide(name));
    let mut buffer = vec![0u16; 2048];
    let mut size = (buffer.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key_w.as_ptr()), PCWSTR(name_w.as_ptr()), RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr() as *mut _), Some(&mut size)) };
    if r.is_err() { return None; }
    Some(String::from_utf16_lossy(&buffer[..(size as usize / 2).saturating_sub(1)]))
}

#[cfg(windows)]
fn registry_delete(key: &str) -> windows::core::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::*;
    let key_w = wide(key);
    unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key_w.as_ptr())).ok() }
}
