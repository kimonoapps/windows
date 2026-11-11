#![cfg_attr(windows, windows_subsystem = "windows")]
//! Kimono for your computer: lives in the tray, keeps Kimono Drive as a folder (a drive in
//! Explorer on Windows), and uploads whole folder trees fast. Updates itself.

mod backup;
mod backup_view;
mod signin;
mod state;
mod system;
mod theme;
mod update;
mod upload;

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::*;
use kimono_drive::Status;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use state::Kimono;

fn main() {
    /* Velopack's install, update and uninstall steps run here and exit before anything else. */
    let app = velopack::VelopackApp::build().on_first_run(|_| {
        system::set_start_at_login(true);
        system::set_explorer_menu(true);
    });
    /* Installing and updating set these up straight away; the app may not be started by the installer. */
    #[cfg(windows)]
    let app = app
        .on_after_install_fast_callback(|_| { system::set_start_at_login(true); system::set_explorer_menu(true); })
        .on_after_update_fast_callback(|_| { system::refresh_start_at_login(); system::set_explorer_menu(true); });
    #[cfg(windows)]
    let app = app.on_before_uninstall_fast_callback(|_| {
        system::set_start_at_login(false);
        system::set_explorer_menu(false);
        let _ = std::fs::remove_dir_all(dirs::home_dir().unwrap_or_default().join("Kimono"));
        let _ = std::fs::remove_dir_all(dirs::data_local_dir().unwrap_or_default().join("Kimono Data"));
    });
    let mut app = app;
    app.run();

    let paths = state::Paths::new();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths.data.join("kimono.log")).ok();
    if let Some(log) = log {
        tracing_subscriber::fmt().with_writer(std::sync::Mutex::new(log)).with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "kimono=info,kimono_drive=info,kimono_core=info".into())).init();
    }

    /* A windowed app has nowhere to print a crash: it goes in the log. */
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("Kimono stopped: {info}\n{backtrace}");
    }));
    tracing::info!("Kimono {} starting", kimono_core::client::VERSION);

    let args: Vec<String> = std::env::args().skip(1).collect();
    let background = args.iter().any(|a| a == "--background");
    let handed: Vec<PathBuf> = args.iter().filter(|a| !a.starts_with("--")).map(PathBuf::from).collect();
    let Some(arrivals) = system::single_instance(&paths.data, &handed) else { return };

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("kimono").build().expect("a runtime");
    let handle = runtime.handle().clone();

    /* Debug builds only: sign in from KIMONO_TEST_SERVER/USERNAME/PASSWORD, for testing on a dev machine, and leave. */
    #[cfg(debug_assertions)]
    if args.iter().any(|a| a == "--test-sign-in") {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let client = kimono_core::Client::new(std::sync::Arc::new(state::Keychain));
        let result = runtime.block_on(client.sign_in(&env("KIMONO_TEST_SERVER"), &env("KIMONO_TEST_USERNAME"), &env("KIMONO_TEST_PASSWORD")));
        tracing::info!("test sign-in: {result:?}");
        println!("{result:?}");
        std::process::exit(if matches!(result, Ok(kimono_core::SignInNext::Done)) { 0 } else { 1 });
    }

    gpui_kit::application().run(move |cx: &mut App| {
        tracing::info!("window system up");
        gpui_kit::init(cx);
        theme::load_fonts(cx);
        /* Closing a window leaves Kimono in the tray. */
        cx.set_quit_mode(QuitMode::Explicit);
        let kimono = Kimono::new(handle.clone());
        kimono.handed.lock().extend(handed.clone());
        let signed_in = kimono.client.is_signed_in();
        cx.set_global(kimono);

        let tray = Tray::new();
        tracing::info!("tray up; signed in: {signed_in}");
        if signed_in {
            cx.global::<Kimono>().start_drive();
            if !handed.is_empty() { open_upload(cx); } else if !background { open_backup(cx); }
        } else {
            open_sign_in(cx);
        }
        update::start(cx);

        /* The tray's menu, and paths handed over by later launches, a few times a second. */
        cx.spawn(async move |cx| {
            let _runtime = runtime;
            let tray = tray;
            loop {
                cx.background_executor().timer(Duration::from_millis(120)).await;
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    let _ = cx.update(|cx| tray.clicked(&event.id, cx));
                }
                while let Ok(paths) = arrivals.try_recv() {
                    let _ = cx.update(|cx| {
                        cx.global::<Kimono>().handed.lock().extend(paths);
                        open_upload(cx);
                    });
                }
                let _ = cx.update(|cx| tray.refresh(cx));
            }
        }).detach();
    });
}

fn window_options(cx: &App, width: f32, height: f32) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(width), px(height)), cx))),
        titlebar: Some(TitlebarOptions { title: Some("Kimono".into()), ..Default::default() }),
        window_min_size: Some(size(px(420.), px(420.))),
        app_id: Some("kimono".into()),
        ..Default::default()
    }
}

/// Brings an open window of this kind forward, if there is one.
fn focus_existing<V: 'static>(cx: &mut App) -> bool {
    for window in cx.windows() {
        if let Some(handle) = window.downcast::<gpui_kit::base::Root>() {
            let found = handle.update(cx, |root, window, _| {
                if root.view().clone().downcast::<V>().is_ok() { window.activate_window(); true } else { false }
            }).unwrap_or(false);
            if found { return true; }
        }
    }
    false
}

pub fn open_sign_in(cx: &mut App) {
    if focus_existing::<signin::SignIn>(cx) { return; }
    let options = window_options(cx, 440., 540.);
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| signin::SignIn::new(window, cx, |window, cx| {
            cx.global::<Kimono>().start_drive();
            window.remove_window();
            open_backup(cx);
        }))
    });
    if let Err(e) = opened { tracing::error!("couldn't open the sign-in window: {e:?}"); }
    /* Started in the background (at sign-in to the computer, from Explorer's menu): come forward. */
    cx.activate(true);
}

/// The main window: the drive, with the folders kept backed up ticked.
pub fn open_backup(cx: &mut App) {
    if !cx.global::<Kimono>().client.is_signed_in() { return open_sign_in(cx); }
    if focus_existing::<backup_view::BackupView>(cx) { return; }
    let options = window_options(cx, 980., 700.);
    if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| backup_view::BackupView::new(window, cx))) {
        tracing::error!("couldn't open the backup window: {e:?}");
    }
    cx.activate(true);
}

pub fn open_upload(cx: &mut App) {
    if !cx.global::<Kimono>().client.is_signed_in() { return open_sign_in(cx); }
    if focus_existing::<upload::Upload>(cx) {
        /* Hand anything new to the open window. */
        let handed: Vec<PathBuf> = std::mem::take(&mut *cx.global::<Kimono>().handed.lock());
        if !handed.is_empty() {
            for window in cx.windows() {
                if let Some(handle) = window.downcast::<gpui_kit::base::Root>() {
                    let _ = handle.update(cx, |root, window, cx| {
                        if let Ok(view) = root.view().clone().downcast::<upload::Upload>() {
                            view.update(cx, |u, cx| u.add(handed.clone(), window, cx));
                        }
                    });
                }
            }
        }
        return;
    }
    let options = window_options(cx, 940., 660.);
    if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| upload::Upload::new(window, cx))) {
        tracing::error!("couldn't open the upload window: {e:?}");
    }
    cx.activate(true);
}

struct Tray {
    _icon: Option<TrayIcon>,
    status: MenuItem,
    backup: MenuItem,
    open: MenuItem,
    open_folder: MenuItem,
    upload: MenuItem,
    login: CheckMenuItem,
    account: MenuItem,
    quit: MenuItem,
}

impl Tray {
    fn new() -> Self {
        let status = MenuItem::new("Starting…", false, None);
        let open_folder = MenuItem::new("Open Kimono folder", true, None);
        let upload = MenuItem::new("Upload…", true, None);
        let backup = MenuItem::new("", false, None);
        let open = MenuItem::new("Open Kimono", true, None);
        let login = CheckMenuItem::new("Start with this computer", true, system::starts_at_login(), None);
        let account = MenuItem::new("Sign out", true, None);
        let quit = MenuItem::new("Quit Kimono", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&status, &backup, &PredefinedMenuItem::separator(), &open, &open_folder, &upload, &PredefinedMenuItem::separator(), &login, &account, &quit]);
        /* Linux trays need GTK's loop, which this app doesn't run: there the window is the way in. */
        let icon = if cfg!(target_os = "linux") { None } else {
            let image = image::load_from_memory(include_bytes!("../assets/tray.png")).ok().map(|i| i.into_rgba8());
            let icon = image.and_then(|i| { let (w, h) = i.dimensions(); tray_icon::Icon::from_rgba(i.into_raw(), w, h).ok() });
            TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Kimono").with_icon_opt(icon).build().ok()
        };
        Self { _icon: icon, status, backup, open, open_folder, upload, login, account, quit }
    }

    fn clicked(&self, id: &tray_icon::menu::MenuId, cx: &mut App) {
        if id == self.open_folder.id() { cx.global::<Kimono>().open_folder(); }
        else if id == self.upload.id() { open_upload(cx); }
        else if id == self.open.id() { open_backup(cx); }
        else if id == self.login.id() { system::set_start_at_login(self.login.is_checked()); }
        else if id == self.account.id() {
            if cx.global::<Kimono>().client.is_signed_in() {
                let kimono = cx.global::<Kimono>();
                kimono.stop_drive(true);
                let client = kimono.client.clone();
                kimono.runtime.spawn(async move { client.sign_out().await });
                for w in cx.windows() { let _ = w.update(cx, |_, window, _| window.remove_window()); }
            }
            open_sign_in(cx);
        }
        else if id == self.quit.id() {
            cx.global::<Kimono>().stop_drive(false);
            cx.quit();
        }
    }

    fn refresh(&self, cx: &mut App) {
        let kimono = cx.global::<Kimono>();
        let signed_in = kimono.client.is_signed_in();
        let text = if !signed_in { "Not signed in".to_string() } else if cfg!(windows) {
            match kimono.status() {
                Status::Starting => "Starting…".into(),
                Status::UpToDate => "Up to date".into(),
                Status::Syncing { left } if left > 0 => format!("Bringing in {left} changes…"),
                Status::Syncing { .. } => "Looking for changes…".into(),
                Status::Offline => "Can't reach Kimono".into(),
                Status::SignedOut => "Sign in again".into(),
                Status::Failed(m) => m,
            }
        } else { "Signed in".into() };
        self.status.set_text(text);
        self.backup.set_text(match kimono.backup.status() {
            backup::Status::Off => "Backups: none chosen".to_string(),
            backup::Status::Idle { failed: 0, last_done: Some(_) } => "Backups up to date".into(),
            backup::Status::Idle { failed, .. } if failed > 0 => format!("Backups: {failed} files couldn't go up"),
            backup::Status::Idle { .. } => "Backups waiting to start".into(),
            backup::Status::Looking { .. } => "Looking for changes to back up…".into(),
            backup::Status::Sending { done, total, .. } => format!("Backing up {} of {}", kimono_core::format::bytes(done), kimono_core::format::bytes(total)),
            backup::Status::Failed(m) => format!("Backups: {m}"),
        });
        self.account.set_text(if signed_in { "Sign out" } else { "Sign in…" });
        self.open_folder.set_enabled(signed_in && cfg!(windows));
        if let Some(update) = update::ready() { self.status.set_text(format!("Kimono {update} installs when you quit")); }
    }
}

trait TrayIconBuilderExt { fn with_icon_opt(self, icon: Option<tray_icon::Icon>) -> Self; }
impl TrayIconBuilderExt for TrayIconBuilder {
    fn with_icon_opt(self, icon: Option<tray_icon::Icon>) -> Self { match icon { Some(i) => self.with_icon(i), None => self } }
}
