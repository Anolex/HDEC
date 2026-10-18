//! Dragging entries out of the window into Finder or Explorer. egui only
//! receives drops, so once an entry drag leaves the window the OS drag is
//! started natively: file promises on macOS (`mac_drag`), virtual files
//! read by Explorer on Windows (`win_drag`). Either way the entries are
//! extracted only when they are dropped, in the background.
#![cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]

use crate::{Job, Msg, Outcome, Password, icon};
use eframe::egui;
use hdec_core::{Ctx, Entry, ExtractOptions, OnFile, Overwrite};
use std::collections::HashSet;
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
    /// The archive's entries at or below `items`, in archive order.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub entries: Vec<Entry>,
    pub jobs: mpsc::Sender<Job>,
    pub egui: egui::Context,
}

impl DragOut {
    /// Extracts the items straight into `dest`, each under its own name,
    /// keeping anything already there. Blocks; shown in the UI as a job.
    pub fn extract(&self, dest: &Path, shown_dest: Option<PathBuf>) -> Result<(), String> {
        self.extract_with(dest, shown_dest, Arc::new(AtomicBool::new(false)), None)
    }

    /// `extract`, also stopped by `cancel`, telling `on_file` about each
    /// finished file (`ExtractOptions::on_file`).
    pub fn extract_with(
        &self,
        dest: &Path,
        shown_dest: Option<PathBuf>,
        cancel: Arc<AtomicBool>,
        on_file: Option<OnFile>,
    ) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.jobs.send(Job {
            rx,
            cancel: cancel.clone(),
            progress: (0, 0, String::new()),
            verb: "Extracting",
            icon: icon::EXPORT,
            subject: crate::file_name(&self.archive),
            started: Instant::now(),
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
            on_file,
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

/// The selection without items inside other items, and without later items
/// whose name is taken already (ignoring case, as Windows does): flattened,
/// they would land on each other.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn distinct(items: &[(String, bool)]) -> Vec<(String, bool)> {
    let all: HashSet<&str> = items.iter().map(|(p, _)| p.as_str()).collect();
    let mut names = HashSet::new();
    items
        .iter()
        .filter(|(p, _)| !parents(p).any(|q| all.contains(q)))
        .filter(|(p, _)| names.insert(crate::base_name(p).to_lowercase()))
        .cloned()
        .collect()
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

/// A file or folder the drop creates, as Windows virtual files describe it.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
pub struct VirtualFile {
    /// Relative to the drop folder, `\`-separated.
    pub name: String,
    /// Where extraction into the staging folder puts it.
    pub staged: PathBuf,
    pub is_dir: bool,
    /// `None` = unknown. Single-file streams list 0, so 0 counts as unknown.
    pub size: Option<u64>,
    pub modified: Option<std::time::SystemTime>,
}

/// Longest name a file descriptor holds, in UTF-16 units (260 with the NUL).
pub const MAX_NAME: usize = 259;

/// Everything extracting `items` (from `distinct`) flattened into `staging`
/// creates, folders before their contents, in archive order. Symlinks are
/// left out (not extracted on Windows), and so are names that differ only
/// in case from an earlier one: on Windows they are the same file. `None`
/// if a name is too long to describe.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn virtual_files(
    items: &[(String, bool)],
    entries: &[Entry],
    staging: &Path,
) -> Option<Vec<VirtualFile>> {
    let selected: HashSet<&str> = items.iter().map(|(p, _)| p.as_str()).collect();
    // Lowercased name → its index in `out`.
    let mut seen = std::collections::HashMap::new();
    let mut out: Vec<VirtualFile> = Vec::new();
    for e in entries {
        if e.link.is_some() {
            continue;
        }
        let Some(item) = std::iter::once(e.path.as_str())
            .chain(parents(&e.path))
            .find(|p| selected.contains(p))
        else {
            continue;
        };
        // Placed exactly as the extractor places it.
        let Some(placed) = hdec_core::flattened(&[item.to_owned()], &e.path) else {
            continue;
        };
        // Extraction refuses such names too.
        let Ok(staged) = hdec_core::safe_path::join(staging, placed) else {
            continue;
        };
        let rel = staged.strip_prefix(staging).ok()?.to_path_buf();
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        for depth in 1..=parts.len() {
            let name = parts[..depth].join("\\");
            let own = depth == parts.len();
            if let Some(&i) = seen.get(&name.to_lowercase()) {
                // A folder listed after its contents: it still has a time.
                let known: &mut VirtualFile = &mut out[i];
                if own && e.is_dir && known.is_dir {
                    known.modified = e.modified;
                }
                continue;
            }
            if name.encode_utf16().count() > MAX_NAME {
                return None;
            }
            seen.insert(name.to_lowercase(), out.len());
            out.push(VirtualFile {
                name,
                staged: staging.join(parts[..depth].iter().collect::<PathBuf>()),
                is_dir: !own || e.is_dir,
                size: (own && !e.is_dir && e.size > 0).then_some(e.size),
                modified: if own { e.modified } else { None },
            });
        }
    }
    Some(out)
}

/// Whether a drop target may still be reading dragged-out files from us.
pub fn copying() -> bool {
    #[cfg(windows)]
    return crate::win_drag::copying();
    #[cfg(not(windows))]
    false
}

/// Windows extracts into a temporary folder that Explorer then copies from.
pub const NEEDS_DIR: bool = cfg!(windows);

/// Whether entries can be dragged out of the window here.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", windows));

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, is_dir: bool) -> Entry {
        Entry {
            path: path.into(),
            size,
            packed: None,
            modified: None,
            is_dir,
            encrypted: false,
            link: None,
        }
    }

    fn item(path: &str, is_dir: bool) -> (String, bool) {
        (path.into(), is_dir)
    }

    #[test]
    fn distinct_drops_nested_and_same_named_items() {
        let items = [
            item("a/docs", true),
            item("a/docs/x.txt", false),
            item("b/Notes.txt", false),
            item("a/notes.txt", false),
            item("a/docs", true),
            item("c.txt", false),
        ];
        let kept: Vec<_> = distinct(&items).into_iter().map(|(p, _)| p).collect();
        assert_eq!(kept, ["a/docs", "b/Notes.txt", "c.txt"]);
    }

    #[test]
    fn virtual_files_are_flattened_with_folders_first() {
        let staging = Path::new("/stage");
        let mut link = entry("a/docs/link", 0, false);
        link.link = Some("x.txt".into());
        let entries = [
            entry("a/docs/sub/deep.bin", 5, false),
            entry("a/docs", 0, true),
            entry("a/docs/x.txt", 3, false),
            entry("a/docs/empty", 0, false),
            link,
            entry("a/other.txt", 9, false),
            entry("top.gz.txt", 0, false),
        ];
        let items = [item("a/docs", true), item("top.gz.txt", false)];
        let files = virtual_files(&items, &entries, staging).unwrap();
        let sep = std::path::MAIN_SEPARATOR_STR;
        let got: Vec<_> = files
            .iter()
            .map(|f| (f.name.as_str(), f.is_dir, f.size))
            .collect();
        assert_eq!(
            got,
            [
                ("docs", true, None),
                ("docs\\sub", true, None),
                ("docs\\sub\\deep.bin", false, Some(5)),
                ("docs\\x.txt", false, Some(3)),
                ("docs\\empty", false, None),
                ("top.gz.txt", false, None),
            ]
        );
        assert_eq!(
            files[2].staged,
            Path::new(&format!("/stage{sep}docs{sep}sub{sep}deep.bin"))
        );
    }

    #[test]
    fn virtual_files_keep_one_of_names_differing_in_case() {
        let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut docs = entry("d/Docs", 0, true);
        docs.modified = Some(when);
        let entries = [
            entry("d/Docs/Makefile", 1, false),
            entry("d/Docs/makefile", 2, false),
            entry("d/docs/b.txt", 3, false),
            // A folder listed after its contents still gets its time.
            docs,
        ];
        let files = virtual_files(&[item("d/Docs", true)], &entries, Path::new("/s")).unwrap();
        let got: Vec<_> = files.iter().map(|f| (f.name.as_str(), f.size)).collect();
        assert_eq!(got, [("Docs", None), ("Docs\\Makefile", Some(1))]);
        assert_eq!(files[0].modified, Some(when));
    }

    #[test]
    fn virtual_files_refuse_names_too_long_to_describe() {
        let long = format!("d/{}", "n".repeat(MAX_NAME));
        let entries = [entry(&long, 1, false)];
        let items = [item("d", true)];
        assert!(virtual_files(&items, &entries, Path::new("/stage")).is_none());
        let fits = format!("d/{}", "n".repeat(MAX_NAME - 2));
        let entries = [entry(&fits, 1, false)];
        assert!(virtual_files(&items, &entries, Path::new("/stage")).is_some());
    }
}
