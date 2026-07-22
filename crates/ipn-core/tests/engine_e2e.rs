//! End-to-end test of the engine on two in-process nodes: originator A creates a
//! network, joiner B joins, A approves the SAS-verified request, and then both
//! see each other in their member lists with B online over the authenticated
//! mesh. Also asserts the emoji SAS matches on both ends.
//!
//! `#[ignore]` (opens real iroh endpoints / discovery / relay). Run with:
//!   cargo test -p ipn-core --test engine_e2e -- --ignored --nocapture

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ipn_core::engine::{Engine, EngineEvent};
use ipn_core::Pace;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-e2e").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Auto-approve every join request that reaches `e`.
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

async fn sees(e: &Engine, node_id: &str) -> bool {
    e.status()
        .await
        .map(|s| s.members.iter().any(|m| m.node_id == node_id))
        .unwrap_or(false)
}

/// Bring up a connected 2-node network (originator A + member B) and return both.
async fn connected_pair(tag: &str) -> (Engine, Engine) {
    let a = Engine::start(scratch(&format!("{tag}-a"))).await.unwrap();
    let b = Engine::start(scratch(&format!("{tag}-b"))).await.unwrap();
    auto_approve(&a);
    let ticket = a
        .create_network("home".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket))
        .await
        .expect("join timed out")
        .expect("join failed");
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if a.live_connection_count() >= 1 && b.live_connection_count() >= 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("pair did not connect");
    (a, b)
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn create_join_and_see_each_other() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ipn_core=debug,iroh=warn")
        .try_init();
    // Don't create real TUN adapters during the test.
    std::env::set_var("NULLGATE_DISABLE_TUN", "1");
    std::env::set_var("NULLGATE_SECRETS_FILE_ONLY", "1");
    let a = Engine::start(scratch("a")).await.unwrap();
    let b = Engine::start(scratch("b")).await.unwrap();

    let a_sas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let b_sas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));

    // A: auto-approve any join request, capturing the SAS it was shown.
    {
        let mut ev = a.subscribe();
        let a2 = a.clone();
        let a_sas = a_sas.clone();
        tokio::spawn(async move {
            while let Ok(e) = ev.recv().await {
                if let EngineEvent::JoinRequest { node_id, sas, .. } = e {
                    *a_sas.lock().unwrap() = sas;
                    let _ = a2.approve_join(&node_id).await;
                }
            }
        });
    }
    // B: capture the SAS it computed for the join.
    {
        let mut ev = b.subscribe();
        let b_sas = b_sas.clone();
        tokio::spawn(async move {
            while let Ok(e) = ev.recv().await {
                if let EngineEvent::JoinSas { sas } = e {
                    *b_sas.lock().unwrap() = sas;
                }
            }
        });
    }

    let ticket = a
        .create_network("home".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await
        .unwrap();

    // Blocks until A approves.
    tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket))
        .await
        .expect("join timed out")
        .expect("join failed");

    // The emoji SAS must have matched on both ends.
    let sa = a_sas.lock().unwrap().clone();
    let sb = b_sas.lock().unwrap().clone();
    assert_eq!(sa.len(), 7, "A should have a 7-emoji SAS");
    assert_eq!(sa, sb, "SAS must match across the two devices");

    // Both should converge to a 2-member roster, with B online from A's view.
    let ok = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let sa = a.status().await.unwrap();
            let sb = b.status().await.unwrap();
            let b_online_for_a = sa
                .members
                .iter()
                .find(|m| !m.is_self)
                .map(|m| m.online)
                .unwrap_or(false);
            if sa.members.len() == 2 && sb.members.len() == 2 && b_online_for_a {
                return (sa, sb);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("did not converge to a connected 2-member network");

    let (sa, sb) = ok;
    assert!(sa.is_originator, "A is the originator");
    assert!(!sb.is_originator, "B is not the originator");
    // IPs are derived from the NodeId (not fixed .1/.2): both are in the subnet and distinct.
    let a_ip = sa.self_ip.expect("A has an IP");
    let b_ip = sb.self_ip.expect("B has an IP");
    assert!(a_ip.starts_with("10.99.0."), "A IP in subnet: {a_ip}");
    assert!(b_ip.starts_with("10.99.0."), "B IP in subnet: {b_ip}");
    assert_ne!(a_ip, b_ip, "members get distinct IPs");

    // B's view of A carries A's real (non-empty) hostname over presence.
    let a_from_b = sb.members.iter().find(|m| !m.is_self).unwrap();
    assert!(
        a_from_b.hostname.as_deref().is_some_and(|h| !h.is_empty()),
        "A's actual hostname should propagate to B"
    );

    // Friendly names are LOCAL nicknames: B nicknames A; it shows in B's view only
    // (no propagation), and A's real hostname is unaffected.
    b.set_nickname(&a_from_b.node_id, Some("Alpha".into())).await.unwrap();
    let sb = b.status().await.unwrap();
    let a_view = sb.members.iter().find(|m| !m.is_self).unwrap();
    assert_eq!(a_view.label.as_deref(), Some("Alpha"), "B's local nickname for A");
    assert!(
        a_view.hostname.as_deref().is_some_and(|h| !h.is_empty()),
        "hostname remains the real OS name alongside the nickname"
    );
    // A never sees B's local nickname for it.
    let sa = a.status().await.unwrap();
    assert!(
        sa.members.iter().find(|m| m.is_self).and_then(|m| m.label.clone()).is_none(),
        "a local nickname must not propagate to the nicknamed device"
    );
}

/// The network-change hint (Android's connectivity signal) must be safe to call and
/// must leave the mesh connected — it rebinds iroh and fires a recovery burst, and
/// on a healthy path that recovers/keeps the connection rather than tearing it down.
/// (We can't synthetically pull the OS network here, so this asserts the observable
/// contract: `network_changed()` is non-destructive and the pair stays connected.)
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn network_changed_keeps_the_mesh_connected() {
    std::env::set_var("NULLGATE_DISABLE_TUN", "1");
    std::env::set_var("NULLGATE_SECRETS_FILE_ONLY", "1");

    let (a, b) = connected_pair("netchg").await;
    let b_id = b.self_node_id_hex();

    // Fire the hint repeatedly (as rapid Wi-Fi/cellular flaps would).
    for _ in 0..3 {
        a.network_changed().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Within a few ticks A must still (or again) hold a live connection to B and see
    // it in the roster — the burst re-seeds/redials, it doesn't drop membership.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if a.live_connection_count() >= 1 && sees(&a, &b_id).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("mesh did not stay connected across network_changed()");
}

/// Self-eviction must still be prompt in `Pace::Background`: a removed device drops
/// to zero connections and stops seeing the network well inside the 60s Background
/// tick — proving the roster-doc live-sync event wakes the slow loop early (a pure
/// 60s cadence could not meet this window).
#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn background_pace_still_evicts_removed_device_promptly() {
    std::env::set_var("NULLGATE_DISABLE_TUN", "1");
    std::env::set_var("NULLGATE_SECRETS_FILE_ONLY", "1");

    let (a, b) = connected_pair("bgpace").await;
    let b_id = b.self_node_id_hex();

    // Both devices go to the battery-saving cadence, as a backgrounded phone would.
    a.set_pace(Pace::Background);
    b.set_pace(Pace::Background);

    // A removes B.
    a.remove_member(&b_id).await.unwrap();

    // B must self-evict (zero connections, network forgotten) well under 60s — only
    // possible because the doc live-sync event wakes B's slow tick immediately.
    tokio::time::timeout(Duration::from_secs(50), async {
        loop {
            if b.live_connection_count() == 0 && !b.has_network().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("removed device did not self-evict promptly under Background pace");

    // And A no longer sees B.
    assert!(!sees(&a, &b_id).await, "A should have dropped B from the roster");
}
