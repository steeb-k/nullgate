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

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("ipn-e2e").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
#[ignore = "opens real iroh endpoints; run with --ignored"]
async fn create_join_and_see_each_other() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("ipn_core=debug,iroh=warn")
        .try_init();
    // Don't create real TUN adapters during the test.
    std::env::set_var("IPN_DISABLE_TUN", "1");
    std::env::set_var("IPN_SECRETS_FILE_ONLY", "1");
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

    // A sets a friendly label; it propagates to B over presence (the hostname is
    // unaffected — still the real OS name).
    a.set_label(Some("Alpha".into())).await.unwrap();
    let labeled = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let sb = b.status().await.unwrap();
            if let Some(a_view) = sb.members.iter().find(|m| !m.is_self) {
                if a_view.label.as_deref() == Some("Alpha") {
                    return a_view.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("A's label did not propagate to B");
    assert!(
        labeled.hostname.as_deref().is_some_and(|h| !h.is_empty()),
        "hostname remains the real OS name alongside the label"
    );
}
