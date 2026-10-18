//! Dragging entries out on Windows, as virtual files: the data object
//! describes every file and folder (`FileGroupDescriptorW`) and hands out
//! each file's contents as a stream (`FileContents`) once it's extracted.
//! The data object supports asynchronous drops, so Explorer returns from the
//! drop at once and reads the streams on its own thread, with its own copy
//! progress, while our window keeps running and shows the extraction.
//!
//! Extraction goes into a staging folder through the usual extractor; a
//! stream request waits until its file there is reported complete and
//! checked (`ExtractOptions::on_file`). The waiting happens on COM
//! worker threads: the data object lives in the multithreaded apartment and
//! the UI thread only holds a proxy to it, so calls from Explorer never
//! reach the UI thread.
//!
//! A name too long for a descriptor (259 characters with its folders) falls
//! back to placeholders: empty files that are swapped for the extracted
//! entries when the button is released, blocking the window meanwhile.

use crate::drag_out::{self, DragOut, VirtualFile};
use hdec_core::OnFile;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::collections::HashSet;
use std::fs;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, DV_E_LINDEX,
    DV_E_TYMED, E_ABORT, E_INVALIDARG, E_NOTIMPL, FILETIME, GlobalFree, HGLOBAL, HWND,
    OLE_E_ADVISENOTSUPPORTED, POINT, S_OK, STG_E_FILENOTFOUND,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoIncrementMTAUsage, CoInitializeEx, CoUninitialize, DATADIR_GET,
    FORMATETC, IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    STGM_READ, STGM_SHARE_DENY_NONE, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropSource, IDropSource_Impl,
    OleInitialize, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT,
    FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW,
    IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, ILFree,
    SHCreateShellItemArrayFromIDLists, SHCreateStdEnumFmtEtc, SHCreateStreamOnFileEx, SHDoDragDrop,
    SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, WindowFromPoint,
};
use windows_core::{AgileReference, BOOL, ComObject, HRESULT, HSTRING, Ref, implement};

/// Data objects still held by a drop target, which may be reading from them.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Whether a drop target may still be copying dragged-out files.
pub fn copying() -> bool {
    LIVE.load(Ordering::Relaxed) > 0
}

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Runs the drag; returns once the button is released. Explorer may still
/// be reading the files then.
pub fn start(frame: &eframe::Frame, mut drag: DragOut, dir: &Path) -> Result<(), String> {
    let handle = frame.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err("no Win32 window".into());
    };
    let window = HWND(handle.hwnd.get() as *mut _);
    drag.items = drag_out::distinct(&drag.items);
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // SAFETY: the UI thread; winit has already initialized OLE there for
    // file drops, so this only adds a reference.
    unsafe {
        let _ = OleInitialize(None);
    }
    match drag_out::virtual_files(&drag.items, &drag.entries, dir) {
        Some(files) => virtual_drag(window, drag, dir, files),
        None => placeholder_drag(window, drag, dir),
    }
    .map_err(|e| e.to_string())
}

#[implement(IDropSource)]
struct Source {
    window: HWND,
    /// What the target under the pointer would do with a drop.
    effect: AtomicU32,
    /// Runs when dropped onto a taker; false = failed, cancel the drop.
    on_drop: Option<Box<dyn Fn() -> bool>>,
}

impl Source {
    fn new(window: HWND, on_drop: Option<Box<dyn Fn() -> bool>>) -> ComObject<Self> {
        ComObject::new(Self {
            window,
            effect: AtomicU32::new(DROPEFFECT_NONE.0),
            on_drop,
        })
    }

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
        match &self.on_drop {
            Some(make_real) if !make_real() => DRAGDROP_S_CANCEL,
            _ => DRAGDROP_S_DROP,
        }
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        self.effect.store(effect.0, Ordering::Relaxed);
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn virtual_drag(
    window: HWND,
    drag: DragOut,
    dir: &Path,
    files: Vec<VirtualFile>,
) -> windows_core::Result<()> {
    let staging = Arc::new(Staging {
        descriptor: descriptor(&files),
        served: files.iter().map(|f| AtomicBool::new(f.is_dir)).collect(),
        files,
        drag,
        dir: dir.to_path_buf(),
        cancel: Arc::new(AtomicBool::new(false)),
        state: Mutex::new(State {
            run: Run::Idle,
            finished: HashSet::new(),
        }),
        changed: Condvar::new(),
    });
    let data = in_mta(DataObject::new(staging.clone()))?;
    // Released over a taker: start now, while the archive is as described,
    // and so the extraction shows in the window right away.
    let source = Source::new(
        window,
        Some(Box::new(move || {
            staging.start();
            true
        })),
    );
    // SAFETY: COM call on the UI thread with live interfaces.
    unsafe {
        SHDoDragDrop(
            Some(window),
            &data,
            &source.to_interface::<IDropSource>(),
            DROPEFFECT_COPY,
        )
    }
    .map(drop)
}

/// Creates `data` in the multithreaded apartment; returns a proxy to it for
/// this (single-threaded) one. Calls through the proxy, and from other
/// processes, run on COM's worker threads.
fn in_mta(data: DataObject) -> windows_core::Result<IDataObject> {
    static MTA: OnceLock<bool> = OnceLock::new();
    // Kept for good: the apartment must outlive every object in it.
    // SAFETY: plain COM call; the cookie is never released.
    if !MTA.get_or_init(|| unsafe { CoIncrementMTAUsage() }.is_ok()) {
        return Err(E_ABORT.into());
    }
    let agile = std::thread::spawn(move || {
        // SAFETY: this thread joins the apartment for the call and leaves it.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        let object: IDataObject = data.into();
        let agile = AgileReference::new(&object);
        drop(object);
        unsafe { CoUninitialize() };
        agile
    })
    .join()
    .map_err(|_| windows_core::Error::from(E_ABORT))??;
    agile.resolve()
}

/// The extraction behind one virtual-file drag.
struct Staging {
    drag: DragOut,
    /// Fresh and ours: extraction goes here, Explorer copies from here.
    dir: PathBuf,
    files: Vec<VirtualFile>,
    /// `FILEGROUPDESCRIPTORW` for `files`.
    descriptor: Vec<u8>,
    /// Per file: its contents were handed out (folders count as served).
    served: Vec<AtomicBool>,
    cancel: Arc<AtomicBool>,
    state: Mutex<State>,
    /// `state` changed.
    changed: Condvar,
}

struct State {
    run: Run,
    /// Files extracted completely so far.
    finished: HashSet<PathBuf>,
}

enum Run {
    Idle,
    Running,
    Done(Result<(), String>),
}

impl Staging {
    /// Starts extracting, once.
    fn start(self: &Arc<Self>) {
        let mut state = self.state.lock().unwrap();
        if !matches!(state.run, Run::Idle) {
            return;
        }
        state.run = Run::Running;
        let this = self.clone();
        std::thread::spawn(move || {
            let notify = this.clone();
            let on_file = OnFile(Arc::new(move |path: &Path| {
                let mut state = notify.state.lock().unwrap();
                state.finished.insert(path.to_path_buf());
                notify.changed.notify_all();
            }));
            let result =
                this.drag
                    .extract_with(&this.dir, None, this.cancel.clone(), Some(on_file));
            this.state.lock().unwrap().run = Run::Done(result);
            this.changed.notify_all();
        });
    }

    /// Blocks until file `index` is extracted and checked.
    fn wait_for(self: &Arc<Self>, index: usize) -> windows_core::Result<&Path> {
        self.start();
        let file = &self.files[index];
        let mut state = self.state.lock().unwrap();
        loop {
            if state.finished.contains(&file.staged) {
                return Ok(&file.staged);
            }
            match &state.run {
                // Hard links are made last, and not reported.
                Run::Done(Ok(())) if file.staged.is_file() => return Ok(&file.staged),
                // Skipped (see the job's warnings).
                Run::Done(Ok(())) => return Err(STG_E_FILENOTFOUND.into()),
                // Reported by the job.
                Run::Done(Err(_)) => return Err(E_ABORT.into()),
                _ => state = self.changed.wait(state).unwrap(),
            }
        }
    }

    /// Nobody will read the rest: stop extracting it.
    fn abandon(&self) {
        if matches!(self.state.lock().unwrap().run, Run::Running) {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// The data object is gone. Unless everything was handed out, the rest
    /// is stopped; once extraction is over the staging folder goes.
    fn released(self: &Arc<Self>) {
        if !self.served.iter().all(|s| s.load(Ordering::Relaxed)) {
            self.abandon();
        }
        let this = self.clone();
        std::thread::spawn(move || {
            let mut state = this.state.lock().unwrap();
            while matches!(state.run, Run::Running) {
                state = this.changed.wait(state).unwrap();
            }
            drop(state);
            // Fails while a stream is still open; the app's temp folder goes on quit.
            let _ = fs::remove_dir_all(&this.dir);
        });
    }
}

/// Registered clipboard format ids.
#[derive(Clone, Copy)]
struct Formats {
    descriptor: u16,
    contents: u16,
    effect: u16,
}

impl Formats {
    fn get() -> Self {
        // SAFETY: constant, NUL-terminated names.
        let id = |name| unsafe { RegisterClipboardFormatW(name) } as u16;
        Self {
            descriptor: id(CFSTR_FILEDESCRIPTORW),
            contents: id(CFSTR_FILECONTENTS),
            effect: id(CFSTR_PREFERREDDROPEFFECT),
        }
    }
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DataObject {
    staging: Arc<Staging>,
    formats: Formats,
    /// Whatever others set on it (drag image, drop results): format and bytes.
    stored: Mutex<Vec<(u16, Vec<u8>)>>,
    in_operation: AtomicBool,
}

impl DataObject {
    fn new(staging: Arc<Staging>) -> Self {
        LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            staging,
            formats: Formats::get(),
            stored: Mutex::new(Vec::new()),
            in_operation: AtomicBool::new(false),
        }
    }
}

impl Drop for DataObject {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
        self.staging.released();
    }
}

impl DataObject {
    /// An `HGLOBAL` format's bytes, if we have it.
    fn bytes(&self, format: u16) -> Option<Vec<u8>> {
        if format == self.formats.descriptor {
            Some(self.staging.descriptor.clone())
        } else if format == self.formats.effect {
            Some(DROPEFFECT_COPY.0.to_le_bytes().to_vec())
        } else {
            let stored = self.stored.lock().unwrap();
            stored
                .iter()
                .find(|(f, _)| *f == format)
                .map(|(_, b)| b.clone())
        }
    }

    fn has(&self, format: u16) -> bool {
        format == self.formats.descriptor
            || format == self.formats.effect
            || self
                .stored
                .lock()
                .unwrap()
                .iter()
                .any(|(f, _)| *f == format)
    }

    fn contents(&self, f: &FORMATETC) -> windows_core::Result<STGMEDIUM> {
        if f.tymed & TYMED_ISTREAM.0 as u32 == 0 {
            return Err(DV_E_TYMED.into());
        }
        let index = usize::try_from(f.lindex)
            .ok()
            .filter(|&i| self.staging.files.get(i).is_some_and(|f| !f.is_dir))
            .ok_or(windows_core::Error::from(DV_E_LINDEX))?;
        let path = self.staging.wait_for(index)?;
        // SAFETY: a valid path; the stream only reads.
        let stream = unsafe {
            SHCreateStreamOnFileEx(
                &HSTRING::from(path),
                (STGM_READ | STGM_SHARE_DENY_NONE).0,
                0,
                false,
                None,
            )?
        };
        self.staging.served[index].store(true, Ordering::Relaxed);
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows_core::Result<STGMEDIUM> {
        // SAFETY: COM passes a valid FORMATETC (or null, refused).
        let f = unsafe { format.as_ref() }.ok_or(windows_core::Error::from(E_INVALIDARG))?;
        if f.cfFormat == self.formats.contents {
            return self.contents(f);
        }
        let bytes = self
            .bytes(f.cfFormat)
            .ok_or(windows_core::Error::from(DV_E_FORMATETC))?;
        if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return Err(DV_E_TYMED.into());
        }
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: to_global(&bytes)?,
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        // SAFETY: as in `GetData`.
        let Some(f) = (unsafe { format.as_ref() }) else {
            return E_INVALIDARG;
        };
        let tymed = if f.cfFormat == self.formats.contents {
            TYMED_ISTREAM
        } else if self.has(f.cfFormat) {
            TYMED_HGLOBAL
        } else {
            return DV_E_FORMATETC;
        };
        if f.tymed & tymed.0 as u32 == 0 {
            DV_E_TYMED
        } else {
            S_OK
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows_core::Result<()> {
        // SAFETY: COM passes valid pointers (or null, refused).
        let (f, m) = unsafe { (format.as_ref(), medium.as_ref()) };
        let (Some(f), Some(m)) = (f, m) else {
            return Err(E_INVALIDARG.into());
        };
        if m.tymed != TYMED_HGLOBAL.0 as u32 {
            return Err(DV_E_TYMED.into());
        }
        // SAFETY: tymed says the union holds an HGLOBAL.
        let bytes = from_global(unsafe { m.u.hGlobal })?;
        if release.as_bool() {
            // SAFETY: ours to free once accepted; it was copied.
            unsafe { ReleaseStgMedium(medium.cast_mut()) };
        }
        let mut stored = self.stored.lock().unwrap();
        stored.retain(|(id, _)| *id != f.cfFormat);
        stored.push((f.cfFormat, bytes));
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows_core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let format = |id, tymed: windows::Win32::System::Com::TYMED| FORMATETC {
            cfFormat: id,
            ptd: std::ptr::null_mut(),
            dwAspect: windows::Win32::System::Com::DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: tymed.0 as u32,
        };
        let mut formats = vec![
            format(self.formats.descriptor, TYMED_HGLOBAL),
            format(self.formats.contents, TYMED_ISTREAM),
            format(self.formats.effect, TYMED_HGLOBAL),
        ];
        let stored = self.stored.lock().unwrap();
        formats.extend(stored.iter().map(|(id, _)| format(*id, TYMED_HGLOBAL)));
        // SAFETY: the enumerator copies the array.
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows_core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows_core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows_core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Lets Explorer finish the drop on its own thread.
impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
    fn SetAsyncMode(&self, _: BOOL) -> windows_core::Result<()> {
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows_core::Result<BOOL> {
        Ok(true.into())
    }

    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows_core::Result<()> {
        self.in_operation.store(true, Ordering::Relaxed);
        // It's coming: no need to wait for the first stream request.
        self.staging.start();
        Ok(())
    }

    fn InOperation(&self) -> windows_core::Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Relaxed).into())
    }

    fn EndOperation(&self, result: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows_core::Result<()> {
        self.in_operation.store(false, Ordering::Relaxed);
        // Cancelled or failed in Explorer: what's left is not needed.
        if result.is_err() {
            self.staging.abandon();
        }
        Ok(())
    }
}

/// `FILEGROUPDESCRIPTORW`: a count, then one descriptor per file.
fn descriptor(files: &[VirtualFile]) -> Vec<u8> {
    let mut out = (files.len() as u32).to_le_bytes().to_vec();
    for file in files {
        let mut d = FILEDESCRIPTORW {
            dwFlags: (FD_ATTRIBUTES.0 | FD_PROGRESSUI.0 | FD_UNICODE.0) as u32,
            dwFileAttributes: if file.is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            },
            ..Default::default()
        };
        if let Some(size) = file.size {
            d.dwFlags |= FD_FILESIZE.0 as u32;
            d.nFileSizeHigh = (size >> 32) as u32;
            d.nFileSizeLow = size as u32;
        }
        if let Some(time) = file.modified.and_then(filetime) {
            d.dwFlags |= FD_WRITESTIME.0 as u32;
            d.ftLastWriteTime = time;
        }
        // `virtual_files` keeps names short enough to leave the NUL.
        let mut name = [0; 260];
        for (slot, unit) in name.iter_mut().zip(file.name.encode_utf16()) {
            *slot = unit;
        }
        d.cFileName = name;
        // SAFETY: a plain, packed C struct, read as its bytes.
        out.extend_from_slice(unsafe {
            std::slice::from_raw_parts((&raw const d).cast::<u8>(), size_of::<FILEDESCRIPTORW>())
        });
    }
    out
}

/// 100 ns intervals since 1601.
fn filetime(t: SystemTime) -> Option<FILETIME> {
    let since_1970 = t.duration_since(UNIX_EPOCH).ok()?;
    let ticks = since_1970.as_nanos() / 100 + 116_444_736_000_000_000;
    let ticks = u64::try_from(ticks).ok()?;
    Some(FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    })
}

fn to_global(bytes: &[u8]) -> windows_core::Result<HGLOBAL> {
    // SAFETY: a fresh allocation of `bytes.len()`, written while locked.
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let at = GlobalLock(global).cast::<u8>();
        if at.is_null() {
            let error = windows_core::Error::from_thread();
            let _ = GlobalFree(Some(global));
            return Err(error);
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
        let _ = GlobalUnlock(global);
        Ok(global)
    }
}

fn from_global(global: HGLOBAL) -> windows_core::Result<Vec<u8>> {
    // SAFETY: read within its size while locked.
    unsafe {
        let at = GlobalLock(global).cast::<u8>();
        if at.is_null() {
            return Err(windows_core::Error::from_thread());
        }
        let bytes = std::slice::from_raw_parts(at, GlobalSize(global)).to_vec();
        let _ = GlobalUnlock(global);
        Ok(bytes)
    }
}

/// The fallback: empty placeholders carried by the shell's own data object,
/// replaced by the extracted entries on release. 7-Zip does the same.
fn placeholder_drag(window: HWND, drag: DragOut, dir: &Path) -> windows_core::Result<()> {
    let mut placeholders = Vec::new();
    for (path, is_dir) in &drag.items {
        // The name extraction will use (Windows-safe).
        let place = hdec_core::safe_path::join(dir, crate::base_name(path))
            .map_err(|e| windows_core::Error::new(E_INVALIDARG, e.to_string()))?;
        // Two names Windows sees as one: the second is extracted as
        // "name (1)", not offered.
        let same = |p: &PathBuf| {
            p.to_string_lossy().to_lowercase() == place.to_string_lossy().to_lowercase()
        };
        if placeholders.iter().any(same) {
            continue;
        }
        if *is_dir {
            fs::create_dir(&place)
        } else {
            fs::File::create_new(&place).map(drop)
        }?;
        placeholders.push(place);
    }
    let data = data_object(&placeholders)?;
    let extracted = Arc::new(AtomicBool::new(false));
    let (done, dir_owned) = (extracted.clone(), dir.to_path_buf());
    let make_real = move || {
        done.store(true, Ordering::Relaxed);
        // Blocks the window until done, as the drop has to wait for the files.
        let swapped = placeholders.iter().all(|place| {
            // Ours and still empty: `remove_dir` refuses anything else.
            if place.is_dir() {
                fs::remove_dir(place)
            } else {
                fs::remove_file(place)
            }
            .is_ok()
        });
        swapped && drag.extract(&dir_owned, None).is_ok()
    };
    let source = Source::new(window, Some(Box::new(make_real)));
    // SAFETY: COM call on the UI thread with live interfaces.
    let result = unsafe {
        SHDoDragDrop(
            Some(window),
            &data,
            &source.to_interface::<IDropSource>(),
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
        )
    };
    if !extracted.load(Ordering::Relaxed) {
        // Nothing dropped: only placeholders to clean up.
        let _ = fs::remove_dir_all(dir);
    }
    result.map(drop)
}

/// Explorer's own data object for `paths`.
fn data_object(paths: &[PathBuf]) -> windows_core::Result<IDataObject> {
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
