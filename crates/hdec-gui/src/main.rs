// No console window behind the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod drag_out;
#[cfg(target_os = "macos")]
mod mac_drag;
#[cfg(target_os = "macos")]
mod mac_open;
#[cfg_attr(target_os = "macos", allow(dead_code))] // macOS uses the native panel
mod picker;
mod theme;
#[cfg(windows)]
mod win_drag;

use eframe::egui::{self, Align, Key, Layout, Margin, RichText, Sense, vec2};
use egui_extras::{Column, TableBuilder};
use hdec_core::{CreateOptions, Ctx, Edit, Entry, Error, ExtractOptions, Format, Level, Overwrite};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};
use theme::{icon, pal};
use zeroize::Zeroizing;

/// Formats offered in the New archive dialog.
const CREATE_FORMATS: &[Format] = &[
    Format::Zip,
    Format::SevenZ,
    Format::Rar,
    Format::Tar,
    Format::TarGz,
    Format::TarXz,
    Format::TarBz2,
    Format::Gz,
    Format::Xz,
    Format::Bz2,
];

/// GZ/XZ/BZ2 compress exactly one file.
fn fits(format: Format, sources: &[PathBuf]) -> bool {
    !matches!(format, Format::Gz | Format::Xz | Format::Bz2)
        || matches!(sources, [one] if one.is_file())
}

/// Default window size.
const WINDOW: [f32; 2] = [728.0, 448.0];

fn main() -> eframe::Result {
    let mut app = App::default();
    // Absolute, so the archive's folder (Extract's default) isn't "".
    let path = std::env::args_os()
        .nth(1)
        .map(|p| std::path::absolute(&p).unwrap_or_else(|_| p.into()));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Otherwise eframe shows its own logo (Dock, taskbar, title bar).
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!(
                    "../../../packaging/macos/icon.png"
                ))
                .expect("bundled icon is a valid PNG"),
            )
            .with_inner_size(WINDOW)
            .with_min_inner_size([560.0, 340.0]),
        ..Default::default()
    };
    #[cfg(target_os = "macos")]
    mac_open::install();
    eframe::run_native(
        "HDEC",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            #[cfg(target_os = "macos")]
            mac_open::set_context(&cc.egui_ctx);
            if let Some(path) = path {
                app.open(&cc.egui_ctx, path);
            }
            Ok(Box::new(app))
        }),
    )
}

type Password = Option<Zeroizing<String>>;

#[derive(Clone)]
enum Action {
    Open(PathBuf),
    Extract(PathBuf, ExtractOptions),
    Test,
    Modify(Edit),
    /// Extract one entry into `dir` and open it with the default app.
    Preview {
        entry: String,
        dir: PathBuf,
    },
    /// Extract one encrypted entry into `dir` to check the password.
    Unlock {
        entry: String,
        dir: PathBuf,
    },
}

enum Outcome {
    Opened(PathBuf, Vec<Entry>),
    Extracted(PathBuf, Vec<String>),
    Tested,
    Created(PathBuf),
    Modified,
    Saved,
    Previewed {
        entry: String,
        file: PathBuf,
    },
    Unlocked,
    /// Dragged out into `dest` (`None`: not known, Explorer copies it on).
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
    DraggedOut {
        dest: Option<PathBuf>,
        count: usize,
        warnings: Vec<String>,
    },
}

enum Msg {
    Progress(u64, u64, String),
    Done(hdec_core::Result<Outcome>),
}

struct Job {
    rx: mpsc::Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    progress: (u64, u64, String),
    verb: &'static str,
    icon: &'static str,
    /// What it's working on, shown under the verb: an archive's file name.
    subject: String,
    started: Instant,
    /// What to retry with a password if this job reports one is needed.
    retry: Option<Action>,
    password: Password,
}

#[derive(Default)]
enum Dialog {
    #[default]
    None,
    Info(String),
    Error(String),
    /// Explorer is still copying dragged-out files; closing again quits.
    StillCopying,
    ConfirmDelete(Vec<String>),
    Rename {
        from: String,
        name: String,
    },
    NewFolder(String),
    SaveBack {
        entry: String,
        file: PathBuf,
    },
    Password {
        input: Zeroizing<String>,
        then: Action,
        wrong: bool,
        show: bool,
    },
    Extract {
        dest: String,
        subfolder: bool,
        overwrite: Overwrite,
    },
    Create(CreateForm),
    /// Edits aren't saved yet; ask before doing `Then`.
    Unsaved(Then),
}

/// What to do once unsaved edits are saved or discarded.
enum Then {
    Open(PathBuf),
    Close,
}

/// Edits go into a copy of the archive; Save writes it back.
struct Working {
    _dir: tempfile::TempDir,
    path: PathBuf,
    dirty: bool,
}

struct CreateForm {
    sources: Vec<PathBuf>,
    /// Archive file name; the format's extension is added if missing.
    name: String,
    /// Folder the archive is saved in.
    folder: String,
    format: Format,
    level: Level,
    zip_crypto: bool,
    password: Zeroizing<String>,
    confirm: Zeroizing<String>,
    show: bool,
    split: bool,
    split_mb: String,
    /// Set by the Add… button; the app opens the picker for it.
    add_more: bool,
}

impl CreateForm {
    fn dest(&self) -> PathBuf {
        let name = self.name.trim();
        let ext = format!(".{}", self.format.extension());
        let file = if name.to_lowercase().ends_with(&ext) {
            name.to_owned()
        } else {
            format!("{name}{ext}")
        };
        Path::new(self.folder.trim()).join(file)
    }
}

/// What picked files and folders are for.
#[derive(Clone, Copy)]
enum PickFor {
    New,
    Add,
    Contents,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SortKey {
    #[default]
    Name,
    Size,
    Packed,
    Ratio,
    Modified,
}

/// What toolbar buttons, context menus, shortcuts and drag & drop ask for.
enum Command {
    /// Folder → show it; file → open it with the default app.
    Open(String),
    Extract,
    Rename,
    Delete,
    NewFolder,
    Add,
    /// Move entries into a folder ("" = root).
    Move(Vec<String>, String),
}

/// Drag & drop payload: entry paths being moved inside the archive, or out
/// of the window (`drag_out`).
struct Dragged(Vec<String>);

/// Jobs that start outside the UI thread (extracting dragged-out entries).
struct JobQueue {
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Job>,
}

impl Default for JobQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }
}

#[derive(Default)]
struct App {
    archive: Option<PathBuf>,
    format: &'static str,
    entries: Vec<Entry>,
    total_size: u64,
    any_encrypted: bool,
    /// Folder → subfolders (full paths), including folders that only exist implicitly.
    dirs: BTreeMap<String, Vec<String>>,
    expanded: BTreeSet<String>,
    /// Folder inside the archive being shown; "" = root.
    cwd: String,
    filter: String,
    sort: SortKey,
    descending: bool,
    rows: Vec<Entry>,
    selected: BTreeSet<String>,
    /// Row index shift-click ranges start from.
    anchor: Option<usize>,
    modifiable: bool,
    /// Password that worked for the open archive.
    password: Password,
    job: Option<Job>,
    dialog: Dialog,
    status: String,
    /// Where entries opened with the default app are extracted; removed on exit.
    preview_dir: Option<tempfile::TempDir>,
    /// Where dragged-out entries are extracted (Windows); removed on exit.
    drag_dir: Option<tempfile::TempDir>,
    /// Numbers the subfolders of the two above.
    previews: u32,
    drag_jobs: JobQueue,
    /// An OS drag took over the mouse: egui never sees the button go up.
    release_pointer: bool,
    /// Files opened with the default app, watched for edits to save back.
    opened: Vec<OpenedFile>,
    /// Why the open archive can't be edited, if it can't.
    read_only: Option<&'static str>,
    /// In-app files-and-folders picker (Windows; macOS uses the native one).
    picker: Option<(picker::Picker, PickFor)>,
    working: Option<Working>,
    /// Runs after a successful save.
    after_save: Option<Then>,
    /// The user chose to quit; let the close request through.
    closing: bool,
    /// Told that quitting stops a drop target's copy; the next close quits.
    warned_copying: bool,
}

struct OpenedFile {
    entry: String,
    file: PathBuf,
    modified: Option<SystemTime>,
}

impl App {
    fn run(&mut self, ctx: &egui::Context, action: Action, password: Password) {
        let mut seed = None;
        if matches!(action, Action::Modify(_)) {
            if self.working.is_none() {
                let original = self.archive.as_deref().unwrap();
                match tempfile::Builder::new().prefix("hdec-edit").tempdir() {
                    Ok(dir) => {
                        let path = dir.path().join(original.file_name().unwrap());
                        self.working = Some(Working {
                            _dir: dir,
                            path,
                            dirty: false,
                        });
                    }
                    Err(e) => {
                        self.dialog = Dialog::Error(format!("Couldn't make a working copy: {e}"));
                        return;
                    }
                }
            }
            let work = &self.working.as_ref().unwrap().path;
            if !work.exists() {
                seed = Some((self.archive.clone().unwrap(), work.clone()));
            }
        }
        // Everything reads the working copy when there is one: that's what's on screen.
        let archive = self.reading();
        let (verb, icon) = match &action {
            Action::Open(_) => ("Opening", icon::FOLDER_OPEN),
            Action::Extract(..) => ("Extracting", icon::EXPORT),
            Action::Test => ("Testing", icon::CHECK_CIRCLE),
            Action::Modify(_) => ("Updating", icon::PENCIL_SIMPLE),
            Action::Preview { .. } => ("Opening", icon::FILE),
            Action::Unlock { .. } => ("Unlocking", icon::LOCK_SIMPLE),
        };
        let retry = Some(action.clone());
        self.spawn(ctx, verb, icon, retry, password, move |c| match action {
            Action::Open(path) => {
                let entries = hdec_core::list(&path, c)?;
                Ok(Outcome::Opened(path, entries))
            }
            Action::Extract(dest, opts) => {
                let warnings = hdec_core::extract(archive.as_deref().unwrap(), &dest, &opts, c)?;
                Ok(Outcome::Extracted(dest, warnings))
            }
            Action::Test => {
                hdec_core::test(archive.as_deref().unwrap(), c).map(|_| Outcome::Tested)
            }
            Action::Modify(edit) => {
                if let Some((original, work)) = seed {
                    std::fs::copy(original, &work).inspect_err(|_| {
                        let _ = std::fs::remove_file(&work);
                    })?;
                }
                hdec_core::modify(archive.as_deref().unwrap(), &edit, c).map(|_| Outcome::Modified)
            }
            Action::Preview { entry, dir } => {
                let opts = ExtractOptions {
                    only: Some(vec![entry.clone()]),
                    overwrite: Overwrite::Replace,
                    ..Default::default()
                };
                hdec_core::extract(archive.as_deref().unwrap(), &dir, &opts, c)?;
                let file = hdec_core::safe_path::join(&dir, &entry)?;
                Ok(Outcome::Previewed { entry, file })
            }
            Action::Unlock { entry, dir } => {
                let opts = ExtractOptions {
                    only: Some(vec![entry]),
                    overwrite: Overwrite::Replace,
                    ..Default::default()
                };
                hdec_core::extract(archive.as_deref().unwrap(), &dir, &opts, c)?;
                Ok(Outcome::Unlocked)
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn(
        &mut self,
        ctx: &egui::Context,
        verb: &'static str,
        icon: &'static str,
        retry: Option<Action>,
        password: Password,
        work: impl FnOnce(&Ctx) -> hdec_core::Result<Outcome> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (thread_cancel, thread_password, egui_ctx) =
            (cancel.clone(), password.clone(), ctx.clone());
        std::thread::spawn(move || {
            let (progress_tx, progress_ctx) = (tx.clone(), egui_ctx.clone());
            let c = Ctx {
                password: thread_password.as_deref().map(String::as_str),
                cancel: thread_cancel,
                progress: Arc::new(move |done, total, name: &str| {
                    let _ = progress_tx.send(Msg::Progress(done, total, name.to_owned()));
                    progress_ctx.request_repaint();
                }),
            };
            let _ = tx.send(Msg::Done(work(&c)));
            egui_ctx.request_repaint();
        });
        self.job = Some(Job {
            rx,
            cancel,
            progress: (0, 0, String::new()),
            verb,
            icon,
            subject: self.archive.as_deref().map(file_name).unwrap_or_default(),
            started: Instant::now(),
            retry,
            password,
        });
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.job else { return };
        let mut done = None;
        for msg in job.rx.try_iter() {
            match msg {
                Msg::Progress(d, t, n) => job.progress = (d, t, n),
                Msg::Done(r) => done = Some(r),
            }
        }
        let Some(result) = done else { return };
        let job = self.job.take().unwrap();
        if result.is_err() && self.working.as_ref().is_some_and(|w| !w.dirty) {
            // Nothing saved-worthy in it, and a failed first edit may have left it half-copied.
            self.working = None;
        }
        match result {
            Ok(outcome) => {
                if job.password.is_some() {
                    self.password = job.password;
                }
                self.finish(ctx, outcome);
            }
            Err(e @ (Error::PasswordRequired | Error::WrongPassword)) if job.retry.is_some() => {
                let wrong = matches!(e, Error::WrongPassword);
                self.dialog = Dialog::Password {
                    input: Default::default(),
                    then: job.retry.unwrap(),
                    wrong,
                    show: false,
                };
            }
            Err(Error::Cancelled) => {
                self.after_save = None;
                self.status = "Cancelled".into();
            }
            Err(e) => {
                self.after_save = None;
                self.dialog = Dialog::Error(e.to_string());
            }
        }
    }

    fn finish(&mut self, ctx: &egui::Context, outcome: Outcome) {
        match outcome {
            Outcome::Opened(path, entries) => {
                // The working copy stands in for the archive it was copied from.
                let path = match &self.working {
                    Some(w) if w.path == path => self.archive.clone().unwrap(),
                    _ => path,
                };
                self.format = hdec_core::detect(&path).map(Format::name).unwrap_or("?");
                let reopened = self.archive.as_ref() == Some(&path);
                self.modifiable = hdec_core::modifiable(&path);
                self.read_only = (!self.modifiable).then(|| read_only_reason(&path, self.format));
                self.total_size = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
                self.any_encrypted = entries.iter().any(|e| e.encrypted);
                self.dirs = dir_tree(&entries);
                self.archive = Some(path);
                self.update_title(ctx);
                self.entries = entries;
                if !reopened {
                    self.status.clear();
                    self.expanded.clear();
                    self.filter.clear();
                    self.opened.clear();
                }
                let keep = reopened && self.dirs.contains_key(&self.cwd);
                self.navigate(if keep {
                    self.cwd.clone()
                } else {
                    String::new()
                });
            }
            Outcome::Extracted(dest, warnings) => {
                self.status = format!("Extracted to {}", dest.display());
                if !warnings.is_empty() {
                    self.dialog = Dialog::Info(format!(
                        "Extracted with warnings:\n\n{}",
                        warnings.join("\n")
                    ));
                }
            }
            Outcome::Tested => self.dialog = Dialog::Info("No errors found.".into()),
            Outcome::Created(path) => self.open(ctx, path),
            Outcome::Modified => {
                let work = self.working.as_mut().unwrap();
                work.dirty = true;
                let path = work.path.clone();
                self.run(ctx, Action::Open(path), self.password.clone());
            }
            Outcome::Saved => {
                self.working = None;
                self.status = "Saved".into();
                self.update_title(ctx);
                if let Some(then) = self.after_save.take() {
                    self.proceed(ctx, then);
                }
            }
            Outcome::Previewed { entry, file } => match launch(&file) {
                Ok(()) => {
                    let modified = modified_time(&file);
                    self.opened.retain(|o| o.file != file);
                    self.opened.push(OpenedFile {
                        entry,
                        file,
                        modified,
                    });
                }
                Err(e) => {
                    self.dialog = Dialog::Error(format!("Couldn't open {}: {e}", file.display()))
                }
            },
            Outcome::Unlocked => self.status = "Unlocked. Drag the items out again.".into(),
            Outcome::DraggedOut {
                dest,
                count,
                warnings,
            } => {
                let items = if count == 1 { "item" } else { "items" };
                self.status = match dest {
                    Some(dest) => format!("Extracted {count} {items} to {}", dest.display()),
                    None => format!("Extracted {count} {items}"),
                };
                if !warnings.is_empty() {
                    self.dialog = Dialog::Info(format!(
                        "Extracted with warnings:\n\n{}",
                        warnings.join("\n")
                    ));
                }
            }
        }
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        if self.dirty() {
            self.dialog = Dialog::Unsaved(Then::Open(path));
            return;
        }
        self.working = None;
        self.password = None;
        self.run(ctx, Action::Open(path), None);
    }

    fn dirty(&self) -> bool {
        self.working.as_ref().is_some_and(|w| w.dirty)
    }

    fn update_title(&self, ctx: &egui::Context) {
        let Some(archive) = &self.archive else { return };
        let name = archive.file_name().unwrap_or_default().to_string_lossy();
        let edited = if self.dirty() { " (edited)" } else { "" };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
            "{name}{edited} — HDEC"
        )));
    }

    /// Writes the working copy over the archive (via `*.hdec-tmp` + rename).
    fn save(&mut self, ctx: &egui::Context) {
        let Some(work) = self.working.as_ref().filter(|w| w.dirty) else {
            return;
        };
        let (from, to) = (work.path.clone(), self.archive.clone().unwrap());
        self.spawn(ctx, "Saving", icon::FLOPPY_DISK, None, None, move |_| {
            let mut tmp = to.clone().into_os_string();
            tmp.push(".hdec-tmp");
            std::fs::copy(&from, &tmp)
                .and_then(|_| std::fs::rename(&tmp, &to))
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&tmp);
                })?;
            Ok(Outcome::Saved)
        });
    }

    fn proceed(&mut self, ctx: &egui::Context, then: Then) {
        match then {
            Then::Open(path) => self.open(ctx, path),
            Then::Close => {
                self.closing = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn navigate(&mut self, cwd: String) {
        // Reveal the folder in the tree: expand it and all its ancestors.
        let mut path = cwd.as_str();
        while !path.is_empty() {
            self.expanded.insert(path.to_owned());
            path = parent_of(path);
        }
        self.cwd = cwd;
        self.filter.clear();
        self.refresh_rows();
    }

    fn go_up(&mut self) {
        let up = self
            .cwd
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_owned();
        self.navigate(up);
    }

    /// Rebuilds the visible rows: the current folder, or every match when filtering.
    fn refresh_rows(&mut self) {
        self.rows = if self.filter.is_empty() {
            children(&self.entries, &self.cwd)
        } else {
            let needle = self.filter.to_lowercase();
            self.entries
                .iter()
                .filter(|e| base_name(&e.path).to_lowercase().contains(&needle))
                .cloned()
                .collect()
        };
        sort_rows(&mut self.rows, self.sort, self.descending);
        self.selected.clear();
        self.anchor = None;
    }

    fn new_archive(&mut self, sources: Vec<PathBuf>) {
        if sources.is_empty() {
            return;
        }
        let format = CREATE_FORMATS[0];
        let first = &sources[0];
        let name = match sources.as_slice() {
            [dir] if dir.is_dir() => dir.file_name(),
            [file] => file.file_stem(),
            _ => first.parent().and_then(Path::file_name),
        };
        let name = name
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".into());
        let folder = first
            .parent()
            .unwrap_or(Path::new("."))
            .display()
            .to_string();
        self.dialog = Dialog::Create(CreateForm {
            sources,
            name,
            folder,
            format,
            level: Level::Normal,
            zip_crypto: false,
            password: Default::default(),
            confirm: Default::default(),
            show: false,
            split: false,
            split_mb: "100".into(),
            add_more: false,
        });
    }

    /// Asks for files and folders in one dialog: double-click opens a folder,
    /// Open takes the selection, folders included.
    fn pick(&mut self, ctx: &egui::Context, purpose: PickFor) {
        #[cfg(target_os = "macos")]
        if let Some(paths) = rfd::FileDialog::new().pick_files_or_folders() {
            self.picked(ctx, purpose, paths);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = ctx;
            let start = self
                .archive
                .as_deref()
                .and_then(Path::parent)
                .map_or_else(picker::home, Path::to_path_buf);
            self.picker = Some((picker::Picker::new(start), purpose));
        }
    }

    fn picked(&mut self, ctx: &egui::Context, purpose: PickFor, paths: Vec<PathBuf>) {
        match purpose {
            PickFor::New => self.new_archive(paths),
            PickFor::Add if self.modifiable => self.add(ctx, paths),
            PickFor::Add => {}
            PickFor::Contents => {
                if let Dialog::Create(form) = &mut self.dialog {
                    add_sources(&mut form.sources, paths);
                }
            }
        }
    }

    fn create(&mut self, ctx: &egui::Context, form: CreateForm) {
        let dest = form.dest();
        let subject = file_name(&dest);
        let opts = CreateOptions {
            format: form.format,
            level: form.level,
            zip_crypto: form.zip_crypto,
            split: form
                .split
                .then(|| form.split_mb.trim().parse::<f64>().ok())
                .flatten()
                .map(|mb| (mb * 1024.0 * 1024.0) as u64),
        };
        let password =
            (form.format.encryptable() && !form.password.is_empty()).then(|| form.password.clone());
        self.spawn(
            ctx,
            "Creating",
            icon::PLUS_SQUARE,
            None,
            password,
            move |c| {
                hdec_core::create(&form.sources, &dest, &opts, c)?;
                Ok(Outcome::Created(dest))
            },
        );
        if let Some(job) = &mut self.job {
            job.subject = subject;
        }
    }

    fn add(&mut self, ctx: &egui::Context, sources: Vec<PathBuf>) {
        let edit = Edit::Add {
            sources,
            into: self.cwd.clone(),
        };
        self.run(ctx, Action::Modify(edit), self.password.clone());
    }

    fn start_extract(&mut self) {
        let archive = self.archive.as_deref().unwrap();
        let dest = archive
            .parent()
            .unwrap_or(Path::new("."))
            .display()
            .to_string();
        self.dialog = Dialog::Extract {
            dest,
            subfolder: true,
            overwrite: Overwrite::Skip,
        };
    }

    fn start_delete(&mut self) {
        if self.modifiable && !self.selected.is_empty() {
            self.dialog = Dialog::ConfirmDelete(self.selected.iter().cloned().collect());
        }
    }

    fn command(&mut self, ctx: &egui::Context, command: Command) {
        if self.job.is_some() || self.archive.is_none() {
            return;
        }
        match command {
            Command::Open(path) => match self.rows.iter().find(|r| r.path == path) {
                Some(e) if e.is_dir => self.navigate(path),
                Some(_) => self.preview(ctx, path),
                None => {}
            },
            Command::Extract => self.start_extract(),
            Command::Rename => self.start_rename(),
            Command::Delete => self.start_delete(),
            Command::NewFolder if self.modifiable => {
                let taken = |n: &str| self.rows.iter().any(|r| base_name(&r.path) == n);
                let name = (1..)
                    .map(|i| {
                        if i == 1 {
                            "New folder".to_owned()
                        } else {
                            format!("New folder {i}")
                        }
                    })
                    .find(|n| !taken(n))
                    .unwrap();
                self.dialog = Dialog::NewFolder(name);
            }
            Command::Add if self.modifiable => self.pick(ctx, PickFor::Add),
            Command::Move(paths, into) if self.modifiable => {
                let pairs: Vec<(String, String)> = paths
                    .into_iter()
                    // Already there, or a folder dropped back onto itself.
                    .filter(|p| parent_of(p) != into && *p != into)
                    .map(|p| {
                        let to = join_path(&into, base_name(&p));
                        (p, to)
                    })
                    .collect();
                if !pairs.is_empty() {
                    self.run(
                        ctx,
                        Action::Modify(Edit::Rename(pairs)),
                        self.password.clone(),
                    );
                }
            }
            _ => {}
        }
    }

    /// Notices files opened from the archive that were changed in another app.
    fn check_opened(&mut self) {
        if self.job.is_some() || !matches!(self.dialog, Dialog::None) {
            return;
        }
        for opened in &mut self.opened {
            let now = modified_time(&opened.file);
            if now.is_none() || now == opened.modified {
                continue;
            }
            opened.modified = now;
            let name = base_name(&opened.entry).to_owned();
            if let Some(reason) = self.read_only {
                self.status = format!("Changes to {name} can't be saved: {reason}");
            } else {
                self.dialog = Dialog::SaveBack {
                    entry: opened.entry.clone(),
                    file: opened.file.clone(),
                };
            }
            return;
        }
    }

    fn save_back(&mut self, ctx: &egui::Context, entry: &str, file: PathBuf) {
        let edit = Edit::Add {
            sources: vec![file],
            into: parent_of(entry).to_owned(),
        };
        self.status = format!("Updated {}", base_name(entry));
        self.run(ctx, Action::Modify(edit), self.password.clone());
    }

    fn preview(&mut self, ctx: &egui::Context, entry: String) {
        // A fresh subfolder per open: the previous copy may still be open in another app.
        let Some(dir) = self.temp_subdir(false) else {
            return;
        };
        self.run(ctx, Action::Preview { entry, dir }, self.password.clone());
    }

    /// A new, not yet created folder in the preview (or drag) temp folder.
    fn temp_subdir(&mut self, drag: bool) -> Option<PathBuf> {
        let (slot, prefix) = if drag {
            (&mut self.drag_dir, "hdec-drag")
        } else {
            (&mut self.preview_dir, "hdec-open")
        };
        if slot.is_none() {
            match tempfile::Builder::new().prefix(prefix).tempdir() {
                Ok(dir) => *slot = Some(dir),
                Err(e) => {
                    self.dialog = Dialog::Error(format!("Couldn't create a temporary folder: {e}"));
                    return None;
                }
            }
        }
        let base = slot.as_ref().unwrap().path().to_path_buf();
        self.previews += 1;
        Some(base.join(self.previews.to_string()))
    }

    /// An entry drag that leaves the window becomes an OS drag; the entries
    /// are extracted where they are dropped.
    fn drag_out(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if !drag_out::SUPPORTED {
            return;
        }
        let Some(dragged) = egui::DragAndDrop::payload::<Dragged>(ctx) else {
            return;
        };
        let window = ctx.content_rect();
        let left = ctx.input(|i| {
            i.pointer.primary_down() && i.pointer.latest_pos().is_none_or(|p| !window.contains(p))
        });
        if !left {
            return;
        }
        egui::DragAndDrop::clear_payload(ctx);
        ctx.stop_dragging();
        self.release_pointer = true;
        ctx.request_repaint();
        if self.job.is_some() {
            return;
        }
        let dirs: BTreeSet<&str> = self
            .rows
            .iter()
            .filter(|r| r.is_dir)
            .map(|r| r.path.as_str())
            .collect();
        let items: Vec<(String, bool)> = dragged
            .0
            .iter()
            .map(|p| (p.clone(), dirs.contains(p.as_str())))
            .collect();
        // Nothing can ask for it once they're dropped: ask now.
        let selected: BTreeSet<&str> = items.iter().map(|(p, _)| p.as_str()).collect();
        let inside = |path: &str| {
            std::iter::once(path)
                .chain(drag_out::parents(path))
                .any(|p| selected.contains(p))
        };
        let locked = self
            .entries
            .iter()
            .filter(|e| e.encrypted && !e.is_dir && inside(&e.path))
            .min_by_key(|e| e.size)
            .map(|e| e.path.clone());
        if let (None, Some(entry)) = (&self.password, locked) {
            if let Some(dir) = self.temp_subdir(false) {
                self.dialog = Dialog::Password {
                    input: Default::default(),
                    then: Action::Unlock { entry, dir },
                    wrong: false,
                    show: false,
                };
            }
            return;
        }
        let dir = if drag_out::NEEDS_DIR {
            match self.temp_subdir(true) {
                Some(dir) => dir,
                None => return,
            }
        } else {
            PathBuf::new()
        };
        // Only Windows describes each file to the drop target.
        let entries = if cfg!(windows) {
            let entries = self.entries.iter().filter(|e| inside(&e.path));
            entries.cloned().collect()
        } else {
            Vec::new()
        };
        let drag = drag_out::DragOut {
            archive: self.reading().unwrap(),
            items,
            password: self.password.clone(),
            entries,
            jobs: self.drag_jobs.tx.clone(),
            egui: ctx.clone(),
        };
        if let Err(e) = drag_out::start(frame, drag, &dir) {
            self.dialog = Dialog::Error(format!("Couldn't drag the items out: {e}"));
        }
    }

    /// What's on screen: the working copy if there is one, else the archive.
    fn reading(&self) -> Option<PathBuf> {
        self.working
            .as_ref()
            .map(|w| w.path.clone())
            .or(self.archive.clone())
    }

    fn start_rename(&mut self) {
        if let (true, [from]) = (
            self.modifiable,
            self.selected.iter().collect::<Vec<_>>().as_slice(),
        ) {
            self.dialog = Dialog::Rename {
                from: (*from).clone(),
                name: base_name(from).to_owned(),
            };
        }
    }

    // ---- layout ----

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let idle = self.job.is_none();
        let has = idle && self.archive.is_some();
        let can_edit = has && self.modifiable;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if theme::tool_button(ui, icon::FOLDER_OPEN, "Open", idle, false).clicked()
                && let Some(p) = rfd::FileDialog::new().pick_file()
            {
                self.open(ui.ctx(), p);
            }
            if theme::tool_button(ui, icon::PLUS_SQUARE, "New", idle, false).clicked() {
                self.pick(ui.ctx(), PickFor::New);
            }
            theme::tool_separator(ui);
            if theme::tool_button(ui, icon::EXPORT, "Extract", has, true).clicked() {
                self.start_extract();
            }
            if theme::tool_button(ui, icon::CHECK_CIRCLE, "Test", has, false).clicked() {
                self.run(ui.ctx(), Action::Test, self.password.clone());
            }
            theme::tool_separator(ui);
            let read_only = self.read_only.filter(|_| has);
            let why = |r: egui::Response| match read_only {
                Some(reason) => r.on_hover_text(reason),
                None => r,
            };
            if why(theme::tool_button(
                ui,
                icon::FILE_PLUS,
                "Add",
                can_edit,
                false,
            ))
            .clicked()
            {
                self.command(ui.ctx(), Command::Add);
            }
            if why(theme::tool_button(
                ui,
                icon::FOLDER_PLUS,
                "New folder",
                can_edit,
                false,
            ))
            .clicked()
            {
                self.command(ui.ctx(), Command::NewFolder);
            }
            if why(theme::tool_button(
                ui,
                icon::TRASH,
                "Delete",
                can_edit && !self.selected.is_empty(),
                false,
            ))
            .clicked()
            {
                self.start_delete();
            }
            if why(theme::tool_button(
                ui,
                icon::PENCIL_SIMPLE,
                "Rename",
                can_edit && self.selected.len() == 1,
                false,
            ))
            .clicked()
            {
                self.start_rename();
            }
            theme::tool_separator(ui);
            let dirty = self.dirty();
            if theme::tool_button(ui, icon::FLOPPY_DISK, "Save", idle && dirty, dirty)
                .on_hover_text("Write your changes to the archive (⌘S / Ctrl+S)")
                .clicked()
            {
                self.save(ui.ctx());
            }
            if self.archive.is_some() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let hint = format!("{}  Filter", icon::MAGNIFYING_GLASS);
                    let field = egui::TextEdit::singleline(&mut self.filter)
                        .hint_text(hint)
                        .desired_width(200.0);
                    if ui.add(field).changed() {
                        self.refresh_rows();
                    }
                });
            }
        });
    }

    fn path_bar(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if theme::icon_button(ui, icon::ARROW_UP, !self.cwd.is_empty()).clicked() {
                self.go_up();
            }
            let name = self
                .archive
                .as_deref()
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let crumb = |ui: &mut egui::Ui, text: String, current: bool| {
                let color = if current { p.text } else { p.muted };
                ui.add(egui::Button::new(RichText::new(text).color(color)).frame(false))
            };
            let root = crumb(
                ui,
                format!("{}  {name}", icon::FILE_ARCHIVE),
                self.cwd.is_empty(),
            );
            if root.clicked() {
                self.navigate(String::new());
            }
            if let Some(dragged) = drop_target(ui, &root, self.modifiable) {
                self.command(ui.ctx(), Command::Move(dragged, String::new()));
            }
            let mut path = String::new();
            let parts: Vec<String> = self
                .cwd
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            for (i, part) in parts.iter().enumerate() {
                path = if path.is_empty() {
                    part.clone()
                } else {
                    format!("{path}/{part}")
                };
                ui.label(RichText::new(icon::CARET_RIGHT).size(11.0).color(p.faint));
                let response = crumb(ui, part.clone(), i + 1 == parts.len());
                if response.clicked() {
                    self.navigate(path.clone());
                }
                if let Some(dragged) = drop_target(ui, &response, self.modifiable) {
                    self.command(ui.ctx(), Command::Move(dragged, path.clone()));
                }
            }
            if !self.filter.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "— {} matches for “{}”",
                        self.rows.len(),
                        self.filter
                    ))
                    .color(p.muted),
                );
            }
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let name = self
                    .archive
                    .as_deref()
                    .and_then(Path::file_name)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                self.tree_row(ui, String::new(), &name, 0);
            });
    }

    fn tree_row(&mut self, ui: &mut egui::Ui, path: String, label: &str, depth: usize) {
        let p = pal(ui.ctx());
        let kids = self.dirs.get(&path).cloned().unwrap_or_default();
        let root = path.is_empty();
        let open = root || self.expanded.contains(&path);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
        let indent = rect.left() + 6.0 + depth as f32 * 16.0;
        if self.cwd == path && self.filter.is_empty() {
            ui.painter().rect_filled(rect, 6.0, p.accent_soft);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 6.0, p.hover);
        }
        let font = egui::FontId::proportional(13.0);
        let y = rect.center().y;
        if !kids.is_empty() && !root {
            let caret = if open {
                icon::CARET_DOWN
            } else {
                icon::CARET_RIGHT
            };
            ui.painter().text(
                egui::pos2(indent + 5.0, y),
                egui::Align2::CENTER_CENTER,
                caret,
                egui::FontId::proportional(10.0),
                p.faint,
            );
        }
        let (glyph, color, family) = if root {
            (icon::FILE_ARCHIVE, p.accent, egui::FontFamily::Proportional)
        } else if self.cwd == path {
            (
                icon::FOLDER_OPEN,
                p.folder,
                egui::FontFamily::Name("phosphor-fill".into()),
            )
        } else {
            (
                icon::FOLDER,
                p.folder,
                egui::FontFamily::Name("phosphor-fill".into()),
            )
        };
        ui.painter().text(
            egui::pos2(indent + 22.0, y),
            egui::Align2::CENTER_CENTER,
            glyph,
            egui::FontId::new(15.0, family),
            color,
        );
        ui.painter().text(
            egui::pos2(indent + 34.0, y),
            egui::Align2::LEFT_CENTER,
            label,
            font,
            p.text,
        );
        if let Some(dragged) = drop_target(ui, &resp, self.modifiable) {
            self.command(ui.ctx(), Command::Move(dragged, path.clone()));
        }
        if resp.clicked() {
            let on_caret = resp
                .interact_pointer_pos()
                .is_some_and(|pos| pos.x < indent + 12.0);
            if on_caret && !root {
                if !self.expanded.remove(&path) {
                    self.expanded.insert(path.clone());
                }
            } else {
                self.navigate(path.clone());
            }
        }
        if open {
            for kid in kids {
                let label = base_name(&kid).to_owned();
                self.tree_row(ui, kid, &label, depth + 1);
            }
        }
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let filtering = !self.filter.is_empty();
        let mut sort_by = None;
        let mut action = None;
        let (command, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
        // Draggable either way: out of the window extracts.
        let modifiable = self.modifiable;
        TableBuilder::new(ui)
            .sense(Sense::click_and_drag())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(120.0).clip(true))
            .column(Column::exact(70.0))
            .column(Column::exact(70.0))
            .column(Column::exact(48.0))
            .column(Column::exact(118.0))
            .header(28.0, |mut header| {
                for (key, title, right) in [
                    (SortKey::Name, "Name", false),
                    (SortKey::Size, "Size", true),
                    (SortKey::Packed, "Packed", true),
                    (SortKey::Ratio, "Ratio", true),
                    (SortKey::Modified, "Modified", false),
                ] {
                    header.col(|ui| {
                        let layout = if right {
                            Layout::right_to_left(Align::Center)
                        } else {
                            Layout::left_to_right(Align::Center)
                        };
                        ui.with_layout(layout, |ui| {
                            let mut text = RichText::new(title).size(12.0).color(p.muted);
                            if self.sort == key {
                                let arrow = if self.descending {
                                    icon::CARET_DOWN
                                } else {
                                    icon::CARET_UP
                                };
                                text = RichText::new(format!("{title} {arrow}"))
                                    .size(12.0)
                                    .color(p.text);
                            }
                            if ui
                                .add(egui::Label::new(text).sense(Sense::click()))
                                .clicked()
                            {
                                sort_by = Some(key);
                            }
                        });
                    });
                }
            })
            .body(|body| {
                body.rows(26.0, self.rows.len(), |mut row| {
                    let index = row.index();
                    let e = &self.rows[index];
                    row.set_selected(self.selected.contains(&e.path));
                    row.col(|ui| {
                        ui.spacing_mut().item_spacing.x = 7.0;
                        if e.is_dir {
                            ui.label(theme::fill(icon::FOLDER).size(16.0).color(p.folder));
                        } else if e.link.is_some() {
                            ui.label(RichText::new(icon::LINK_SIMPLE).size(16.0).color(p.accent));
                        } else {
                            ui.label(
                                RichText::new(theme::file_icon(&e.path))
                                    .size(16.0)
                                    .color(p.muted),
                            );
                        }
                        let name = if filtering {
                            e.path.as_str()
                        } else {
                            base_name(&e.path)
                        };
                        let mut text = egui::text::LayoutJob::default();
                        let font = egui::FontId::proportional(13.0);
                        text.append(name, 0.0, egui::TextFormat::simple(font.clone(), p.text));
                        if let Some(target) = e.link.as_deref().filter(|t| !t.is_empty()) {
                            let arrow = format!("  {}  {target}", icon::ARROW_RIGHT);
                            text.append(&arrow, 0.0, egui::TextFormat::simple(font, p.muted));
                        }
                        ui.add(egui::Label::new(text).truncate().selectable(false));
                        if e.encrypted {
                            ui.label(theme::fill(icon::LOCK_SIMPLE).size(12.0).color(p.folder));
                        }
                    });
                    let numeric = |ui: &mut egui::Ui, text: String| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(text).color(p.muted));
                        });
                    };
                    row.col(|ui| {
                        numeric(
                            ui,
                            if e.is_dir || e.link.is_some() {
                                String::new()
                            } else {
                                human_size(e.size)
                            },
                        )
                    });
                    row.col(|ui| {
                        numeric(
                            ui,
                            e.packed
                                .filter(|_| !e.is_dir && e.link.is_none())
                                .map(human_size)
                                .unwrap_or_default(),
                        )
                    });
                    row.col(|ui| {
                        numeric(
                            ui,
                            ratio(e)
                                .map(|r| {
                                    if r < 0.01 {
                                        "<1%".into()
                                    } else {
                                        format!("{:.0}%", r * 100.0)
                                    }
                                })
                                .unwrap_or_default(),
                        )
                    });
                    row.col(|ui| {
                        ui.label(
                            RichText::new(e.modified.map(local_time).unwrap_or_default())
                                .color(p.muted),
                        );
                    });

                    let response = row.response();
                    if response.drag_started() {
                        let paths = if self.selected.contains(&e.path) {
                            self.selected.iter().cloned().collect()
                        } else {
                            vec![e.path.clone()]
                        };
                        response.dnd_set_drag_payload(Dragged(paths));
                    }
                    if e.is_dir && modifiable {
                        if response.dnd_hover_payload::<Dragged>().is_some() {
                            let stroke = egui::Stroke::new(1.5, p.accent);
                            let painter = response.ctx.layer_painter(response.layer_id);
                            painter.rect_stroke(
                                response.rect,
                                6.0,
                                stroke,
                                egui::StrokeKind::Inside,
                            );
                        }
                        if let Some(dragged) = response.dnd_release_payload::<Dragged>() {
                            action = Some(Command::Move(dragged.0.clone(), e.path.clone()));
                        }
                    }
                    if response.double_clicked() {
                        action = Some(Command::Open(e.path.clone()));
                    } else if response.clicked()
                        || (response.secondary_clicked() && !self.selected.contains(&e.path))
                    {
                        match (shift, self.anchor) {
                            (true, Some(a)) => {
                                self.selected = self.rows[a.min(index)..=a.max(index)]
                                    .iter()
                                    .map(|r| r.path.clone())
                                    .collect();
                            }
                            _ if command => {
                                if !self.selected.remove(&e.path) {
                                    self.selected.insert(e.path.clone());
                                }
                                self.anchor = Some(index);
                            }
                            _ => {
                                self.selected = BTreeSet::from([e.path.clone()]);
                                self.anchor = Some(index);
                            }
                        }
                    }
                    response.context_menu(|ui| {
                        let open = if e.is_dir {
                            icon::FOLDER_OPEN
                        } else {
                            icon::ARROW_SQUARE_OUT
                        };
                        if ui.button(format!("{open}  Open")).clicked() {
                            action = Some(Command::Open(e.path.clone()));
                        }
                        if ui.button(format!("{}  Extract…", icon::EXPORT)).clicked() {
                            action = Some(Command::Extract);
                        }
                        if self.modifiable {
                            ui.separator();
                            if ui
                                .add_enabled(
                                    self.selected.len() == 1,
                                    egui::Button::new(format!("{}  Rename…", icon::PENCIL_SIMPLE)),
                                )
                                .clicked()
                            {
                                action = Some(Command::Rename);
                            }
                            if ui
                                .button(
                                    RichText::new(format!("{}  Delete", icon::TRASH))
                                        .color(p.danger),
                                )
                                .clicked()
                            {
                                action = Some(Command::Delete);
                            }
                        }
                    });
                });
            });
        if let Some(key) = sort_by {
            if self.sort == key {
                self.descending = !self.descending;
            } else {
                (self.sort, self.descending) = (key, false);
            }
            sort_rows(&mut self.rows, self.sort, self.descending);
            self.anchor = None;
        }
        // Space below the rows: click clears the selection, right-click offers folder actions.
        let rest = ui.available_rect_before_wrap();
        if rest.height() > 0.0 {
            let response = ui.interact(rest, ui.id().with("below-rows"), Sense::click());
            if response.clicked() {
                self.selected.clear();
            }
            if self.modifiable {
                response.context_menu(|ui| {
                    if ui
                        .button(format!("{}  New folder…", icon::FOLDER_PLUS))
                        .clicked()
                    {
                        action = Some(Command::NewFolder);
                    }
                    if ui.button(format!("{}  Add…", icon::FILE_PLUS)).clicked() {
                        action = Some(Command::Add);
                    }
                });
            }
        }
        if let Some(action) = action {
            self.command(ui.ctx(), action);
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let size = vec2(460.0, 260.0);
        let rect = egui::Rect::from_center_size(ui.max_rect().center(), size);
        theme::dashed_rect(ui, rect, p.border);
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(28.0)), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(icon::FILE_ARCHIVE).size(52.0).color(p.accent));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Drop files here")
                        .size(17.0)
                        .strong()
                        .color(p.text),
                );
                ui.label(
                    RichText::new("An archive opens it. Anything else starts a new archive.")
                        .color(p.muted),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 290.0).max(0.0) / 2.0);
                    if theme::primary_button(
                        ui,
                        &format!("{}  Open archive…", icon::FOLDER_OPEN),
                        true,
                    )
                    .clicked()
                        && let Some(p) = rfd::FileDialog::new().pick_file()
                    {
                        self.open(ui.ctx(), p);
                    }
                    if theme::button(ui, &format!("{}  New archive…", icon::PLUS_SQUARE)).clicked()
                    {
                        self.pick(ui.ctx(), PickFor::New);
                    }
                });
                ui.add_space(14.0);
                ui.label(
                    RichText::new("ZIP · 7Z · RAR · TAR · GZ · XZ · BZ2 · ZST · LZ · SFX")
                        .size(11.0)
                        .color(p.faint),
                );
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        ui.horizontal_centered(|ui| {
            if let Some(job) = &self.job {
                let (done, total, name) = &job.progress;
                ui.label(RichText::new(job.icon).color(p.accent));
                ui.label(RichText::new(job.verb).color(p.text));
                let fraction = (*total > 0).then(|| *done as f32 / *total as f32);
                theme::progress_bar(ui, fraction, 300.0, 6.0);
                let pct = fraction
                    .map(|f| format!("{:.0}% · ", (f * 100.0).min(100.0)))
                    .unwrap_or_default();
                let cancel = job.cancel.clone();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(egui::Button::new("Cancel").small()).clicked() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    ui.add(
                        egui::Label::new(RichText::new(format!("{pct}{name}")).color(p.muted))
                            .truncate(),
                    );
                });
                return;
            }
            if self.archive.is_some() {
                theme::pill(ui, self.format);
                if self.dirty() {
                    ui.label(RichText::new("Unsaved changes").color(p.accent))
                        .on_hover_text("Click Save to write them to the archive.");
                }
                if let Some(reason) = self.read_only {
                    let badge = format!("{} Read-only", icon::LOCK_SIMPLE);
                    ui.label(RichText::new(badge).color(p.muted))
                        .on_hover_text(reason);
                }
                ui.label(
                    RichText::new(format!(
                        "{} items · {}",
                        self.entries.len(),
                        human_size(self.total_size)
                    ))
                    .color(p.muted),
                );
                if !self.selected.is_empty() {
                    let size: u64 = self
                        .rows
                        .iter()
                        .filter(|r| self.selected.contains(&r.path))
                        .map(|r| r.size)
                        .sum();
                    ui.label(
                        RichText::new(format!(
                            "{} selected · {}",
                            self.selected.len(),
                            human_size(size)
                        ))
                        .color(p.muted),
                    );
                }
            }
            ui.label(RichText::new(&self.status).color(p.muted));
            if self.any_encrypted && self.archive.is_some() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("Some files are encrypted").color(p.muted));
                    ui.label(theme::fill(icon::LOCK_SIMPLE).color(p.folder));
                });
            }
        });
    }

    fn drop_overlay(&self, ctx: &egui::Context) {
        if !ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            return;
        }
        let p = pal(ctx);
        let rect = ctx.content_rect().shrink(10.0);
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop"),
        ));
        painter.rect_filled(rect, 12.0, p.bg.gamma_multiply(0.85));
        painter.rect_stroke(
            rect,
            12.0,
            egui::Stroke::new(2.0, p.accent),
            egui::StrokeKind::Inside,
        );
        let text = if matches!(self.dialog, Dialog::Create(_)) {
            "Add to the new archive".to_owned()
        } else if self.modifiable {
            format!("Add to “/{}”", self.cwd)
        } else {
            "Drop to open or create an archive".to_owned()
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(16.0),
            p.text,
        );
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.archive.is_none()
            || self.job.is_some()
            || !matches!(self.dialog, Dialog::None)
            || self.picker.is_some()
            || ctx.memory(|m| m.focused().is_some())
        {
            return;
        }
        if ctx.input(|i| i.key_pressed(Key::S) && i.modifiers.command) {
            self.save(ctx);
        }
        let (up, all, delete, enter, rename, new_folder) = ctx.input(|i| {
            (
                i.key_pressed(Key::Backspace) && !i.modifiers.command,
                i.key_pressed(Key::A) && i.modifiers.command,
                i.key_pressed(Key::Delete)
                    || (i.key_pressed(Key::Backspace) && i.modifiers.command),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::F2),
                i.key_pressed(Key::N) && i.modifiers.command && i.modifiers.shift,
            )
        });
        if rename {
            self.command(ctx, Command::Rename);
        }
        if new_folder {
            self.command(ctx, Command::NewFolder);
        }
        if up && !self.cwd.is_empty() {
            self.go_up();
        }
        if all {
            self.selected = self.rows.iter().map(|r| r.path.clone()).collect();
        }
        if delete {
            self.start_delete();
        }
        if enter && let [one] = self.selected.iter().collect::<Vec<_>>().as_slice() {
            let one = (*one).clone();
            self.command(ctx, Command::Open(one));
        }
    }

    /// The running job, once it has taken long enough to be worth a window:
    /// bar, current entry, bytes, speed, time left, Cancel.
    fn progress_dialog(&mut self, ctx: &egui::Context) {
        const AFTER: Duration = Duration::from_millis(400);
        let Some(job) = &self.job else { return };
        let elapsed = job.started.elapsed();
        if elapsed < AFTER {
            ctx.request_repaint_after(AFTER - elapsed);
            return;
        }
        let p = pal(ctx);
        let (done, total, name) = &job.progress;
        let (done, total) = (*done, *total);
        let fraction = (total > 0).then(|| (done as f32 / total as f32).min(1.0));
        let secs = elapsed.as_secs_f64();
        // Average since the start: steadier than the last few reports.
        let rate = (secs >= 1.0 && done > 0).then(|| done as f64 / secs);
        let cancelling = job.cancel.load(Ordering::Relaxed);
        let cancel = job.cancel.clone();
        theme::dialog_with(
            ctx,
            "progress",
            420.0,
            job.icon,
            job.verb,
            &job.subject,
            |ui| {
                ui.add_space(4.0);
                theme::progress_bar(ui, fraction, ui.available_width(), 8.0);
                ui.add_space(6.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                let line = |ui: &mut egui::Ui, left: String, right: String| {
                    egui::Sides::new().shrink_left().show(
                        ui,
                        |ui| {
                            ui.add(egui::Label::new(RichText::new(left).color(p.muted)).truncate())
                        },
                        |ui| ui.label(RichText::new(right).color(p.muted)),
                    );
                };
                let pct = fraction
                    .map(|f| format!("{:.0}%", f * 100.0))
                    .unwrap_or_default();
                line(ui, name.clone(), pct);
                let amount = if total > 0 {
                    format!("{} of {}", human_size(done.min(total)), human_size(total))
                } else {
                    String::new()
                };
                let pace = match rate {
                    _ if cancelling => "Cancelling…".to_owned(),
                    Some(rate) if total > done => format!(
                        "{}/s · about {} left",
                        human_size(rate as u64),
                        human_duration((total - done) as f64 / rate)
                    ),
                    Some(rate) => format!("{}/s", human_size(rate as u64)),
                    None => String::new(),
                };
                line(ui, amount, pace);
                theme::dialog_footer(ui, |ui| {
                    let button = egui::Button::new("Cancel");
                    if ui.add_enabled(!cancelling, button).clicked()
                        || ui.input(|i| i.key_pressed(Key::Escape))
                    {
                        cancel.store(true, Ordering::Relaxed);
                    }
                });
            },
        );
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let p = pal(ctx);
        let archive_name = self
            .archive
            .as_deref()
            .and_then(Path::file_name)
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut pick_contents = false;
        let dialog = std::mem::take(&mut self.dialog);
        self.dialog = match dialog {
            Dialog::None => Dialog::None,
            Dialog::Info(text) if message(ctx, icon::CHECK_CIRCLE, "Done", &text) => Dialog::None,
            Dialog::Error(text) if message(ctx, icon::WARNING, "Something went wrong", &text) => {
                Dialog::None
            }
            Dialog::StillCopying
                if message(
                    ctx,
                    icon::WARNING,
                    "Still copying",
                    "The files you dragged out are still being copied. Quitting now \
                     stops that copy. Close the window again to quit anyway.",
                ) =>
            {
                Dialog::None
            }
            open @ (Dialog::Info(_) | Dialog::Error(_) | Dialog::StillCopying) => open,
            Dialog::ConfirmDelete(names) => {
                let subtitle = format!(
                    "{} item(s) will be removed from {archive_name}. This can't be undone.",
                    names.len()
                );
                let result =
                    theme::dialog(ctx, icon::TRASH, "Delete from archive", &subtitle, |ui| {
                        let mut result = None;
                        theme::dialog_footer(ui, |ui| {
                            if theme::danger_button(ui, "Delete").clicked() {
                                result = Some(true);
                            }
                            if theme::button(ui, "Cancel").clicked() {
                                result = Some(false);
                            }
                        });
                        result
                    });
                match result {
                    Some(true) => {
                        self.run(
                            ctx,
                            Action::Modify(Edit::Delete(names)),
                            self.password.clone(),
                        );
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::ConfirmDelete(names),
                }
            }
            Dialog::Rename { from, mut name } => {
                let result = theme::dialog_with(
                    ctx,
                    "dialog",
                    340.0,
                    icon::PENCIL_SIMPLE,
                    "Rename",
                    &from,
                    |ui| {
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY),
                        );
                        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        let valid = valid_name(&name);
                        let mut result = None;
                        theme::dialog_footer(ui, |ui| {
                            if theme::primary_button(ui, "Rename", valid).clicked()
                                || (enter && valid)
                            {
                                result = Some(true);
                            }
                            if theme::button(ui, "Cancel").clicked() {
                                result = Some(false);
                            }
                        });
                        result
                    },
                );
                match result {
                    Some(true) => {
                        let to = join_path(parent_of(&from), name.trim());
                        // Unchanged: nothing to do (7Z/TAR/RAR would be rewritten whole).
                        if to != from {
                            self.run(
                                ctx,
                                Action::Modify(Edit::Rename(vec![(from, to)])),
                                self.password.clone(),
                            );
                        }
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::Rename { from, name },
                }
            }
            Dialog::SaveBack { entry, file } => {
                let subtitle = format!("{} was changed in another app.", base_name(&entry));
                let result = theme::dialog(
                    ctx,
                    icon::FLOPPY_DISK,
                    "Update the archive?",
                    &subtitle,
                    |ui| {
                        ui.label(format!("Update “{entry}” in {archive_name}?"));
                        let mut result = None;
                        theme::dialog_footer(ui, |ui| {
                            if theme::primary_button(ui, "Update", true).clicked() {
                                result = Some(true);
                            }
                            if theme::button(ui, "Not now").clicked() {
                                result = Some(false);
                            }
                        });
                        result
                    },
                );
                match result {
                    Some(true) => {
                        self.save_back(ctx, &entry, file);
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::SaveBack { entry, file },
                }
            }
            Dialog::NewFolder(mut name) => {
                let place = format!("In /{}", self.cwd);
                let result = theme::dialog_with(
                    ctx,
                    "dialog",
                    340.0,
                    icon::FOLDER_PLUS,
                    "New folder",
                    &place,
                    |ui| {
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY),
                        );
                        if !field.has_focus() && !field.lost_focus() {
                            field.request_focus();
                        }
                        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                        let valid = valid_name(&name);
                        let mut result = None;
                        theme::dialog_footer(ui, |ui| {
                            if theme::primary_button(ui, "Create", valid).clicked()
                                || (enter && valid)
                            {
                                result = Some(true);
                            }
                            if theme::button(ui, "Cancel").clicked() {
                                result = Some(false);
                            }
                        });
                        result
                    },
                );
                match result {
                    Some(true) => {
                        let dir = join_path(&self.cwd, name.trim());
                        self.run(
                            ctx,
                            Action::Modify(Edit::CreateDir(dir)),
                            self.password.clone(),
                        );
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::NewFolder(name),
                }
            }
            Dialog::Password {
                mut input,
                then,
                wrong,
                mut show,
            } => {
                let result = theme::dialog(
                    ctx,
                    icon::LOCK_SIMPLE,
                    "Password required",
                    &archive_name,
                    |ui| {
                        let enter = password_field(ui, &mut input, &mut show, true);
                        if wrong {
                            ui.label(RichText::new("Wrong password, try again.").color(p.danger));
                        }
                        let mut result = None;
                        theme::dialog_footer(ui, |ui| {
                            if theme::primary_button(ui, "Unlock", !input.is_empty()).clicked()
                                || (enter && !input.is_empty())
                            {
                                result = Some(true);
                            }
                            if theme::button(ui, "Cancel").clicked() {
                                result = Some(false);
                            }
                        });
                        result
                    },
                );
                match result {
                    Some(true) => {
                        self.run(ctx, then, Some(input));
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::Password {
                        input,
                        then,
                        wrong,
                        show,
                    },
                }
            }
            Dialog::Extract {
                mut dest,
                mut subfolder,
                mut overwrite,
            } => {
                let stem = archive_stem(self.archive.as_deref().unwrap());
                let subtitle = match self.selected.len() {
                    0 => format!("All {} items from {archive_name}", self.entries.len()),
                    n => format!("{n} selected item(s) from {archive_name}"),
                };
                let result = theme::dialog(ctx, icon::EXPORT, "Extract", &subtitle, |ui| {
                    egui::Grid::new("extract")
                        .num_columns(2)
                        .spacing(vec2(12.0, 10.0))
                        .show(ui, |ui| {
                            theme::form_label(ui, "To");
                            let start = dest.clone();
                            path_field(ui, &mut dest, || {
                                rfd::FileDialog::new().set_directory(start).pick_folder()
                            });
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(&mut subfolder, format!("Into a new folder “{stem}”"));
                            ui.end_row();
                            theme::form_label(ui, "If file exists");
                            theme::segmented(
                                ui,
                                &mut overwrite,
                                &[
                                    (Overwrite::Skip, "Skip"),
                                    (Overwrite::Replace, "Replace"),
                                    (Overwrite::Rename, "Keep both"),
                                ],
                            );
                            ui.end_row();
                        });
                    let mut result = None;
                    theme::dialog_footer(ui, |ui| {
                        if theme::primary_button(ui, "Extract", !dest.is_empty()).clicked() {
                            result = Some(true);
                        }
                        if theme::button(ui, "Cancel").clicked() {
                            result = Some(false);
                        }
                    });
                    result
                });
                match result {
                    Some(true) => {
                        let mut target = PathBuf::from(&dest);
                        if subfolder {
                            target.push(&stem);
                        }
                        let only = (!self.selected.is_empty())
                            .then(|| self.selected.iter().cloned().collect());
                        self.run(
                            ctx,
                            Action::Extract(
                                target,
                                ExtractOptions {
                                    only,
                                    overwrite,
                                    ..Default::default()
                                },
                            ),
                            self.password.clone(),
                        );
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::Extract {
                        dest,
                        subfolder,
                        overwrite,
                    },
                }
            }
            Dialog::Create(mut form) => {
                let result = theme::dialog(ctx, icon::PLUS_SQUARE, "New archive", "", |ui| {
                    create_form(ui, &mut form)
                });
                pick_contents = std::mem::take(&mut form.add_more);
                match result {
                    Some(true) => {
                        self.create(ctx, form);
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::Create(form),
                }
            }
            Dialog::Unsaved(then) => {
                let title = format!("Save changes to {archive_name}?");
                let result = theme::dialog_with(
                    ctx,
                    "dialog",
                    380.0,
                    icon::FLOPPY_DISK,
                    &title,
                    "Your edits are lost if you don't save them.",
                    |ui| {
                        let mut result = None;
                        theme::dialog_footer(ui, |ui| {
                            if theme::primary_button(ui, "Save", true).clicked() {
                                result = Some(Some(true));
                            }
                            if theme::button(ui, "Don't save").clicked() {
                                result = Some(Some(false));
                            }
                            if theme::button(ui, "Cancel").clicked()
                                || ui.input(|i| i.key_pressed(Key::Escape))
                            {
                                result = Some(None);
                            }
                        });
                        result
                    },
                );
                match result {
                    Some(Some(true)) => {
                        self.after_save = Some(then);
                        self.save(ctx);
                        Dialog::None
                    }
                    Some(Some(false)) => {
                        self.working = None;
                        self.proceed(ctx, then);
                        Dialog::None
                    }
                    Some(None) => Dialog::None,
                    None => Dialog::Unsaved(then),
                }
            }
        };
        if pick_contents {
            self.pick(ctx, PickFor::Contents);
        }
        if let Some((picker, purpose)) = &mut self.picker {
            let title = match purpose {
                PickFor::New => "New archive from…",
                PickFor::Add | PickFor::Contents => "Add files and folders",
            };
            if let Some(picked) = picker.show(ctx, title) {
                let purpose = *purpose;
                self.picker = None;
                if let picker::Picked::Open(paths) = picked {
                    self.picked(ctx, purpose, paths);
                }
            }
        }
    }
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if std::mem::take(&mut self.release_pointer) {
            raw_input.events.push(egui::Event::PointerButton {
                pos: egui::pos2(-1.0, -1.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            });
            raw_input.events.push(egui::Event::PointerGone);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = pal(&ctx);
        if self.job.is_none()
            && let Ok(job) = self.drag_jobs.rx.try_recv()
        {
            self.job = Some(job);
        }
        self.poll_job(&ctx);
        // Our own dragged-out files dropped back onto the window: nothing to do.
        let drag_dir = self.drag_dir.as_ref().map(|d| d.path().to_path_buf());
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| drag_dir.as_ref().is_none_or(|d| !p.starts_with(d)))
                .collect()
        });
        if !dropped.is_empty() && self.job.is_none() {
            match dropped.as_slice() {
                _ if let Dialog::Create(form) = &mut self.dialog => {
                    add_sources(&mut form.sources, dropped)
                }
                // Another dialog is up (rename, password, unsaved changes…): not now.
                _ if !matches!(self.dialog, Dialog::None) || self.picker.is_some() => {}
                // An editable archive is open: everything dropped goes into it.
                _ if self.modifiable => self.add(&ctx, dropped),
                [one] if one.is_file() && hdec_core::detect(one).is_ok() => {
                    self.open(&ctx, one.clone())
                }
                _ => self.new_archive(dropped),
            }
        }
        // Opened from Finder; waits while something else is going on. One
        // window, so of several files the last one wins.
        #[cfg(target_os = "macos")]
        if self.job.is_none()
            && matches!(self.dialog, Dialog::None)
            && self.picker.is_none()
            && let Some(path) = mac_open::take().pop()
        {
            self.open(&ctx, path);
        }
        if !drag_out::copying() {
            self.warned_copying = false;
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            if self.dirty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.dialog = Dialog::Unsaved(Then::Close);
            } else if drag_out::copying() && !self.warned_copying {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.warned_copying = true;
                self.dialog = Dialog::StillCopying;
            }
        }
        self.shortcuts(&ctx);
        if !self.opened.is_empty() {
            self.check_opened();
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }

        let bar = |fill| {
            egui::Frame::new()
                .fill(fill)
                .inner_margin(Margin::symmetric(10, 4))
        };
        egui::Panel::top("toolbar")
            .frame(bar(p.panel))
            .show(ui, |ui| self.toolbar(ui));
        if self.archive.is_some() {
            egui::Panel::top("path")
                .frame(bar(p.bg))
                .show(ui, |ui| self.path_bar(ui));
        }
        egui::Panel::bottom("status")
            .exact_size(28.0)
            .frame(bar(p.panel))
            .show(ui, |ui| self.status_bar(ui));
        if self.archive.is_some() {
            egui::Panel::left("tree")
                .resizable(true)
                .default_size(160.0)
                .min_size(140.0)
                .frame(
                    egui::Frame::new()
                        .fill(p.panel)
                        .inner_margin(Margin::same(6)),
                )
                .show(ui, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(4, 0)),
            )
            .show(ui, |ui| {
                if self.archive.is_some() {
                    self.table(ui);
                } else {
                    self.empty_state(ui);
                }
            });
        self.drag_out(&ctx, frame);
        self.dialogs(&ctx);
        self.progress_dialog(&ctx);
        self.drop_overlay(&ctx);
        drag_preview(&ctx, self.modifiable);
    }
}

/// Info/error dialog. Returns true when dismissed.
fn message(ctx: &egui::Context, glyph: &str, title: &str, text: &str) -> bool {
    theme::dialog(ctx, glyph, title, "", |ui| {
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .show(ui, |ui| ui.label(text));
        let mut close = false;
        theme::dialog_footer(ui, |ui| {
            close = theme::primary_button(ui, "OK", true).clicked()
        });
        close || ui.input(|i| i.key_pressed(Key::Enter) || i.key_pressed(Key::Escape))
    })
}

/// Masked text field with a show/hide toggle. Returns true if Enter was pressed in it.
fn password_field(ui: &mut egui::Ui, value: &mut String, show: &mut bool, focus: bool) -> bool {
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(value)
                .password(!*show)
                .desired_width(ui.available_width() - 34.0),
        );
        if focus && !field.has_focus() && !field.lost_focus() {
            field.request_focus();
        }
        if theme::icon_button(ui, if *show { icon::EYE_SLASH } else { icon::EYE }, true).clicked() {
            *show = !*show;
        }
        field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))
    })
    .inner
}

fn path_field(ui: &mut egui::Ui, value: &mut String, browse: impl FnOnce() -> Option<PathBuf>) {
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(value).desired_width(ui.available_width() - 82.0));
        if theme::button(ui, "Browse…").clicked()
            && let Some(p) = browse()
        {
            *value = p.display().to_string();
        }
    });
}

fn create_form(ui: &mut egui::Ui, form: &mut CreateForm) -> Option<bool> {
    let p = pal(ui.ctx());
    let mut mismatch = false;
    // Scrolls when the window is too short for the whole form.
    let max_height = ui.ctx().content_rect().height() - 150.0;
    egui::ScrollArea::vertical()
        .max_height(max_height)
        .show(ui, |ui| {
            egui::Grid::new("create")
                .num_columns(2)
                .spacing(vec2(12.0, 6.0))
                .show(ui, |ui| {
                    theme::form_label(ui, "Contents");
                    form.add_more |= sources_list(ui, &mut form.sources);
                    ui.end_row();

                    theme::form_label(ui, "Name");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(220.0));
                        ui.label(
                            RichText::new(format!(".{}", form.format.extension())).color(p.muted),
                        );
                    });
                    ui.end_row();

                    theme::form_label(ui, "Save in");
                    let folder = form.folder.clone();
                    path_field(ui, &mut form.folder, || {
                        rfd::FileDialog::new().set_directory(folder).pick_folder()
                    });
                    ui.end_row();

                    theme::form_label(ui, "Format");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("format")
                            .selected_text(form.format.name())
                            .width(84.0)
                            .show_ui(ui, |ui| {
                                for f in CREATE_FORMATS.iter().filter(|f| fits(**f, &form.sources))
                                {
                                    ui.selectable_value(&mut form.format, *f, f.name());
                                }
                            });
                        if form.format != Format::Tar {
                            theme::segmented(
                                ui,
                                &mut form.level,
                                &[
                                    (Level::Store, "Store"),
                                    (Level::Fast, "Fast"),
                                    (Level::Normal, "Normal"),
                                    (Level::Max, "Max"),
                                ],
                            );
                        }
                    });
                    ui.end_row();

                    if form.format.encryptable() {
                        theme::form_label(ui, "Password");
                        password_field(ui, &mut form.password, &mut form.show, false);
                        ui.end_row();
                        theme::form_label(ui, "Confirm");
                        ui.add_enabled(
                            !form.show,
                            egui::TextEdit::singleline(&mut *form.confirm)
                                .password(true)
                                .desired_width(ui.available_width() - 34.0),
                        );
                        ui.end_row();
                        mismatch = !form.show && form.password != form.confirm;
                        ui.label("");
                        let hint = match (form.format, mismatch, form.password.is_empty()) {
                            (_, true, _) => RichText::new("Passwords don't match.").color(p.danger),
                            (_, _, true) => {
                                RichText::new("Leave empty for no encryption.").color(p.faint)
                            }
                            (Format::SevenZ | Format::Rar, ..) => {
                                RichText::new("AES-256. File names are encrypted too.")
                                    .color(p.faint)
                            }
                            _ if form.zip_crypto => RichText::new(
                                "ZipCrypto is weak, but Explorer and Archive Utility can open it.",
                            )
                            .color(p.faint),
                            _ => RichText::new("AES-256. File names stay visible.").color(p.faint),
                        };
                        ui.label(hint.size(11.5));
                        ui.end_row();
                        if form.format == Format::Zip && !form.password.is_empty() {
                            ui.label("");
                            ui.checkbox(
                                &mut form.zip_crypto,
                                "Legacy ZipCrypto (for Explorer / Archive Utility)",
                            );
                            ui.end_row();
                        }
                    }

                    theme::form_label(ui, "Split");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut form.split, "");
                        ui.add_enabled(
                            form.split,
                            egui::TextEdit::singleline(&mut form.split_mb).desired_width(64.0),
                        );
                        ui.label(RichText::new("MB").color(p.muted));
                        for (label, mb) in [("CD", "700"), ("DVD", "4480"), ("FAT32", "4095")] {
                            if ui
                                .add_enabled(form.split, egui::Button::new(label).small())
                                .clicked()
                            {
                                form.split_mb = mb.into();
                            }
                        }
                    });
                    ui.end_row();
                });
        });
    if !fits(form.format, &form.sources) {
        form.format = Format::Zip;
    }
    if form.show {
        *form.confirm = form.password.to_string();
    }
    let bad_split = form.split
        && !form
            .split_mb
            .trim()
            .parse::<f64>()
            .is_ok_and(|mb| mb >= 0.01);
    // Creating replaces an existing file of that name: say so before it happens.
    let exists = valid_name(form.name.trim()) && {
        let dest = form.dest();
        let mut first_part = dest.clone().into_os_string();
        first_part.push(".001");
        dest.exists() || (form.split && Path::new(&first_part).exists())
    };
    let mut result = None;
    theme::dialog_footer(ui, |ui| {
        if theme::primary_button(
            ui,
            if exists { "Replace" } else { "Create" },
            !mismatch
                && !bad_split
                && valid_name(form.name.trim())
                && !form.folder.trim().is_empty()
                && !form.sources.is_empty(),
        )
        .clicked()
        {
            result = Some(true);
        }
        if theme::button(ui, "Cancel").clicked() {
            result = Some(false);
        }
        if exists {
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                let name = form.dest();
                let name = name.file_name().unwrap_or_default().to_string_lossy();
                ui.label(
                    RichText::new(format!("{name} already exists"))
                        .size(11.5)
                        .color(p.danger),
                );
            });
        }
    });
    result
}

/// What goes into a new archive: removable rows, plus a button to add more.
/// Returns true when Add… was clicked.
fn sources_list(ui: &mut egui::Ui, sources: &mut Vec<PathBuf>) -> bool {
    let p = pal(ui.ctx());
    let width = ui.available_width();
    ui.vertical(|ui| {
        ui.set_width(width);
        let mut remove = None;
        egui::ScrollArea::vertical()
            .max_height(104.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, src) in sources.iter().enumerate() {
                    let name = src
                        .file_name()
                        .map_or_else(|| src.display().to_string(), |n| n.to_string_lossy().into());
                    ui.horizontal(|ui| {
                        let (glyph, color) = if src.is_dir() {
                            (icon::FOLDER, p.accent)
                        } else {
                            (theme::file_icon(&name), p.muted)
                        };
                        ui.label(RichText::new(glyph).color(color));
                        ui.add(egui::Label::new(&name).truncate())
                            .on_hover_text(src.display().to_string());
                        if src.is_dir() {
                            ui.label(
                                RichText::new("and everything inside")
                                    .size(11.5)
                                    .color(p.faint),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::icon_button(ui, icon::X, true)
                                .on_hover_text("Remove")
                                .clicked()
                            {
                                remove = Some(i);
                            }
                        });
                    });
                }
            });
        if let Some(i) = remove {
            sources.remove(i);
        }
        ui.add(egui::Button::new(format!("{}  Add…", icon::PLUS)).small())
            .clicked()
    })
    .inner
}

fn add_sources(sources: &mut Vec<PathBuf>, paths: Vec<PathBuf>) {
    for path in paths {
        if !sources.contains(&path) {
            sources.push(path);
        }
    }
}

/// Accepts a dropped `Dragged` payload on `response`, highlighting it while hovered.
fn drop_target(ui: &egui::Ui, response: &egui::Response, enabled: bool) -> Option<Vec<String>> {
    if !enabled {
        return None;
    }
    if response.dnd_hover_payload::<Dragged>().is_some() {
        let stroke = egui::Stroke::new(1.5, pal(ui.ctx()).accent);
        ui.painter()
            .rect_stroke(response.rect, 6.0, stroke, egui::StrokeKind::Inside);
    }
    response
        .dnd_release_payload::<Dragged>()
        .map(|d| d.0.clone())
}

/// Label following the pointer while entries are dragged: a move inside
/// the archive, or (read-only) only out of the window.
fn drag_preview(ctx: &egui::Context, modifiable: bool) {
    let (Some(dragged), Some(pos)) = (
        egui::DragAndDrop::payload::<Dragged>(ctx),
        ctx.pointer_latest_pos(),
    ) else {
        return;
    };
    let p = pal(ctx);
    let glyph = if modifiable {
        icon::ARROWS_OUT_CARDINAL
    } else {
        icon::EXPORT
    };
    let text = match dragged.0.as_slice() {
        [one] => format!("{glyph}  {}", base_name(one)),
        many => format!("{glyph}  {} items", many.len()),
    };
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("drag"),
    ));
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(13.0), p.text);
    let rect =
        egui::Rect::from_min_size(pos + vec2(14.0, 10.0), galley.size()).expand2(vec2(8.0, 4.0));
    painter.rect_filled(rect, 6.0, p.panel);
    painter.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, p.accent),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.min + vec2(8.0, 4.0), galley, p.text);
}

/// Opens a file with the system's default app.
fn launch(path: &Path) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(drop)
}

fn modified_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_only_reason(path: &Path, format: &str) -> &'static str {
    let name = path.to_string_lossy().to_ascii_lowercase();
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    if ext.len() == 3 && ext.bytes().all(|b| b.is_ascii_digit()) {
        "Split archives are read-only. Extract it, or convert it to a single archive."
    } else if ext == "exe" {
        "Self-extracting archives are read-only."
    } else if format == "RAR" {
        "Multi-volume RAR sets (.part1.rar, .r00) are read-only. Extract them to edit."
    } else if matches!(format, "GZ" | "XZ" | "BZ2" | "ZST" | "LZ" | "LZMA") {
        "This is a single compressed file, not an archive of files."
    } else {
        "HDEC can read this format but not write it."
    }
}

fn valid_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".."
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn base_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
}

/// Direct children of `cwd`. Folders that exist only implicitly
/// (as a prefix of deeper entries) are synthesized.
fn children(entries: &[Entry], cwd: &str) -> Vec<Entry> {
    let prefix = if cwd.is_empty() {
        String::new()
    } else {
        format!("{cwd}/")
    };
    let mut dirs = BTreeMap::new();
    let mut files = Vec::new();
    for e in entries {
        let Some(rest) = e.path.strip_prefix(&prefix).filter(|r| !r.is_empty()) else {
            continue;
        };
        match rest.split_once('/') {
            Some((dir, _)) => {
                dirs.entry(dir.to_owned()).or_insert_with(|| Entry {
                    path: format!("{prefix}{dir}"),
                    size: 0,
                    packed: None,
                    modified: None,
                    is_dir: true,
                    encrypted: false,
                    link: None,
                });
            }
            None if e.is_dir => {
                dirs.insert(rest.to_owned(), e.clone());
            }
            None => files.push(e.clone()),
        }
    }
    dirs.into_values().chain(files).collect()
}

/// Every folder (explicit or implied by a deeper path) → its subfolders, sorted.
fn dir_tree(entries: &[Entry]) -> BTreeMap<String, Vec<String>> {
    let mut all = BTreeSet::new();
    for e in entries {
        let mut path = e.path.as_str();
        if e.is_dir {
            all.insert(path.to_owned());
        }
        while let Some((parent, _)) = path.rsplit_once('/') {
            all.insert(parent.to_owned());
            path = parent;
        }
    }
    let mut tree: BTreeMap<String, Vec<String>> = BTreeMap::from([(String::new(), Vec::new())]);
    for dir in &all {
        let parent = dir.rsplit_once('/').map_or("", |(p, _)| p);
        tree.entry(parent.to_owned()).or_default().push(dir.clone());
        tree.entry(dir.clone()).or_default();
    }
    tree
}

fn ratio(e: &Entry) -> Option<f64> {
    (!e.is_dir && e.link.is_none() && e.size > 0).then_some(e.packed? as f64 / e.size as f64)
}

/// Folders first, then by `key`; names break ties.
fn sort_rows(rows: &mut [Entry], key: SortKey, descending: bool) {
    rows.sort_by(|a, b| {
        let by_key = match key {
            SortKey::Name => std::cmp::Ordering::Equal,
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Packed => a.packed.cmp(&b.packed),
            SortKey::Ratio => ratio(a)
                .partial_cmp(&ratio(b))
                .unwrap_or(std::cmp::Ordering::Equal),
            SortKey::Modified => a.modified.cmp(&b.modified),
        };
        let by_name = || {
            base_name(&a.path)
                .to_lowercase()
                .cmp(&base_name(&b.path).to_lowercase())
        };
        let ord = by_key.then_with(by_name);
        b.is_dir
            .cmp(&a.is_dir)
            .then(if descending { ord.reverse() } else { ord })
    });
}

/// `photos.tar.gz` → `photos`, `Report 1.5.zip` → `Report 1.5`,
/// `set.part2.rar` / `set.7z.003` → `set`.
fn archive_stem(path: &Path) -> String {
    let mut name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let cut = |name: &mut String, suffix: &str| {
        let keep = name.len().saturating_sub(suffix.len());
        let matches = name.is_char_boundary(keep) && name[keep..].eq_ignore_ascii_case(suffix);
        if matches {
            name.truncate(keep);
        }
        matches
    };
    // Volume numbers: `.001`, `.r00`, `.part2` (before `.rar`).
    if let Some((base, n)) = name.rsplit_once('.')
        && n.len() == 3
        && n.trim_start_matches(['r', 'R'])
            .bytes()
            .all(|b| b.is_ascii_digit())
    {
        name = base.to_owned();
    }
    let compound = [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz"];
    if !compound.iter().any(|ext| cut(&mut name, ext)) {
        if let Some((base, _)) = name.rsplit_once('.') {
            name = base.to_owned();
        }
        if let Some((base, part)) = name.rsplit_once('.')
            && part.len() > 4
            && part[..4].eq_ignore_ascii_case("part")
            && part[4..].bytes().all(|b| b.is_ascii_digit())
        {
            name = base.to_owned();
        }
    }
    if name.is_empty() {
        "archive".into()
    } else {
        name
    }
}

/// A path's last component, for display.
fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// "12 s", "3 min", "1 h 5 min".
fn human_duration(secs: f64) -> String {
    let secs = secs.ceil() as u64;
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs.div_ceil(60)),
        _ => format!("{} h {} min", secs / 3600, secs % 3600 / 60),
    }
}

fn human_size(bytes: u64) -> String {
    let mut size = bytes as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if size < 1024.0 {
            return if unit == "B" {
                format!("{bytes} B")
            } else {
                format!("{size:.1} {unit}")
            };
        }
        size /= 1024.0;
    }
    format!("{size:.1} TB")
}

fn local_time(t: SystemTime) -> String {
    jiff::Timestamp::try_from(t)
        .map(|ts| {
            ts.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            path: path.into(),
            size,
            packed: Some(size / 2),
            modified: None,
            is_dir,
            encrypted: false,
            link: None,
        }
    }

    #[test]
    fn lists_children_with_implicit_dirs() {
        let entries = [
            entry("b.txt", false, 1),
            entry("a/x/y.txt", false, 1),
            entry("c", true, 0),
            entry("a/z.txt", false, 1),
        ];
        let names = |cwd| {
            children(&entries, cwd)
                .into_iter()
                .map(|e| e.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(""), ["a", "c", "b.txt"]);
        assert_eq!(names("a"), ["a/x", "a/z.txt"]);
        assert_eq!(names("a/x"), ["a/x/y.txt"]);
    }

    #[test]
    fn builds_folder_tree() {
        let entries = [
            entry("a/x/y.txt", false, 1),
            entry("c", true, 0),
            entry("b.txt", false, 1),
        ];
        let tree = dir_tree(&entries);
        assert_eq!(tree[""], ["a", "c"]);
        assert_eq!(tree["a"], ["a/x"]);
        assert!(tree["a/x"].is_empty());
    }

    #[test]
    fn sorts_folders_first() {
        let mut rows = vec![
            entry("big", false, 9),
            entry("Dir", true, 0),
            entry("a", false, 5),
            entry("small", false, 1),
        ];
        let names = |rows: &[Entry]| rows.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
        sort_rows(&mut rows, SortKey::Name, false);
        assert_eq!(names(&rows), ["Dir", "a", "big", "small"]);
        sort_rows(&mut rows, SortKey::Size, true);
        assert_eq!(names(&rows), ["Dir", "big", "a", "small"]);
    }

    /// Runs background jobs (and the reload that follows an edit) to completion.
    fn pump(app: &mut App, ctx: &egui::Context) {
        for _ in 0..1000 {
            app.poll_job(ctx);
            if app.job.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("job didn't finish");
    }

    #[test]
    fn file_manager_commands_edit_the_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("t.zip");
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../hdec-core/tests/fixtures/infozip.zip");
        std::fs::copy(fixture, &archive).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.open(&ctx, archive.clone());
        pump(&mut app, &ctx);
        assert!(app.modifiable);
        let original = std::fs::read(&archive).unwrap();
        let has = |app: &App, p: &str| app.entries.iter().any(|e| e.path == p);

        app.navigate("tree".into());
        app.command(
            &ctx,
            Command::Move(vec!["tree/a.txt".into()], "tree/sub".into()),
        );
        pump(&mut app, &ctx);
        if let Dialog::Error(e) = &app.dialog {
            panic!("{e}")
        }
        assert!(
            has(&app, "tree/sub/a.txt") && !has(&app, "tree/a.txt"),
            "{:?}",
            app.entries.iter().map(|e| &e.path).collect::<Vec<_>>()
        );
        assert_eq!(app.cwd, "tree", "stays in the folder after reload");

        app.run(
            &ctx,
            Action::Modify(Edit::CreateDir("tree/fresh".into())),
            None,
        );
        pump(&mut app, &ctx);
        assert!(has(&app, "tree/fresh"));

        app.run(
            &ctx,
            Action::Modify(Edit::Delete(vec!["tree/sub".into()])),
            None,
        );
        pump(&mut app, &ctx);
        assert!(!has(&app, "tree/sub/a.txt") && !has(&app, "tree/sub/b.txt"));

        // Moving a folder into itself is refused with a message, archive untouched.
        app.command(
            &ctx,
            Command::Move(vec!["tree".into()], "tree/fresh".into()),
        );
        pump(&mut app, &ctx);
        assert!(matches!(app.dialog, Dialog::Error(_)));
        assert!(has(&app, "tree/fresh"));
        // Dropped back onto itself (a short drag): nothing happens, no error.
        app.dialog = Dialog::None;
        app.command(
            &ctx,
            Command::Move(vec!["tree/fresh".into()], "tree/fresh".into()),
        );
        assert!(app.job.is_none() && matches!(app.dialog, Dialog::None));

        // Nothing reaches the archive until Save.
        assert_eq!(std::fs::read(&archive).unwrap(), original);
        assert!(app.dirty());
        app.dialog = Dialog::None;
        app.open(&ctx, archive.clone());
        assert!(matches!(app.dialog, Dialog::Unsaved(Then::Open(_))));
        app.dialog = Dialog::None;
        app.save(&ctx);
        pump(&mut app, &ctx);
        assert!(!app.dirty() && app.working.is_none());
        let saved: Vec<String> = hdec_core::list(&archive, &test_ctx())
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        assert!(saved.iter().any(|p| p == "tree/fresh"), "{saved:?}");
        assert!(
            !saved.iter().any(|p| p.starts_with("tree/sub")),
            "{saved:?}"
        );
        assert!(!archive.with_extension("zip.hdec-tmp").exists());
    }

    fn test_ctx() -> Ctx<'static> {
        Ctx {
            password: None,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(|_, _, _| {}),
        }
    }

    #[test]
    fn saves_edited_files_back() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("t.zip");
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../hdec-core/tests/fixtures/infozip.zip");
        std::fs::copy(fixture, &archive).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.open(&ctx, archive.clone());
        pump(&mut app, &ctx);

        // As if `Open` extracted tree/sub/b.txt and an editor then saved it.
        let file = tmp.path().join("edit/tree/sub/b.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "edited").unwrap();
        let entry = "tree/sub/b.txt".to_string();
        app.opened.push(OpenedFile {
            entry: entry.clone(),
            file: file.clone(),
            modified: Some(SystemTime::UNIX_EPOCH),
        });
        app.check_opened();
        let Dialog::SaveBack { entry: e, file: f } = std::mem::take(&mut app.dialog) else {
            panic!("no save prompt")
        };
        app.save_back(&ctx, &e, f);
        pump(&mut app, &ctx);
        app.check_opened();
        assert!(
            matches!(app.dialog, Dialog::None),
            "no second prompt for the same change"
        );

        app.save(&ctx);
        pump(&mut app, &ctx);
        let out = tmp.path().join("out");
        hdec_core::extract(&archive, &out, &ExtractOptions::default(), &test_ctx()).unwrap();
        assert_eq!(std::fs::read(out.join(&entry)).unwrap(), b"edited");
        assert_eq!(app.entries.iter().filter(|e| e.path == entry).count(), 1);

        // Read-only archives say why instead of prompting.
        app.read_only = Some(read_only_reason(Path::new("x.rar"), "RAR"));
        std::fs::write(&file, "again").unwrap();
        app.opened[0].modified = Some(SystemTime::UNIX_EPOCH);
        app.check_opened();
        assert!(matches!(app.dialog, Dialog::None) && app.status.contains("RAR"));
    }

    /// Renders each UI state to `target/ui-shots/*.png` for eyeballing.
    /// Run with `cargo test -p hdec-gui -- --ignored screenshots`.
    #[test]
    #[ignore = "needs a GPU; writes PNGs for manual review"]
    fn screenshots() {
        use egui_kittest::Harness;
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../hdec-core/tests/fixtures/rar5.rar");
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-shots");
        std::fs::create_dir_all(&out).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = Ctx {
            password: None,
            cancel,
            progress: Arc::new(|_, _, _| {}),
        };
        let entries = hdec_core::list(&fixture, &ctx).unwrap();

        type Setup = fn(&mut App, &egui::Context, &Path, &[Entry]);
        let opened: Setup = |app, ctx, path, entries| {
            app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
            app.navigate("tree/sub".into());
            app.selected.insert("tree/sub/b.txt".into());
        };
        let states: [(&str, Setup); 13] = [
            ("saveback", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.dialog = Dialog::SaveBack {
                    entry: "tree/sub/b.txt".into(),
                    file: PathBuf::new(),
                };
            }),
            ("links", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.navigate("tree".into());
            }),
            ("newfolder", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.modifiable = true; // pretend: shows the edit toolbar enabled
                app.navigate("tree".into());
                app.selected.insert("tree/a.txt".into());
                app.command(ctx, Command::NewFolder);
            }),
            ("empty", |_, _, _, _| {}),
            ("opened", opened),
            ("extract", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.start_extract();
            }),
            ("create", |app, _, path, _| {
                app.new_archive(vec![
                    path.parent().unwrap().to_path_buf(),
                    path.to_path_buf(),
                ]);
                if let Dialog::Create(form) = &mut app.dialog {
                    form.format = Format::SevenZ;
                    *form.password = "secret".into();
                    *form.confirm = "secret".into();
                    form.split = true;
                }
            }),
            ("password", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.dialog = Dialog::Password {
                    input: Default::default(),
                    then: Action::Test,
                    wrong: true,
                    show: false,
                };
            }),
            ("still_copying", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.dialog = Dialog::StillCopying;
            }),
            ("progress", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                let (_tx, rx) = mpsc::channel();
                let progress = (650 << 20, 1000 << 20, "tree/big.bin".into());
                let cancel = Arc::new(AtomicBool::new(false));
                app.job = Some(Job {
                    rx,
                    cancel,
                    progress,
                    verb: "Extracting",
                    icon: icon::EXPORT,
                    subject: file_name(path),
                    started: Instant::now() - Duration::from_secs(14),
                    retry: None,
                    password: None,
                });
            }),
            ("unsaved", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("x");
                app.working = Some(Working {
                    _dir: dir,
                    path,
                    dirty: true,
                });
                app.dialog = Dialog::Unsaved(Then::Close);
            }),
            ("picker", |app, _, path, _| {
                let dir = path
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .canonicalize()
                    .unwrap();
                app.picker = Some((picker::Picker::new(dir), PickFor::Add));
            }),
            ("filter", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.filter = "txt".into();
                app.refresh_rows();
            }),
        ];
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            for (name, setup) in states {
                let mut harness = Harness::builder()
                    .with_size(WINDOW)
                    .with_theme(theme)
                    .build_eframe(|cc| {
                        theme::install(&cc.egui_ctx);
                        let mut app = App::default();
                        setup(&mut app, &cc.egui_ctx, &fixture, &entries);
                        app
                    });
                harness.run_steps(4);
                let suffix = if theme == egui::Theme::Dark {
                    "dark"
                } else {
                    "light"
                };
                harness
                    .render()
                    .unwrap()
                    .save(out.join(format!("{name}_{suffix}.png")))
                    .unwrap();
            }
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(archive_stem(Path::new("/x/photos.tar.gz")), "photos");
        assert_eq!(archive_stem(Path::new(".hidden")), "archive");
        assert_eq!(
            archive_stem(Path::new("Report 1.5 final.zip")),
            "Report 1.5 final"
        );
        assert_eq!(archive_stem(Path::new("set.part02.rar")), "set");
        assert_eq!(archive_stem(Path::new("set.r01")), "set");
        assert_eq!(archive_stem(Path::new("data.7z.003")), "data");
        assert_eq!(archive_stem(Path::new("A.TGZ")), "A");
        assert_eq!(archive_stem(Path::new("noext")), "noext");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(base_name("a/b/c.txt"), "c.txt");
        assert_eq!(base_name("dir/"), "dir");
    }
}
