//! The shared iroh node: one endpoint + blob store + gossip + docs, all behind a
//! single [`Router`]. This is the connectivity + replication substrate the IPN
//! engine builds on:
//!   - the **endpoint** is the authenticated P2P transport (the mesh links),
//!   - **gossip** carries live presence (hostname / observed IP / last-seen),
//!   - **docs** hosts the signed membership roster (multi-writer, mergeable).
//!
//! The device identity (iroh [`SecretKey`]) is persisted to `node.key` in the
//! data dir so the endpoint id (this device's NodeId) is stable across restarts.
//!
//! Mirrors the proven setup in seed-sync-gtk's `seed-core::node`.

use std::path::Path;

use anyhow::Context;
use iroh::{
    protocol::{Router, RouterBuilder},
    Endpoint, SecretKey,
};
use iroh_blobs::{store::fs::FsStore, BlobsProtocol};
use iroh_docs::{api::DocsApi, protocol::Docs};
use iroh_gossip::net::Gossip;

/// A running iroh node with the protocols IPN needs.
pub struct IrohNode {
    pub endpoint: Endpoint,
    pub blobs: FsStore,
    pub gossip: Gossip,
    pub docs: Docs,
    router: Router,
    /// The device key seed (ed25519). The NodeId is its public half, so this
    /// same key signs roster adds and presence — binding signatures to the NodeId.
    node_secret: [u8; 32],
}

impl IrohNode {
    /// Bootstrap the node, creating the data dir layout if needed:
    /// `node.key`, `blobs/`, `docs/`.
    pub async fn spawn(data_dir: &Path) -> anyhow::Result<Self> {
        Self::spawn_with(data_dir, |b| b).await
    }

    /// Like [`spawn`](Self::spawn) but lets the caller register additional
    /// protocol handlers (custom ALPNs, e.g. the IPN mesh/join protocols) on the
    /// router before it starts its accept loop.
    pub async fn spawn_with<F>(data_dir: &Path, add_protocols: F) -> anyhow::Result<Self>
    where
        F: FnOnce(RouterBuilder) -> RouterBuilder,
    {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create data dir {}", data_dir.display()))?;

        let secret_key = load_or_create_secret_key(data_dir)?;
        let node_secret = secret_key.to_bytes();
        // The endpoint id is the public half of the device key; we need it to
        // build the mDNS service below, before the secret key is moved into the
        // endpoint builder.
        let endpoint_id = secret_key.public();

        // The N0 preset wires up n0 DNS discovery + relays (internet path). On
        // top of that we add mDNS-based local-network address lookup so two
        // members on the same LAN can find and reach each other with no internet
        // at all. Building mDNS can fail on a host with no usable IPv4/IPv6 (or
        // where multicast is unavailable) — degrade to "no LAN discovery" with a
        // warning rather than failing endpoint startup.
        let mut builder = Endpoint::builder(iroh::endpoint::presets::N0).secret_key(secret_key);
        match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint_id) {
            Ok(mdns) => builder = builder.address_lookup(mdns),
            Err(e) => tracing::warn!("local-network (mDNS) discovery unavailable: {e}"),
        }
        let endpoint = builder.bind().await.context("bind iroh endpoint")?;

        let blobs_dir = data_dir.join("blobs");
        let docs_dir = data_dir.join("docs");
        std::fs::create_dir_all(&blobs_dir).context("create blobs dir")?;
        std::fs::create_dir_all(&docs_dir).context("create docs dir")?;

        let blobs = FsStore::load(&blobs_dir).await.context("open blob store")?;
        let gossip = Gossip::builder().spawn(endpoint.clone());
        // `Docs::persistent` treats its argument as a directory and creates
        // `docs.redb` inside it.
        let docs = Docs::persistent(docs_dir)
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await
            .context("spawn docs")?;

        let builder = Router::builder(endpoint.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(iroh_docs::ALPN, docs.clone());
        let router = add_protocols(builder).spawn();

        Ok(Self {
            endpoint,
            blobs,
            gossip,
            docs,
            router,
            node_secret,
        })
    }

    /// This device's ed25519 signing key (same key as the NodeId), used to sign
    /// roster `Add`s and presence heartbeats.
    pub fn device_signing_key(&self) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&self.node_secret)
    }

    pub fn docs_api(&self) -> &DocsApi {
        self.docs.api()
    }

    /// This device's endpoint id / NodeId (32 bytes).
    pub fn node_id_bytes(&self) -> [u8; 32] {
        *self.endpoint.id().as_bytes()
    }

    /// This node's current dialable address.
    pub fn addr(&self) -> iroh::EndpointAddr {
        self.endpoint.addr()
    }

    /// Wait until the endpoint has contacted a relay (and thus has a complete,
    /// dialable [`addr`](Self::addr) with relay URL + direct addresses).
    pub async fn wait_online(&self) {
        self.endpoint.online().await;
    }

    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.router.shutdown().await?;
        Ok(())
    }
}

/// Load this device's secret key from the OS keystore (file fallback), or
/// generate and persist a new one. See [`crate::secrets`].
fn load_or_create_secret_key(data_dir: &Path) -> anyhow::Result<SecretKey> {
    if let Some(bytes) = crate::secrets::load(data_dir, "node-key")? {
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    crate::secrets::store(data_dir, "node-key", &key.to_bytes()).context("store device key")?;
    Ok(key)
}
