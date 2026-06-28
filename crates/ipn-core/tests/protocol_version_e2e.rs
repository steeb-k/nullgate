//! Smoke test for mesh/join protocol-version negotiation: a device speaking a
//! different protocol version is rejected at the handshake with a clear error,
//! rather than failing in a confusing way.
//!
//! `#[ignore]` (opens real iroh endpoints). Run with:
//!   cargo test -p ipn-core --test protocol_version_e2e -- --ignored --nocapture

use std::net::Ipv4Addr;
use std::time::Duration;

use ipn_core::engine::Engine;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-ver-e2e").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn version_mismatch_is_rejected_clearly() {
    std::env::set_var("IPN_DISABLE_TUN", "1");
    std::env::set_var("IPN_SECRETS_FILE_ONLY", "1");

    let a = Engine::start(scratch("a")).await.unwrap();
    let b = Engine::start(scratch("b")).await.unwrap();
    // B speaks a different protocol version than A.
    b.set_protocol_version(999);

    let ticket = a
        .create_network("home".into(), Ipv4Addr::new(10, 99, 0, 0))
        .await
        .unwrap();

    let res = tokio::time::timeout(Duration::from_secs(30), b.join_network(&ticket)).await;
    let err = res
        .expect("join should fail fast, not hang")
        .expect_err("join must be rejected on version mismatch");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("protocol"),
        "error should mention the protocol mismatch, got: {err:#}"
    );
}
