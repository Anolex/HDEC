fn main() {
    // The .exe's own icon (Explorer, shortcuts). The window icon is set at runtime.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/windows/hdec.ico");
        res.compile().expect("embedding the Windows icon");
    }
}
