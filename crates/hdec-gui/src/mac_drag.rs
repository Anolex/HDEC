//! Dragging entries out on macOS: one file promise (`NSFilePromiseProvider`)
//! per entry. When they're dropped, Finder names the destination folder and
//! everything is extracted straight into it: no temporary copy.

use crate::drag_out::DragOut;
use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType, NSFilePromiseProvider,
    NSFilePromiseProviderDelegate, NSView, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSError, NSOperationQueue, NSPoint, NSProcessInfo, NSRect, NSSize, NSString, NSURL,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Ivars {
    drag: DragOut,
    /// Promises are written here, one at a time, off the main thread.
    queue: Retained<NSOperationQueue>,
    /// The folder everything was extracted into and how it went: the first
    /// promise extracts all entries, the others only report.
    extracted: Mutex<Option<(PathBuf, Result<(), String>)>>,
    answered: AtomicUsize,
    /// Ended without a drop: no promise will be asked for.
    cancelled: AtomicBool,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HDECDragSource"]
    #[ivars = Ivars]
    struct Source;

    unsafe impl NSObjectProtocol for Source {}

    unsafe impl NSDraggingSource for Source {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operations(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            if operation == NSDragOperation::None {
                self.ivars().cancelled.store(true, Ordering::Relaxed);
            }
        }
    }

    unsafe impl NSFilePromiseProviderDelegate for Source {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(
            &self,
            provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            provider
                .userInfo()
                .and_then(|name| name.downcast::<NSString>().ok())
                .unwrap_or_else(|| NSString::from_str("Untitled"))
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let result = self.extract(url);
            match result {
                Ok(()) => completion.call((std::ptr::null_mut(),)),
                Err(_) => {
                    // Finder reports it too; the details are in our window.
                    let error = NSError::new(1, &NSString::from_str("HDEC"));
                    completion.call((Retained::as_ptr(&error).cast_mut(),));
                }
            }
            self.ivars().answered.fetch_add(1, Ordering::Relaxed);
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl Source {
    fn new(mtm: MainThreadMarker, drag: DragOut) -> Retained<Self> {
        let queue = NSOperationQueue::new();
        queue.setMaxConcurrentOperationCount(1);
        let this = Self::alloc(mtm).set_ivars(Ivars {
            drag,
            queue,
            extracted: Mutex::new(None),
            answered: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }

    /// Runs on `queue`. Every promise of one drop goes to the same folder.
    fn extract(&self, url: &NSURL) -> Result<(), String> {
        let ivars = self.ivars();
        let folder = url
            .path()
            .map(|p| PathBuf::from(p.to_string()))
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .ok_or("no destination folder")?;
        let mut extracted = ivars.extracted.lock().unwrap();
        if let Some((done, result)) = &*extracted
            && *done == folder
        {
            return result.clone();
        }
        let result = ivars.drag.extract(&folder, Some(folder.clone()));
        *extracted = Some((folder, result.clone()));
        result
    }

    /// Nothing more will be asked of it.
    fn finished(&self) -> bool {
        let ivars = self.ivars();
        ivars.cancelled.load(Ordering::Relaxed)
            || ivars.answered.load(Ordering::Relaxed) >= ivars.drag.items.len()
    }
}

thread_local! {
    /// Promise providers only hold their delegate weakly: sources live here
    /// until they're done.
    static LIVE: RefCell<Vec<Retained<Source>>> = const { RefCell::new(Vec::new()) };
}

pub fn start(frame: &eframe::Frame, drag: DragOut) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let handle = frame.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return Err("no AppKit window".into());
    };
    // SAFETY: eframe's view, alive for as long as its window.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let window = view.window().ok_or("no window")?;
    let location = window.mouseLocationOutsideOfEventStream();
    // A drag starts from a mouse event: the current one if it's the drag,
    // else one made up where the mouse is.
    let event = NSApplication::sharedApplication(mtm)
        .currentEvent()
        .filter(|e| {
            matches!(
                e.r#type(),
                NSEventType::LeftMouseDragged | NSEventType::LeftMouseDown
            )
        })
        .or_else(|| {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::LeftMouseDragged,
                location,
                NSEventModifierFlags::empty(),
                NSProcessInfo::processInfo().systemUptime(),
                window.windowNumber(),
                None,
                0,
                1,
                1.0,
            )
        })
        .ok_or("no mouse event to drag with")?;

    let source = Source::new(mtm, drag);
    let at = view.convertPoint_fromView(location, None);
    let workspace = NSWorkspace::sharedWorkspace();
    let items: Vec<Retained<NSDraggingItem>> = source
        .ivars()
        .drag
        .items
        .iter()
        .enumerate()
        .map(|(i, (path, is_dir))| {
            let name = crate::base_name(path);
            let file_type = NSString::from_str(if *is_dir {
                "public.folder"
            } else {
                "public.data"
            });
            let provider = NSFilePromiseProvider::initWithFileType_delegate(
                NSFilePromiseProvider::alloc(),
                &file_type,
                ProtocolObject::from_ref(&*source),
            );
            // SAFETY: read back as an NSString in `file_name`.
            unsafe { provider.setUserInfo(Some(&NSString::from_str(name))) };
            let item = NSDraggingItem::initWithPasteboardWriter(
                NSDraggingItem::alloc(),
                ProtocolObject::from_ref(&*provider),
            );
            let kind = match Path::new(name).extension() {
                _ if *is_dir => file_type,
                Some(ext) => NSString::from_str(&ext.to_string_lossy()),
                None => file_type,
            };
            // Its replacement needs the UniformTypeIdentifiers framework.
            #[allow(deprecated)]
            let icon = workspace.iconForFileType(&kind);
            // Fanned out a little, like Finder's.
            let shift = (i.min(4) * 4) as f64;
            let bounds = NSRect::new(
                NSPoint::new(at.x - 16.0 + shift, at.y - 16.0 - shift),
                NSSize::new(32.0, 32.0),
            );
            // SAFETY: an NSImage is what the contents may be.
            unsafe { item.setDraggingFrame_contents(bounds, Some(&icon)) };
            item
        })
        .collect();
    view.beginDraggingSessionWithItems_event_source(
        &NSArray::from_retained_slice(&items),
        &event,
        ProtocolObject::from_ref(&*source),
    );
    LIVE.with_borrow_mut(|live| {
        live.retain(|s| !s.finished());
        live.push(source);
    });
    Ok(())
}
