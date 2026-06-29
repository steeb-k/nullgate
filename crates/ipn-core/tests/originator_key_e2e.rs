//! Smoke test for originator key backup & recovery: the originator exports a
//! recovery code; another member imports it and gains originator powers (e.g. it
//! can now freeze the roster). A code for a different network is rejected.
//!
//! `#[ignore]` (opens real iroh endpoints). Run with:
//!   cargo test -p ipn-core --test originator_key_e2e -- --ignored --nocapture

use std::net::Ipv4Addr;
use std::time::Duration;

use ipn_core::engine::{Engine, EngineEvent};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-okey-e2e").join(name);
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

async fn members(e: &Engine) -> usize {
    e.status().await.map(|s| s.members.len()).unwrap_or(0)
}
async fn is_originator(e: &Engine) -> bool {
    e.status().await.map(|s| s.is_originator).unwrap_or(false)
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn originator_key_backup_and_restore() {
    std::env::set_var("NULLGATE_DISABLE_TUN", "1");
    std::env::set_var("NULLGATE_SECRETS_FILE_ONLY", "1");

    let a = Engine::start(scratch("a")).await.unwrap();
    let b = Engine::start(scratch("b")).await.unwrap();
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
            if members(&a).await == 2 && members(&b).await == 2 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("network did not form");

    // A is the originator; B is not (yet).
    assert!(is_originator(&a).await);
    assert!(!is_originator(&b).await);

    // A exports its recovery code; B imports it and becomes originator-capable.
    let code = a.export_originator_key().await.unwrap();
    assert!(code.starts_with("ipnkey1"));
    b.import_originator_key(&code).await.unwrap();
    assert!(is_originator(&b).await, "B should hold originator powers after import");

    // B can now perform an originator-only action.
    b.set_frozen(true).await.expect("B (now originator) can freeze");

    // A recovery code for a *different* network is rejected.
    let foreign = ipn_core::network::encode_recovery_key(&[9u8; 32]);
    assert!(
        b.import_originator_key(&foreign).await.is_err(),
        "a different network's recovery code must be rejected"
    );
}
