//! Dragging entries out on Windows. Windows never tells a drag source where
//! its files were dropped, so the drag carries one empty marker folder with
//! a unique name instead. Explorer copies it into the drop folder (at once:
//! it's empty) and the shell announces the new folder to anyone listening
//! (`SHChangeNotifyRegister`). That names the destination: the marker is
//! removed and the entries are extracted straight there as a normal job,
//! at full speed and with only our own progress window.
//!
//! Only Explorer makes sense as a target: folder windows, the desktop,
//! folder icons, the navigation pane, file dialogs.

use crate::drag_out::DragOut;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, E_ABORT, HANDLE, HWND,
    LPARAM, LRESULT, POINT, S_OK, WPARAM,
};
use windows::Win32::System::Com::{IBindCtx, IDataObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropSource, IDropSource_Impl, OleInitialize,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, CSIDL_DESKTOP, GPFIDL_DEFAULT, ILFree, SHCNE_MKDIR, SHCNE_RENAMEFOLDER,
    SHCNE_UPDATEDIR, SHCNRF_NewDelivery, SHCNRF_ShellLevel, SHChangeNotification_Lock,
    SHChangeNotification_Unlock, SHChangeNotifyDeregister, SHChangeNotifyEntry,
    SHChangeNotifyRegister, SHCreateShellItemArrayFromIDLists, SHDoDragDrop, SHGetPathFromIDListEx,
    SHGetSpecialFolderLocation, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GA_ROOT, GetAncestor,
    GetCursorPos, GetMessageW, MSG, PostMessageW, RegisterClassW, SetTimer, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_TIMER, WNDCLASSW, WindowFromPoint,
};
use windows_core::{BOOL, ComObject, HRESULT, HSTRING, implement, w};

/// A shell change notification for the watch window.
const WM_SHELL_CHANGE: u32 = WM_APP + 1;
/// The drag is over: see `Watch::next`.
const WM_NEXT: u32 = WM_APP + 2;
/// The marker turned up.
const WM_FOUND: u32 = WM_APP + 3;
/// How long after the drop the marker may take to turn up (Explorer may
/// first ask for permission to write there).
const PATIENCE: Duration = Duration::from_secs(60);

/// Runs the drag; returns once the button is released. The extraction
/// follows on another thread once the drop folder is known.
pub fn start(frame: &eframe::Frame, drag: DragOut, dir: &Path) -> Result<(), String> {
    let handle = frame.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err("no Win32 window".into());
    };
    let window = HWND(handle.hwnd.get() as *mut _);
    // SAFETY: the UI thread; winit has already initialized OLE there for
    // file drops, so this only adds a reference.
    unsafe {
        let _ = OleInitialize(None);
    }
    let name = OsString::from(format!(
        "HDEC drop {:016x}",
        std::collections::hash_map::RandomState::new().hash_one(std::process::id())
    ));
    let marker = dir.join(&name);
    fs::create_dir_all(&marker).map_err(|e| e.to_string())?;
    let result = drag_marker(window, name, marker, drag);
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result.map_err(|e| e.to_string())
}

fn drag_marker(
    window: HWND,
    name: OsString,
    marker: PathBuf,
    drag: DragOut,
) -> windows_core::Result<()> {
    let data = data_object(&marker)?;
    // Listening before the drag starts: the copy can beat `SHDoDragDrop`'s return.
    let watch = Watch::start(name, marker)?;
    let source = ComObject::new(Source {
        window,
        effect: AtomicU32::new(DROPEFFECT_NONE.0),
        dropped: AtomicBool::new(false),
    });
    // SAFETY: COM call on the UI thread with live interfaces.
    let result = unsafe {
        SHDoDragDrop(
            Some(window),
            &data,
            &source.to_interface::<IDropSource>(),
            DROPEFFECT_COPY,
        )
    };
    let dropped = result.is_ok() && source.dropped.load(Ordering::Relaxed);
    watch.next(dropped.then_some(drag));
    result.map(drop)
}

#[implement(IDropSource)]
struct Source {
    window: HWND,
    /// What the target under the pointer would do with a drop.
    effect: AtomicU32,
    /// Released over a target that takes it.
    dropped: AtomicBool,
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
        self.dropped.store(true, Ordering::Relaxed);
        DRAGDROP_S_DROP
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        self.effect.store(effect.0, Ordering::Relaxed);
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// What to extract once the drag is over; `None` = not dropped.
type Next = Option<DragOut>;

/// A hidden window on its own thread, told about new folders anywhere.
struct Watch {
    /// The window, as a number: handles aren't `Send`.
    window: isize,
    next: mpsc::Sender<Next>,
}

impl Watch {
    fn start(name: OsString, marker: PathBuf) -> windows_core::Result<Self> {
        let (ready_tx, ready) = mpsc::channel();
        let (next, next_rx) = mpsc::channel();
        std::thread::spawn(move || watch(name, marker, &ready_tx, &next_rx));
        let window = ready
            .recv()
            .map_err(|_| windows_core::Error::from(E_ABORT))??;
        Ok(Self { window, next })
    }

    fn next(self, next: Next) {
        let _ = self.next.send(next);
        let window = HWND(self.window as *mut _);
        // SAFETY: posting to our watch window; if it's gone, nothing happens.
        let _ = unsafe { PostMessageW(Some(window), WM_NEXT, WPARAM(0), LPARAM(0)) };
    }
}

/// What the watch window's procedure knows (it runs on the watch thread).
struct Watching {
    /// The marker's name.
    name: OsString,
    /// Our own copy, in the temporary folder.
    marker: PathBuf,
    found: Option<PathBuf>,
}

thread_local! {
    static WATCHING: RefCell<Option<Watching>> = const { RefCell::new(None) };
}

/// The watch thread: reports its window through `ready`, waits for the end
/// of the drag, then for the marker to turn up (shown as the job, which can
/// be cancelled), then extracts.
fn watch(
    name: OsString,
    marker: PathBuf,
    ready: &mpsc::Sender<windows_core::Result<isize>>,
    next: &mpsc::Receiver<Next>,
) {
    let temp = marker.parent().map(Path::to_path_buf);
    WATCHING.with_borrow_mut(|w| {
        *w = Some(Watching {
            name,
            // The shell reports long names; %TEMP% may be in 8.3 form.
            marker: fs::canonicalize(&marker).unwrap_or(marker),
            found: None,
        })
    });
    let listener = match listen() {
        Ok(listener) => listener,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(listener.0.0 as isize));
    let found = || WATCHING.with_borrow(|w| w.as_ref().and_then(|w| w.found.clone()));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut then = None;
    let mut msg = MSG::default();
    // SAFETY: a plain message loop for our own window.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        match msg.message {
            WM_NEXT => match next.try_recv() {
                Ok(Some(drag)) => {
                    let job = drag.job(cancel.clone());
                    job.progress(0, 0, "Waiting for Explorer…");
                    then = Some((drag, job, Instant::now()));
                    // SAFETY: a timer on our own window, to check for Cancel and patience.
                    unsafe { SetTimer(Some(listener.0), 1, 200, None) };
                }
                // Not dropped.
                _ => break,
            },
            WM_TIMER => {
                let waited = then.as_ref().map_or(Duration::ZERO, |t| t.2.elapsed());
                if cancel.load(Ordering::Relaxed) || waited > PATIENCE {
                    break;
                }
            }
            // SAFETY: dispatching to our own window procedure.
            _ => unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
        if then.is_some() && found().is_some() {
            break;
        }
    }
    let found = found();
    drop(listener);
    if let Some(temp) = temp {
        let _ = fs::remove_dir_all(temp);
    }
    let Some((drag, job, _)) = then else { return };
    let Some(dropped) = found.filter(|_| !cancel.load(Ordering::Relaxed)) else {
        job.done(Err(if cancel.load(Ordering::Relaxed) {
            hdec_core::Error::Cancelled
        } else {
            hdec_core::Error::Io(std::io::Error::other(
                "Couldn't tell where the items were dropped. Drop them into a folder \
                 window or onto the desktop.",
            ))
        }));
        return;
    };
    // Still the empty folder we made: `remove_dir` refuses anything else.
    // Explorer may hold it open for a moment after making it.
    let mut notes = Vec::new();
    for attempt in 0.. {
        match fs::remove_dir(&dropped) {
            Ok(()) => break,
            Err(_) if !dropped.exists() => break,
            Err(_) if attempt < 40 => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                notes.push(format!("Couldn't remove {}: {e}", dropped.display()));
                break;
            }
        }
    }
    let dest = dropped.parent().unwrap_or(&dropped).to_path_buf();
    let _ = drag.extract_in(&job, cancel, &dest, Some(dest.clone()), notes);
}

/// The hidden window, registered for new folders anywhere in the shell.
struct Listener(HWND, u32);

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: ours, registered and created by `listen`.
        unsafe {
            let _ = SHChangeNotifyDeregister(self.1);
            let _ = DestroyWindow(self.0);
        }
    }
}

fn listen() -> windows_core::Result<Listener> {
    static CLASS: OnceLock<u16> = OnceLock::new();
    // SAFETY: class registration and window creation with valid arguments;
    // the desktop's ID list is freed after registering.
    unsafe {
        let instance = GetModuleHandleW(None)?.into();
        let class = *CLASS.get_or_init(|| {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: w!("HDECDropWatch"),
                ..Default::default()
            })
        });
        if class == 0 {
            return Err(windows_core::Error::from_thread());
        }
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("HDECDropWatch"),
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;
        let desktop = match SHGetSpecialFolderLocation(None, CSIDL_DESKTOP as i32) {
            Ok(pidl) => pidl,
            Err(e) => {
                let _ = DestroyWindow(window);
                return Err(e);
            }
        };
        let entry = SHChangeNotifyEntry {
            pidl: desktop,
            fRecursive: true.into(),
        };
        let events = SHCNE_MKDIR.0 | SHCNE_RENAMEFOLDER.0 | SHCNE_UPDATEDIR.0;
        let id = SHChangeNotifyRegister(
            window,
            SHCNRF_ShellLevel | SHCNRF_NewDelivery,
            events as i32,
            WM_SHELL_CHANGE,
            1,
            &entry,
        );
        ILFree(Some(desktop.cast_const()));
        if id == 0 {
            let _ = DestroyWindow(window);
            return Err(windows_core::Error::from_thread());
        }
        Ok(Listener(window, id))
    }
}

extern "system" fn window_proc(window: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg != WM_SHELL_CHANGE {
        // SAFETY: the default handling for everything else.
        return unsafe { DefWindowProcW(window, msg, wparam, lparam) };
    }
    let mut pidls: *mut *mut ITEMIDLIST = std::ptr::null_mut();
    let mut event = 0;
    // SAFETY: the shell's new-delivery protocol: lock, read the (up to two)
    // ID lists, unlock.
    unsafe {
        let lock = SHChangeNotification_Lock(
            HANDLE(wparam.0 as *mut _),
            lparam.0 as u32,
            Some(&mut pidls),
            Some(&mut event),
        );
        if lock.is_invalid() {
            return LRESULT(0);
        }
        if !pidls.is_null() {
            for i in 0..2 {
                let pidl = *pidls.add(i);
                if !pidl.is_null()
                    && let Some(path) = path_of(pidl)
                    && marker_found(&path)
                {
                    let _ = PostMessageW(Some(window), WM_FOUND, WPARAM(0), LPARAM(0));
                }
            }
        }
        let _ = SHChangeNotification_Unlock(lock);
    }
    LRESULT(0)
}

/// Whether `path` is the dropped marker, or a folder now holding it (for a
/// folder-changed notice instead of a new-folder one).
fn marker_found(path: &Path) -> bool {
    WATCHING.with_borrow_mut(|w| {
        let Some(w) = w.as_mut().filter(|w| w.found.is_none()) else {
            return false;
        };
        let candidate = if path.file_name() == Some(w.name.as_os_str()) {
            path.to_path_buf()
        } else {
            path.join(&w.name)
        };
        if !candidate.is_dir() {
            return false;
        }
        let ours = fs::canonicalize(&candidate).is_ok_and(|c| c == w.marker);
        if !ours {
            w.found = Some(candidate);
        }
        !ours
    })
}

/// A file-system ID list's path; `None` for virtual items.
fn path_of(pidl: *const ITEMIDLIST) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    // SAFETY: a valid ID list from the shell; the buffer's size is passed.
    let ok = unsafe { SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT) }.as_bool();
    let len = buf.iter().position(|&c| c == 0)?;
    (ok && len > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// Explorer's own data object for `path`.
fn data_object(path: &Path) -> windows_core::Result<IDataObject> {
    let mut pidl = std::ptr::null_mut();
    // SAFETY: `pidl` receives a newly allocated ID list, freed below.
    unsafe {
        SHParseDisplayName(&HSTRING::from(path), None::<&IBindCtx>, &mut pidl, 0, None)?;
    }
    let list = [pidl.cast_const()];
    // SAFETY: a valid absolute ID list; the array copies it.
    let items = unsafe { SHCreateShellItemArrayFromIDLists(&list) };
    // SAFETY: allocated by SHParseDisplayName, not used after this.
    unsafe { ILFree(Some(pidl.cast_const())) };
    // SAFETY: a live shell item array.
    unsafe { items?.BindToHandler(None::<&IBindCtx>, &BHID_DataObject) }
}
