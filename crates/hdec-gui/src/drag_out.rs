//! Dragging entries out of the window into Finder or Explorer. egui only
//! receives drops, so once an entry drag leaves the window the OS drag is
//! started natively: file promises on macOS (`mac_drag`), a shell data
//! object over placeholders on Windows (`win_drag`). Either way the
//! entries are extracted only when they are dropped.
#![cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]

use crate::{Job, Msg, Outcome, Password, icon};
use eframe::egui;
use hdec_core::{Ctx, ExtractOptions, Overwrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};

/// What's being dragged out. The OS calls `extract` once there is a
/// destination, possibly from another thread.
pub struct DragOut {
    pub archive: PathBuf,
    /// Full entry paths and whether each is a folder.
    pub items: Vec<(String, bool)>,
    pub password: Password,
    pub jobs: mpsc::Sender<Job>,
    pub egui: egui::Context,
}

impl DragOut {
    /// Extracts the items straight into `dest`, each under its own name,
    /// keeping anything already there. Blocks; shown in the UI as a job.
    pub fn extract(&self, dest: &Path, shown_dest: Option<PathBuf>) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let _ = self.jobs.send(Job {
            rx,
            cancel: cancel.clone(),
            progress: (0, 0, String::new()),
            verb: "Extracting",
            icon: icon::EXPORT,
            retry: None,
            password: None,
        });
        self.egui.request_repaint();
        let (progress_tx, egui) = (tx.clone(), self.egui.clone());
        let ctx = Ctx {
            password: self.password.as_deref().map(String::as_str),
            cancel,
            progress: Arc::new(move |done, total, name: &str| {
                let _ = progress_tx.send(Msg::Progress(done, total, name.to_owned()));
                egui.request_repaint();
            }),
        };
        let opts = ExtractOptions {
            only: Some(self.items.iter().map(|(p, _)| p.clone()).collect()),
            overwrite: Overwrite::Rename,
            flatten: true,
        };
        let result = hdec_core::extract(&self.archive, dest, &opts, &ctx);
        let answer = result.as_ref().map(drop).map_err(ToString::to_string);
        let _ = tx.send(Msg::Done(result.map(|warnings| Outcome::DraggedOut {
            dest: shown_dest,
            count: self.items.len(),
            warnings,
        })));
        self.egui.request_repaint();
        answer
    }
}

/// Starts the OS drag. Returns once it has started (macOS) or ended (Windows).
#[cfg(target_os = "macos")]
pub fn start(frame: &eframe::Frame, drag: DragOut, _dir: &Path) -> Result<(), String> {
    crate::mac_drag::start(frame, drag)
}

#[cfg(windows)]
pub fn start(frame: &eframe::Frame, drag: DragOut, dir: &Path) -> Result<(), String> {
    crate::win_drag::start(frame, drag, dir)
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn start(_frame: &eframe::Frame, _drag: DragOut, _dir: &Path) -> Result<(), String> {
    Err("Dragging files out isn't supported on this system.".into())
}

/// Windows extracts into a temporary folder that Explorer then copies from.
pub const NEEDS_DIR: bool = cfg!(windows);

/// Whether entries can be dragged out of the window here.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", windows));
