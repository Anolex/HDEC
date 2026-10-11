use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::path::PathBuf;

fn main() -> eframe::Result {
    let mut app = App::default();
    if let Some(path) = std::env::args_os().nth(1) {
        app.open(path.into());
    }
    eframe::run_native(
        "HDEC",
        eframe::NativeOptions::default(),
        Box::new(|_| Ok(Box::new(app))),
    )
}

#[derive(Default)]
struct App {
    status: String,
}

impl App {
    fn open(&mut self, path: PathBuf) {
        self.status = match hdec_core::detect(&path) {
            Ok(format) => format!("{} — {}", path.display(), format.name()),
            Err(e) => format!("{}: {e}", path.display()),
        };
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(path) =
            ui.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()))
        {
            self.open(path);
        }
        egui::Panel::bottom("status").show(ui, |ui| ui.label(&self.status));
        egui::CentralPanel::default().show(ui, |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::remainder())
                .columns(Column::auto().at_least(90.0), 3)
                .header(20.0, |mut header| {
                    for title in ["Name", "Size", "Packed", "Modified"] {
                        header.col(|ui| {
                            ui.strong(title);
                        });
                    }
                })
                .body(|_| {});
        });
    }
}
