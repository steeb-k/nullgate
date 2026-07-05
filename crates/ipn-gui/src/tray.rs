//! System tray integration for the **tray agent** (`--agent`). Three actions:
//! **Open Nullgate**, **Restart Nullgate daemon**, and **Quit Nullgate**.
//!
//! On Windows/macOS we use the `tray-icon` crate. On Linux that crate's backend
//! pulls in GTK3 + libappindicator, which clashes with this GTK4 app, so Linux
//! uses a pure-Rust StatusNotifier implementation (`ksni`) on its own thread,
//! bridged back to the GTK main loop over an `async-channel`.
//!
//! The tray lives in the lightweight user-session agent, not the GUI, so it
//! survives the GUI window being closed or crashing. Each action is delivered to
//! the agent over a dedicated `async-channel`; the agent decides what to do
//! (launch the GUI, restart the privileged daemon, or disconnect + quit).

/// Where the tray sends each menu action. The agent owns the receiving ends.
#[derive(Clone)]
pub struct TrayActions {
    /// Open (or focus) the GUI window. Also fired by clicking the tray icon.
    pub open: async_channel::Sender<()>,
    /// (Re)start the privileged Nullgate daemon (elevates on the agent side).
    pub restart_daemon: async_channel::Sender<()>,
    /// Disconnect from the network and quit the agent.
    pub quit: async_channel::Sender<()>,
}

// Tray icon (the only one): the "gate" mark, used as-is on every theme. The 64px
// source is scaled down at runtime to whatever each platform's tray wants.
const TRAY_PNG: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../img/nullgate-tray-icon-64.png"));

#[cfg(any(windows, target_os = "macos"))]
pub fn install(actions: TrayActions) {
    use gtk::glib;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    // Make native popups (the tray context menu) honor the app's color scheme.
    #[cfg(windows)]
    set_preferred_app_mode(adw::StyleManager::default().is_dark());

    let open = MenuItem::new("Open Nullgate", true, None);
    let restart = MenuItem::new("Restart Nullgate daemon", true, None);
    let quit = MenuItem::new("Quit Nullgate", true, None);
    let menu = Menu::new();
    let _ = menu.append(&open);
    let _ = menu.append(&restart);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit);
    let open_id = open.id().clone();
    let restart_id = restart.id().clone();
    let quit_id = quit.id().clone();

    let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Nullgate");
    if let Some(icon) = load_tray_icon() {
        builder = builder.with_icon(icon);
    }
    let _icon = match builder.build() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("tray unavailable: {e}");
            return;
        }
    };
    tracing::info!("system tray installed");

    // tray-icon delivers events on global channels; poll them on the GTK loop. The
    // icon is moved into the closure so it stays alive.
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        let _keep = &_icon;
        let mut open_window = false;
        let mut restart_daemon = false;
        let mut quit = false;
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            // Open on double-click (Windows) or a left single-click (covers macOS,
            // where DoubleClick isn't emitted — and friendlier on Windows too).
            match ev {
                TrayIconEvent::DoubleClick { .. }
                | TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => open_window = true,
                _ => {}
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == open_id {
                open_window = true;
            } else if ev.id == restart_id {
                restart_daemon = true;
            } else if ev.id == quit_id {
                quit = true;
            }
        }
        if open_window {
            let _ = actions.open.try_send(());
        }
        if restart_daemon {
            let _ = actions.restart_daemon.try_send(());
        }
        if quit {
            let _ = actions.quit.try_send(());
        }
        glib::ControlFlow::Continue
    });
}

#[cfg(any(windows, target_os = "macos"))]
fn load_tray_icon() -> Option<tray_icon::Icon> {
    use gtk::gdk_pixbuf::{InterpType, Pixbuf};
    let src = Pixbuf::from_read(std::io::Cursor::new(TRAY_PNG)).ok()?;
    let pb = src.scale_simple(32, 32, InterpType::Bilinear)?;
    let pb = if pb.has_alpha() { pb } else { pb.add_alpha(false, 0, 0, 0).ok()? };
    let (w, h) = (pb.width(), pb.height());
    let rowstride = pb.rowstride() as usize;
    let nch = pb.n_channels() as usize;
    let bytes = pb.read_pixel_bytes();
    let bytes = bytes.as_ref();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let row = &bytes[y * rowstride..y * rowstride + w as usize * nch];
        for px in row.chunks_exact(nch) {
            let a = if nch == 4 { px[3] } else { 255 };
            rgba.extend_from_slice(&[px[0], px[1], px[2], a]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, w as u32, h as u32).ok()
}

/// Opt this process into Windows dark mode (uxtheme ordinal 135) so the native
/// tray context menu matches the app theme.
#[cfg(windows)]
fn set_preferred_app_mode(dark: bool) {
    use std::os::raw::c_void;
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
    }
    let mode: i32 = if dark { 2 } else { 0 }; // ForceDark / Default
    let dll: Vec<u16> = "uxtheme.dll".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let lib = LoadLibraryW(dll.as_ptr());
        if lib.is_null() {
            return;
        }
        let proc = GetProcAddress(lib, 135 as *const u8);
        if proc.is_null() {
            return;
        }
        let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(proc);
        set_mode(mode);
    }
}

// ----------------------------------------------------------------------------
// Linux: StatusNotifier tray via `ksni`.
// ----------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod linux {
    use gtk::glib;

    use super::TrayActions;

    enum TrayCmd {
        Open,
        RestartDaemon,
        Quit,
    }

    struct IpnTray {
        icons: Vec<ksni::Icon>,
        tx: async_channel::Sender<TrayCmd>,
    }

    impl ksni::Tray for IpnTray {
        fn id(&self) -> String {
            "io.github.steeb_k.Nullgate".into()
        }
        fn title(&self) -> String {
            "Nullgate".into()
        }
        fn category(&self) -> ksni::Category {
            ksni::Category::ApplicationStatus
        }
        fn status(&self) -> ksni::Status {
            ksni::Status::Active
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icons.clone()
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.tx.try_send(TrayCmd::Open);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            use ksni::MenuItem;
            vec![
                StandardItem {
                    label: "Open Nullgate".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::Open);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Restart Nullgate daemon".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::RestartDaemon);
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit Nullgate".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    fn load_icons() -> Vec<ksni::Icon> {
        use gtk::gdk_pixbuf::{InterpType, Pixbuf};
        let Ok(src) = Pixbuf::from_read(std::io::Cursor::new(super::TRAY_PNG)) else {
            return Vec::new();
        };
        [22, 32, 48, 64]
            .into_iter()
            .filter_map(|sz| {
                let pb = src.scale_simple(sz, sz, InterpType::Bilinear)?;
                let pb = if pb.has_alpha() { pb } else { pb.add_alpha(false, 0, 0, 0).ok()? };
                let (w, h) = (pb.width(), pb.height());
                let rowstride = pb.rowstride() as usize;
                let nch = pb.n_channels() as usize;
                let rgba = pb.read_pixel_bytes();
                let rgba = rgba.as_ref();
                let mut data = Vec::with_capacity((w * h * 4) as usize);
                for y in 0..h as usize {
                    let row = &rgba[y * rowstride..y * rowstride + w as usize * nch];
                    for px in row.chunks_exact(nch) {
                        let a = if nch == 4 { px[3] } else { 255 };
                        data.extend_from_slice(&[a, px[0], px[1], px[2]]);
                    }
                }
                Some(ksni::Icon { width: w, height: h, data })
            })
            .collect()
    }

    pub fn install(actions: TrayActions) {
        let icons = load_icons();
        if icons.is_empty() {
            tracing::warn!("tray icon failed to decode; tray disabled");
            return;
        }
        let (tx, rx) = async_channel::unbounded::<TrayCmd>();

        glib::spawn_future_local(async move {
            while let Ok(cmd) = rx.recv().await {
                match cmd {
                    TrayCmd::Open => {
                        let _ = actions.open.try_send(());
                    }
                    TrayCmd::RestartDaemon => {
                        let _ = actions.restart_daemon.try_send(());
                    }
                    TrayCmd::Quit => {
                        let _ = actions.quit.try_send(());
                    }
                }
            }
        });

        let tray = IpnTray { icons, tx };
        let spawn = std::thread::Builder::new().name("ipn-tray".into()).spawn(move || {
            use ksni::TrayMethods;
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!("tray runtime unavailable: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                match tray.spawn().await {
                    Ok(_handle) => {
                        tracing::info!("system tray installed (ksni)");
                        std::future::pending::<()>().await;
                    }
                    Err(e) => tracing::warn!("tray unavailable: {e}"),
                }
            });
        });
        if let Err(e) = spawn {
            tracing::warn!("could not start tray thread: {e}");
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::install;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub fn install(_actions: TrayActions) {
    tracing::info!("tray not enabled on this platform build");
}
