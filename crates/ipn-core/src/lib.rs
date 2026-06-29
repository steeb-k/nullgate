//! `ipn-core` — the engine for **iroh-private-network** (IPN): a peer-to-peer
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
pub mod engine;
pub mod geo;
pub mod membership;
pub mod network;
pub mod node;
pub mod presence;
pub mod roster;
pub mod router;
pub mod secrets;
pub mod tun_device;

pub use engine::{Engine, EngineEvent, MemberView, NetworkStatus};
pub use network::{NetworkSecret, Ticket};
pub use node::IrohNode;
pub use roster::{Config, Entry, Member, Op, Roster};
