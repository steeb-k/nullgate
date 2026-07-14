//! System tray integration for the **tray agent** (`--agent`). Three fixed actions:
//! **Open Nullgate**, **Restart Nullgate daemon**, and **Quit Nullgate** — plus a
//! dynamic section above them holding one entry per device that has an optional
//! action configured (see `actions.rs`).
//!
//! On Windows/macOS we use the `tray-icon` crate. On Linux that crate's backend
//! pulls in GTK3 + libappindicator, which clashes with this GTK4 app, so Linux
//! uses a pure-Rust StatusNotifier implementation (`ksni`) on its own thread,
//! bridged back to the GTK main loop over an `async-channel`.
//!
//! The tray lives in the lightweight user-session agent, not the GUI, so it
//! survives the GUI window being closed or crashing. Each action is delivered to
//! the agent over a dedicated `async-channel`; the agent decides what to do
//! (launch the GUI, restart the privileged daemon, disconnect + quit, or spawn a
//! device's command).
//!
//! The device section is *pushed*, not pulled: the agent sends a fresh list on an
//! `async-channel` whenever the member list or the actions file changes, and each
//! backend rebuilds its menu from it. Neither backend can hand out a live menu
//! handle across threads, so this is the seam.

/// One entry in the tray's per-device section.
#[derive(Clone, PartialEq, Eq)]
pub struct TrayItem {
    /// Which member to run the action for; echoed back on [`TrayActions::run_action`].
    pub node_id: String,
    /// What the entry reads — "workshop-pc (RDP)".
    pub text: String,
}

/// Where the tray sends each menu action. The agent owns the receiving ends.
#[derive(Clone)]
pub struct TrayActions {
    /// Open (or focus) the GUI window. Also fired by clicking the tray icon.
    pub open: async_channel::Sender<()>,
    /// (Re)start the privileged Nullgate daemon (elevates on the agent side).
    pub restart_daemon: async_channel::Sender<()>,
    /// Disconnect from the network and quit the agent.
    pub quit: async_channel::Sender<()>,
    /// Run one device's action button, by NodeId.
    pub run_action: async_channel::Sender<String>,
}

// Tray icon (the only one): the "gate" mark, used as-is on every theme. The 64px
// source is scaled down at runtime to whatever each platform's tray wants.
const TRAY_PNG: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../img/nullgate-tray-icon-64.png"));

#[cfg(any(windows, target_os = "macos"))]
pub fn install(actions: TrayActions, items_rx: async_channel::Receiver<Vec<TrayItem>>) {
    use std::collections::HashMap;

    use gtk::glib;
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    // Make native popups (the tray context menu) honor the app's color scheme.
    #[cfg(windows)]
    set_preferred_app_mode(adw::StyleManager::default().is_dark());

    /// The whole menu, rebuilt from scratch whenever the device section changes.
    /// Rebuilding mints new `MenuId`s, so the ids the event poll matches on are
    /// returned alongside it rather than captured once.
    struct Ids {
        open: MenuId,
        restart: MenuId,
        quit: MenuId,
        /// Menu id → NodeId, for the device section.
        devices: HashMap<MenuId, String>,
    }

    fn build_menu(items: &[TrayItem]) -> (Menu, Ids) {
        let menu = Menu::new();
        let mut devices = HashMap::new();
        // Device actions come first, in their own section above the fixed items.
        for item in items {
            let mi = MenuItem::new(&item.text, true, None);
            devices.insert(mi.id().clone(), item.node_id.clone());
            let _ = menu.append(&mi);
        }
        if !items.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let open = MenuItem::new("Open Nullgate", true, None);
        let restart = MenuItem::new("Restart Nullgate daemon", true, None);
        let quit = MenuItem::new("Quit Nullgate", true, None);
        let _ = menu.append(&open);
        let _ = menu.append(&restart);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit);
        let ids = Ids {
            open: open.id().clone(),
            restart: restart.id().clone(),
            quit: quit.id().clone(),
            devices,
        };
        (menu, ids)
    }

    let (menu, mut ids) = build_menu(&[]);
    let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Nullgate");
    if let Some(icon) = load_tray_icon() {
        builder = builder.with_icon(icon);
    }
    let icon = match builder.build() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("tray unavailable: {e}");
            return;
        }
    };
    tracing::info!("system tray installed");

    // tray-icon delivers events on global channels; poll them on the GTK loop. The
    // icon is moved into the closure so it stays alive — and so the menu can be
    // swapped when the device section changes.
    let mut shown: Vec<TrayItem> = Vec::new();
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        // Coalesce: only the newest list matters, and rebuilding the menu while its
        // popup is open would be wasted work anyway.
        let mut latest = None;
        while let Ok(items) = items_rx.try_recv() {
            latest = Some(items);
        }
        if let Some(items) = latest {
            if items != shown {
                let (menu, new_ids) = build_menu(&items);
                icon.set_menu(Some(Box::new(menu)));
                ids = new_ids;
                shown = items;
            }
        }

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
            if ev.id == ids.open {
                open_window = true;
            } else if ev.id == ids.restart {
                restart_daemon = true;
            } else if ev.id == ids.quit {
                quit = true;
            } else if let Some(node_id) = ids.devices.get(&ev.id) {
                let _ = actions.run_action.try_send(node_id.clone());
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

    use super::{TrayActions, TrayItem};

    enum TrayCmd {
        Open,
        RestartDaemon,
        Quit,
        RunAction(String),
    }

    struct IpnTray {
        icons: Vec<ksni::Icon>,
        /// The dynamic per-device section; replaced wholesale via `Handle::update`.
        items: Vec<TrayItem>,
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

            let mut out: Vec<MenuItem<Self>> = Vec::new();
            // Device actions first, in their own section above the fixed items.
            for item in &self.items {
                let node_id = item.node_id.clone();
                out.push(
                    StandardItem {
                        label: item.text.clone(),
                        activate: Box::new(move |t: &mut Self| {
                            let _ = t.tx.try_send(TrayCmd::RunAction(node_id.clone()));
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            if !self.items.is_empty() {
                out.push(MenuItem::Separator);
            }
            out.push(
                StandardItem {
                    label: "Open Nullgate".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::Open);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            out.push(
                StandardItem {
                    label: "Restart Nullgate daemon".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::RestartDaemon);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            out.push(MenuItem::Separator);
            out.push(
                StandardItem {
                    label: "Quit Nullgate".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.try_send(TrayCmd::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            out
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

    pub fn install(actions: TrayActions, items_rx: async_channel::Receiver<Vec<TrayItem>>) {
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
                    TrayCmd::RunAction(node_id) => {
                        let _ = actions.run_action.try_send(node_id);
                    }
                }
            }
        });

        let tray = IpnTray { icons, items: Vec::new(), tx };
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
                    Ok(handle) => {
                        tracing::info!("system tray installed (ksni)");
                        // The handle is the only way to mutate a spawned ksni tray, and
                        // it lives on this thread — so the device section is fed in over
                        // the channel rather than touched from the GTK loop.
                        while let Ok(items) = items_rx.recv().await {
                            handle
                                .update(|tray: &mut IpnTray| {
                                    tray.items = items;
                                })
                                .await;
                        }
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
pub fn install(_actions: TrayActions, _items_rx: async_channel::Receiver<Vec<TrayItem>>) {
    tracing::info!("tray not enabled on this platform build");
}
