//! End-to-end proof that the roster's role rules hold over **real iroh-docs
//! replication** — not just in the in-memory unit tests.
//!
//! Two in-process nodes share one membership document (both hold the write
//! capability). Node A is the originator. We show:
//!   1. A web-of-trust add by A's device reaches B, who folds it into the same
//!      membership.
//!   2. After A (originator) removes B, B — still holding the doc write-cap —
//!      forges an `Add`. That forged entry *replicates to A's document*, yet A's
//!      rebuilt roster rejects it (signer is no longer a member) and B stays out.
//!
//! Marked `#[ignore]` because it opens real iroh endpoints (discovery/relay);
//! run with: `cargo test -p ipn-core --test membership_docs -- --ignored`.

use std::time::Duration;

use anyhow::Result;
use ed25519_dalek::SigningKey;
use iroh::{protocol::Router, Endpoint};
use iroh_blobs::{store::mem::MemStore, BlobsProtocol};
use iroh_docs::{
    api::protocol::{AddrInfoOptions, ShareMode},
    protocol::Docs,
};
use iroh_gossip::net::Gossip;

use ipn_core::membership::{build_roster, load_entries, publish_entry};
use ipn_core::roster::{sign, Config, InviteKind, Op, Role};

const NET: [u8; 32] = [9u8; 32];
const INV: [u8; 16] = [7u8; 16];

struct Node {
    router: Router,
    blobs: MemStore,
    docs: Docs,
}

impl Node {
    async fn spawn() -> Result<Self> {
        let endpoint = Endpoint::bind(iroh::endpoint::presets::N0).await?;
        let blobs = MemStore::new();
        let gossip = Gossip::builder().spawn(endpoint.clone());
        let docs = Docs::memory()
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
            .accept(iroh_gossip::ALPN, gossip)
            .accept(iroh_docs::ALPN, docs.clone())
            .spawn();
        Ok(Self {
            router,
            blobs,
            docs,
        })
    }

    fn addr(&self) -> iroh::EndpointAddr {
        self.router.endpoint().addr()
    }

    async fn shutdown(self) -> Result<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}
fn id(k: &SigningKey) -> [u8; 32] {
    k.verifying_key().to_bytes()
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn removed_member_cannot_forge_over_docs() -> Result<()> {
    let a = Node::spawn().await?;
    let b = Node::spawn().await?;

    let om = key(1); // originator master authority (held by A)
    let dev_a = key(2); // A's device (genesis member)
    let dev_b = key(3); // B's device (admitted, then removed)
    let backdoor = key(60); // member B will try to forge in after removal

    let cfg = Config {
        network_id: NET,
        originator_id: id(&om),
        subnet: std::net::Ipv4Addr::new(10, 99, 0, 0),
    };

    // --- A authors the membership document ---
    let a_api = a.docs.api();
    let a_author = a_api.author_create().await?;
    let doc_a = a_api.create().await?;

    // Genesis: originator master vouches its own device in as a Controller.
    publish_entry(
        &doc_a,
        a_author,
        &sign(
            NET,
            &om,
            Op::Add {
                node_id: id(&dev_a),
                hostname: "originator-pc".into(),
                role: Role::Controller,
                virtual_ip: [10, 99, 0, 2],
                invite_nonce: [0u8; 16],
                ts: 1,
            },
        ),
    )
    .await?;
    // Seed the current Peer invite so a Controller can admit a Peer.
    publish_entry(
        &doc_a,
        a_author,
        &sign(
            NET,
            &om,
            Op::SetInvite {
                kind: InviteKind::Peer,
                nonce: INV,
                single_use: false,
                ts: 1,
            },
        ),
    )
    .await?;
    // A's device (a Controller) admits B's device as a Peer.
    publish_entry(
        &doc_a,
        a_author,
        &sign(
            NET,
            &dev_a,
            Op::Add {
                node_id: id(&dev_b),
                hostname: "b-laptop".into(),
                role: Role::Peer,
                virtual_ip: [10, 99, 0, 3],
                invite_nonce: INV,
                ts: 2,
            },
        ),
    )
    .await?;

    // --- B joins the document and syncs both ways ---
    let ticket = doc_a.share(ShareMode::Write, AddrInfoOptions::Addresses).await?;
    let b_api = b.docs.api();
    let (doc_b, _events) = b_api.import_and_subscribe(ticket).await?;
    doc_b.start_sync(vec![a.addr()]).await?;
    doc_a.start_sync(vec![b.addr()]).await?;

    // B sees itself admitted.
    wait_until(Duration::from_secs(30), || async {
        let r = build_roster(&cfg, &doc_b, b.blobs.blobs()).await.ok()?;
        (r.is_member(&id(&dev_b)) && r.len() == 2).then_some(())
    })
    .await
    .expect("B should observe itself as a member");

    // --- A (originator) removes B ---
    publish_entry(
        &doc_a,
        a_author,
        &sign(
            NET,
            &om,
            Op::Remove {
                node_id: id(&dev_b),
                ts: 3,
            },
        ),
    )
    .await?;

    // --- B, still holding the doc write-cap, forges an Add of `backdoor` ---
    let b_author = b_api.author_create().await?;
    let forged = sign(
        NET,
        &dev_b,
        Op::Add {
            node_id: id(&backdoor),
            hostname: "backdoor".into(),
            role: Role::Peer,
            virtual_ip: [10, 99, 0, 4],
            invite_nonce: INV,
            ts: 4,
        },
    );
    let forged_id = forged.id();
    publish_entry(&doc_b, b_author, &forged).await?;

    // Wait until the forged entry has actually REPLICATED to A's document, so the
    // assertion that follows is meaningful (A saw it and rejected it, rather than
    // simply not having received it yet).
    wait_until(Duration::from_secs(30), || async {
        let entries = load_entries(&doc_a, a.blobs.blobs()).await.ok()?;
        entries.iter().any(|e| e.id() == forged_id).then_some(())
    })
    .await
    .expect("forged entry should replicate to A");

    // A folds the document: the forged add is rejected, B stays removed.
    let r = build_roster(&cfg, &doc_a, a.blobs.blobs()).await?;
    assert!(!r.is_member(&id(&backdoor)), "forged add must be rejected");
    assert!(!r.is_member(&id(&dev_b)), "removed member must stay out");
    assert!(r.is_member(&id(&dev_a)), "originator device remains");
    assert_eq!(r.len(), 1);

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// Poll `cond` until it returns `Some`, or the timeout elapses.
async fn wait_until<F, Fut>(timeout: Duration, mut cond: F) -> Option<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<()>>,
{
    tokio::time::timeout(timeout, async {
        loop {
            if cond().await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .ok()
}
