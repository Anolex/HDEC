//! Dragging entries out on Windows, as virtual files: the data object
//! describes every file and folder (`FileGroupDescriptorW`) and hands out
//! each file's contents as a stream (`FileContents`) once it's extracted.
//! The data object supports asynchronous drops, so Explorer returns from the
//! drop at once and reads the streams on its own thread, while our window
//! keeps running. Its job covers extraction and Explorer's reading, and ends
//! when Explorer has everything (listing any file it didn't take).
//!
//! Extraction goes into a staging folder through the usual extractor. A
//! stream is handed out at once; reading it waits until its file is
//! reported complete and checked (`ExtractOptions::on_file`). The waiting
//! happens on COM worker threads: the data object and its streams live in
//! the multithreaded apartment and the UI thread only holds a proxy, so
//! calls from Explorer never reach the UI thread. The staging folder goes
//! once Explorer has let go of all of it.
//!
//! A name too long for a descriptor (259 characters with its folders) falls
//! back to placeholders: empty files that are swapped for the extracted
//! entries when the button is released, blocking the window meanwhile.

use crate::drag_out::{self, DragOut, JobHandle, VirtualFile};
use hdec_core::OnFile;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::collections::HashSet;
use std::ffi::c_void;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, DV_E_LINDEX,
    DV_E_TYMED, E_ABORT, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, ERROR_CANCELLED, FILETIME,
    GlobalFree, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, POINT, S_FALSE, S_OK, STG_E_ACCESSDENIED,
    STG_E_FILENOTFOUND, STG_E_INVALIDFUNCTION, STG_E_INVALIDPOINTER, STG_E_MEDIUMFULL,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoIncrementMTAUsage, CoInitializeEx, CoTaskMemAlloc, CoUninitialize,
    DATADIR_GET, FORMATETC, IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG,
    STATFLAG_NONAME, STATSTG, STGC, STGM_READ, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
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
    SHCreateShellItemArrayFromIDLists, SHCreateStdEnumFmtEtc, SHDoDragDrop, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetCursorPos, WindowFromPoint,
};
use windows_core::{AgileReference, BOOL, ComObject, HRESULT, HSTRING, PWSTR, Ref, implement};

/// Drags whose staging is still in use: extracting, or a drop target may
/// still be reading.
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
    let staging = Staging::new(drag, dir.to_path_buf(), files);
    let data = in_mta(DataObject::new(staging.clone()))?;
    // Released over a taker: start now, while the archive is as described,
    // and so the job shows in the window right away.
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

/// The extraction behind one virtual-file drag, and what the drop target
/// has read of it. One job in the window covers both: it ends once the
/// target has read everything, can't read any more, or the job is cancelled.
struct Staging {
    drag: DragOut,
    /// Fresh and ours: extraction goes here, the target reads from here.
    dir: PathBuf,
    files: Vec<VirtualFile>,
    /// `FILEGROUPDESCRIPTORW` for `files`.
    descriptor: Vec<u8>,
    /// What the target will read: the declared sizes.
    total: u64,
    cancel: Arc<AtomicBool>,
    job: OnceLock<JobHandle>,
    state: Mutex<State>,
    /// `state` changed.
    changed: Condvar,
}

struct State {
    run: Run,
    /// Files extracted completely so far.
    finished: HashSet<PathBuf>,
    /// Per file: read to the end (folders count as read).
    delivered: Vec<bool>,
    /// Bytes the target has read.
    read: u64,
    /// Extraction progress: done, total.
    extracted: (u64, u64),
    /// The data object and live streams: what the target can still read through.
    readers: usize,
    /// The target said the drop is over (`EndOperation`).
    ended: bool,
    /// We cancelled extraction: nothing would read the rest.
    stopped: bool,
    /// Why the target's drop failed, if it did and wasn't cancelled.
    failure: Option<String>,
    reported: Option<Instant>,
}

enum Run {
    Idle,
    Running,
    Done(hdec_core::Result<Vec<String>>),
    /// The job has ended.
    Settled,
}

impl Staging {
    fn new(drag: DragOut, dir: PathBuf, files: Vec<VirtualFile>) -> Arc<Self> {
        LIVE.fetch_add(1, Ordering::Relaxed);
        let folders: Vec<bool> = files.iter().map(|f| f.is_dir).collect();
        Arc::new(Self {
            descriptor: descriptor(&files),
            total: files.iter().filter_map(|f| f.size).sum(),
            files,
            drag,
            dir,
            cancel: Arc::new(AtomicBool::new(false)),
            job: OnceLock::new(),
            state: Mutex::new(State {
                run: Run::Idle,
                finished: HashSet::new(),
                delivered: folders,
                read: 0,
                extracted: (0, 0),
                readers: 0,
                ended: false,
                stopped: false,
                failure: None,
                reported: None,
            }),
            changed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Starts extracting, once. The thread stays until the job has ended.
    fn start(self: &Arc<Self>) {
        let mut state = self.lock();
        if !matches!(state.run, Run::Idle) {
            return;
        }
        state.run = Run::Running;
        drop(state);
        let _ = self.job.set(self.drag.job(self.cancel.clone()));
        let this = self.clone();
        std::thread::spawn(move || {
            let (progress, notify) = (this.clone(), this.clone());
            let result = this.drag.run(
                &this.dir,
                this.cancel.clone(),
                Arc::new(move |done, total, name: &str| {
                    let mut state = progress.lock();
                    state.extracted = (done, total);
                    progress.report(&mut state, name);
                }),
                Some(OnFile(Arc::new(move |path: &Path| {
                    notify.lock().finished.insert(path.to_path_buf());
                    notify.changed.notify_all();
                }))),
            );
            let mut state = this.lock();
            state.run = Run::Done(result);
            this.changed.notify_all();
            while !this.settle(&mut state) {
                state = this
                    .changed
                    .wait_timeout(state, Duration::from_millis(200))
                    .unwrap()
                    .0;
            }
        });
    }

    /// Ends the job once extraction is over and the target has read
    /// everything, can't read more, or the job was cancelled. True once ended.
    fn settle(&self, state: &mut State) -> bool {
        if matches!(state.run, Run::Settled) {
            return true;
        }
        let failed = match &state.run {
            Run::Done(Err(e)) => !(state.stopped && matches!(e, hdec_core::Error::Cancelled)),
            Run::Done(Ok(_)) => false,
            _ => return false,
        };
        // By the user, here or in the target.
        let cancelled = self.cancel.load(Ordering::Relaxed) && !state.stopped;
        let all = state.delivered.iter().all(|d| *d);
        if !(failed || all || cancelled || state.ended || state.readers == 0) {
            return false;
        }
        let Run::Done(result) = std::mem::replace(&mut state.run, Run::Settled) else {
            unreachable!("checked above")
        };
        self.changed.notify_all();
        // Extracted but not read. What the extractor skipped is in its warnings.
        let extracted = |f: &VirtualFile| state.finished.contains(&f.staged) || f.staged.is_file();
        let missed: Vec<&str> = (self.files.iter().zip(&state.delivered))
            .filter(|(f, delivered)| !**delivered && extracted(f))
            .map(|(f, _)| f.name.as_str())
            .collect();
        let files = self.files.iter().filter(|f| !f.is_dir).count();
        let taken = (self.files.iter().zip(&state.delivered))
            .filter(|(f, delivered)| !f.is_dir && **delivered)
            .count();
        // Stopped by us: what was read is what the target took.
        let result = match result {
            Err(hdec_core::Error::Cancelled) if state.stopped => Ok(Vec::new()),
            result => result,
        };
        let failure = state.failure.take();
        let result = result.and_then(|mut warnings| {
            if let Some(failure) = failure {
                let error = format!("The drop target couldn't finish copying: {failure}");
                return Err(hdec_core::Error::Io(std::io::Error::other(error)));
            }
            // Cancelled by the user, or the target took none of it.
            if !missed.is_empty() && (cancelled || taken == 0) {
                return Err(hdec_core::Error::Cancelled);
            }
            if !missed.is_empty() {
                let more = missed.len().saturating_sub(10);
                let mut list = missed[..missed.len() - more].join("\n");
                if more > 0 {
                    list.push_str(&format!("\n…and {more} more"));
                }
                warnings.push(format!(
                    "{} of {files} files were not copied by the drop target:\n{list}",
                    missed.len()
                ));
            }
            Ok(self.drag.outcome(None, warnings))
        });
        if let Some(job) = self.job.get() {
            job.done(result);
        }
        true
    }

    /// Shows extraction and reading as one bar: each counts half.
    fn report(&self, state: &mut State, name: &str) {
        let Some(job) = self.job.get() else { return };
        let now = Instant::now();
        if state
            .reported
            .is_some_and(|t| now - t < Duration::from_millis(100))
        {
            return;
        }
        state.reported = Some(now);
        let (done, total) = state.extracted;
        let size = if self.total > 0 { self.total } else { total };
        let extracted = if total > 0 {
            (done.min(total) as f64 / total as f64 * size as f64) as u64
        } else {
            0
        };
        job.progress((extracted + state.read.min(size)) / 2, size, name);
    }

    /// Blocks until file `index` is extracted and checked; returns its path.
    fn wait_for(self: &Arc<Self>, index: usize) -> windows_core::Result<PathBuf> {
        self.start();
        let file = &self.files[index];
        let mut state = self.lock();
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(E_ABORT.into());
            }
            if state.finished.contains(&file.staged) {
                return Ok(file.staged.clone());
            }
            match &state.run {
                // Hard links are made last, and not reported.
                Run::Done(Ok(_)) if file.staged.is_file() => return Ok(file.staged.clone()),
                // Skipped (see the job's warnings).
                Run::Done(Ok(_)) => return Err(STG_E_FILENOTFOUND.into()),
                // Reported by the job.
                Run::Done(Err(_)) | Run::Settled => return Err(E_ABORT.into()),
                // Timed, to notice a cancel.
                Run::Idle | Run::Running => {
                    state = self
                        .changed
                        .wait_timeout(state, Duration::from_millis(200))
                        .unwrap()
                        .0;
                }
            }
        }
    }

    fn reader_added(&self) {
        self.lock().readers += 1;
    }

    /// Nothing left to read through.
    fn reader_gone(&self) {
        let mut state = self.lock();
        state.readers -= 1;
        if state.readers == 0 {
            self.stop(&mut state);
        }
        self.changed.notify_all();
    }

    /// The target is done with the drop, with `result`.
    fn ended(&self, result: HRESULT) {
        let mut state = self.lock();
        state.ended = true;
        let cancelled = [E_ABORT, HRESULT::from_win32(ERROR_CANCELLED.0)];
        if cancelled.contains(&result) {
            self.cancel.store(true, Ordering::Relaxed);
        } else {
            if result.is_err() {
                state.failure = Some(windows_core::Error::from(result).message());
            }
            self.stop(&mut state);
        }
        self.changed.notify_all();
    }

    /// Nothing will read what's still being extracted: stop extracting.
    fn stop(&self, state: &mut State) {
        let busy = matches!(state.run, Run::Running) && !self.cancel.load(Ordering::Relaxed);
        if busy && !state.delivered.iter().all(|d| *d) {
            state.stopped = true;
            self.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// The target read `bytes` of file `index`, up to its end if `at_end`.
    fn read(&self, index: usize, bytes: u64, at_end: bool) {
        let mut state = self.lock();
        state.read += bytes;
        if at_end && !state.delivered[index] {
            state.delivered[index] = true;
            self.changed.notify_all();
        }
        let name = &self.files[index].name;
        self.report(&mut state, name);
    }
}

impl Drop for Staging {
    /// Nothing refers to the folder any more: the job has ended and the
    /// target let go of everything.
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
        let dir = std::mem::take(&mut self.dir);
        std::thread::spawn(move || fs::remove_dir_all(dir));
    }
}

/// One file's contents for the drop target. Reads wait for the file to be
/// extracted, on COM worker threads (the stream lives in the MTA too).
#[implement(IStream)]
struct Stream {
    /// Once extracted: the file and its length. Closed before `staging`
    /// goes, which may remove the folder.
    file: Mutex<Option<(fs::File, u64)>>,
    staging: Arc<Staging>,
    index: usize,
}

impl Stream {
    fn new(staging: Arc<Staging>, index: usize) -> Self {
        staging.reader_added();
        Self {
            staging,
            index,
            file: Mutex::new(None),
        }
    }

    /// The file, waiting for it first if need be.
    fn opened(&self) -> windows_core::Result<MutexGuard<'_, Option<(fs::File, u64)>>> {
        let mut file = self.file.lock().unwrap();
        if file.is_none() {
            let path = self.staging.wait_for(self.index)?;
            let opened = fs::File::open(&path).map_err(io_error)?;
            let len = opened.metadata().map_err(io_error)?.len();
            *file = Some((opened, len));
        }
        Ok(file)
    }

    /// Reads up to `buf.len()` bytes, fewer only at the end.
    fn fill(&self, file: &mut fs::File, len: u64, buf: &mut [u8]) -> windows_core::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            if self.staging.cancel.load(Ordering::Relaxed) {
                return Err(E_ABORT.into());
            }
            match file.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_error(e)),
            }
        }
        let at_end = file.stream_position().map_or(true, |p| p >= len);
        self.staging.read(self.index, n as u64, at_end);
        Ok(n)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // An empty file needs no reading: taken once asked for.
        let empty = match self.file.get_mut().unwrap() {
            Some((_, len)) => *len == 0,
            None => {
                let staged = &self.staging.files[self.index].staged;
                let finished = self.staging.lock().finished.contains(staged);
                finished && fs::metadata(staged).is_ok_and(|m| m.len() == 0)
            }
        };
        if empty {
            self.staging.read(self.index, 0, true);
        }
        self.staging.reader_gone();
    }
}

impl ISequentialStream_Impl for Stream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() && cb > 0 {
            return STG_E_INVALIDPOINTER;
        }
        let read = (|| {
            let mut guard = self.opened()?;
            let (file, len) = guard.as_mut().expect("opened");
            if cb == 0 {
                return Ok(0);
            }
            // SAFETY: COM hands us a buffer of `cb` bytes.
            let buf = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
            self.fill(file, *len, buf)
        })();
        match read {
            Ok(n) => {
                if !pcbread.is_null() {
                    // SAFETY: an out-pointer from COM.
                    unsafe { *pcbread = n as u32 };
                }
                if n < cb as usize { S_FALSE } else { S_OK }
            }
            Err(e) => e.code(),
        }
    }

    fn Write(&self, _: *const c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for Stream_Impl {
    fn Seek(
        &self,
        offset: i64,
        origin: STREAM_SEEK,
        position: *mut u64,
    ) -> windows_core::Result<()> {
        let mut guard = self.opened()?;
        let (file, _) = guard.as_mut().expect("opened");
        let to = match origin {
            STREAM_SEEK_SET => {
                SeekFrom::Start(u64::try_from(offset).map_err(|_| STG_E_INVALIDFUNCTION)?)
            }
            STREAM_SEEK_CUR => SeekFrom::Current(offset),
            STREAM_SEEK_END => SeekFrom::End(offset),
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let at = file.seek(to).map_err(io_error)?;
        if !position.is_null() {
            // SAFETY: an out-pointer from COM.
            unsafe { *position = at };
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows_core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        target: Ref<IStream>,
        cb: u64,
        pcbread: *mut u64,
        pcbwritten: *mut u64,
    ) -> windows_core::Result<()> {
        let target = target.ok()?;
        let mut guard = self.opened()?;
        let (file, len) = guard.as_mut().expect("opened");
        let mut buf = vec![0; 256 << 10];
        let (mut read, mut written) = (0u64, 0u64);
        while read < cb {
            let want = (cb - read).min(buf.len() as u64) as usize;
            let n = self.fill(file, *len, &mut buf[..want])?;
            if n == 0 {
                break;
            }
            read += n as u64;
            let mut wrote = 0;
            // SAFETY: `n` initialized bytes of `buf`.
            unsafe { target.Write(buf.as_ptr().cast(), n as u32, Some(&mut wrote)) }.ok()?;
            written += u64::from(wrote);
            if wrote as usize != n {
                return Err(STG_E_MEDIUMFULL.into());
            }
        }
        // SAFETY: out-pointers from COM (or null).
        unsafe {
            if !pcbread.is_null() {
                *pcbread = read;
            }
            if !pcbwritten.is_null() {
                *pcbwritten = written;
            }
        }
        Ok(())
    }

    fn Commit(&self, _: &STGC) -> windows_core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows_core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows_core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, stat: *mut STATSTG, flags: &STATFLAG) -> windows_core::Result<()> {
        if stat.is_null() {
            return Err(STG_E_INVALIDPOINTER.into());
        }
        let file = &self.staging.files[self.index];
        // A declared size needs no waiting.
        let size = match file.size {
            Some(size) => size,
            None => self.opened()?.as_ref().expect("opened").1,
        };
        let name = if flags.0 & STATFLAG_NONAME.0 != 0 {
            PWSTR::null()
        } else {
            let base = file.name.rsplit('\\').next().unwrap_or(&file.name);
            co_string(base)?
        };
        // SAFETY: COM's out-pointer, checked above.
        unsafe {
            *stat = STATSTG {
                pwcsName: name,
                r#type: STGTY_STREAM.0 as u32,
                cbSize: size,
                mtime: file.modified.and_then(filetime).unwrap_or_default(),
                grfMode: STGM_READ,
                ..Default::default()
            };
        }
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// A string the caller frees with `CoTaskMemFree`.
fn co_string(s: &str) -> windows_core::Result<PWSTR> {
    let wide: Vec<u16> = s.encode_utf16().chain([0]).collect();
    // SAFETY: a fresh allocation of the right size, filled before use.
    unsafe {
        let at = CoTaskMemAlloc(wide.len() * 2).cast::<u16>();
        if at.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), at, wide.len());
        Ok(PWSTR(at))
    }
}

fn io_error(e: std::io::Error) -> windows_core::Error {
    match e.raw_os_error() {
        Some(code) => HRESULT::from_win32(code as u32).into(),
        None => E_FAIL.into(),
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
        staging.reader_added();
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
        self.staging.reader_gone();
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
        // Answered at once: reading it waits for the file instead.
        let stream: IStream = Stream::new(self.staging.clone(), index).into();
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
        self.staging.start();
        Ok(())
    }

    fn InOperation(&self) -> windows_core::Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Relaxed).into())
    }

    fn EndOperation(&self, result: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows_core::Result<()> {
        self.in_operation.store(false, Ordering::Relaxed);
        self.staging.ended(result);
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
