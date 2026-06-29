//! Smoke test for secret rotation (mass-revoke): after the originator rotates,
//! a device holding the OLD ticket is fully locked out — it loses its connection
//! and can no longer see the originator — while the originator continues under a
//! brand-new secret (a different ticket).
//!
//! `#[ignore]` (opens real iroh endpoints). Run with:
//!   cargo test -p ipn-core --test rotate_e2e -- --ignored --nocapture

use std::net::Ipv4Addr;
use std::time::Duration;

use ipn_core::engine::{Engine, EngineEvent};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-rot-e2e").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
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

async fn sees(e: &Engine, node_id: &str) -> bool {
    e.status()
        .await
        .map(|s| s.members.iter().any(|m| m.node_id == node_id))
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn rotating_secret_locks_out_old_ticket_holders() {
    std::env::set_var("NULLGATE_DISABLE_TUN", "1");
    std::env::set_var("NULLGATE_SECRETS_FILE_ONLY", "1");

    let a = Engine::start(scratch("a")).await.unwrap();
    let b = Engine::start(scratch("b")).await.unwrap();
    let a_id = a.self_node_id_hex();
    let b_id = b.self_node_id_hex();

    auto_approve(&a);
    let old_ticket = a
        .create_network("home".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), b.join_network(&old_ticket))
        .await
        .expect("join timed out")
        .expect("join failed");

    // Fully connected: A and B see each other, B has a live link.
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if sees(&a, &b_id).await && sees(&b, &a_id).await && b.live_connection_count() >= 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("network did not form");

    // --- Originator rotates the secret ---
    let new_ticket = a.rotate_network().await.unwrap();
    assert_ne!(new_ticket, old_ticket, "rotate must mint a different ticket");

    // A is still in a network (the fresh one), B is no longer a member of it.
    assert!(a.has_network().await, "A keeps a (new) network after rotate");

    // B (old ticket) is locked out: loses its connection and can't see A anymore.
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if b.live_connection_count() == 0 && !sees(&b, &a_id).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("old-ticket device was not locked out after rotate");

    // B self-evicted (it discovered it was removed) and holds no network.
    assert!(!b.has_network().await, "removed device should auto-leave");
    // And A's new network does not contain B.
    assert!(!sees(&a, &b_id).await, "rotated network must not contain the old member");
}
