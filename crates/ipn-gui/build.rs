//! On Windows, embed the app icon (img/icon-stacked.ico) into the GUI executable so
//! the taskbar/window/Explorer icon is Nullgate's. No-op on other platforms.

fn main() {
    #[cfg(windows)]
    embed_windows_icon();
}

#[cfg(windows)]
fn embed_windows_icon() {
    let ico = concat!(env!("CARGO_MANIFEST_DIR"), "/../../img/icon-stacked.ico");
    if std::path::Path::new(ico).exists() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico);
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows icon: {e}");
        }
    } else {
        println!("cargo:warning=img/icon-stacked.ico not found; skipping Windows icon embed");
    }
    println!("cargo:rerun-if-changed=../../img/icon-stacked.ico");
}
