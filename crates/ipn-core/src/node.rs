//! The shared iroh node: one endpoint + blob store + gossip + docs, all behind a
//! single [`Router`]. This is the connectivity + replication substrate the Nullgate
//! engine builds on:
//!   - the **endpoint** is the authenticated P2P transport (the mesh links),
//!   - **gossip** carries live presence (hostname / observed IP / last-seen),
//!   - **docs** hosts the signed membership roster (multi-writer, mergeable).
//!
//! The device identity (iroh [`SecretKey`]) is persisted to `node.key` in the
//! data dir so the endpoint id (this device's NodeId) is stable across restarts.
//!
//! Mirrors the proven setup in seed-sync-gtk's `seed-core::node`.

use std::{path::Path, sync::Arc};

use anyhow::Context;
use iroh::{
    protocol::{Router, RouterBuilder},
    Endpoint, RelayMap, RelayMode, SecretKey,
};
use iroh_blobs::{store::fs::FsStore, BlobsProtocol};
use iroh_docs::{api::DocsApi, protocol::Docs};
use iroh_gossip::net::Gossip;

/// A running iroh node with the protocols Nullgate needs.
pub struct IrohNode {
    pub endpoint: Endpoint,
    pub blobs: FsStore,
    pub gossip: Gossip,
    pub docs: Docs,
    router: Router,
    /// The device key seed (ed25519). The NodeId is its public half, so this
    /// same key signs roster adds and presence — binding signatures to the NodeId.
    node_secret: [u8; 32],
    /// Live handle into the endpoint's path selector: the set of relay URLs
    /// whose paths outrank other relays. The engine updates it when the user
    /// edits relay settings (the selector itself is fixed at bind time).
    pub preferred_relays: crate::relays::PreferredRelays,
}

impl IrohNode {
    /// Bootstrap the node, creating the data dir layout if needed:
    /// `node.key`, `blobs/`, `docs/`.
    pub async fn spawn(data_dir: &Path) -> anyhow::Result<Self> {
        Self::spawn_with(data_dir, |b| b).await
    }

    /// Like [`spawn`](Self::spawn) but lets the caller register additional
    /// protocol handlers (custom ALPNs, e.g. the Nullgate mesh/join protocols) on the
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
        #[cfg(not(target_os = "android"))]
        let endpoint_id = secret_key.public();

        // The N0 preset wires up n0 DNS discovery + relays (internet path). On
        // top of that we add mDNS-based local-network address lookup so two
        // members on the same LAN can find and reach each other with no internet
        // at all. Building mDNS can fail on a host with no usable IPv4/IPv6 (or
        // where multicast is unavailable) — degrade to "no LAN discovery" with a
        // warning rather than failing endpoint startup.
        let mut builder = Endpoint::builder(iroh::endpoint::presets::N0).secret_key(secret_key);

        // Android runs on a metered radio, and iroh's *broken-network* behavior is
        // its most expensive: when no relay is reachable, every 20-26s net-report
        // falls back to the maximal probe plan (3 HTTPS probes x every relay, each
        // a fresh TCP+TLS handshake, plus 7-way staggered DNS per probe) — a
        // measured multi-GB/month burn on a phone whose connectivity flaps daily.
        // `minimal()` keeps the QAD probes (which feed address discovery, the part
        // that matters) and drops the HTTPS/captive-portal extras. The portmapper
        // re-probes UPnP/NAT-PMP/PCP every report too, and behind a VpnService +
        // carrier NAT a port mapping is never obtainable — pure waste. Desktop
        // keeps both defaults: unmetered, stable links, and UPnP can actually win
        // a direct path there.
        #[cfg(target_os = "android")]
        {
            use iroh::endpoint::{NetReportConfig, PortmapperConfig};
            builder = builder
                .net_report_config(NetReportConfig::minimal())
                .portmapper_config(PortmapperConfig::Disabled);
        }

        // Custom relay servers (see `crate::relays`). The path selector is
        // installed unconditionally — with no preferred relays it behaves like
        // iroh's default — because it can't be swapped after bind, while the
        // preferred set and the relay map can both change at runtime.
        //
        // The map gets the *desired* set (under `Preferred` that's the custom
        // relays plus the public ones), but the selector's preferred set gets
        // the **custom URLs only**: its job is to bias traffic onto the user's
        // relays, so telling it to prefer the defaults too would be a no-op.
        let relay_settings = crate::relays::load_relay_settings(data_dir);
        let preferred_relays = crate::relays::PreferredRelays::default();
        builder = builder.path_selector(Arc::new(crate::relays::PreferMyRelaySelector::new(
            preferred_relays.clone(),
        )));
        match (relay_settings.urls(), relay_settings.desired_relay_configs()) {
            (Ok(custom), Ok(desired)) if !custom.is_empty() => {
                preferred_relays.set(custom.into_iter().collect());
                builder = builder.relay_mode(RelayMode::Custom(RelayMap::from_iter(desired)));
            }
            (Ok(_), Ok(_)) => {} // no custom relays; keep the N0 defaults
            // A broken relays.cbor must not brick connectivity: fall back to
            // the defaults and let the user re-save valid settings.
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!("ignoring invalid relay settings: {e:#}")
            }
        }
        // No mDNS on Android: the discoverer multicasts a query every ~700ms for
        // the endpoint's whole lifetime (swarm-discovery's "interactive" cadence,
        // not configurable from here), the multicast lock that would let replies
        // in is foreground-only anyway, and LAN-direct paths still form without
        // it — QUIC NAT traversal exchanges local addresses over the relay. On
        // desktop it stays: it's what lets two members on the same LAN connect
        // with no internet at all.
        #[cfg(not(target_os = "android"))]
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
            preferred_relays,
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

    /// Shut this node down **in place**, releasing everything a replacement node
    /// needs: the router (whose shutdown fans out to each protocol handler —
    /// docs closes its redb, gossip quiesces) and the endpoint's UDP sockets.
    /// By-reference on purpose: during an engine-level rebuild the old node's
    /// `Arc` can still be held briefly by in-flight tasks, and every operation on
    /// a closed node fails cleanly instead of panicking. This is the primitive
    /// behind the engine health check's recovery — on Android a doze can leave
    /// the endpoint's sockets dead with no OS signal and no iroh API to rebind
    /// them, so the only real fix is a fresh bind (what a manual VPN toggle did).
    pub async fn close(&self) {
        if let Err(e) = self.router.shutdown().await {
            tracing::warn!("router shutdown: {e:#}");
        }
        // Belt-and-braces: make sure the blob store actor is stopped so blobs.db
        // is unlocked for the successor node.
        if let Err(e) = self.blobs.shutdown().await {
            tracing::debug!("blob store shutdown: {e:#}");
        }
        self.endpoint.close().await;
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
