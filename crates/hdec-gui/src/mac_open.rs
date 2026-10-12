//! Files opened from Finder (double-click, Open With, dropped on the Dock
//! icon). macOS sends these to the app delegate's `application:openURLs:`,
//! not as arguments. winit owns the delegate and doesn't implement that
//! method, so it is added to winit's delegate class just before launch ends.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationWillFinishLaunchingNotification};
use objc2_foundation::{NSArray, NSNotification, NSNotificationCenter, NSObject, NSURL};
use std::path::PathBuf;
use std::sync::Mutex;

/// Opened files not yet picked up by the app.
static QUEUE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// Set once the UI exists, to wake it when a file arrives.
static WAKE: Mutex<Option<eframe::egui::Context>> = Mutex::new(None);

extern "C-unwind" fn open_urls(
    _this: &AnyObject,
    _cmd: Sel,
    _app: &NSApplication,
    urls: &NSArray<NSURL>,
) {
    let paths = urls
        .iter()
        .filter_map(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()));
    QUEUE.lock().unwrap().extend(paths);
    if let Some(ctx) = &*WAKE.lock().unwrap() {
        ctx.request_repaint();
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HDECLaunchObserver"]
    struct Observer;

    impl Observer {
        /// The delegate exists and the launch's files haven't been sent yet.
        #[unsafe(method(willFinishLaunching:))]
        fn will_finish_launching(&self, _note: &NSNotification) {
            let app = NSApplication::sharedApplication(self.mtm());
            let Some(delegate) = app.delegate() else {
                return;
            };
            let object: &AnyObject = (*delegate).as_ref();
            let class = object.class();
            // SAFETY: the signature matches `open_urls` (void, self, _cmd,
            // two objects); adding fails harmlessly if winit ever has one.
            unsafe {
                let imp: Imp = std::mem::transmute(
                    open_urls as extern "C-unwind" fn(&AnyObject, Sel, &NSApplication, &NSArray<NSURL>),
                );
                objc2::ffi::class_addMethod(
                    class as *const AnyClass as *mut AnyClass,
                    sel!(application:openURLs:),
                    imp,
                    c"v@:@@".as_ptr(),
                );
            }
        }
    }
);

/// Call before the event loop starts.
pub fn install() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let observer: Retained<Observer> = unsafe { msg_send![Observer::alloc(mtm), init] };
    unsafe {
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
            &observer,
            sel!(willFinishLaunching:),
            Some(NSApplicationWillFinishLaunchingNotification),
            None,
        );
    }
    // The notification center doesn't keep observers alive.
    std::mem::forget(observer);
}

/// Lets new arrivals wake the UI; call once it exists.
pub fn set_context(ctx: &eframe::egui::Context) {
    *WAKE.lock().unwrap() = Some(ctx.clone());
}

/// Files opened since the last call, oldest first.
pub fn take() -> Vec<PathBuf> {
    std::mem::take(&mut QUEUE.lock().unwrap())
}
