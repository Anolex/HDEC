use eframe::egui;
use egui_extras::{Column, TableBuilder};
use hdec_core::{CreateOptions, Ctx, Entry, Error, ExtractOptions, Format, Level, Overwrite};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;
use zeroize::Zeroizing;

/// Formats offered in the Create dialog (grows as core gains writers).
const CREATE_FORMATS: &[Format] = &[Format::Zip];

fn main() -> eframe::Result {
    let mut app = App::default();
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    eframe::run_native(
        "HDEC",
        eframe::NativeOptions::default(),
        Box::new(move |cc| {
            if let Some(path) = path {
                app.run(&cc.egui_ctx, Action::Open(path), None);
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
}

enum Outcome {
    Opened(PathBuf, Vec<Entry>),
    Extracted(PathBuf, Vec<String>),
    Tested,
    Created(PathBuf),
}

enum Msg {
    Progress(u64, u64, String),
    Done(hdec_core::Result<Outcome>),
}

struct Job {
    rx: mpsc::Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    progress: (u64, u64, String),
    /// What to retry with a password if this job reports one is needed.
    retry: Option<Action>,
    password: Password,
}

#[derive(Default)]
enum Dialog {
    #[default]
    None,
    Message(String),
    Password {
        input: Zeroizing<String>,
        then: Action,
        wrong: bool,
    },
    Extract {
        dest: String,
        subfolder: bool,
        overwrite: Overwrite,
    },
    Create(CreateForm),
}

struct CreateForm {
    sources: Vec<PathBuf>,
    dest: String,
    format: Format,
    level: Level,
    zip_crypto: bool,
    password: Zeroizing<String>,
    confirm: Zeroizing<String>,
}

#[derive(Default)]
struct App {
    archive: Option<PathBuf>,
    entries: Vec<Entry>,
    /// Folder inside the archive being shown; "" = root.
    cwd: String,
    rows: Vec<Entry>,
    selected: BTreeSet<String>,
    /// Password that worked for the open archive.
    password: Password,
    job: Option<Job>,
    dialog: Dialog,
    status: String,
}

impl App {
    fn run(&mut self, ctx: &egui::Context, action: Action, password: Password) {
        let archive = self.archive.clone();
        let retry = Some(action.clone());
        self.spawn(ctx, retry, password, move |c| match action {
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
        });
    }

    fn spawn(
        &mut self,
        ctx: &egui::Context,
        retry: Option<Action>,
        password: Password,
        work: impl FnOnce(&Ctx) -> hdec_core::Result<Outcome> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (thread_cancel, thread_password, egui_ctx) =
            (cancel.clone(), password.clone(), ctx.clone());
        std::thread::spawn(move || {
            let progress = |done, total, name: &str| {
                let _ = tx.send(Msg::Progress(done, total, name.to_owned()));
                egui_ctx.request_repaint();
            };
            let c = Ctx {
                password: thread_password.as_deref().map(String::as_str),
                cancel: &thread_cancel,
                progress: &progress,
            };
            let _ = tx.send(Msg::Done(work(&c)));
            egui_ctx.request_repaint();
        });
        self.job = Some(Job {
            rx,
            cancel,
            progress: (0, 0, String::new()),
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
                };
            }
            Err(Error::Cancelled) => self.status = "Cancelled".into(),
            Err(e) => self.dialog = Dialog::Message(format!("Error: {e}")),
        }
    }

    fn finish(&mut self, ctx: &egui::Context, outcome: Outcome) {
        match outcome {
            Outcome::Opened(path, entries) => {
                let format = hdec_core::detect(&path).map(Format::name).unwrap_or("?");
                self.status = format!("{} — {format}, {} entries", path.display(), entries.len());
                let title = path.file_name().unwrap_or_default().to_string_lossy();
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{title} — HDEC")));
                self.archive = Some(path);
                self.entries = entries;
                self.navigate(String::new());
            }
            Outcome::Extracted(dest, warnings) => {
                self.status = format!("Extracted to {}", dest.display());
                if !warnings.is_empty() {
                    self.dialog = Dialog::Message(format!(
                        "Extracted with warnings:\n\n{}",
                        warnings.join("\n")
                    ));
                }
            }
            Outcome::Tested => self.dialog = Dialog::Message("No errors found.".into()),
            Outcome::Created(path) => self.open(ctx, path),
        }
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.password = None;
        self.run(ctx, Action::Open(path), None);
    }

    fn navigate(&mut self, cwd: String) {
        self.rows = children(&self.entries, &cwd);
        self.cwd = cwd;
        self.selected.clear();
    }

    fn new_archive(&mut self, sources: Vec<PathBuf>) {
        if sources.is_empty() {
            return;
        }
        let format = CREATE_FORMATS[0];
        let first = &sources[0];
        let stem = if sources.len() == 1 {
            first.file_name()
        } else {
            first.parent().and_then(Path::file_name)
        };
        let stem = stem
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".into());
        let dest = first.with_file_name(format!("{stem}.{}", format.extension()));
        self.dialog = Dialog::Create(CreateForm {
            sources,
            dest: dest.display().to_string(),
            format,
            level: Level::Normal,
            zip_crypto: false,
            password: Default::default(),
            confirm: Default::default(),
        });
    }

    fn create(&mut self, ctx: &egui::Context, form: CreateForm) {
        let dest = PathBuf::from(&form.dest);
        let opts = CreateOptions {
            format: form.format,
            level: form.level,
            zip_crypto: form.zip_crypto,
        };
        let password = (!form.password.is_empty()).then(|| form.password.clone());
        self.status = format!("Creating {}", dest.display());
        self.spawn(ctx, None, password, move |c| {
            hdec_core::create(&form.sources, &dest, &opts, c)?;
            Ok(Outcome::Created(dest))
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let idle = self.job.is_none();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(idle, |ui| {
                if ui.button("Open…").clicked()
                    && let Some(p) = rfd::FileDialog::new().pick_file()
                {
                    self.open(ui.ctx(), p);
                }
                if ui.button("New from files…").clicked() {
                    self.new_archive(rfd::FileDialog::new().pick_files().unwrap_or_default());
                }
                if ui.button("New from folder…").clicked() {
                    self.new_archive(rfd::FileDialog::new().pick_folder().into_iter().collect());
                }
                ui.separator();
                let has_archive = self.archive.is_some();
                if ui
                    .add_enabled(has_archive, egui::Button::new("Extract…"))
                    .clicked()
                {
                    let archive = self.archive.as_deref().unwrap();
                    self.dialog = Dialog::Extract {
                        dest: archive
                            .parent()
                            .unwrap_or(Path::new("."))
                            .display()
                            .to_string(),
                        subfolder: true,
                        overwrite: Overwrite::Skip,
                    };
                }
                if ui
                    .add_enabled(has_archive, egui::Button::new("Test"))
                    .clicked()
                {
                    self.run(ui.ctx(), Action::Test, self.password.clone());
                }
            });
        });
    }

    fn breadcrumb(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.cwd.is_empty(), egui::Button::new("⬆"))
                .clicked()
            {
                let up = self
                    .cwd
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent)
                    .to_owned();
                self.navigate(up);
            }
            if ui.link("/").clicked() {
                self.navigate(String::new());
            }
            let mut path = String::new();
            for part in self.cwd.clone().split('/').filter(|p| !p.is_empty()) {
                path = if path.is_empty() {
                    part.to_owned()
                } else {
                    format!("{path}/{part}")
                };
                if ui.link(part).clicked() {
                    self.navigate(path.clone());
                }
                ui.label("/");
            }
        });
    }

    fn table(&mut self, ui: &mut egui::Ui) {
        let mut go_into = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::remainder())
            .columns(Column::auto().at_least(90.0), 3)
            .header(20.0, |mut header| {
                for title in ["Name", "Size", "Packed", "Modified"] {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|body| {
                body.rows(20.0, self.rows.len(), |mut row| {
                    let e = &self.rows[row.index()];
                    row.set_selected(self.selected.contains(&e.path));
                    let name = e.path.rsplit('/').next().unwrap_or(&e.path);
                    let icon = if e.is_dir {
                        "📁"
                    } else if e.encrypted {
                        "🔒"
                    } else {
                        "📄"
                    };
                    row.col(|ui| {
                        ui.label(format!("{icon} {name}"));
                    });
                    row.col(|ui| {
                        if !e.is_dir {
                            ui.label(human_size(e.size));
                        }
                    });
                    row.col(|ui| {
                        if let (false, Some(p)) = (e.is_dir, e.packed) {
                            ui.label(human_size(p));
                        }
                    });
                    row.col(|ui| {
                        ui.label(e.modified.map(local_time).unwrap_or_default());
                    });
                    let response = row.response();
                    if response.double_clicked() && e.is_dir {
                        go_into = Some(e.path.clone());
                    } else if response.clicked() {
                        let multi = response.ctx.input(|i| i.modifiers.command);
                        if !multi {
                            self.selected.clear();
                        }
                        if !self.selected.remove(&e.path) {
                            self.selected.insert(e.path.clone());
                        }
                    }
                });
            });
        if let Some(dir) = go_into {
            self.navigate(dir);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| match &self.job {
            Some(job) => {
                let (done, total, name) = &job.progress;
                let fraction = if *total > 0 {
                    *done as f32 / *total as f32
                } else {
                    0.0
                };
                if ui.button("Cancel").clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .text(name.as_str())
                        .show_percentage(),
                );
            }
            None => {
                ui.label(&self.status);
            }
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let dialog = std::mem::take(&mut self.dialog);
        self.dialog = match dialog {
            Dialog::None => Dialog::None,
            Dialog::Message(text) => {
                let mut open = true;
                modal(ctx, |ui| {
                    ui.label(&text);
                    open = !ui.button("OK").clicked();
                });
                if open {
                    Dialog::Message(text)
                } else {
                    Dialog::None
                }
            }
            Dialog::Password {
                mut input,
                then,
                wrong,
            } => {
                let mut result = None;
                modal(ctx, |ui| {
                    ui.heading("Password required");
                    if wrong {
                        ui.colored_label(ui.visuals().error_fg_color, "Wrong password, try again.");
                    }
                    let field = ui.add(egui::TextEdit::singleline(&mut *input).password(true));
                    field.request_focus();
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() || enter {
                            result = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            result = Some(false);
                        }
                    });
                });
                match result {
                    Some(true) => {
                        self.run(ctx, then, Some(input));
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::Password { input, then, wrong },
                }
            }
            Dialog::Extract {
                mut dest,
                mut subfolder,
                mut overwrite,
            } => {
                let mut result = None;
                modal(ctx, |ui| {
                    ui.heading(if self.selected.is_empty() {
                        "Extract all"
                    } else {
                        "Extract selected"
                    });
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut dest);
                        if ui.button("Browse…").clicked()
                            && let Some(p) =
                                rfd::FileDialog::new().set_directory(&dest).pick_folder()
                        {
                            dest = p.display().to_string();
                        }
                    });
                    ui.checkbox(&mut subfolder, "Into a subfolder named after the archive");
                    ui.horizontal(|ui| {
                        ui.label("Existing files:");
                        ui.radio_value(&mut overwrite, Overwrite::Skip, "Skip");
                        ui.radio_value(&mut overwrite, Overwrite::Replace, "Replace");
                        ui.radio_value(&mut overwrite, Overwrite::Rename, "Keep both");
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Extract").clicked() {
                            result = Some(true);
                        }
                        if ui.button("Cancel").clicked() {
                            result = Some(false);
                        }
                    });
                });
                match result {
                    Some(true) => {
                        let mut target = PathBuf::from(&dest);
                        if subfolder {
                            target.push(archive_stem(self.archive.as_deref().unwrap()));
                        }
                        let only = (!self.selected.is_empty())
                            .then(|| self.selected.iter().cloned().collect());
                        self.run(
                            ctx,
                            Action::Extract(target, ExtractOptions { only, overwrite }),
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
                let mut result = None;
                modal(ctx, |ui| create_form(ui, &mut form, &mut result));
                match result {
                    Some(true) => {
                        self.create(ctx, form);
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::Create(form),
                }
            }
        };
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() && self.job.is_none() {
            match dropped.as_slice() {
                [one] if one.is_file() && hdec_core::detect(one).is_ok() => {
                    self.open(&ctx, one.clone())
                }
                _ => self.new_archive(dropped),
            }
        }
        egui::Panel::top("toolbar").show(ui, |ui| {
            self.toolbar(ui);
            if self.archive.is_some() {
                self.breadcrumb(ui);
            }
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if self.archive.is_some() {
                self.table(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Drop an archive to open it, or files to create one.")
                });
            }
        });
        self.dialogs(&ctx);
    }
}

fn create_form(ui: &mut egui::Ui, form: &mut CreateForm, result: &mut Option<bool>) {
    ui.heading("New archive");
    ui.label(format!("{} item(s)", form.sources.len()));
    ui.horizontal(|ui| {
        ui.text_edit_singleline(&mut form.dest);
        if ui.button("Browse…").clicked()
            && let Some(p) = rfd::FileDialog::new().set_file_name(&form.dest).save_file()
        {
            form.dest = p.display().to_string();
        }
    });
    let old_ext = form.format.extension();
    egui::ComboBox::from_label("Format")
        .selected_text(form.format.name())
        .show_ui(ui, |ui| {
            for f in CREATE_FORMATS {
                ui.selectable_value(&mut form.format, *f, f.name());
            }
        });
    if let Some(stem) = form.dest.strip_suffix(&format!(".{old_ext}")) {
        form.dest = format!("{stem}.{}", form.format.extension());
    }
    ui.horizontal(|ui| {
        ui.label("Compression:");
        for (level, label) in [
            (Level::Store, "Store"),
            (Level::Fast, "Fast"),
            (Level::Normal, "Normal"),
            (Level::Max, "Max"),
        ] {
            ui.radio_value(&mut form.level, level, label);
        }
    });
    egui::Grid::new("password").show(ui, |ui| {
        ui.label("Password:");
        ui.add(egui::TextEdit::singleline(&mut *form.password).password(true));
        ui.end_row();
        ui.label("Confirm:");
        ui.add(egui::TextEdit::singleline(&mut *form.confirm).password(true));
        ui.end_row();
    });
    let mismatch = form.password != form.confirm;
    if mismatch {
        ui.colored_label(ui.visuals().error_fg_color, "Passwords don't match.");
    }
    if form.format == Format::Zip && !form.password.is_empty() {
        ui.checkbox(
            &mut form.zip_crypto,
            "Legacy ZipCrypto (weak; opens in Explorer / Archive Utility)",
        );
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !mismatch && !form.dest.is_empty(),
                egui::Button::new("Create"),
            )
            .clicked()
        {
            *result = Some(true);
        }
        if ui.button("Cancel").clicked() {
            *result = Some(false);
        }
    });
}

fn modal(ctx: &egui::Context, add: impl FnOnce(&mut egui::Ui)) {
    egui::Modal::new(egui::Id::new("dialog")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        add(ui);
    });
}

/// Direct children of `cwd`, folders first. Folders that exist only implicitly
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
                });
            }
            None if e.is_dir => {
                dirs.insert(rest.to_owned(), e.clone());
            }
            None => files.push(e.clone()),
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    dirs.into_values().chain(files).collect()
}

/// `photos.tar.gz` → `photos`
fn archive_stem(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let stem = name.split_once('.').map_or(&*name, |(s, _)| s);
    if stem.is_empty() {
        "archive".into()
    } else {
        stem.into()
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

    fn entry(path: &str, is_dir: bool) -> Entry {
        Entry {
            path: path.into(),
            size: 0,
            packed: None,
            modified: None,
            is_dir,
            encrypted: false,
        }
    }

    #[test]
    fn lists_children_with_implicit_dirs() {
        let entries = [
            entry("b.txt", false),
            entry("a/x/y.txt", false),
            entry("c", true),
            entry("a/z.txt", false),
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
    fn helpers() {
        assert_eq!(archive_stem(Path::new("/x/photos.tar.gz")), "photos");
        assert_eq!(archive_stem(Path::new(".hidden")), "archive");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
    }
}
