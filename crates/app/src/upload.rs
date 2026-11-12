//! Uploading a lot, fast: drop folders (or pick them), see what's in them and how big, like
//! TreeSize, choose where on Kimono they go, and send them all at once.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::*;
use gpui_kit::prelude::*;
use kimono_core::bulk::{BulkUpload, Options};
use kimono_core::format;
use kimono_core::scan::{self, FoundFolder, ScanProgress};
use kimono_core::tus::FileResume;
use kimono_core::Node;

use crate::state::Kimono;
use crate::theme::{self, Palette, Tone};

enum Found {
    Scanning(Arc<ScanProgress>),
    Folder(Arc<FoundFolder>),
    File(u64),
}

struct Picked {
    path: PathBuf,
    found: Found,
}

#[derive(Clone)]
struct Place {
    id: String,
    /// "Family / Photos / 2024"
    shown: String,
}

struct Picker {
    trail: Vec<Place>,
    folders: Option<Result<Vec<Node>, String>>,
}

/// An upload that was started and hasn't finished: kept on disk so a restart carries on with it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Job {
    destination: String,
    shown: String,
    paths: Vec<PathBuf>,
}

fn job_file(cx: &App) -> PathBuf {
    cx.global::<Kimono>().paths.data.join("upload-job.json")
}

/// Whether an upload was left unfinished (Kimono quit, the computer restarted).
pub fn unfinished(cx: &App) -> bool {
    job_file(cx).exists()
}

struct Running {
    upload: Arc<BulkUpload>,
    task: tokio::task::JoinHandle<()>,
    started: Instant,
    samples: VecDeque<(Instant, i64)>,
    finished: Option<Instant>,
}

pub struct Upload {
    picked: Vec<Picked>,
    open: HashSet<PathBuf>,
    destination: Option<Place>,
    picker: Option<Picker>,
    running: Option<Running>,
    ticking: bool,
    /// Carrying on with an unfinished upload: it starts by itself once everything is measured.
    resuming: bool,
}

impl Upload {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut me = Self { picked: vec![], open: HashSet::new(), destination: None, picker: None, running: None, ticking: false, resuming: false };
        let job: Option<Job> = std::fs::read(job_file(cx)).ok().and_then(|b| serde_json::from_slice(&b).ok());
        match job {
            Some(job) => {
                me.destination = Some(Place { id: job.destination, shown: job.shown });
                me.resuming = true;
                me.add(job.paths, window, cx);
            }
            None => me.choose_default_destination(window, cx),
        }
        let handed: Vec<PathBuf> = std::mem::take(&mut *cx.global::<Kimono>().handed.lock());
        me.add(handed, window, cx);
        me
    }

    fn choose_default_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kimono = cx.global::<Kimono>();
        let client = kimono.client.clone();
        let work = kimono.runtime.spawn(async move { client.spaces().await });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(spaces)) = work.await else { return };
            let _ = this.update(cx, |this, cx| {
                if this.destination.is_none() {
                    if let Some(s) = spaces.iter().find(|s| s.can_edit()) {
                        this.destination = Some(Place { id: s.root_id.clone(), shown: s.name.clone() });
                        cx.notify();
                    }
                }
            });
        }).detach();
    }

    /// Takes folders and files to upload, and starts measuring the folders.
    pub fn add(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        for path in paths {
            tracing::info!("to upload: {}", path.display());
            if self.picked.iter().any(|p| p.path == path) { continue; }
            if path.is_dir() {
                let progress = Arc::new(ScanProgress::default());
                self.picked.push(Picked { path: path.clone(), found: Found::Scanning(progress.clone()) });
                let scanning = cx.background_executor().spawn({ let path = path.clone(); async move { scan::scan(&path, &progress) } });
                cx.spawn_in(window, async move |this, cx| {
                    let found = Arc::new(scanning.await);
                    let _ = this.update_in(cx, |this, window, cx| {
                        if let Some(p) = this.picked.iter_mut().find(|p| p.path == path) { p.found = Found::Folder(found); }
                        this.open_biggest();
                        if this.resuming && !this.picked.iter().any(|p| matches!(p.found, Found::Scanning(_))) {
                            this.resuming = false;
                            this.start(window, cx);
                        }
                        cx.notify();
                    });
                }).detach();
            } else if let Ok(meta) = std::fs::metadata(&path) {
                self.picked.push(Picked { path, found: Found::File(meta.len()) });
            }
        }
        self.tick(window, cx);
        cx.notify();
    }

    /// Opens the biggest folder one level, so the first look already shows where the space goes.
    fn open_biggest(&mut self) {
        if self.picked.iter().any(|p| matches!(p.found, Found::Scanning(_))) || !self.open.is_empty() { return; }
        if let Some(f) = self.picked.iter().filter_map(|p| match &p.found { Found::Folder(f) => Some(f), _ => None }).max_by_key(|f| f.size) {
            self.open.insert(f.path.clone());
        }
    }

    /// While something counts up, the window redraws a few times a second.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ticking { return; }
        self.ticking = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let busy = this.update(cx, |this, cx| {
                    if let Some(run) = &mut this.running {
                        let now = Instant::now();
                        run.samples.push_back((now, run.upload.stats.done_bytes.load(Ordering::Relaxed)));
                        while run.samples.len() > 2 && now - run.samples.front().unwrap().0 > Duration::from_secs(6) { run.samples.pop_front(); }
                        if run.finished.is_none() && run.task.is_finished() { run.finished = Some(now); }
                    }
                    let scanning = this.picked.iter().any(|p| matches!(p.found, Found::Scanning(_)));
                    let uploading = this.running.as_ref().is_some_and(|r| r.finished.is_none());
                    cx.notify();
                    if !(scanning || uploading) { this.ticking = false; }
                    scanning || uploading
                }).unwrap_or(false);
                if !busy { break; }
            }
        }).detach();
    }

    fn pick(&mut self, folders: bool, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions { files: !folders, directories: folders, multiple: true, prompt: Some("Upload".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await {
                let _ = this.update_in(cx, |this, window, cx| this.add(paths, window, cx));
            }
        }).detach();
    }

    fn totals(&self) -> (u64, u64, bool) {
        let (mut files, mut bytes, mut scanning) = (0, 0, false);
        for p in &self.picked {
            match &p.found {
                Found::Scanning(s) => { scanning = true; files += s.files.load(Ordering::Relaxed); bytes += s.bytes.load(Ordering::Relaxed); }
                Found::Folder(f) => { files += f.file_count; bytes += f.size; }
                Found::File(size) => { files += 1; bytes += size; }
            }
        }
        (files, bytes, scanning)
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(destination) = self.destination.clone() else { return };
        let kimono = cx.global::<Kimono>();
        let folders: Vec<FoundFolder> = self.picked.iter().filter_map(|p| match &p.found { Found::Folder(f) => Some((**f).clone()), _ => None }).collect();
        let files: Vec<PathBuf> = self.picked.iter().filter(|p| matches!(p.found, Found::File(_))).map(|p| p.path.clone()).collect();
        let resume = Arc::new(FileResume::open(kimono.paths.data.join("uploads.json")));
        /* Kept until it's all up, so a restart carries on with it. */
        let job = Job { destination: destination.id.clone(), shown: destination.shown.clone(), paths: self.picked.iter().map(|p| p.path.clone()).collect() };
        let file = job_file(cx);
        let _ = std::fs::write(&file, serde_json::to_vec(&job).unwrap_or_default());
        let upload = BulkUpload::new(kimono.client.clone(), destination.id, folders, files, Options::default(), Some(resume));
        let task = kimono.runtime.spawn({
            let upload = upload.clone();
            async move {
                /* Stay awake until it's done: people start a big upload and walk away. */
                let _awake = crate::awake::hold();
                upload.run().await;
                if upload.failures().is_empty() { let _ = std::fs::remove_file(&file); }
            }
        });
        self.running = Some(Running { upload, task, started: Instant::now(), samples: VecDeque::new(), finished: None });
        self.tick(window, cx);
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = &mut self.running { run.task.abort(); run.finished = Some(Instant::now()); }
        /* Stopped on purpose: not something to carry on with after a restart. */
        let _ = std::fs::remove_file(job_file(cx));
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.picked.clear();
        self.open.clear();
        self.running = None;
        cx.notify();
    }

    // ---- Where it goes ----

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = Some(Picker { trail: vec![], folders: None });
        self.load_picker(window, cx);
    }

    fn load_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else { return };
        picker.folders = None;
        let kimono = cx.global::<Kimono>();
        let client = kimono.client.clone();
        let at = picker.trail.last().map(|p| p.id.clone());
        let work = kimono.runtime.spawn(async move {
            match at {
                /* The top: your spaces you can add to, as folders. */
                None => client.spaces().await.map(|spaces| spaces.into_iter().filter(|s| s.can_edit()).map(|s| Node {
                    id: s.root_id, space_id: s.id, parent_id: None, kind: "folder".into(), name: s.name, size: 0, mime: None,
                    created_at: 0., updated_at: 0., version: 0, content_version: 0, role: Some(s.role), shared_with_you: None,
                }).collect::<Vec<_>>()),
                Some(id) => client.folder(&id).await.map(|l| l.children.into_iter().filter(|c| c.is_folder()).collect()),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await.unwrap_or_else(|_| Err(kimono_core::Problem::unreachable())).map_err(|p| p.message);
            let _ = this.update(cx, |this, cx| {
                if let Some(picker) = &mut this.picker { picker.folders = Some(result); }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    // ---- Drawing ----

    fn render_picker(&self, picker: &Picker, p: &Palette, cx: &mut Context<Self>) -> Div {
        let crumbs = std::iter::once("Kimono".to_string()).chain(picker.trail.iter().map(|t| t.shown.rsplit(" / ").next().unwrap_or("").to_string()));
        let mut crumb_row = div().flex().flex_wrap().items_center().gap(px(6.)).text_size(px(13.));
        for (i, name) in crumbs.enumerate() {
            if i > 0 { crumb_row = crumb_row.child(div().text_color(p.outline).child("/")); }
            crumb_row = crumb_row.child(div().id(("crumb", i)).cursor_pointer().text_color(p.on_surface_variant).hover(|s| s.text_color(p.primary))
                .child(name).on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(picker) = &mut this.picker { picker.trail.truncate(i); }
                    this.load_picker(window, cx);
                })));
        }
        let list: Div = match &picker.folders {
            None => div().p(px(16.)).text_color(p.on_surface_variant).child("Looking…"),
            Some(Err(m)) => div().p(px(16.)).text_color(p.error).child(m.clone()),
            Some(Ok(folders)) if folders.is_empty() => div().p(px(16.)).text_color(p.on_surface_variant).child(if picker.trail.is_empty() { "No spaces you can add to." } else { "No folders here." }),
            Some(Ok(folders)) => div().flex().flex_col().children(folders.iter().enumerate().map(|(i, f)| {
                let (id, name) = (f.id.clone(), f.name.clone());
                div().id(("folder", i)).flex().items_center().gap(px(10.)).px(px(14.)).py(px(9.)).rounded(px(6.)).cursor_pointer()
                    .hover(|s| s.bg(p.container_high))
                    .child(div().size(px(14.)).rounded(px(2.)).border_1().border_color(p.outline))
                    .child(div().text_size(px(14.)).child(name.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(picker) = &mut this.picker {
                            let shown = match picker.trail.last() { Some(t) => format!("{} / {}", t.shown, name), None => name.clone() };
                            picker.trail.push(Place { id: id.clone(), shown });
                        }
                        this.load_picker(window, cx);
                    }))
            })),
        };
        let here = picker.trail.last().cloned();
        div().absolute().inset_0().flex().items_center().justify_center().bg(p.on_surface.opacity(0.28)).child(
            div().id("picker").occlude().w(px(480.)).h(px(440.)).flex().flex_col().gap(px(14.)).p(px(22.)).rounded(px(16.)).bg(p.background)
                .border_1().border_color(p.outline_variant)
                .child(div().font_family(theme::SERIF).text_size(px(20.)).text_color(p.ink).child("Upload to"))
                .child(crumb_row)
                .child(div().id("folders").flex_1().overflow_y_scroll().rounded(px(10.)).bg(p.surface).p(px(4.)).child(list))
                .child(div().flex().justify_end().gap(px(10.))
                    .child(theme::seal("cancel-pick", "Cancel", Tone::Quiet, true, p).on_click(cx.listener(|this, _, _, cx| { this.picker = None; cx.notify(); })))
                    .child(theme::seal("here", "Upload here", Tone::Inked, here.is_some(), p).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(place) = here.clone() { this.destination = Some(place); this.picker = None; cx.notify(); }
                    })))),
        )
    }

    fn render_tree(&self, p: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let size_of = |x: &Picked| match &x.found { Found::Folder(f) => f.size, Found::File(s) => *s, Found::Scanning(s) => s.bytes.load(Ordering::Relaxed) };
        /* Biggest first, as TreeSize lists them; each root measured against the biggest, and its share of everything. */
        let mut order: Vec<&Picked> = self.picked.iter().collect();
        order.sort_by(|a, b| size_of(b).cmp(&size_of(a)));
        let biggest = order.first().map(|x| size_of(x)).unwrap_or(1).max(1);
        let total: u64 = order.iter().map(|x| size_of(x)).sum::<u64>().max(1);
        let mut rows: Vec<AnyElement> = vec![header(p).into_any_element()];
        let mut n = 0usize;
        for picked in order {
            let name = picked.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| picked.path.to_string_lossy().into_owned());
            n += 1;
            match &picked.found {
                Found::Scanning(s) => rows.push(row(n, 0, &name, None, format!("{}…", format::bytes(s.bytes.load(Ordering::Relaxed))), None,
                    format::count(s.files.load(Ordering::Relaxed), "file", "files"), 0., true, p, cx).into_any_element()),
                Found::File(size) => rows.push(row(n, 0, &name, None, format::bytes(*size), Some(*size as f32 / total as f32), String::new(),
                    *size as f32 / biggest as f32, false, p, cx).into_any_element()),
                Found::Folder(f) => self.folder_rows(f, 0, biggest, total, &mut rows, &mut n, p, cx),
            }
        }
        div().id("tree").flex_1().overflow_y_scroll().flex().flex_col().children(rows)
    }

    /// A folder's row and, when open, its folders (biggest first) and then its loose files as one row.
    /// `measure` is what its bar is measured against; `share_of` what its percentage is of.
    #[allow(clippy::too_many_arguments)]
    fn folder_rows(&self, f: &FoundFolder, depth: usize, measure: u64, share_of: u64, rows: &mut Vec<AnyElement>, n: &mut usize, p: &Palette, cx: &mut Context<Self>) {
        if rows.len() > 2000 { return; }
        let open = self.open.contains(&f.path);
        let mut detail = format::count(f.file_count, "file", "files");
        if f.unreadable > 0 { detail.push_str(&format!(" · {} unreadable", f.unreadable)); }
        *n += 1;
        rows.push(row(*n, depth, &f.name, Some((open, f.path.clone())), format::bytes(f.size), Some(f.size as f32 / share_of.max(1) as f32),
            detail, f.size as f32 / measure.max(1) as f32, false, p, cx).into_any_element());
        if !open { return; }
        /* Inside, everything is measured against this folder: the bars read as shares of it. */
        let inside = f.size.max(1);
        for sub in f.folders.iter().take(300) { self.folder_rows(sub, depth + 1, inside, inside, rows, n, p, cx); }
        if !f.files.is_empty() {
            let bytes: u64 = f.files.iter().map(|x| x.size).sum();
            *n += 1;
            rows.push(row(*n, depth + 1, &format::count(f.files.len() as u64, "file here", "files here"), None, format::bytes(bytes),
                Some(bytes as f32 / inside as f32), String::new(), bytes as f32 / inside as f32, false, p, cx).into_any_element());
        }
    }

    fn render_progress(&self, run: &Running, p: &Palette, cx: &mut Context<Self>) -> Div {
        let stats = &run.upload.stats;
        let total = stats.total_bytes.load(Ordering::Relaxed).max(1);
        let done = stats.done_bytes.load(Ordering::Relaxed).max(0) as u64;
        let (files_done, files_total) = (stats.done_files.load(Ordering::Relaxed), stats.total_files.load(Ordering::Relaxed));
        let speed = match (run.samples.front(), run.samples.back()) {
            (Some(a), Some(b)) if b.0 > a.0 => (b.1 - a.1) as f64 / (b.0 - a.0).as_secs_f64(),
            _ => 0.,
        };
        let left = if speed > 1. { format::duration(Duration::from_secs_f64((total - done.min(total)) as f64 / speed)) } else { String::new() };
        let failed = stats.failed_files.load(Ordering::Relaxed);
        let skipped = stats.skipped_files.load(Ordering::Relaxed);
        let finished = run.finished.is_some();
        let headline = if !finished { match crate::network::holdup() {
                Some(why) => format!("{why} · {} of {}", format::bytes(done), format::bytes(total)),
                None => format!("{} of {}", format::bytes(done), format::bytes(total)),
            } }
            else if failed > 0 { format!("Done, but {} didn't upload", format::count(failed, "file", "files")) }
            else { format!("All {} on Kimono", format::count(files_total, "file", "files")) };
        let mut line = vec![format::count(files_done, "file", "files") + &format!(" of {files_total}")];
        if !finished && speed > 1. { line.push(format::speed(speed)); }
        if !finished && !left.is_empty() { line.push(format!("{left} left")); }
        if skipped > 0 { line.push(format!("{skipped} already there")); }
        if finished { line.push(format!("took {}", format::duration(run.finished.unwrap() - run.started))); }

        let active = run.upload.active();
        let failures = run.upload.failures();
        div().flex().flex_col().gap(px(10.)).p(px(18.)).rounded(px(14.)).bg(p.surface).border_1().border_color(p.outline_variant)
            .child(div().flex().justify_between().items_end()
                .child(div().text_size(px(18.)).font_weight(FontWeight::MEDIUM).child(headline))
                .child(div().text_size(px(13.)).text_color(p.on_surface_variant).child(line.join("  ·  "))))
            .child(theme::bar(done as f32 / total as f32, p))
            .children((!finished).then(|| div().flex().flex_col().gap(px(4.)).children(active.iter().take(6).map(|a| {
                let fraction = if a.size == 0 { 1. } else { a.sent.load(Ordering::Relaxed).max(0) as f32 / a.size as f32 };
                div().flex().items_center().gap(px(10.)).text_size(px(12.)).text_color(p.on_surface_variant)
                    .child(div().w(px(260.)).truncate().child(a.name.clone()))
                    .child(div().flex_1().child(theme::bar(fraction, p).h(px(3.))))
                    .child(div().w(px(70.)).text_right().child(format::bytes(a.size)))
            }))))
            .children((!failures.is_empty()).then(|| div().flex().flex_col().gap(px(2.)).text_size(px(12.)).text_color(p.error)
                .children(failures.iter().take(5).map(|f| div().truncate().child(format!("{} — {}", f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), f.reason))))))
            .child(div().flex().justify_end().gap(px(10.)).children(if finished {
                vec![theme::seal("again", "Upload more", Tone::Quiet, true, p).on_click(cx.listener(|this, _, _, cx| this.clear(cx))).into_any_element()]
            } else {
                vec![theme::seal("stop", "Stop", Tone::Quiet, true, p).on_click(cx.listener(|this, _, _, cx| this.stop(cx))).into_any_element()]
            }))
    }
}

const NAME_W: f32 = 260.;
const SIZE_W: f32 = 84.;
const SHARE_W: f32 = 52.;
const FILES_W: f32 = 130.;

fn header(p: &Palette) -> Div {
    let cell = |w: f32, text: &'static str| div().w(px(w)).text_right().child(text);
    div().flex().items_center().gap(px(10.)).h(px(28.)).pl(px(32.)).pr(px(12.)).text_size(px(12.)).text_color(p.on_surface_variant)
        .border_b_1().border_color(p.outline_variant).mb(px(4.))
        .child(div().w(px(NAME_W - 22.)).child("Name"))
        .child(div().flex_1())
        .child(cell(SIZE_W, "Size"))
        .child(cell(SHARE_W, "%"))
        .child(cell(FILES_W, "Files"))
}

#[allow(clippy::too_many_arguments)]
fn row(n: usize, depth: usize, name: &str, toggle: Option<(bool, PathBuf)>, size: String, share: Option<f32>, detail: String, fraction: f32, scanning: bool, p: &Palette, cx: &mut Context<Upload>) -> Stateful<Div> {
    let chevron = match &toggle { Some((true, _)) => "▾", Some((false, _)) => "▸", None => "" };
    let indent = depth as f32 * 18.;
    let share = share.map(|s| if s >= 0.995 { "100%".to_string() } else if s >= 0.1 { format!("{:.0}%", s * 100.) } else if s > 0. { format!("{:.1}%", s * 100.) } else { "0%".into() }).unwrap_or_default();
    let mut r = div().id(("row", n)).flex().items_center().gap(px(10.)).h(px(32.)).pl(px(10.)).pr(px(12.)).rounded(px(6.))
        .hover(|s| s.bg(p.container))
        .child(div().w(px(NAME_W)).flex().items_center().gap(px(8.)).pl(px(indent))
            .child(div().w(px(12.)).text_color(p.outline).child(chevron))
            .child(div().flex_1().truncate().text_size(px(14.)).text_color(if toggle.is_some() || depth == 0 { p.on_surface } else { p.on_surface_variant }).child(name.to_string())))
        .child(div().flex_1().child(if scanning { theme::bar(0., p).opacity(0.5) } else { theme::bar(fraction, p).h(px(6.)) }))
        .child(div().w(px(SIZE_W)).text_right().text_size(px(13.)).child(size))
        .child(div().w(px(SHARE_W)).text_right().text_size(px(12.)).text_color(p.on_surface_variant).child(share))
        .child(div().w(px(FILES_W)).text_right().text_size(px(12.)).text_color(p.on_surface_variant).child(detail));
    if let Some((_, path)) = toggle {
        r = r.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
            if !this.open.remove(&path) { this.open.insert(path.clone()); }
            cx.notify();
        }));
    }
    r
}

impl Render for Upload {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(window);
        let (files, bytes, scanning) = self.totals();
        let empty = self.picked.is_empty();
        let uploading = self.running.is_some();
        let destination = self.destination.as_ref().map(|d| d.shown.clone()).unwrap_or_else(|| "Choose a place".into());

        let drop_zone = div().id("drop").flex().flex_col().items_center().justify_center().gap(px(16.)).flex_1().rounded(px(18.))
            .border_2().border_dashed().border_color(p.outline_variant).bg(p.surface)
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.border_color(p.primary).bg(p.primary_container.opacity(0.35)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.add(paths.paths().to_vec(), window, cx)))
            .child(div().font_family(theme::SERIF).text_size(px(22.)).text_color(p.ink).child("Drop folders here"))
            .child(div().text_size(px(14.)).text_color(p.on_surface_variant).child("As many as you like. Whatever's already on Kimono is skipped."))
            .child(div().flex().gap(px(10.))
                .child(theme::seal("folders", "Choose folders", Tone::Inked, true, &p).on_click(cx.listener(|this, _, window, cx| this.pick(true, window, cx))))
                .child(theme::seal("files", "Choose files", Tone::Quiet, true, &p).on_click(cx.listener(|this, _, window, cx| this.pick(false, window, cx)))));

        let body: AnyElement = if empty { drop_zone.into_any_element() } else {
            div().flex().flex_col().flex_1().gap(px(12.)).min_h_0()
                .child(div().id("list").flex().flex_col().flex_1().min_h_0().rounded(px(14.)).bg(p.surface).p(px(6.))
                    .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(p.primary_container.opacity(0.35)))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.add(paths.paths().to_vec(), window, cx)))
                    .child(self.render_tree(&p, cx)))
                .children(self.running.as_ref().map(|run| self.render_progress(run, &p, cx)))
                .into_any_element()
        };

        let footer = (!empty && !uploading).then(|| div().flex().items_center().gap(px(10.))
            .child(div().flex_1().text_size(px(14.)).text_color(p.on_surface_variant)
                .child(format!("{}  ·  {}{}", format::count(files, "file", "files"), format::bytes(bytes), if scanning { "  ·  still counting" } else { "" })))
            .child(theme::seal("add", "Add more", Tone::Quiet, true, &p).on_click(cx.listener(|this, _, window, cx| this.pick(true, window, cx))))
            .child(theme::seal("clear", "Clear", Tone::Quiet, true, &p).on_click(cx.listener(|this, _, _, cx| this.clear(cx))))
            .child(theme::seal("upload", "Upload", Tone::Inked, !scanning && self.destination.is_some(), &p)
                .on_click(cx.listener(|this, _, window, cx| this.start(window, cx)))));

        div().size_full().relative().flex().flex_col().gap(px(16.)).p(px(24.)).bg(p.background).text_color(p.on_surface).font_family(theme::SANS)
            .child(div().flex().items_center().gap(px(12.))
                .child(div().flex_1().font_family(theme::SERIF).text_size(px(26.)).text_color(p.ink).child("Upload"))
                .child(div().text_size(px(13.)).text_color(p.on_surface_variant).child("to"))
                .child(div().id("destination").flex().items_center().gap(px(8.)).px(px(12.)).h(px(34.)).rounded(px(8.)).bg(p.container_high)
                    .when(!uploading, |d| d.cursor_pointer().hover(|s| s.bg(p.container_highest)).on_click(cx.listener(|this, _, window, cx| this.open_picker(window, cx))))
                    .child(div().text_size(px(14.)).max_w(px(320.)).truncate().child(destination))
                    .child(div().text_color(p.outline).child("▾"))))
            .child(body)
            .children(footer)
            .children(self.picker.as_ref().map(|picker| self.render_picker(picker, &p, cx)))
    }
}
