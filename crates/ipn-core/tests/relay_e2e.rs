//! E2e smoke test for **custom relay servers** (ignored: opens real sockets and
//! spins up two in-process iroh relay servers).
//!
//! Proves the two mechanics the engine's relay settings rely on:
//!   1. An endpoint pinned to a custom relay map (with the
//!      [`PreferMyRelaySelector`] installed, as `IrohNode` does) homes on the
//!      custom relay and carries data through it.
//!   2. The **runtime** `insert_relay`/`remove_relay` flow used by
//!      `Engine::set_relay_settings` and the fallback watchdog actually moves
//!      the endpoint to the new relay without a rebind — new connections work
//!      through the newly-inserted relay and the old one drops out of the
//!      advertised address.
//!
//! Run with: cargo test -p ipn-core --test relay_e2e -- --ignored

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::{ensure, Context, Result};
use iroh::{
    endpoint::presets, tls::CaTlsConfig, Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl,
    TransportAddr,
};
use ipn_core::relays::{PreferMyRelaySelector, PreferredRelays};

const ALPN: &[u8] = b"ipn/relay-e2e/0";
const PAYLOAD: &[u8] = b"relay e2e payload";

async fn bind_endpoint(map: RelayMap, preferred: PreferredRelays) -> Result<Endpoint> {
    // Mirrors IrohNode::spawn_with's relay wiring: custom relay map + the
    // preferring path selector. Minimal preset = no discovery services, so
    // dials only work through addresses we hand out — which keeps the test
    // honest about what the relay carries. Insecure TLS is for the test
    // relays' self-signed certs only.
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Custom(map))
        .path_selector(Arc::new(PreferMyRelaySelector::new(preferred)))
        .ca_tls_config(CaTlsConfig::insecure_skip_verify())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    Ok(ep)
}

/// Waits until the endpoint's advertised relay set is exactly `want`.
async fn wait_for_relays(ep: &Endpoint, want: &BTreeSet<RelayUrl>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let have: BTreeSet<RelayUrl> = ep.addr().relay_urls().cloned().collect();
            if have == *want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .with_context(|| format!("timed out waiting for advertised relays {want:?}"))
}

/// Dials `to` through `relay` only and round-trips a payload.
async fn roundtrip_via_relay(from: &Endpoint, to: &Endpoint, relay: &RelayUrl) -> Result<()> {
    let addr = EndpointAddr {
        id: to.id(),
        addrs: [TransportAddr::Relay(relay.clone())].into(),
    };
    let conn = tokio::time::timeout(Duration::from_secs(20), from.connect(addr, ALPN))
        .await
        .context("connect timed out")??;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(PAYLOAD).await?;
    let mut echo = vec![0u8; PAYLOAD.len()];
    tokio::time::timeout(Duration::from_secs(10), recv.read_exact(&mut echo))
        .await
        .context("echo timed out")??;
    ensure!(echo == PAYLOAD, "echo mismatch");
    conn.close(0u32.into(), b"done");
    Ok(())
}

fn spawn_echo_loop(ep: Endpoint) {
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            if let Ok(conn) = incoming.await {
                tokio::spawn(async move {
                    if let Ok((mut send, mut recv)) = conn.accept_bi().await {
                        let mut buf = vec![0u8; PAYLOAD.len()];
                        if recv.read_exact(&mut buf).await.is_ok() {
                            let _ = send.write_all(&buf).await;
                        }
                    }
                    conn.closed().await;
                });
            }
        }
    });
}

#[tokio::test]
#[ignore]
async fn custom_relay_carries_data_and_swaps_at_runtime() -> Result<()> {
    let (map_a, url_a, _guard_a) = iroh::test_utils::run_relay_server().await?;
    let (_map_b, url_b, _guard_b) = iroh::test_utils::run_relay_server().await?;

    let preferred = PreferredRelays::default();
    preferred.set([url_a.clone()].into_iter().collect());

    let a = bind_endpoint(map_a.clone(), preferred.clone()).await?;
    let b = bind_endpoint(map_a.clone(), preferred.clone()).await?;
    spawn_echo_loop(b.clone());

    // 1) Both endpoints home on the custom relay and it carries data.
    let only_a: BTreeSet<RelayUrl> = [url_a.clone()].into_iter().collect();
    wait_for_relays(&a, &only_a).await?;
    wait_for_relays(&b, &only_a).await?;
    roundtrip_via_relay(&a, &b, &url_a).await?;

    // 2) Runtime swap — the exact flow Engine::set_relay_settings uses:
    //    insert the new relay, then remove the old, on the live endpoints.
    let cfg_b = iroh::RelayConfig::from(url_b.clone());
    for ep in [&a, &b] {
        ep.insert_relay(url_b.clone(), Arc::new(cfg_b.clone())).await;
        ep.remove_relay(&url_a).await;
    }
    preferred.set([url_b.clone()].into_iter().collect());

    let only_b: BTreeSet<RelayUrl> = [url_b.clone()].into_iter().collect();
    wait_for_relays(&a, &only_b).await?;
    wait_for_relays(&b, &only_b).await?;
    roundtrip_via_relay(&a, &b, &url_b).await?;

    a.close().await;
    b.close().await;
    Ok(())
}
