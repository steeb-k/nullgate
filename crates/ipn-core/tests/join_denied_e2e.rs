//! Smoke test: a declined join must leave the joiner cleanly at no-network (the
//! provisional activation is torn down), not lingering "in" the network.
//!
//! `#[ignore]` (opens real iroh endpoints). Run with:
//!   cargo test -p ipn-core --test join_denied_e2e -- --ignored --nocapture

use std::net::Ipv4Addr;
use std::time::Duration;

use ipn_core::engine::{Engine, EngineEvent};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-deny-e2e").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn declined_join_resets_to_no_network() {
    std::env::set_var("NULLGATE_DISABLE_TUN", "1");
    std::env::set_var("NULLGATE_SECRETS_FILE_ONLY", "1");

    let a = Engine::start(scratch("a")).await.unwrap();
    let b = Engine::start(scratch("b")).await.unwrap();

    // A auto-DENIES every join request.
    {
        let mut ev = a.subscribe();
        let a2 = a.clone();
        tokio::spawn(async move {
            while let Ok(e) = ev.recv().await {
                if let EngineEvent::JoinRequest { node_id, .. } = e {
                    let _ = a2.deny_join(&node_id).await;
                }
            }
        });
    }

    let ticket = a
        .create_network("home".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await
        .unwrap();

    // The join attempt must fail (declined)...
    let res = tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket)).await;
    assert!(
        res.expect("join should resolve, not hang").is_err(),
        "a declined join must return an error"
    );

    // ...and B must be back at no-network (status errors, no config).
    assert!(
        b.status().await.is_err(),
        "declined joiner should have no network"
    );

    // And B can try again (it isn't stuck "already belongs to a network").
    let retry = tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket)).await;
    let err = format!("{:#}", retry.expect("retry should resolve").unwrap_err());
    assert!(
        !err.contains("already belongs"),
        "joiner must not be stuck in a network after a decline; got: {err}"
    );
}
