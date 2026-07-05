//! `ipn-core` — the engine for **Nullgate**: a peer-to-peer
//! virtual LAN over [iroh].
//!
//! This crate is UI-agnostic and IPC-agnostic (it compiles to Android too): it
//! owns the iroh node, the membership roster, admission/verification, packet
//! routing, and presence. Desktop runs it inside a privileged daemon; Android
//! runs it in-process behind a UniFFI facade.
//!
//! [iroh]: https://www.iroh.computer
//!
//! Current status: Phase 0 scaffold — the iroh node ([`IrohNode`]) is in place;
//! the router, roster, admission, and TUN layers land next.

pub mod admission;
pub mod conntrack;
pub mod engine;
// Geolocation is an originator-only convenience (resolve members' public IPs to a
// city) and pulls a second TLS stack (ureq) + the MMDB reader; it isn't shipped on
// mobile, where it would only bloat the APK.
#[cfg(not(target_os = "android"))]
pub mod geo;
pub mod membership;
pub mod network;
pub mod node;
pub mod presence;
pub mod roster;
pub mod router;
pub mod secrets;
pub mod tun_device;

use std::sync::OnceLock;

pub use engine::{AuditEntry, Engine, EngineEvent, MemberView, NetworkStatus};
pub use network::{NetworkSecret, Ticket};
pub use node::IrohNode;
pub use roster::{Config, Entry, InviteKind, Member, Op, Role, Roster};

/// Process-wide override for this device's shared display name (the value other
/// members see, normally the OS hostname). Android sets this once at startup to a
/// stable, user-uneditable `"<Manufacturer> <Model> (<suffix>)"` string, because
/// the Android OS hostname is meaningless (`localhost`). Desktop never sets it, so
/// [`engine`]'s `current_hostname()` keeps reading the live OS hostname there.
static DEVICE_NAME: OnceLock<String> = OnceLock::new();

/// Set the device-name override (see [`DEVICE_NAME`]). First write wins; later
/// calls are ignored. Call before [`Engine::start`].
pub fn set_device_name_override(name: String) {
    let _ = DEVICE_NAME.set(name);
}

/// The device-name override, if one was set.
pub(crate) fn device_name_override() -> Option<String> {
    DEVICE_NAME.get().cloned()
}
