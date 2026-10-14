//! Dragging entries out on Windows. Explorer only takes files that exist, so
//! each entry gets an empty placeholder in a temporary folder and the drag
//! carries the shell's own data object for those (icons and drag image
//! included). Once the button is released over a target, the placeholders
//! are replaced by the extracted entries and the drop goes through; Explorer
//! copies or moves them on from there. 7-Zip does the same.

use crate::drag_out::DragOut;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, POINT, S_OK,
};
use windows::Win32::System::Com::{IBindCtx, IDataObject};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropSource, IDropSource_Impl,
    OleInitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, ILFree, SHCreateShellItemArrayFromIDLists, SHDoDragDrop, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, WindowFromPoint,
};
use windows::core::{BOOL, ComObject, HRESULT, HSTRING, implement};

#[implement(IDropSource)]
struct Source {
    drag: DragOut,
    dir: PathBuf,
    placeholders: Vec<PathBuf>,
    window: HWND,
    /// What the target under the pointer would do with a drop.
    effect: AtomicU32,
    extracted: AtomicBool,
}

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if keys.0 & MK_LBUTTON.0 != 0 {
            return S_OK;
        }
        // Released: over nothing that takes it, or back over our own window.
        if self.effect.load(Ordering::Relaxed) == DROPEFFECT_NONE.0 || self.over_own_window() {
            return DRAGDROP_S_CANCEL;
        }
        self.extracted.store(true, Ordering::Relaxed);
        // Blocks the window until done, as the drop has to wait for the files.
        match self.extract() {
            Ok(()) => DRAGDROP_S_DROP,
            Err(_) => DRAGDROP_S_CANCEL,
        }
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        self.effect.store(effect.0, Ordering::Relaxed);
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

impl Source {
    fn over_own_window(&self) -> bool {
        let mut at = POINT::default();
        // SAFETY: plain Win32 calls on a valid out-pointer.
        unsafe {
            GetCursorPos(&mut at).is_ok()
                && GetAncestor(WindowFromPoint(at), GA_ROOT) == self.window
        }
    }

    /// Placeholders out, entries in; failures are reported by the job.
    fn extract(&self) -> Result<(), String> {
        for place in &self.placeholders {
            // Ours and still empty: `remove_dir` refuses anything else.
            if place.is_dir() {
                fs::remove_dir(place)
            } else {
                fs::remove_file(place)
            }
            .map_err(|e| e.to_string())?;
        }
        self.drag.extract(&self.dir, None)
    }
}

/// Runs the whole drag; returns once it's over.
pub fn start(frame: &eframe::Frame, drag: DragOut, dir: &Path) -> Result<(), String> {
    let handle = frame.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err("no Win32 window".into());
    };
    let window = HWND(handle.hwnd.get() as *mut _);
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut placeholders = Vec::new();
    for (path, is_dir) in &drag.items {
        // The name extraction will use (Windows-safe).
        let place =
            hdec_core::safe_path::join(dir, crate::base_name(path)).map_err(|e| e.to_string())?;
        // Two selected entries of one name: the second is extracted as "name (1)", not offered.
        if placeholders.contains(&place) {
            continue;
        }
        if *is_dir {
            fs::create_dir(&place)
        } else {
            fs::File::create_new(&place).map(drop)
        }
        .map_err(|e| e.to_string())?;
        placeholders.push(place);
    }
    let data = data_object(&placeholders).map_err(|e| e.to_string())?;
    let source = ComObject::new(Source {
        drag,
        dir: dir.to_path_buf(),
        placeholders,
        window,
        effect: AtomicU32::new(DROPEFFECT_NONE.0),
        extracted: AtomicBool::new(false),
    });
    // SAFETY: COM calls on the UI thread; winit has already initialized OLE
    // there for file drops, so this only adds a reference.
    let result = unsafe {
        let _ = OleInitialize(None);
        SHDoDragDrop(
            Some(window),
            &data,
            &source.to_interface::<IDropSource>(),
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
        )
    };
    if !source.extracted.load(Ordering::Relaxed) {
        // Nothing dropped: only placeholders to clean up.
        let _ = fs::remove_dir_all(dir);
    }
    result.map(drop).map_err(|e| e.to_string())
}

/// Explorer's own data object for `paths`.
fn data_object(paths: &[PathBuf]) -> windows::core::Result<IDataObject> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
    let result = (|| {
        for path in paths {
            let mut pidl = std::ptr::null_mut();
            // SAFETY: `pidl` receives a newly allocated ID list, freed below.
            unsafe {
                SHParseDisplayName(
                    &HSTRING::from(path.as_path()),
                    None::<&IBindCtx>,
                    &mut pidl,
                    0,
                    None,
                )?
            };
            pidls.push(pidl);
        }
        let list: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.cast_const()).collect();
        // SAFETY: valid absolute ID lists; the array copies them.
        let items = unsafe { SHCreateShellItemArrayFromIDLists(&list)? };
        unsafe { items.BindToHandler(None::<&IBindCtx>, &BHID_DataObject) }
    })();
    for pidl in pidls {
        // SAFETY: allocated by SHParseDisplayName, not used after this.
        unsafe { ILFree(Some(pidl.cast_const())) };
    }
    result
}
