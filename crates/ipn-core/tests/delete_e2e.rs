//! Smoke test for network deletion: a 3-node pool (originator A + members B, C)
//! is fully connected, then A deletes the network. We verify the security
//! property the user cares about: **once killed, members lose visibility of each
//! other and hold no ghost connections** — B and C must drop to zero live mesh
//! connections and must no longer list each other as members.
//!
//! `#[ignore]` (opens real iroh endpoints). Run with:
//!   cargo test -p ipn-core --test delete_e2e -- --ignored --nocapture

use std::net::Ipv4Addr;
use std::time::Duration;

use ipn_core::engine::{Engine, EngineEvent};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-del-e2e").join(name);
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

async fn member_count(e: &Engine) -> usize {
    e.status().await.map(|s| s.members.len()).unwrap_or(0)
}

/// Does `e`'s member list include `node_id`?
async fn sees(e: &Engine, node_id: &str) -> bool {
    e.status()
        .await
        .map(|s| s.members.iter().any(|m| m.node_id == node_id))
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn deleting_network_kills_all_visibility_and_connections() {
    std::env::set_var("IPN_DISABLE_TUN", "1");
    std::env::set_var("IPN_SECRETS_FILE_ONLY", "1");

    let a = Engine::start(scratch("a")).await.unwrap();
    let b = Engine::start(scratch("b")).await.unwrap();
    let c = Engine::start(scratch("c")).await.unwrap();
    let b_id = b.self_node_id_hex();
    let c_id = c.self_node_id_hex();

    // A admits any joiner (B and C verify via SAS, auto-approved here).
    auto_approve(&a);

    let ticket = a
        .create_network("home".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket))
        .await
        .expect("B join timed out")
        .expect("B join failed");
    tokio::time::timeout(Duration::from_secs(30), c.join_network(&ticket))
        .await
        .expect("C join timed out")
        .expect("C join failed");

    // All three converge to a 3-member roster, and B and C actually connect to
    // each other (so we have a real connection to later prove gets torn down).
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if member_count(&a).await == 3
                && member_count(&b).await == 3
                && member_count(&c).await == 3
                && sees(&b, &c_id).await
                && sees(&c, &b_id).await
                && b.live_connection_count() >= 2
                && c.live_connection_count() >= 2
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("network did not fully form (3 connected members)");

    // --- Originator deletes the network ---
    a.delete_network().await.unwrap();

    // The originator has left.
    assert!(!a.has_network().await, "A should have left after delete");

    // B and C must lose all live connections AND stop seeing each other.
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if b.live_connection_count() == 0
                && c.live_connection_count() == 0
                && !sees(&b, &c_id).await
                && !sees(&c, &b_id).await
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("after delete, B/C still had ghost connections or could see each other");
}
