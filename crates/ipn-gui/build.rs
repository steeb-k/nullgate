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

    // Cross-compiling to ARM64 (aarch64-pc-windows-gnullvm, the target the MSYS2
    // GTK stack forces on us): winresource deliberately passes no `--target` to
    // windres for aarch64, so an unprefixed windres happily emits an *x64* object
    // and the link then dies with "machine type x64 conflicts with arm64". The
    // aarch64-prefixed driver from llvm-mingw infers the right target from its own
    // name. Host builds are unaffected — this only fires when the TARGET is aarch64.
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_arch == "aarch64" && target_env == "gnu" {
        res.set_windres_path("aarch64-w64-mingw32-windres");
    }

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
