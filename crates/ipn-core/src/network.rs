//! Network identity: the single root **network secret** and everything derived
//! from it, plus the join **ticket** that carries a network to a new device.
//!
//! One secret, many keys (HKDF-SHA256, domain-separated) so the user manages
//! exactly one thing:
//!   * `network_id`        — stable id used for entry domain separation,
//!   * `psk`               — admission proof (HMAC key), rotatable mass-revoke,
//!   * `rendezvous`        — private discovery seed (outsiders can't find us),
//!   * `docs_namespace`    — the iroh-docs replica everyone opens *deterministically*
//!     (so the roster syncs with no doc-ticket exchange).
//!
//! The **originator master key** is a *separate*, exportable ed25519 keypair (the
//! sole authority for removals/freeze); only its public half travels in the
//! ticket. See [[roster]] for how these are used.

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use iroh::EndpointAddr;
use hkdf::Hkdf;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::net::Ipv4Addr;

use crate::roster::{Id, InviteKind, Nonce};

const HKDF_SALT: &[u8] = b"ipn-v1";
const TICKET_PREFIX: &str = "ng1";

/// The root secret for a network. Whoever holds it can find, authenticate to,
/// and write the roster of the network — membership is then gated by the role
/// rules in [`crate::roster`]. Rotating it is the hard mass-revoke.
#[derive(Clone)]
pub struct NetworkSecret([u8; 32]);

impl NetworkSecret {
    pub fn generate() -> Self {
        let mut b = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut b);
        Self(b)
    }

    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    fn derive(&self, info: &[u8]) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), &self.0);
        let mut okm = [0u8; 32];
        hk.expand(info, &mut okm).expect("32 is a valid HKDF length");
        okm
    }

    /// Stable network identifier (domain separation for signed roster entries).
    pub fn network_id(&self) -> Id {
        self.derive(b"network-id")
    }

    /// Pre-shared key for the admission HMAC proof.
    pub fn psk(&self) -> [u8; 32] {
        self.derive(b"psk")
    }

    /// Seed for the private discovery rendezvous (gossip topic).
    pub fn rendezvous(&self) -> [u8; 32] {
        self.derive(b"rendezvous")
    }

    /// Seed for the deterministic iroh-docs namespace that holds the roster.
    pub fn docs_namespace_seed(&self) -> [u8; 32] {
        self.derive(b"docs-namespace")
    }
}

impl std::fmt::Debug for NetworkSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NetworkSecret(<redacted>)")
    }
}

/// A join ticket: the minimum a new device needs to find, authenticate to, and
/// validate a network. It carries the secret (so the holder can derive psk /
/// rendezvous / namespace) and the originator's *public* key (to validate the
/// roster). It does **not** carry the originator master *secret*.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ticket {
    pub name: String,
    pub subnet: [u8; 4],
    secret: [u8; 32],
    pub originator_id: Id,
    /// The inviter's dialable address for the initial bootstrap (full address, so
    /// it works before DNS discovery propagates). Other members are then reached
    /// by NodeId via iroh discovery.
    pub bootstrap: EndpointAddr,
    /// Which tier this ticket admits the joiner at (`Peer` or `Controller`).
    #[serde(default = "default_invite_kind")]
    pub invite_kind: InviteKind,
    /// The current invite nonce the admitting `Add` must cite — ties the join to
    /// the live invite so regenerating a ticket invalidates the prior one.
    #[serde(default)]
    pub invite_nonce: Nonce,
}

fn default_invite_kind() -> InviteKind {
    InviteKind::Peer
}

impl Ticket {
    pub fn new(
        name: String,
        subnet: Ipv4Addr,
        secret: &NetworkSecret,
        originator_id: Id,
        bootstrap: EndpointAddr,
        invite_kind: InviteKind,
        invite_nonce: Nonce,
    ) -> Self {
        Self {
            name,
            subnet: subnet.octets(),
            secret: secret.to_bytes(),
            originator_id,
            bootstrap,
            invite_kind,
            invite_nonce,
        }
    }

    pub fn secret(&self) -> NetworkSecret {
        NetworkSecret::from_bytes(self.secret)
    }

    pub fn subnet(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.subnet)
    }

    /// Encode to a single copy-pasteable string: `ng1<base32>`.
    pub fn encode(&self) -> String {
        let mut buf = Vec::new();
        ciborium::into_writer(self, &mut buf).expect("serialize ticket");
        format!(
            "{TICKET_PREFIX}{}",
            data_encoding::BASE32_NOPAD.encode(&buf).to_lowercase()
        )
    }

    pub fn decode(s: &str) -> Result<Self> {
        let s = s.trim();
        let body = s
            .strip_prefix(TICKET_PREFIX)
            .context("not a Nullgate ticket (missing ng1 prefix)")?;
        let bytes = data_encoding::BASE32_NOPAD
            .decode(body.to_uppercase().as_bytes())
            .context("ticket is not valid base32")?;
        let ticket: Ticket = ciborium::from_reader(bytes.as_slice()).context("decode ticket")?;
        if ticket.name.len() > 128 {
            bail!("ticket name too long");
        }
        Ok(ticket)
    }
}

/// The originator master keypair — exportable super-admin authority. Generate
/// once when creating a network; back up the recovery code so the authority
/// survives device loss; re-import on a new device.
pub fn generate_originator_key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

const RECOVERY_PREFIX: &str = "ngkey1";

/// Encode the originator master secret as a single copy/save-able **recovery
/// code** (`ngkey1<base32>`). Anyone holding this can administer the network, so
/// it must be stored securely.
pub fn encode_recovery_key(secret: &[u8; 32]) -> String {
    format!(
        "{RECOVERY_PREFIX}{}",
        data_encoding::BASE32_NOPAD.encode(secret).to_lowercase()
    )
}

/// Decode a recovery code back to the 32-byte originator secret.
pub fn decode_recovery_key(s: &str) -> Result<[u8; 32]> {
    let body = s
        .trim()
        .strip_prefix(RECOVERY_PREFIX)
        .context("not a Nullgate recovery code (missing ngkey1 prefix)")?;
    let bytes = data_encoding::BASE32_NOPAD
        .decode(body.to_uppercase().as_bytes())
        .context("recovery code is not valid base32")?;
    bytes
        .as_slice()
        .try_into()
        .context("recovery code must be 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivations_are_stable_and_distinct() {
        let s = NetworkSecret::from_bytes([42u8; 32]);
        // Stable across calls.
        assert_eq!(s.psk(), s.psk());
        assert_eq!(s.network_id(), s.network_id());
        // Distinct from each other (domain separation works).
        assert_ne!(s.psk(), s.rendezvous());
        assert_ne!(s.psk(), s.network_id());
        assert_ne!(s.rendezvous(), s.docs_namespace_seed());
        assert_ne!(s.network_id(), s.docs_namespace_seed());
    }

    #[test]
    fn different_secrets_derive_differently() {
        let a = NetworkSecret::from_bytes([1u8; 32]);
        let b = NetworkSecret::from_bytes([2u8; 32]);
        assert_ne!(a.psk(), b.psk());
        assert_ne!(a.network_id(), b.network_id());
    }

    #[test]
    fn ticket_roundtrips() {
        let secret = NetworkSecret::from_bytes([7u8; 32]);
        // A valid ed25519 public key (raw bytes like [4u8;32] aren't a valid point).
        let boot_id = iroh::SecretKey::from_bytes(&[4u8; 32]).public();
        let boot = EndpointAddr::from_parts(boot_id, Vec::<iroh::TransportAddr>::new());
        let t = Ticket::new(
            "home".into(),
            Ipv4Addr::new(10, 99, 0, 0),
            &secret,
            [3u8; 32],
            boot.clone(),
            InviteKind::Peer,
            [5u8; 16],
        );
        let encoded = t.encode();
        assert!(encoded.starts_with("ng1"));
        let back = Ticket::decode(&encoded).unwrap();
        assert_eq!(back.name, "home");
        assert_eq!(back.subnet(), Ipv4Addr::new(10, 99, 0, 0));
        assert_eq!(back.secret().to_bytes(), secret.to_bytes());
        assert_eq!(back.originator_id, [3u8; 32]);
        assert_eq!(back.bootstrap, boot);
        assert_eq!(back.invite_nonce, [5u8; 16]);
    }

    #[test]
    fn ticket_rejects_garbage() {
        assert!(Ticket::decode("hello").is_err());
        assert!(Ticket::decode("ng1!!!!").is_err());
    }

    #[test]
    fn recovery_key_roundtrips() {
        let secret = [42u8; 32];
        let code = encode_recovery_key(&secret);
        assert!(code.starts_with("ngkey1"));
        assert_eq!(decode_recovery_key(&code).unwrap(), secret);
        assert!(decode_recovery_key("nope").is_err());
        assert!(decode_recovery_key("ngkey1!!!!").is_err());
    }
}
