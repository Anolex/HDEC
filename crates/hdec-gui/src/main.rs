// No console window behind the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;

use eframe::egui::{self, Align, Key, Layout, Margin, RichText, Sense, vec2};
use egui_extras::{Column, TableBuilder};
use hdec_core::{CreateOptions, Ctx, Edit, Entry, Error, ExtractOptions, Format, Level, Overwrite};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;
use theme::{icon, pal};
use zeroize::Zeroizing;

/// Formats offered in the New archive dialog.
const CREATE_FORMATS: &[Format] = &[
    Format::Zip,
    Format::SevenZ,
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

fn main() -> eframe::Result {
    let mut app = App::default();
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 640.0])
            .with_min_inner_size([640.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "HDEC",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
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
}

enum Outcome {
    Opened(PathBuf, Vec<Entry>),
    Extracted(PathBuf, Vec<String>),
    Tested,
    Created(PathBuf),
    Modified,
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
    ConfirmDelete(Vec<String>),
    Rename {
        from: String,
        name: String,
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
}

struct CreateForm {
    sources: Vec<PathBuf>,
    dest: String,
    format: Format,
    level: Level,
    zip_crypto: bool,
    password: Zeroizing<String>,
    confirm: Zeroizing<String>,
    show: bool,
    split: bool,
    split_mb: String,
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

/// Context-menu commands on a table row.
enum RowAction {
    Extract,
    Rename,
    Delete,
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
}

impl App {
    fn run(&mut self, ctx: &egui::Context, action: Action, password: Password) {
        let archive = self.archive.clone();
        let (verb, icon) = match &action {
            Action::Open(_) => ("Opening", icon::FOLDER_OPEN),
            Action::Extract(..) => ("Extracting", icon::EXPORT),
            Action::Test => ("Testing", icon::CHECK_CIRCLE),
            Action::Modify(_) => ("Updating", icon::PENCIL_SIMPLE),
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
                hdec_core::modify(archive.as_deref().unwrap(), &edit, c).map(|_| Outcome::Modified)
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
                    show: false,
                };
            }
            Err(Error::Cancelled) => self.status = "Cancelled".into(),
            Err(e) => self.dialog = Dialog::Error(e.to_string()),
        }
    }

    fn finish(&mut self, ctx: &egui::Context, outcome: Outcome) {
        match outcome {
            Outcome::Opened(path, entries) => {
                self.format = hdec_core::detect(&path).map(Format::name).unwrap_or("?");
                let title = path.file_name().unwrap_or_default().to_string_lossy();
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{title} — HDEC")));
                let reopened = self.archive.as_ref() == Some(&path);
                self.modifiable = hdec_core::modifiable(&path);
                self.total_size = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
                self.any_encrypted = entries.iter().any(|e| e.encrypted);
                self.dirs = dir_tree(&entries);
                self.archive = Some(path);
                self.entries = entries;
                self.status.clear();
                if !reopened {
                    self.expanded.clear();
                    self.filter.clear();
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
                let path = self.archive.clone().unwrap();
                self.run(ctx, Action::Open(path), self.password.clone());
            }
        }
    }

    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.password = None;
        self.run(ctx, Action::Open(path), None);
    }

    fn navigate(&mut self, cwd: String) {
        let mut parent = cwd.as_str();
        while let Some((p, _)) = parent.rsplit_once('/') {
            self.expanded.insert(p.to_owned());
            parent = p;
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
            show: false,
            split: false,
            split_mb: "100".into(),
        });
    }

    fn create(&mut self, ctx: &egui::Context, form: CreateForm) {
        let dest = PathBuf::from(&form.dest);
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
            let new = theme::tool_button(ui, icon::PLUS_SQUARE, "New", idle, false);
            egui::Popup::menu(&new).show(|ui| {
                if ui
                    .button(format!("{}  From files…", icon::FILE_PLUS))
                    .clicked()
                {
                    self.new_archive(rfd::FileDialog::new().pick_files().unwrap_or_default());
                }
                if ui
                    .button(format!("{}  From folder…", icon::FOLDER_PLUS))
                    .clicked()
                {
                    self.new_archive(rfd::FileDialog::new().pick_folder().into_iter().collect());
                }
            });
            theme::tool_separator(ui);
            if theme::tool_button(ui, icon::EXPORT, "Extract", has, true).clicked() {
                self.start_extract();
            }
            if theme::tool_button(ui, icon::CHECK_CIRCLE, "Test", has, false).clicked() {
                self.run(ui.ctx(), Action::Test, self.password.clone());
            }
            theme::tool_separator(ui);
            let add = theme::tool_button(ui, icon::FILE_PLUS, "Add", can_edit, false);
            egui::Popup::menu(&add).show(|ui| {
                if ui.button(format!("{}  Files…", icon::FILE_PLUS)).clicked()
                    && let Some(files) = rfd::FileDialog::new().pick_files()
                {
                    self.add(ui.ctx(), files);
                }
                if ui
                    .button(format!("{}  Folder…", icon::FOLDER_PLUS))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    self.add(ui.ctx(), vec![dir]);
                }
            });
            if theme::tool_button(
                ui,
                icon::TRASH,
                "Delete",
                can_edit && !self.selected.is_empty(),
                false,
            )
            .clicked()
            {
                self.start_delete();
            }
            if theme::tool_button(
                ui,
                icon::PENCIL_SIMPLE,
                "Rename",
                can_edit && self.selected.len() == 1,
                false,
            )
            .clicked()
            {
                self.start_rename();
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
            if crumb(
                ui,
                format!("{}  {name}", icon::FILE_ARCHIVE),
                self.cwd.is_empty(),
            )
            .clicked()
            {
                self.navigate(String::new());
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
                if crumb(ui, part.clone(), i + 1 == parts.len()).clicked() {
                    self.navigate(path.clone());
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
        let mut go_into = None;
        let mut sort_by = None;
        let mut menu_action = None;
        let (command, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
        TableBuilder::new(ui)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::exact(86.0))
            .column(Column::exact(86.0))
            .column(Column::exact(58.0))
            .column(Column::exact(136.0))
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
                        ui.add(
                            egui::Label::new(RichText::new(name).color(p.text))
                                .truncate()
                                .selectable(false),
                        );
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
                            if e.is_dir {
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
                                .filter(|_| !e.is_dir)
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
                    if response.double_clicked() && e.is_dir {
                        go_into = Some(e.path.clone());
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
                        if e.is_dir && ui.button(format!("{}  Open", icon::FOLDER_OPEN)).clicked() {
                            go_into = Some(e.path.clone());
                        }
                        if ui.button(format!("{}  Extract…", icon::EXPORT)).clicked() {
                            menu_action = Some(RowAction::Extract);
                        }
                        if self.modifiable {
                            ui.separator();
                            if ui
                                .button(format!("{}  Rename…", icon::PENCIL_SIMPLE))
                                .clicked()
                            {
                                menu_action = Some(RowAction::Rename);
                            }
                            if ui
                                .button(
                                    RichText::new(format!("{}  Delete", icon::TRASH))
                                        .color(p.danger),
                                )
                                .clicked()
                            {
                                menu_action = Some(RowAction::Delete);
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
        match menu_action {
            Some(RowAction::Extract) => self.start_extract(),
            Some(RowAction::Rename) => self.start_rename(),
            Some(RowAction::Delete) => self.start_delete(),
            None => {}
        }
        if let Some(dir) = go_into {
            self.navigate(dir);
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
                        self.new_archive(rfd::FileDialog::new().pick_files().unwrap_or_default());
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
                theme::progress_bar(ui, fraction, 300.0);
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
        let text = if self.modifiable {
            "Drop to open an archive or add files"
        } else {
            "Drop to open or create an archive"
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
            || ctx.memory(|m| m.focused().is_some())
        {
            return;
        }
        let (up, all, delete, enter) = ctx.input(|i| {
            (
                i.key_pressed(Key::Backspace) && !i.modifiers.command,
                i.key_pressed(Key::A) && i.modifiers.command,
                i.key_pressed(Key::Delete)
                    || (i.key_pressed(Key::Backspace) && i.modifiers.command),
                i.key_pressed(Key::Enter),
            )
        });
        if up && !self.cwd.is_empty() {
            self.go_up();
        }
        if all {
            self.selected = self.rows.iter().map(|r| r.path.clone()).collect();
        }
        if delete {
            self.start_delete();
        }
        if enter
            && let [dir] = self
                .rows
                .iter()
                .filter(|r| r.is_dir && self.selected.contains(&r.path))
                .collect::<Vec<_>>()
                .as_slice()
        {
            let dir = dir.path.clone();
            self.navigate(dir);
        }
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
        let dialog = std::mem::take(&mut self.dialog);
        self.dialog = match dialog {
            Dialog::None => Dialog::None,
            Dialog::Info(text) if message(ctx, icon::CHECK_CIRCLE, "Done", &text) => Dialog::None,
            Dialog::Error(text) if message(ctx, icon::WARNING, "Something went wrong", &text) => {
                Dialog::None
            }
            open @ (Dialog::Info(_) | Dialog::Error(_)) => open,
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
                let result = theme::dialog(ctx, icon::PENCIL_SIMPLE, "Rename", &from, |ui| {
                    let field =
                        ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY));
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    let valid =
                        !name.is_empty() && !name.contains('/') && name != "." && name != "..";
                    let mut result = None;
                    theme::dialog_footer(ui, |ui| {
                        if theme::primary_button(ui, "Rename", valid).clicked() || (enter && valid)
                        {
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
                        let to = match from.rsplit_once('/') {
                            Some((parent, _)) => format!("{parent}/{name}"),
                            None => name,
                        };
                        self.run(
                            ctx,
                            Action::Modify(Edit::Rename { from, to }),
                            self.password.clone(),
                        );
                        Dialog::None
                    }
                    Some(false) => Dialog::None,
                    None => Dialog::Rename { from, name },
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
                let subtitle = match form.sources.as_slice() {
                    [one] => base_name(&one.to_string_lossy()).to_owned(),
                    [first, rest @ ..] => format!(
                        "{} and {} more",
                        base_name(&first.to_string_lossy()),
                        rest.len()
                    ),
                    [] => String::new(),
                };
                let result =
                    theme::dialog(ctx, icon::PLUS_SQUARE, "New archive", &subtitle, |ui| {
                        create_form(ui, &mut form)
                    });
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
        let p = pal(&ctx);
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
                _ if self.modifiable => self.add(&ctx, dropped),
                _ => self.new_archive(dropped),
            }
        }
        self.shortcuts(&ctx);

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
                .default_size(210.0)
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
        self.dialogs(&ctx);
        self.drop_overlay(&ctx);
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
    let old_ext = form.format.extension();
    let mut mismatch = false;
    egui::Grid::new("create")
        .num_columns(2)
        .spacing(vec2(12.0, 10.0))
        .show(ui, |ui| {
            theme::form_label(ui, "Save as");
            let dest = form.dest.clone();
            path_field(ui, &mut form.dest, || {
                rfd::FileDialog::new()
                    .set_file_name(base_name(&dest))
                    .save_file()
            });
            ui.end_row();

            theme::form_label(ui, "Format");
            egui::ComboBox::from_id_salt("format")
                .selected_text(form.format.name())
                .width(140.0)
                .show_ui(ui, |ui| {
                    for f in CREATE_FORMATS.iter().filter(|f| fits(**f, &form.sources)) {
                        ui.selectable_value(&mut form.format, *f, f.name());
                    }
                });
            ui.end_row();

            if form.format != Format::Tar {
                theme::form_label(ui, "Compression");
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
                ui.end_row();
            }

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
                    (_, _, true) => RichText::new("Leave empty for no encryption.").color(p.faint),
                    (Format::SevenZ, ..) => {
                        RichText::new("AES-256. File names are encrypted too.").color(p.faint)
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
    if let Some(stem) = form.dest.strip_suffix(&format!(".{old_ext}")) {
        form.dest = format!("{stem}.{}", form.format.extension());
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
    let mut result = None;
    theme::dialog_footer(ui, |ui| {
        if theme::primary_button(
            ui,
            "Create",
            !mismatch && !bad_split && !form.dest.is_empty(),
        )
        .clicked()
        {
            result = Some(true);
        }
        if theme::button(ui, "Cancel").clicked() {
            result = Some(false);
        }
    });
    result
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
    (!e.is_dir && e.size > 0).then_some(e.packed? as f64 / e.size as f64)
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

    fn entry(path: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            path: path.into(),
            size,
            packed: Some(size / 2),
            modified: None,
            is_dir,
            encrypted: false,
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
        let states: [(&str, Setup); 7] = [
            ("empty", |_, _, _, _| {}),
            ("opened", opened),
            ("extract", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                app.start_extract();
            }),
            ("create", |app, _, path, _| {
                app.new_archive(vec![path.to_path_buf(), path.with_file_name("rar4.rar")]);
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
            ("progress", |app, ctx, path, entries| {
                app.finish(ctx, Outcome::Opened(path.to_path_buf(), entries.to_vec()));
                let (_tx, rx) = mpsc::channel();
                let progress = (620, 1000, "tree/big.bin".into());
                let cancel = Arc::new(AtomicBool::new(false));
                app.job = Some(Job {
                    rx,
                    cancel,
                    progress,
                    verb: "Extracting",
                    icon: icon::EXPORT,
                    retry: None,
                    password: None,
                });
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
                    .with_size(vec2(1040.0, 640.0))
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
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(base_name("a/b/c.txt"), "c.txt");
        assert_eq!(base_name("dir/"), "dir");
    }
}
