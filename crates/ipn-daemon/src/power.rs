//! Follow the machine's power state, so a sleeping device stops flapping on the mesh.
//!
//! The daemon survives a suspend — the OS simply freezes it. Peers eventually time
//! the QUIC connection out, and then *every* wake brings the mesh straight back up.
//! On macOS that includes **dark wakes**, which Power Nap schedules every few
//! minutes on battery whether or not "wake for network access" is on. Each
//! reconnect is a real handshake, so every other device in the pool announces
//! "<host> came online" — several times a night, for a laptop that never left its
//! bag. The `notify_newly_online` debounce in `ipn-gui` can't help: it suppresses
//! blips shorter than two minutes, and a sleeping laptop has been dark for hours.
//!
//! So the daemon leaves the network before the system sleeps, and rejoins only on a
//! *full* wake. A dark wake stays offline, which also happens to be the truth:
//! nothing can reach a laptop that is seconds away from sleeping again.
//!
//! Only macOS has a backend. Windows' Modern Standby (S0) has the identical problem
//! and wants `PowerRegisterSuspendResumeNotification`; Linux wants a logind sleep
//! inhibitor. Both would reuse [`PowerHandler`] — the policy here is deliberately
//! platform-free — but neither is wired up, because neither can be tested yet.

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Arc;

use ipn_core::Engine;

#[cfg(target_os = "macos")]
mod macos;

/// Start watching for system sleep/wake, if this platform has a backend. Set
/// `NULLGATE_DISABLE_POWER_EVENTS=1` to opt out and keep the old behavior.
pub fn spawn(engine: Engine) {
    if std::env::var_os("NULLGATE_DISABLE_POWER_EVENTS").is_some() {
        tracing::info!("power: sleep/wake handling disabled (NULLGATE_DISABLE_POWER_EVENTS)");
        return;
    }
    #[cfg(target_os = "macos")]
    macos::spawn(Arc::new(PowerHandler::new(engine)));
    #[cfg(not(target_os = "macos"))]
    {
        let _ = engine;
        tracing::debug!("power: no sleep/wake backend on this platform");
    }
}

/// What to do when the machine sleeps and wakes. Platform backends call these; the
/// decision of *when* is theirs, the decision of *what* is here.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct PowerHandler {
    engine: Engine,
    /// Set when *we* took the network down for a suspend. A wake only rejoins a
    /// device that was actually connected — someone who hit "Disconnect" before
    /// closing the lid stays disconnected.
    resume_on_wake: AtomicBool,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl PowerHandler {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            resume_on_wake: AtomicBool::new(false),
        }
    }

    /// The system is about to sleep. Leave the network now, so peers see a clean
    /// close rather than an idle timeout minutes later — and so the next dark wake
    /// finds nothing to bring back up.
    ///
    /// The caller must not let the machine sleep until this returns.
    pub(crate) async fn on_sleep(&self) {
        // `status()` errors when no network is configured, and reports `online:
        // false` when the user already disconnected by hand. Either way there is
        // nothing to suspend — and so nothing to restore on wake.
        if !self.engine.status().await.map(|s| s.online).unwrap_or(false) {
            return;
        }
        // Record the intent to resume *before* tearing down: if the disconnect only
        // half-succeeds, the next full wake should still put us back on the mesh.
        self.resume_on_wake.store(true, Ordering::SeqCst);
        match self.engine.set_online(false).await {
            Ok(()) => tracing::info!("power: system sleeping — left the network"),
            Err(e) => tracing::warn!("power: leaving the network before sleep failed: {e:#}"),
        }
    }

    /// A full (user) wake — not a dark wake. Rejoin, but only if we were the ones
    /// who left.
    pub(crate) async fn on_wake(&self) {
        if !self.resume_on_wake.swap(false, Ordering::SeqCst) {
            return;
        }
        match self.engine.set_online(true).await {
            Ok(()) => tracing::info!("power: system awake — rejoined the network"),
            Err(e) => tracing::warn!("power: rejoining the network after wake failed: {e:#}"),
        }
    }
}
