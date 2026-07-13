//! E2e smoke tests for **custom relay servers** (ignored: opens real sockets and
//! spins up in-process iroh relay servers; the engine tests also reach the public
//! n0 relays, like the rest of the ignored e2e suite).
//!
//! Endpoint-level mechanics the engine's relay settings rely on:
//!   1. An endpoint pinned to a custom relay map (with the
//!      [`PreferMyRelaySelector`] installed, as `IrohNode` does) homes on the
//!      custom relay and carries data through it.
//!   2. The **runtime** `insert_relay`/`remove_relay` flow used by
//!      `Engine::set_relay_settings` actually moves the endpoint to the new relay
//!      without a rebind — new connections work through the newly-inserted relay
//!      and the old one drops out of the advertised address.
//!
//! Engine-level regressions of the July 2026 partition:
//!   3. Under `Preferred`, the public relays stay in the live relay map alongside
//!      the custom one, so a device with a token-gated relay can still *reach* a
//!      peer that is homed on a public relay. Under `Only` they do not. This is
//!      the map that `set_relay_settings` used to overwrite with the custom relay
//!      alone, which is what made configured and unconfigured devices mutually
//!      unreachable.
//!   4. `set_relay_settings` returns promptly under a **live mesh** and the map
//!      really changes — no 20-minute hang, and no daemon restart.
//!
//! Run with: cargo test -p ipn-core --test relay_e2e -- --ignored

use std::{collections::BTreeSet, net::Ipv4Addr, sync::Arc, time::Duration};

use anyhow::{ensure, Context, Result};
use iroh::{
    endpoint::presets, tls::CaTlsConfig, Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl,
    TransportAddr,
};
use ipn_core::{
    engine::{Engine, EngineEvent},
    relays::{PreferMyRelaySelector, PreferredRelays, RelayApply, RelayPolicy, RelayServer, RelaySettings},
};

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

// ---------------------------------------------------------------------------
// Engine-level: the partition regressions
// ---------------------------------------------------------------------------

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-relay-e2e").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn test_env() {
    std::env::set_var("NULLGATE_DISABLE_TUN", "1");
    std::env::set_var("NULLGATE_SECRETS_FILE_ONLY", "1");
}

fn auto_approve(e: &Engine) {
    let mut ev = e.subscribe();
    let e2 = e.clone();
    tokio::spawn(async move {
        while let Ok(event) = ev.recv().await {
            if let EngineEvent::JoinRequest { node_id, .. } = event {
                let _ = e2.approve_join(&node_id).await;
            }
        }
    });
}

fn custom(url: &RelayUrl, mode: RelayPolicy) -> RelaySettings {
    RelaySettings {
        servers: vec![RelayServer {
            url: url.to_string(),
            // A token is what makes a relay *exclusive*: peers without it get
            // `401` and have no path here. Exactly the outage's shape.
            token: Some("e2e-token".into()),
        }],
        mode,
    }
}

/// Wait until the engine reports the relay change reached the live endpoint.
async fn wait_applied(e: &Engine) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match e.relay_status().apply {
                RelayApply::Applied => return Ok(()),
                RelayApply::Failed { reason } => {
                    anyhow::bail!("relay settings never reached the endpoint: {reason}")
                }
                RelayApply::Pending => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .context("timed out waiting for the relay settings to apply")?
}

/// The relay we are currently homed on, if any. An endpoint advertises exactly
/// one relay — its home relay — so this is the only address peers can reach us
/// at over a relay, and thus the sharpest observable of what is in the live map.
///
/// Read through `relay_connections()` rather than `status().home_relay` because
/// these tests run engines with **no network**, and `status()` fails without one.
fn home_relay(e: &Engine) -> Option<RelayUrl> {
    e.relay_connections()
        .into_iter()
        .find(|(_, connected)| *connected)
        .map(|(url, _)| url)
}

/// Wait until we are homed on some relay (`want_some`) or on none.
async fn wait_home_relay(e: &Engine, want_some: bool) -> Result<Option<RelayUrl>> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let home = home_relay(e);
            if home.is_some() == want_some {
                return home;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .with_context(|| format!("timed out waiting for home_relay.is_some() == {want_some}"))
}

/// The public relays iroh ships as its defaults.
fn public_relays() -> Vec<RelayUrl> {
    let relays: Vec<Arc<iroh::RelayConfig>> =
        iroh::endpoint::default_relay_mode().relay_map().relays();
    relays.iter().map(|c| c.url.clone()).collect()
}

/// The core regression, tested where a daemon actually picks the policy up: at
/// **startup**, from `relays.cbor`.
///
/// With a custom relay that cannot be reached, `Preferred` must still home on a
/// public relay — they are in the map beside the custom one — while `Only` must
/// leave us with no relay at all. Under the old behaviour `Preferred` was
/// byte-identical to `Only` (the custom relay *replaced* the public ones), which
/// is exactly how a token-gated relay left configured and unconfigured devices
/// mutually unreachable.
///
/// Tested from a cold start, not by mutating a running engine, because iroh keeps
/// a home relay it has already picked even after that relay leaves the map (see
/// `settle_home_relay` in `ipn-core::engine`) — so a running endpoint would stay
/// on its public relay either way and the assertion wouldn't discriminate.
#[tokio::test]
#[ignore = "opens real iroh endpoints (incl. the public n0 relays); run with --ignored"]
async fn preferred_keeps_the_public_relays_and_only_drops_them() -> Result<()> {
    test_env();
    // Deliberately unreachable (discard port). Stands in for "a relay this device
    // cannot use" — the position an unconfigured peer is in when it meets a
    // token-gated one.
    let dead: RelayUrl = "https://127.0.0.1:9".parse()?;

    // Preferred: the public relays are in the map too, so a dead custom relay
    // cannot strand us.
    let dir = scratch("policy-preferred");
    std::fs::create_dir_all(&dir)?;
    ipn_core::relays::save_relay_settings(&dir, &custom(&dead, RelayPolicy::Preferred))?;
    let e = Engine::start(&dir).await?;
    let home = wait_home_relay(&e, true).await?.context(
        "REGRESSION: Preferred left us with no relay — it dropped the public relays from \
         the map, so a peer without the custom relay cannot reach us",
    )?;
    ensure!(home != dead, "the dead relay cannot be our home relay");
    ensure!(
        public_relays().contains(&home),
        "expected to home on a public relay, got {home}"
    );

    // Only: no public relays in the map, so an unreachable custom relay leaves us
    // with no relay at all. That is the point of `Only`, and the cost of it.
    let dir = scratch("policy-only");
    std::fs::create_dir_all(&dir)?;
    ipn_core::relays::save_relay_settings(&dir, &custom(&dead, RelayPolicy::Only))?;
    let e = Engine::start(&dir).await?;
    tokio::time::sleep(Duration::from_secs(20)).await;
    ensure!(
        home_relay(&e).is_none(),
        "Only must not use the public relays, but we homed on {:?}",
        home_relay(&e)
    );
    Ok(())
}

/// A custom relay on **one** device must not partition it from a peer that has
/// none — the July 2026 outage, end to end. Under the old behaviour A's map was
/// the custom relay *alone*, so A had no transport that could reach B's public
/// home relay (and B could not reach A's token-gated one): mutually invisible.
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn a_custom_relay_on_one_device_does_not_partition_it_from_a_peer_without_one() -> Result<()> {
    test_env();
    let (_map, url, _guard) = iroh::test_utils::run_relay_server().await?;

    let a = Engine::start(scratch("partial-a")).await?;
    let b = Engine::start(scratch("partial-b")).await?;
    let a_id = a.self_node_id_hex();
    let b_id = b.self_node_id_hex();

    // A runs the token-gated relay; B knows nothing about it.
    a.set_relay_settings(custom(&url, RelayPolicy::Preferred)).await?;
    wait_applied(&a).await?;

    auto_approve(&a);
    let ticket = a
        .create_network("relay-partial".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await?;
    tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket))
        .await
        .context("B join timed out")??;

    let sees = |e: &Engine, id: String| {
        let e = e.clone();
        async move {
            e.status()
                .await
                .map(|s| s.members.iter().any(|m| m.node_id == id))
                .unwrap_or(false)
        }
    };
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if sees(&a, b_id.clone()).await
                && sees(&b, a_id.clone()).await
                && a.live_connection_count() >= 1
                && b.live_connection_count() >= 1
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context(
        "REGRESSION: a device with a token-gated custom relay could not reach a peer without it",
    )?;
    Ok(())
}

/// The hang. Under a live mesh — where iroh's socket actor is busy with real
/// peer traffic — `set_relay_settings` must return promptly and the map must
/// actually change. This used to block for 20+ minutes inside `insert_relay`,
/// leaving disk, selector, endpoint map and reported settings all disagreeing,
/// and needing a daemon restart to take effect.
#[tokio::test]
#[ignore = "opens real iroh endpoints (incl. the public n0 relays); run with --ignored"]
async fn set_relay_settings_applies_under_a_live_mesh_without_a_restart() -> Result<()> {
    test_env();
    let a = Engine::start(scratch("live-a")).await?;
    let b = Engine::start(scratch("live-b")).await?;

    auto_approve(&a);
    let ticket = a
        .create_network("relay-live".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await?;
    tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket))
        .await
        .context("B join timed out")??;

    // Wait for a genuinely live mesh: docs sync + gossip + a real connection, so
    // the socket actor is doing the ResolveRemote/AddConnection work that used to
    // back up behind a stuck relay send.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if a.live_connection_count() >= 1 && b.live_connection_count() >= 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("mesh did not form")?;

    // Pick a *real, reachable* relay that is NOT the one A is currently homed on,
    // and make it A's only relay. If the change truly reaches the live endpoint,
    // A's home relay has to move onto it — the old one is no longer in the map to
    // be probed, and the new one is the only thing that can answer. An in-process
    // test relay is no good here: its self-signed cert is only trusted by an
    // endpoint built with `insecure_skip_verify`, which a real `Engine` is not.
    let before = home_relay(&a).context("A should be homed on a public relay before we start")?;
    let target = public_relays()
        .into_iter()
        .find(|u| *u != before)
        .context("need a second public relay to move to")?;

    // The call itself must not block on the endpoint. This is the hang.
    let start = std::time::Instant::now();
    a.set_relay_settings(RelaySettings {
        servers: vec![RelayServer { url: target.to_string(), token: None }],
        mode: RelayPolicy::Only,
    })
    .await?;
    let elapsed = start.elapsed();
    ensure!(
        elapsed < Duration::from_secs(2),
        "set_relay_settings took {elapsed:?} — it must not await the endpoint on the request path"
    );

    // It must be reported honestly the instant it returns: `relay show` used to
    // report the *old* settings after a wedged write.
    let st = a.relay_status();
    ensure!(
        st.settings.servers.iter().any(|s| s.url == target.to_string()),
        "the saved settings must be readable immediately, not after the endpoint catches up"
    );

    // And it must actually reach the live endpoint under mesh load, with no
    // restart: A moves off the relay we removed and onto the one we added.
    wait_applied(&a).await?;
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            if home_relay(&a).as_ref() == Some(&target) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "the live endpoint never moved to the newly-configured relay (restart required?): \
             still on {:?}, wanted {target}",
            home_relay(&a)
        )
    })?;
    Ok(())
}
