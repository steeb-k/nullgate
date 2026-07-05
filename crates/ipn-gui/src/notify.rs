//! Desktop notifications — now owned by the **tray agent** (`--agent`), so peer
//! online/offline and join-request alerts fire even when the GUI window is closed.
//!
//! Linux/macOS use GLib's `GNotification`; **Windows uses native WinRT toasts**
//! (Action Center), because `GNotification`'s Windows backend spawns a confusing
//! second notification-area icon beside the tray icon. Every notification is
//! click-to-open: on Linux/macOS the body click (and an "Open Nullgate" button)
//! activate the agent's [`OPEN_GUI_ACTION`]; on Windows the toast's `on_activated`
//! launches the GUI directly. Both routes call [`crate::launch_gui`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

// `send_notification` (GNotification) is used only on Linux/macOS; on Windows we
// emit WinRT toasts instead, so the GTK prelude would be unused there.
#[cfg(not(windows))]
use adw::prelude::*;
use ipn_ipc::NetworkStatus;

/// Name of the GApplication action the agent registers to open the GUI window.
/// A notification's default click + its "Open Nullgate" button target
/// `app.<OPEN_GUI_ACTION>` (Linux/macOS). On Windows the toast bypasses this and
/// launches the GUI from its activation callback instead.
pub(crate) const OPEN_GUI_ACTION: &str = "open-gui";

/// Show a desktop notification (title + optional body), click-to-open wired in.
///
/// Repeats of the same title are throttled to once per 30s (a peer flapping
/// offline/online during an update shouldn't burst toasts). `app` is the agent's
/// (headless) application; on Windows it is unused (WinRT toasts are attributed to
/// the registered AppUserModelID instead — see [`init_windows_app_id`]).
pub(crate) fn notify(app: &adw::Application, title: &str, body: Option<&str>) {
    // notify() is only ever called on the GTK main thread, so thread_local is safe.
    thread_local! {
        static LAST: std::cell::RefCell<HashMap<String, Instant>> =
            std::cell::RefCell::new(HashMap::new());
    }
    let suppressed = LAST.with(|m| {
        let mut m = m.borrow_mut();
        let now = Instant::now();
        if m.get(title).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(30)) {
            return true;
        }
        m.insert(title.to_string(), now);
        false
    });
    if suppressed {
        return;
    }

    #[cfg(not(windows))]
    {
        let action = format!("app.{OPEN_GUI_ACTION}");
        let n = gtk::gio::Notification::new(title);
        if let Some(b) = body {
            n.set_body(Some(b));
        }
        // The agent's application id has no `.desktop` entry, so pin the icon
        // explicitly rather than relying on the notification daemon's desktop-entry
        // lookup (registered under APP_ID by `crate::install_app_icon`).
        n.set_icon(&gtk::gio::ThemedIcon::new(crate::APP_ID));
        // Clicking the notification body — or its button — opens the window.
        n.set_default_action(&action);
        n.add_button("Open Nullgate", &action);
        app.send_notification(None, &n);
    }
    #[cfg(windows)]
    {
        let _ = app;
        windows_toast(title, body);
    }
}

/// Show a native Windows toast (Action Center). Attributed to our registered
/// AppUserModelID (see [`init_windows_app_id`]); clicking it — or its button —
/// launches the GUI via [`crate::launch_gui`]. Failures are non-fatal.
#[cfg(windows)]
fn windows_toast(title: &str, body: Option<&str>) {
    use tauri_winrt_notification::Toast;
    let mut toast = Toast::new(crate::APP_ID).title(title);
    if let Some(b) = body {
        toast = toast.text1(b);
    }
    // The action string is delivered to on_activated; we open the GUI regardless of
    // whether the body or the explicit button was clicked.
    toast = toast.add_button("Open Nullgate", "open").on_activated(|_action| {
        crate::launch_gui();
        Ok(())
    });
    if let Err(e) = toast.show() {
        tracing::debug!("windows toast failed: {e}");
    }
}

/// Windows: bind this process to our AppUserModelID and register it under HKCU so
/// WinRT toasts are permitted and attributed to "Nullgate" (the host exe otherwise).
/// `ToastNotificationManager` refuses to show toasts for an unregistered AUMID.
/// Idempotent — safe to call on every launch; the MSI shortcut carries the same id.
#[cfg(windows)]
pub(crate) fn init_windows_app_id() {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn SetCurrentProcessExplicitAppUserModelID(app_id: *const u16) -> i32;
    }
    let wide: Vec<u16> =
        OsStr::new(crate::APP_ID).encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(wide.as_ptr());
    }
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    if let Ok((key, _)) =
        hkcu.create_subkey(format!(r"Software\Classes\AppUserModelId\{}", crate::APP_ID))
    {
        let _ = key.set_value("DisplayName", &"Nullgate");
    }
}

/// How long a peer must have been offline before we announce it came back online.
/// This absorbs the brief presence blips from the daemon's memory-watchdog
/// restarts (iroh#4293 stopgap) so a device restarting doesn't spam every machine
/// on the mesh with "came online". Override with `NULLGATE_ONLINE_DEBOUNCE_SECS`.
fn online_notify_debounce() -> Duration {
    const DEFAULT_SECS: u64 = 120; // 2 minutes — headroom for slower machines.
    let secs = std::env::var("NULLGATE_ONLINE_DEBOUNCE_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_SECS);
    Duration::from_secs(secs)
}

/// Notify when a peer transitions offline→online, but only after it had been
/// offline for at least [`online_notify_debounce`]. `offline_since` records, per
/// peer, when we first observed it dark this session; a peer that reappears sooner
/// than the debounce (a restart blip) is announced silently.
pub(crate) fn notify_newly_online(
    app: &adw::Application,
    prev: Option<&NetworkStatus>,
    new: &NetworkStatus,
    offline_since: &mut HashMap<String, Instant>,
) {
    let debounce = online_notify_debounce();
    let now = Instant::now();
    for m in &new.members {
        if m.is_self {
            continue;
        }
        let was_online_prev = prev
            .map(|p| p.members.iter().any(|q| q.node_id == m.node_id && q.online))
            .unwrap_or(false);
        let announce = online_transition_notifies(
            &m.node_id,
            m.online,
            prev.is_some(),
            was_online_prev,
            offline_since,
            now,
            debounce,
        );
        if announce {
            let name = m
                .label
                .clone()
                .or_else(|| m.hostname.clone())
                .unwrap_or_else(|| crate::short_id(&m.node_id));
            notify(app, &format!("{name} came online"), None);
        }
    }
    // Forget peers no longer in the network so the map can't accrete stale ids.
    offline_since.retain(|id, _| new.members.iter().any(|m| &m.node_id == id));
}

/// Pure decision + bookkeeping for one peer, factored out of [`notify_newly_online`]
/// so the debounce timing is testable without GTK. Updates `offline_since` and
/// returns whether to show a "came online" toast. `now`/`debounce` are injected so
/// tests can drive the clock deterministically.
fn online_transition_notifies(
    node_id: &str,
    online: bool,
    have_baseline: bool,
    was_online_prev: bool,
    offline_since: &mut HashMap<String, Instant>,
    now: Instant,
    debounce: Duration,
) -> bool {
    if !online {
        // Offline now: remember when it first went dark (keep the earliest stamp).
        offline_since.entry(node_id.to_string()).or_insert(now);
        return false;
    }
    // Back online: clear any offline stamp and measure how long it was gone.
    let was_offline_for = offline_since.remove(node_id).map(|t| now.duration_since(t));
    // First snapshot (no baseline) adopts state silently; an already-online peer
    // is no transition at all.
    if !have_baseline || was_online_prev {
        return false;
    }
    // Suppress a brief absence (a watchdog restart blip); announce a real return,
    // or a peer we never observed offline (e.g. one just added to the network).
    was_offline_for.is_none_or(|d| d >= debounce)
}

#[cfg(test)]
mod online_debounce_tests {
    use super::*;

    const DB: Duration = Duration::from_secs(120);

    /// A peer offline longer than the debounce, then back, is announced.
    #[test]
    fn long_absence_notifies() {
        let mut seen = HashMap::new();
        let t0 = Instant::now();
        // Observed offline at t0 (baseline exists, was offline in prev).
        assert!(!online_transition_notifies("a", false, true, false, &mut seen, t0, DB));
        // Back online 200s later → real return.
        let later = t0 + Duration::from_secs(200);
        assert!(online_transition_notifies("a", true, true, false, &mut seen, later, DB));
        assert!(!seen.contains_key("a")); // stamp cleared
    }

    /// A restart blip (offline < debounce) is suppressed.
    #[test]
    fn brief_blip_is_silent() {
        let mut seen = HashMap::new();
        let t0 = Instant::now();
        assert!(!online_transition_notifies("a", false, true, false, &mut seen, t0, DB));
        let later = t0 + Duration::from_secs(20); // watchdog-restart-sized gap
        assert!(!online_transition_notifies("a", true, true, false, &mut seen, later, DB));
    }

    /// The earliest offline stamp wins, so repeated offline snapshots don't reset
    /// the clock and let a long absence masquerade as a blip.
    #[test]
    fn offline_stamp_is_not_reset() {
        let mut seen = HashMap::new();
        let t0 = Instant::now();
        online_transition_notifies("a", false, true, false, &mut seen, t0, DB);
        // Seen offline again 90s later — must keep the t0 stamp.
        online_transition_notifies("a", false, true, false, &mut seen, t0 + Duration::from_secs(90), DB);
        // Online at t0+130s → 130s ≥ 120s from the *original* stamp → notify.
        assert!(online_transition_notifies("a", true, true, false, &mut seen, t0 + Duration::from_secs(130), DB));
    }

    /// No baseline snapshot (first status, or just-reconnected) never notifies.
    #[test]
    fn first_snapshot_is_silent() {
        let mut seen = HashMap::new();
        assert!(!online_transition_notifies("a", true, false, false, &mut seen, Instant::now(), DB));
    }

    /// A peer already online in the previous snapshot is no transition.
    #[test]
    fn already_online_is_no_transition() {
        let mut seen = HashMap::new();
        assert!(!online_transition_notifies("a", true, true, true, &mut seen, Instant::now(), DB));
    }

    /// A peer that appears online without ever being seen offline (e.g. just added
    /// to the network) is announced.
    #[test]
    fn never_seen_offline_notifies() {
        let mut seen = HashMap::new();
        assert!(online_transition_notifies("a", true, true, false, &mut seen, Instant::now(), DB));
    }
}
