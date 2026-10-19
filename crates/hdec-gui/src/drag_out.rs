//! Dragging entries out of the window into Finder or Explorer. egui only
//! receives drops, so once an entry drag leaves the window the OS drag is
//! started natively: file promises on macOS (`mac_drag`), a marker folder
//! whose drop folder the shell reports on Windows (`win_drag`). Either way
//! the entries are extracted straight into the drop folder, in the
//! background.
#![cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]

use crate::{Job, Msg, Outcome, Password, icon};
use eframe::egui;
use hdec_core::{Ctx, ExtractOptions, Overwrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};
use std::time::Instant;

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
    #[cfg_attr(windows, allow(dead_code))]
    pub fn extract(&self, dest: &Path, shown_dest: Option<PathBuf>) -> Result<(), String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let job = self.job(cancel.clone());
        self.extract_in(&job, cancel, dest, shown_dest, Vec::new())
    }

    /// `extract` as part of `job` (whose Cancel sets `cancel`), with `notes`
    /// added to its warnings.
    pub fn extract_in(
        &self,
        job: &JobHandle,
        cancel: Arc<AtomicBool>,
        dest: &Path,
        shown_dest: Option<PathBuf>,
        notes: Vec<String>,
    ) -> Result<(), String> {
        let progress = job.clone();
        let ctx = Ctx {
            password: self.password.as_deref().map(String::as_str),
            cancel,
            progress: Arc::new(move |done, total, name: &str| progress.progress(done, total, name)),
        };
        let opts = ExtractOptions {
            only: Some(self.items.iter().map(|(p, _)| p.clone()).collect()),
            overwrite: Overwrite::Rename,
            flatten: true,
        };
        let result = hdec_core::extract(&self.archive, dest, &opts, &ctx);
        let answer = result.as_ref().map(drop).map_err(ToString::to_string);
        job.done(result.map(|mut warnings| {
            warnings.extend(notes);
            Outcome::DraggedOut {
                dest: shown_dest,
                count: self.items.len(),
                warnings,
            }
        }));
        answer
    }

    /// Shows a job for this drag in the window; its Cancel sets `cancel`.
    pub fn job(&self, cancel: Arc<AtomicBool>) -> JobHandle {
        let (tx, rx) = mpsc::channel();
        let _ = self.jobs.send(Job {
            rx,
            cancel,
            progress: (0, 0, String::new()),
            verb: "Extracting",
            icon: icon::EXPORT,
            subject: crate::file_name(&self.archive),
            started: Instant::now(),
            retry: None,
            password: None,
        });
        self.egui.request_repaint();
        JobHandle {
            tx,
            egui: self.egui.clone(),
        }
    }
}

/// A job in the window, fed from any thread.
#[derive(Clone)]
pub struct JobHandle {
    tx: mpsc::Sender<Msg>,
    egui: egui::Context,
}

impl JobHandle {
    pub fn progress(&self, done: u64, total: u64, name: &str) {
        let _ = self.tx.send(Msg::Progress(done, total, name.to_owned()));
        self.egui.request_repaint();
    }

    pub fn done(&self, result: hdec_core::Result<Outcome>) {
        let _ = self.tx.send(Msg::Done(result));
        self.egui.request_repaint();
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

/// `a/b/c` → `a/b`, `a`.
pub fn parents(path: &str) -> impl Iterator<Item = &str> {
    let mut rest = path;
    std::iter::from_fn(move || {
        let (parent, _) = rest.rsplit_once('/')?;
        rest = parent;
        Some(parent)
    })
}

/// Windows drags a marker folder made in a temporary folder.
pub const NEEDS_DIR: bool = cfg!(windows);

/// Whether entries can be dragged out of the window here.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", windows));
