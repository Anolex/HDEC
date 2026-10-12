//! "Files and folders" picker for Windows, whose native dialog can't choose both
//! at once. macOS uses NSOpenPanel through rfd instead. Double-click opens a
//! folder or picks a file; Open picks everything selected, folders included.

use crate::theme::{self, icon, pal};
use eframe::egui::{self, Align, Key, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

struct Item {
    name: String,
    is_dir: bool,
    size: u64,
}

pub struct Picker {
    dir: PathBuf,
    typed: String,
    items: Vec<Item>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    error: Option<String>,
}

pub enum Picked {
    Open(Vec<PathBuf>),
    Cancel,
}

impl Picker {
    pub fn new(start: PathBuf) -> Self {
        let mut picker = Self {
            dir: start.clone(),
            typed: String::new(),
            items: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            error: None,
        };
        picker.navigate(start);
        picker
    }

    fn navigate(&mut self, dir: PathBuf) {
        match fs::read_dir(&dir) {
            Ok(read) => {
                let mut items: Vec<Item> = read
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        // Follows symlinks; broken ones are left out.
                        let meta = fs::metadata(e.path()).ok()?;
                        (!hidden(&name, &meta)).then_some(Item {
                            name,
                            is_dir: meta.is_dir(),
                            size: meta.len(),
                        })
                    })
                    .collect();
                items.sort_by_key(|i| (!i.is_dir, i.name.to_lowercase()));
                self.items = items;
                self.dir = dir;
                self.selected.clear();
                self.anchor = None;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e}", dir.display())),
        }
        self.typed = self.dir.display().to_string();
    }

    /// Draws the picker; returns the outcome once the user is done.
    pub fn show(&mut self, ctx: &egui::Context, title: &str) -> Option<Picked> {
        let p = pal(ctx);
        theme::dialog_with(ctx, "picker", 620.0, icon::PLUS_SQUARE, title, "", |ui| {
            let mut go = None;
            let mut result = None;
            ui.horizontal(|ui| {
                let up = self.dir.parent().map(Path::to_path_buf);
                if theme::icon_button(ui, icon::ARROW_UP, up.is_some())
                    .on_hover_text("Up")
                    .clicked()
                {
                    go = up;
                }
                let places = theme::icon_button(ui, icon::HOUSE, true).on_hover_text("Places");
                egui::Popup::menu(&places).show(|ui| {
                    for (name, path) in places_list() {
                        if ui.button(name).clicked() {
                            go = Some(path);
                        }
                    }
                });
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.typed).desired_width(ui.available_width()),
                );
                if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    go = Some(PathBuf::from(self.typed.trim()));
                }
            });
            if let Some(err) = &self.error {
                ui.label(RichText::new(err).size(11.5).color(p.danger));
            }
            ui.add_space(4.0);

            let (command, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
            egui::Frame::new()
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(6)
                .show(ui, |ui| {
                    TableBuilder::new(ui)
                        .id_salt(&self.dir)
                        .sense(Sense::click())
                        .cell_layout(Layout::left_to_right(Align::Center))
                        .min_scrolled_height(320.0)
                        .max_scroll_height(320.0)
                        .auto_shrink(false)
                        .column(Column::remainder().clip(true))
                        .column(Column::exact(86.0))
                        .body(|body| {
                            body.rows(26.0, self.items.len(), |mut row| {
                                let i = row.index();
                                let item = &self.items[i];
                                row.set_selected(self.selected.contains(&i));
                                row.col(|ui| {
                                    ui.add_space(4.0);
                                    if item.is_dir {
                                        ui.label(
                                            theme::fill(icon::FOLDER).size(16.0).color(p.folder),
                                        );
                                    } else {
                                        ui.label(
                                            RichText::new(theme::file_icon(&item.name))
                                                .size(16.0)
                                                .color(p.muted),
                                        );
                                    }
                                    ui.add(
                                        egui::Label::new(&item.name).truncate().selectable(false),
                                    );
                                });
                                row.col(|ui| {
                                    if !item.is_dir {
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.add_space(8.0);
                                                ui.label(
                                                    RichText::new(crate::human_size(item.size))
                                                        .color(p.muted),
                                                );
                                            },
                                        );
                                    }
                                });
                                let response = row.response();
                                if response.double_clicked() {
                                    let path = self.dir.join(&item.name);
                                    if item.is_dir {
                                        go = Some(path);
                                    } else {
                                        result = Some(Picked::Open(vec![path]));
                                    }
                                } else if response.clicked() {
                                    match (shift, self.anchor) {
                                        (true, Some(a)) => {
                                            self.selected = (a.min(i)..=a.max(i)).collect();
                                        }
                                        _ if command => {
                                            if !self.selected.remove(&i) {
                                                self.selected.insert(i);
                                            }
                                            self.anchor = Some(i);
                                        }
                                        _ => {
                                            self.selected = BTreeSet::from([i]);
                                            self.anchor = Some(i);
                                        }
                                    }
                                }
                            });
                        });
                });

            let open = !self.selected.is_empty();
            theme::dialog_footer(ui, |ui| {
                if theme::primary_button(ui, "Open", open).clicked() {
                    result = Some(Picked::Open(self.paths()));
                }
                if theme::button(ui, "Cancel").clicked() {
                    result = Some(Picked::Cancel);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let hint = match self.selected.len() {
                        0 => "Double-click a folder to open it, or select it to add it.".into(),
                        1 => "1 selected".into(),
                        n => format!("{n} selected"),
                    };
                    ui.label(RichText::new(hint).size(11.5).color(p.faint));
                });
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                result = Some(Picked::Cancel);
            }
            if open
                && ui.memory(|m| m.focused().is_none())
                && ui.input(|i| i.key_pressed(Key::Enter))
            {
                result = Some(Picked::Open(self.paths()));
            }
            if let Some(dir) = go {
                self.navigate(dir);
            }
            result
        })
    }

    fn paths(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .map(|&i| self.dir.join(&self.items[i].name))
            .collect()
    }
}

fn hidden(name: &str, meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        if meta.file_attributes() & HIDDEN != 0 {
            return true;
        }
    }
    let _ = meta;
    name.starts_with('.')
}

pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn places_list() -> Vec<(String, PathBuf)> {
    let home = home();
    let mut out = vec![("Home".to_owned(), home.clone())];
    for name in ["Desktop", "Documents", "Downloads"] {
        let path = home.join(name);
        if path.is_dir() {
            out.push((name.to_owned(), path));
        }
    }
    if cfg!(windows) {
        for letter in 'A'..='Z' {
            let root = PathBuf::from(format!("{letter}:\\"));
            if root.is_dir() {
                out.push((format!("{letter}:"), root));
            }
        }
    } else {
        out.push(("/".to_owned(), PathBuf::from("/")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_folders_first_without_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("b.txt"), "x").unwrap();
        fs::write(tmp.path().join(".hidden"), "x").unwrap();
        fs::create_dir(tmp.path().join("Z")).unwrap();
        let mut picker = Picker::new(tmp.path().to_path_buf());
        let names: Vec<_> = picker.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Z", "b.txt"]);
        picker.selected = BTreeSet::from([0, 1]);
        assert_eq!(
            picker.paths(),
            [tmp.path().join("Z"), tmp.path().join("b.txt")]
        );
        picker.navigate(tmp.path().join("missing"));
        assert!(picker.error.is_some());
        assert_eq!(picker.dir, tmp.path());
    }
}
