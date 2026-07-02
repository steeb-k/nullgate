//! Per-connection admission: a **PSK challenge** that proves the peer holds the
//! network secret, plus a **Matrix-style emoji SAS** for human verification when
//! a new device joins.
//!
//! iroh already authenticates the transport: the connection is end-to-end
//! encrypted and `conn.remote_id()` is the TLS-verified peer NodeId (a relay
//! cannot forge it). On top of that we add:
//!   * **PSK proof** — knowing the network's discovery rendezvous is not enough
//!     to be admitted; the peer must also prove possession of the derived PSK.
//!   * **SAS** — a 7-emoji short authentication string, identical on both ends,
//!     derived from the session transcript. During a join the two humans compare
//!     it to confirm they're verifying the same device before it's vouched in.
//!
//! The transcript is built from the *sorted* pair of NodeIds and nonces so both
//! ends compute byte-identical values without a designated initiator.

use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use iroh::endpoint::Connection;
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::roster::Id;

type HmacSha256 = Hmac<Sha256>;

const DOMAIN: &[u8] = b"ipn-admission-v1";

/// Wire-protocol version for the member mesh + join handshakes. Exchanged in-band
/// at the start of every handshake so a version mismatch produces a clear error
/// (rather than a confusing connection failure). Bump on any incompatible change
/// to the handshake or data-plane framing.
pub const PROTOCOL_VERSION: u32 = 1;

/// Outcome of a successful handshake.
#[derive(Clone, Debug)]
pub struct Verified {
    /// The TLS-verified, PSK-proven peer NodeId.
    pub peer_id: Id,
    /// The 7-emoji short authentication string for this session (compare across
    /// the two devices during a join).
    pub sas: Vec<&'static str>,
}

/// Run the handshake as the **dialing** side (opens the control stream).
pub async fn dial(conn: &Connection, my_id: Id, psk: &[u8; 32], version: u32) -> Result<Verified> {
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;
    run(&mut send, &mut recv, my_id, *conn.remote_id().as_bytes(), psk, version).await
}

/// Run the handshake as the **accepting** side (accepts the control stream).
pub async fn accept(conn: &Connection, my_id: Id, psk: &[u8; 32], version: u32) -> Result<Verified> {
    let (mut send, mut recv) = conn.accept_bi().await.context("accept control stream")?;
    run(&mut send, &mut recv, my_id, *conn.remote_id().as_bytes(), psk, version).await
}

async fn run(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    my_id: Id,
    peer_id: Id,
    psk: &[u8; 32],
    version: u32,
) -> Result<Verified> {
    // 0. Exchange + check the protocol version, for a clear mismatch error.
    send.write_all(&version.to_be_bytes()).await.context("send version")?;
    let mut vbuf = [0u8; 4];
    recv.read_exact(&mut vbuf).await.context("recv version")?;
    let peer_version = u32::from_be_bytes(vbuf);
    if peer_version != version {
        // Finish (not reset) our send so the peer reliably reads our version and
        // produces the same clear error instead of a bare "connection lost".
        let _ = send.finish();
        bail!(
            "protocol mismatch: peer speaks Nullgate protocol v{peer_version}, we speak v{version} \
             — update Nullgate on one of the devices"
        );
    }

    // 1. Exchange fresh nonces.
    let mut na = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut na);
    send.write_all(&na).await.context("send nonce")?;
    let mut nb = [0u8; 32];
    recv.read_exact(&mut nb).await.context("recv nonce")?;

    // 2. Both sides derive the identical transcript and PSK tag, then exchange
    //    and compare tags in constant time.
    let transcript = transcript(&my_id, &peer_id, &na, &nb);
    let my_tag = tag(psk, &transcript);
    send.write_all(&my_tag).await.context("send tag")?;
    let mut peer_tag = [0u8; 32];
    recv.read_exact(&mut peer_tag).await.context("recv tag")?;

    if my_tag.ct_eq(&peer_tag).unwrap_u8() != 1 {
        bail!("admission failed: bad PSK (peer does not hold the network secret)");
    }

    Ok(Verified {
        peer_id,
        sas: sas_emojis(psk, &transcript),
    })
}

/// Canonical transcript: domain || sorted(id_a,id_b) || sorted(na,nb). Sorting
/// makes it independent of which side dialed.
fn transcript(a: &Id, b: &Id, na: &[u8; 32], nb: &[u8; 32]) -> Vec<u8> {
    let (lo_id, hi_id) = if a <= b { (a, b) } else { (b, a) };
    let (lo_n, hi_n) = if na <= nb { (na, nb) } else { (nb, na) };
    let mut t = Vec::with_capacity(DOMAIN.len() + 128);
    t.extend_from_slice(DOMAIN);
    t.extend_from_slice(lo_id);
    t.extend_from_slice(hi_id);
    t.extend_from_slice(lo_n);
    t.extend_from_slice(hi_n);
    t
}

fn tag(psk: &[u8; 32], transcript: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(psk).expect("HMAC accepts any key length");
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}

/// Derive the short authentication string: 7 emojis (42 bits) from the
/// transcript, domain-separated from the PSK tag.
pub fn sas_emojis(psk: &[u8; 32], transcript: &[u8]) -> Vec<&'static str> {
    let mut mac = HmacSha256::new_from_slice(psk).expect("HMAC accepts any key length");
    mac.update(b"sas");
    mac.update(transcript);
    let bytes: [u8; 32] = mac.finalize().into_bytes().into();
    // Take 7 * 6 = 42 bits as seven 6-bit indices into the emoji table.
    let mut out = Vec::with_capacity(7);
    let mut acc = 0u32;
    let mut nbits = 0u32;
    let mut idx = 0usize;
    while out.len() < 7 {
        if nbits < 6 {
            acc = (acc << 8) | bytes[idx] as u32;
            idx += 1;
            nbits += 8;
        }
        nbits -= 6;
        let v = ((acc >> nbits) & 0x3f) as usize;
        out.push(SAS_EMOJI[v]);
    }
    out
}

/// The 64-emoji SAS table (the Matrix SAS emoji set).
pub const SAS_EMOJI: [&str; 64] = [
    "🐶", "🐱", "🦁", "🐎", "🦄", "🐷", "🐘", "🐰", "🐼", "🐓", "🐧", "🐢", "🐟", "🐙", "🦋", "🌷",
    "🌳", "🌵", "🍄", "🌏", "🌙", "☁️", "🔥", "🍌", "🍎", "🍓", "🌽", "🍕", "🎂", "❤️", "😀", "🤖",
    "🎩", "👓", "🔧", "🎅", "👍", "☂️", "⌛", "⏰", "🎁", "💡", "📕", "✏️", "📎", "✂️", "🔒", "🔑",
    "🔨", "☎️", "🏁", "🚂", "🚲", "✈️", "🚀", "🏆", "⚽", "🎸", "🎺", "🔔", "⚓", "🎧", "📁", "📌",
];

/// Human-readable names for [`SAS_EMOJI`], index-aligned (the canonical Matrix SAS
/// emoji names). Text-only clients — the CLI, a piped/headless terminal — render
/// these words instead of the emojis, which a terminal can't reliably display and
/// two people can't reliably compare glyph-for-glyph. Keep this in lock-step with
/// `SAS_EMOJI`: same order, same length.
pub const SAS_WORD: [&str; 64] = [
    "Dog", "Cat", "Lion", "Horse", "Unicorn", "Pig", "Elephant", "Rabbit",
    "Panda", "Rooster", "Penguin", "Turtle", "Fish", "Octopus", "Butterfly", "Flower",
    "Tree", "Cactus", "Mushroom", "Globe", "Moon", "Cloud", "Fire", "Banana",
    "Apple", "Strawberry", "Corn", "Pizza", "Cake", "Heart", "Smiley", "Robot",
    "Hat", "Glasses", "Wrench", "Santa", "Thumbs Up", "Umbrella", "Hourglass", "Clock",
    "Gift", "Light Bulb", "Book", "Pencil", "Paperclip", "Scissors", "Lock", "Key",
    "Hammer", "Telephone", "Flag", "Train", "Bicycle", "Airplane", "Rocket", "Trophy",
    "Ball", "Guitar", "Trumpet", "Bell", "Anchor", "Headphones", "Folder", "Pin",
];

/// The word name for one SAS emoji, or `None` if it isn't in the table (shouldn't
/// happen for a value this crate produced).
pub fn word_for_emoji(emoji: &str) -> Option<&'static str> {
    SAS_EMOJI.iter().position(|e| *e == emoji).map(|i| SAS_WORD[i])
}

/// Render a received SAS (the emoji strings as they travel over IPC) as words,
/// for a text-only client. Unknown entries fall back to `"?"` rather than being
/// dropped, so the word count still matches the emoji count.
pub fn sas_words(sas: &[String]) -> Vec<&'static str> {
    sas.iter()
        .map(|e| word_for_emoji(e).unwrap_or("?"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_is_order_independent() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let na = [10u8; 32];
        let nb = [20u8; 32];
        // Whoever computes it, the bytes are identical.
        assert_eq!(transcript(&a, &b, &na, &nb), transcript(&b, &a, &nb, &na));
    }

    #[test]
    fn both_sides_compute_the_same_tag_and_sas() {
        let psk = [9u8; 32];
        let a = [1u8; 32];
        let b = [2u8; 32];
        let na = [10u8; 32];
        let nb = [20u8; 32];
        let t_ab = transcript(&a, &b, &na, &nb);
        let t_ba = transcript(&b, &a, &nb, &na);
        assert_eq!(tag(&psk, &t_ab), tag(&psk, &t_ba));
        assert_eq!(sas_emojis(&psk, &t_ab), sas_emojis(&psk, &t_ba));
    }

    #[test]
    fn wrong_psk_yields_different_tag() {
        let t = transcript(&[1u8; 32], &[2u8; 32], &[3u8; 32], &[4u8; 32]);
        assert_ne!(tag(&[0u8; 32], &t), tag(&[1u8; 32], &t));
    }

    #[test]
    fn every_emoji_has_a_word_and_they_round_trip() {
        assert_eq!(SAS_EMOJI.len(), SAS_WORD.len());
        // Words are the display form of exactly the emojis we emit.
        for (i, e) in SAS_EMOJI.iter().enumerate() {
            assert_eq!(word_for_emoji(e), Some(SAS_WORD[i]));
        }
        // A real SAS (emoji Strings, as they arrive over IPC) maps to its words.
        let t = transcript(&[1u8; 32], &[2u8; 32], &[3u8; 32], &[4u8; 32]);
        let sas: Vec<String> = sas_emojis(&[7u8; 32], &t).iter().map(|s| s.to_string()).collect();
        let words = sas_words(&sas);
        assert_eq!(words.len(), sas.len());
        assert!(words.iter().all(|w| *w != "?"), "no emoji should be unmapped");
    }

    #[test]
    fn sas_is_seven_emojis_and_varies() {
        let t1 = transcript(&[1u8; 32], &[2u8; 32], &[3u8; 32], &[4u8; 32]);
        let t2 = transcript(&[1u8; 32], &[2u8; 32], &[3u8; 32], &[5u8; 32]);
        let s1 = sas_emojis(&[7u8; 32], &t1);
        assert_eq!(s1.len(), 7);
        assert_ne!(s1, sas_emojis(&[7u8; 32], &t2));
    }
}
