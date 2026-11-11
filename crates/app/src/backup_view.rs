//! The main window: your drive as TreeSize shows it (biggest first, with how much each folder
//! holds), with a box on every folder. Tick a folder and Kimono keeps it backed up.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use gpui_kit::prelude::*;
use kimono_core::format;
use kimono_core::scan::{self, FoundFolder, ScanProgress};
use kimono_core::Space;

use crate::backup::{self, Backup, Status};
use crate::state::Kimono;
use crate::theme::{self, Palette, Tone};

enum Top {
    /// Being measured; `last` is what the last scan found, shown meanwhile.
    Scanning(PathBuf, Arc<ScanProgress>, Option<Arc<FoundFolder>>),
    Done(Arc<FoundFolder>),
}

impl Top {
    fn size(&self) -> u64 {
        match self {
            Top::Scanning(_, _, Some(last)) => last.size,
            Top::Scanning(_, s, None) => s.bytes.load(Ordering::Relaxed),
            Top::Done(f) => f.size,
        }
    }
    fn path(&self) -> &PathBuf {
        match self { Top::Scanning(p, _, _) => p, Top::Done(f) => &f.path }
    }
}

/// The last scan, kept so the window opens on it at once while a fresh one runs.
fn cache_file(cx: &App) -> PathBuf {
    cx.global::<Kimono>().paths.data.join("drive-sizes.json")
}

pub struct BackupView {
    /// What's measured: the C: drive on Windows, the home folder elsewhere.
    root: PathBuf,
    tops: Vec<Top>,
    open: HashSet<PathBuf>,
    spaces: Option<Vec<Space>>,
    choosing: bool,
    ticking: bool,
}

/// Where the tree starts.
pub fn start_folder() -> PathBuf {
    #[cfg(windows)]
    { PathBuf::from(std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()) + "\\") }
    #[cfg(not(windows))]
    { dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")) }
}

fn backup(cx: &App) -> Arc<Backup> {
    cx.global::<Kimono>().backup.clone()
}

impl BackupView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let root = start_folder();
        let mut me = Self { root: root.clone(), tops: vec![], open: HashSet::new(), spaces: None, choosing: false, ticking: false };
        me.measure(window, cx);
        me.tick(window, cx);
        me
    }

    /// Measures each folder at the top separately, many at once, so sizes fill in as they're found.
    fn measure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cached: Vec<FoundFolder> = std::fs::read(cache_file(cx)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut cached: std::collections::HashMap<PathBuf, Arc<FoundFolder>> = cached.into_iter().map(|f| (f.path.clone(), Arc::new(f))).collect();
        let Ok(entries) = std::fs::read_dir(&self.root) else { return };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if !kind.is_dir() || kind.is_symlink() { continue; }
            let name = entry.file_name().to_string_lossy().into_owned();
            if scan::is_clutter(&name) || name.starts_with('$') || name.starts_with('.') { continue; }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                /* Hidden system folders (the page file's, recovery, …) aren't things anyone backs up. */
                if entry.metadata().map(|m| m.file_attributes() & 0x6 == 0x6).unwrap_or(false) { continue; }
            }
            let path = entry.path();
            let progress = Arc::new(ScanProgress::default());
            self.tops.push(Top::Scanning(path.clone(), progress.clone(), cached.remove(&path)));
            let scanning = cx.background_executor().spawn(async move { scan::scan(&path, &progress) });
            cx.spawn_in(window, async move |this, cx| {
                let found = Arc::new(scanning.await);
                let _ = this.update(cx, |this, cx| {
                    if let Some(t) = this.tops.iter_mut().find(|t| *t.path() == found.path) { *t = Top::Done(found); }
                    if this.tops.iter().all(|t| matches!(t, Top::Done(_))) { this.remember(cx); }
                    cx.notify();
                });
            }).detach();
        }
    }

    /// Keeps this scan for next time, written off the window's thread.
    fn remember(&self, cx: &mut Context<Self>) {
        let all: Vec<FoundFolder> = self.tops.iter().filter_map(|t| match t { Top::Done(f) => Some((**f).clone()), _ => None }).collect();
        let file = cache_file(cx);
        cx.background_executor().spawn(async move {
            if let Ok(bytes) = serde_json::to_vec(&all) {
                let temp = file.with_extension("tmp");
                if std::fs::write(&temp, bytes).is_ok() { let _ = std::fs::rename(temp, &file); }
            }
        }).detach();
    }

    /// Redraws while sizes count up or a backup is going, a few times a second.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ticking { return; }
        self.ticking = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() { break; }
            }
        }).detach();
    }

    fn choose_space(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.choosing = true;
        let kimono = cx.global::<Kimono>();
        let client = kimono.client.clone();
        let work = kimono.runtime.spawn(async move { client.spaces().await });
        cx.spawn_in(window, async move |this, cx| {
            let spaces = work.await.ok().and_then(|r| r.ok()).unwrap_or_default().into_iter().filter(|s| s.can_edit()).collect();
            let _ = this.update(cx, |this, cx| { this.spaces = Some(spaces); cx.notify(); });
        }).detach();
        cx.notify();
    }

    fn toggle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let backup = backup(cx);
        let on = !backup.config.lock().chosen.contains(&path);
        backup.set(&path, on);
        cx.notify();
    }

    fn rows(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut order: Vec<&Top> = self.tops.iter().collect();
        order.sort_by(|a, b| b.size().cmp(&a.size()));
        let total = order.iter().map(|t| t.size()).sum::<u64>().max(1);
        let biggest = order.first().map(|t| t.size()).unwrap_or(1).max(1);
        let chosen = backup(cx).config.lock().chosen.clone();
        let mut rows = vec![header(p).into_any_element()];
        let mut n = 0;
        for top in order {
            match top {
                Top::Scanning(_, _, Some(last)) => self.folder(last, 0, biggest, total, &chosen, &mut rows, &mut n, p, cx),
                Top::Scanning(path, s, None) => {
                    n += 1;
                    let line = Line { n, depth: 0, name: name_of(path), path: path.clone(), open: None, size: format!("{}…", format::bytes(s.bytes.load(Ordering::Relaxed))),
                        share: String::new(), files: format::count(s.files.load(Ordering::Relaxed), "file", "files"), fraction: s.bytes.load(Ordering::Relaxed) as f32 / biggest as f32, counting: true };
                    rows.push(self.line(line, &chosen, p, cx).into_any_element());
                }
                Top::Done(f) => self.folder(f, 0, biggest, total, &chosen, &mut rows, &mut n, p, cx),
            }
        }
        rows
    }

    #[allow(clippy::too_many_arguments)]
    fn folder(&self, f: &FoundFolder, depth: usize, measure: u64, share_of: u64, chosen: &[PathBuf], rows: &mut Vec<AnyElement>, n: &mut usize, p: &Palette, cx: &mut Context<Self>) {
        if rows.len() > 3000 { return; }
        let open = self.open.contains(&f.path);
        *n += 1;
        let line = Line { n: *n, depth, name: f.name.clone(), path: f.path.clone(), open: (!f.folders.is_empty()).then_some(open), size: format::bytes(f.size),
            share: share(f.size, share_of), files: if f.unreadable > 0 && f.file_count == 0 { "No access".into() } else { format::count(f.file_count, "file", "files") }, fraction: f.size as f32 / measure.max(1) as f32, counting: false };
        rows.push(self.line(line, chosen, p, cx).into_any_element());
        if !open { return; }
        let inside = f.size.max(1);
        for sub in f.folders.iter().take(400) { self.folder(sub, depth + 1, inside, inside, chosen, rows, n, p, cx); }
        /* Its own files after its folders, biggest first: each can be backed up on its own too. */
        let mut files: Vec<&kimono_core::scan::FoundFile> = f.files.iter().collect();
        files.sort_by(|a, b| b.size.cmp(&a.size));
        for file in files.iter().take(300) {
            if rows.len() > 3000 { return; }
            *n += 1;
            let line = Line { n: *n, depth: depth + 1, name: file.name.clone(), path: f.path.join(&file.name), open: None, size: format::bytes(file.size),
                share: share(file.size, inside), files: String::new(), fraction: file.size as f32 / inside as f32, counting: false };
            rows.push(self.line(line, chosen, p, cx).into_any_element());
        }
        if files.len() > 300 {
            *n += 1;
            rows.push(div().id(("more", *n)).pl(px(48. + (depth + 1) as f32 * 18.)).h(px(30.)).flex().items_center().text_size(px(12.)).text_color(p.on_surface_variant)
                .child(format!("and {} smaller files", files.len() - 300)).into_any_element());
        }
    }

    fn line(&self, l: Line, chosen: &[PathBuf], p: &Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        /* Ticked itself, or part of a folder above that's ticked. */
        let own = chosen.contains(&l.path);
        let within = !own && chosen.iter().any(|c| l.path.starts_with(c));
        let path = l.path.clone();
        let tick = div().id(("tick", l.n)).size(px(18.)).flex().items_center().justify_center().rounded(px(3.))
            .border_1().border_color(if own { p.primary } else if within { p.primary.opacity(0.4) } else { p.outline })
            .bg(if own { p.primary } else if within { p.primary.opacity(0.18) } else { p.background })
            .text_color(p.on_primary).text_size(px(12.))
            .children((own || within).then(|| "✓"))
            .when(!within, |d| d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.toggle(path.clone(), cx))));
        let chevron = match l.open { Some(true) => "▾", Some(false) => "▸", None => "" };
        let toggle_path = l.path.clone();
        let indent = l.depth as f32 * 18.;
        div().id(("line", l.n)).flex().items_center().gap(px(10.)).h(px(34.)).pl(px(10.)).pr(px(12.)).rounded(px(6.))
            .hover(|s| s.bg(p.container))
            .child(tick)
            .child(div().id(("open", l.n)).w(px(NAME_W)).flex().items_center().gap(px(8.)).pl(px(indent))
                .when(l.open.is_some(), |d| d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                    if !this.open.remove(&toggle_path) { this.open.insert(toggle_path.clone()); }
                    cx.notify();
                })))
                .child(div().w(px(12.)).text_color(p.outline).child(chevron))
                .child(div().flex_1().truncate().text_size(px(14.)).text_color(if own || within { p.primary_deep } else { p.on_surface }).child(l.name)))
            .child(div().flex_1().child(if l.counting { theme::bar(l.fraction, p).h(px(6.)).opacity(0.5) } else { theme::bar(l.fraction, p).h(px(6.)) }))
            .child(div().w(px(84.)).text_right().text_size(px(13.)).child(l.size))
            .child(div().w(px(52.)).text_right().text_size(px(12.)).text_color(p.on_surface_variant).child(l.share))
            .child(div().w(px(120.)).text_right().text_size(px(12.)).text_color(p.on_surface_variant).child(l.files))
    }

    fn status_line(&self, cx: &App) -> String {
        let backup = backup(cx);
        let config = backup.config.lock().clone();
        let count = config.chosen.len();
        let chosen = if count == 0 { "Tick what to keep backed up".to_string() } else { format::count(count as u64, "item", "items") + " backed up" };
        let state = match backup.status() {
            Status::Off => String::new(),
            Status::Idle { last_done: Some(at), failed } => {
                let ago = (chrono_now() - at).max(0) / 1000;
                let when = if ago < 60 { "just now".to_string() } else { format!("{} ago", format::duration(Duration::from_secs(ago as u64))) };
                if failed > 0 { format!("Last run {when}; {failed} files couldn't go up") } else { format!("Up to date, {when}") }
            }
            Status::Idle { .. } => "Waiting to start".into(),
            Status::Looking { folder } => format!("Looking through {folder}…"),
            Status::Sending { done, total, files_left } => format!("Backing up {} of {} · {} to go", format::bytes(done), format::bytes(total), format::count(files_left, "file", "files")),
            Status::Failed(m) => m,
        };
        if state.is_empty() { chosen } else { format!("{chosen}  ·  {state}") }
    }

    fn render_spaces(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let list: Div = match &self.spaces {
            None => div().p(px(16.)).text_color(p.on_surface_variant).child("Looking…"),
            Some(spaces) if spaces.is_empty() => div().p(px(16.)).text_color(p.on_surface_variant).child("No spaces you can add to."),
            Some(spaces) => div().flex().flex_col().children(spaces.iter().enumerate().map(|(i, s)| {
                let (root, name) = (s.root_id.clone(), s.name.clone());
                div().id(("space", i)).px(px(14.)).py(px(10.)).rounded(px(6.)).cursor_pointer().hover(|st| st.bg(p.container_high))
                    .text_size(px(14.)).child(name.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        backup(cx).set_destination(root.clone(), name.clone());
                        this.choosing = false;
                        cx.notify();
                    }))
            })),
        };
        div().absolute().inset_0().flex().items_center().justify_center().bg(p.on_surface.opacity(0.28)).child(
            div().id("spaces").occlude().w(px(420.)).flex().flex_col().gap(px(14.)).p(px(22.)).rounded(px(16.)).bg(p.background)
                .border_1().border_color(p.outline_variant)
                .child(div().font_family(theme::SERIF).text_size(px(20.)).text_color(p.ink).child("Back up to"))
                .child(div().text_size(px(13.)).text_color(p.on_surface_variant).child(format!("Into Backups / {} in the space you pick.", backup::computer_name())))
                .child(div().rounded(px(10.)).bg(p.surface).p(px(4.)).child(list))
                .child(div().flex().justify_end().child(theme::seal("close-spaces", "Cancel", Tone::Quiet, true, p)
                    .on_click(cx.listener(|this, _, _, cx| { this.choosing = false; cx.notify(); })))),
        )
    }
}

struct Line {
    n: usize,
    depth: usize,
    name: String,
    path: PathBuf,
    open: Option<bool>,
    size: String,
    share: String,
    files: String,
    fraction: f32,
    counting: bool,
}

const NAME_W: f32 = 270.;

fn name_of(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn share(part: u64, of: u64) -> String {
    let s = part as f32 / of.max(1) as f32;
    if s >= 0.995 { "100%".into() } else if s >= 0.1 { format!("{:.0}%", s * 100.) } else if s > 0. { format!("{:.1}%", s * 100.) } else { "0%".into() }
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn header(p: &Palette) -> Div {
    let cell = |w: f32, text: &'static str| div().w(px(w)).text_right().child(text);
    div().flex().items_center().gap(px(10.)).h(px(28.)).pl(px(38.)).pr(px(12.)).text_size(px(12.)).text_color(p.on_surface_variant)
        .border_b_1().border_color(p.outline_variant).mb(px(4.))
        .child(div().w(px(NAME_W)).pl(px(20.)).child("Name"))
        .child(div().flex_1())
        .child(cell(84., "Size"))
        .child(cell(52., "%"))
        .child(cell(120., "Files"))
}

impl Render for BackupView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(window);
        let rows = self.rows(&p, cx);
        let measured: u64 = self.tops.iter().map(|t| t.size()).sum();
        let counting = self.tops.iter().any(|t| matches!(t, Top::Scanning(_, _, None)));
        let space = backup(cx).config.lock().space_name.clone().unwrap_or_else(|| "Choose a space".into());
        let root_name = self.root.to_string_lossy().trim_end_matches(['\\', '/']).to_string();

        div().size_full().relative().flex().flex_col().gap(px(14.)).p(px(24.)).bg(p.background).text_color(p.on_surface).font_family(theme::SANS)
            .child(div().flex().items_center().gap(px(12.))
                .child(div().flex().flex_col().flex_1()
                    .child(div().font_family(theme::SERIF).text_size(px(26.)).text_color(p.ink).child("Back up"))
                    .child(div().text_size(px(13.)).text_color(p.on_surface_variant)
                        .child(format!("{root_name}  ·  {}{}", format::bytes(measured), if counting { " so far" } else { "" }))))
                .child(div().text_size(px(13.)).text_color(p.on_surface_variant).child("to"))
                .child(div().id("space").flex().items_center().gap(px(8.)).px(px(12.)).h(px(34.)).rounded(px(8.)).bg(p.container_high)
                    .cursor_pointer().hover(|s| s.bg(p.container_highest))
                    .on_click(cx.listener(|this, _, window, cx| this.choose_space(window, cx)))
                    .child(div().text_size(px(14.)).max_w(px(280.)).truncate().child(space))
                    .child(div().text_color(p.outline).child("▾"))))
            .child(div().id("drive").flex_1().min_h_0().overflow_y_scroll().rounded(px(14.)).bg(p.surface).p(px(6.)).flex().flex_col().children(rows))
            .child(div().flex().items_center().gap(px(10.))
                .child(div().flex_1().text_size(px(14.)).text_color(p.on_surface_variant).child(self.status_line(cx)))
                .child(theme::seal("upload-once", "Upload once…", Tone::Quiet, true, &p).on_click(cx.listener(|_, _, _, cx| crate::open_upload(cx))))
                .child(theme::seal("now", "Back up now", Tone::Inked, !backup(cx).config.lock().chosen.is_empty(), &p)
                    .on_click(cx.listener(|_, _, _, cx| backup(cx).now()))))
            .children(self.choosing.then(|| self.render_spaces(&p, cx)))
    }
}
