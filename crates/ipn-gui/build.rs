//! On Windows, embed the app icon (img/nullgate-icon.ico) and version-info strings into
//! the GUI executable so the taskbar/window/Explorer icon and the name Task Manager
//! shows are Nullgate's. No-op on other platforms.

fn main() {
    #[cfg(windows)]
    embed_windows_resource();
}

#[cfg(windows)]
fn embed_windows_resource() {
    let mut res = winresource::WindowsResource::new();

    // Task Manager / Explorer read these version-info strings, not the file name.
    // winresource defaults them from CARGO_PKG_* (i.e. "ipn-gui"); pin them to the
    // product brand so the running process presents as Nullgate.
    res.set("FileDescription", "Nullgate");
    res.set("ProductName", "Nullgate");
    res.set("OriginalFilename", "nullgate.exe");
    res.set("InternalName", "nullgate.exe");

    let ico = concat!(env!("CARGO_MANIFEST_DIR"), "/../../img/nullgate-icon.ico");
    if std::path::Path::new(ico).exists() {
        res.set_icon(ico);
    } else {
        println!("cargo:warning=img/nullgate-icon.ico not found; skipping Windows icon embed");
    }

    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed Windows resource: {e}");
    }
    println!("cargo:rerun-if-changed=../../img/nullgate-icon.ico");
}
