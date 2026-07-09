//! Probe a **custom iroh relay server**: verifies the two transports a relay
//! offers and that a Bearer auth token is honored end-to-end.
//!
//!   1. HTTPS/WebSocket — the actual relay data path. Two endpoints (relay map
//!      pinned to ONLY the custom relay, no discovery services) connect and
//!      round-trip data through it, sending the token as an
//!      `Authorization: Bearer` header on the websocket upgrade.
//!   2. QUIC — the relay's QUIC address-discovery endpoint (UDP, default port
//!      7842), observed via the endpoint's net-report probes.
//!   3. If a token was given: a token-less endpoint tries the same relay and
//!      must be REJECTED (proves the server enforces the token).
//!
//! Groundwork for user-configurable relay servers in settings.
//!
//! Usage:
//!     cargo run -p ipn-core --example relay_probe -- <relay-url> [--token T] [--quic-port P]

use std::time::Duration;

use anyhow::{bail, Context};
use iroh::{
    endpoint::presets, Endpoint, EndpointAddr, RelayConfig, RelayMap, RelayMode, RelayUrl,
    TransportAddr, Watcher,
};

const ALPN: &[u8] = b"ipn/relay-probe/0";
const PAYLOAD: &[u8] = b"nullgate relay probe payload 0.1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let url: RelayUrl = args
        .next()
        .context("usage: relay_probe <relay-url> [--token T] [--quic-port P]")?
        .parse()
        .context("parse relay url")?;
    let mut token: Option<String> = None;
    let mut quic_port: Option<u16> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--token" => token = Some(args.next().context("--token needs a value")?),
            "--quic-port" => {
                quic_port = Some(args.next().context("--quic-port needs a value")?.parse()?)
            }
            other => bail!("unknown arg: {other}"),
        }
    }

    let mut config = RelayConfig::from(url.clone());
    if let (Some(port), Some(quic)) = (quic_port, config.quic.as_mut()) {
        quic.port = port;
    }
    if let Some(t) = &token {
        config = config.with_auth_token(t.clone());
    }
    println!(
        "relay: {url}  (auth token: {}, QUIC addr-discovery port: {})",
        if token.is_some() { "yes" } else { "no" },
        config.quic.as_ref().map(|q| q.port).unwrap_or(0),
    );

    // Phase 1: home-relay connection (HTTPS/WebSocket upgrade, token in header).
    let bind = |cfg: RelayConfig| {
        Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Custom(RelayMap::from(cfg)))
            .alpns(vec![ALPN.to_vec()])
            .bind()
    };
    let a = bind(config.clone()).await.context("bind endpoint A")?;
    let https_ok = tokio::time::timeout(Duration::from_secs(15), a.online())
        .await
        .is_ok();
    println!(
        "\n[1] HTTPS/WS relay connection: {}",
        if https_ok {
            "OK — endpoint is online with the relay as home relay"
        } else {
            "FAILED — could not establish a relay connection within 15s"
        }
    );

    // Phase 2: QUIC address discovery, read from the net-report probes.
    let mut watcher = a.net_report();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(report) = watcher.get() {
            let probes: Vec<String> = report
                .relay_latency
                .iter()
                .map(|(probe, url, lat)| format!("{probe:?} via {url}: {lat:?}"))
                .collect();
            // QAD probes only ever appear in the report if the QUIC connection
            // to the relay succeeded; an HTTPS-only report means QUIC failed.
            let quic_ok = probes.iter().any(|p| p.starts_with("Qad"));
            if quic_ok || std::time::Instant::now() > deadline {
                println!(
                    "[2] QUIC address discovery: {}",
                    if quic_ok { "OK" } else { "FAILED (no QAD probe succeeded)" }
                );
                for p in &probes {
                    println!("      probe {p}");
                }
                println!(
                    "      udp_v4={} udp_v6={} observed_public_v4={:?} preferred_relay={:?}",
                    report.udp_v4, report.udp_v6, report.global_v4, report.preferred_relay
                );
                break;
            }
        } else if std::time::Instant::now() > deadline {
            println!("[2] QUIC address discovery: FAILED (no net-report within 30s)");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Phase 3: actually carry data across the relay. Endpoint B accepts; A dials
    // it with ONLY the relay transport address (Minimal preset = no discovery),
    // so the handshake and the round-trip have no path other than the relay.
    let b = bind(config.clone()).await.context("bind endpoint B")?;
    let _ = tokio::time::timeout(Duration::from_secs(15), b.online()).await;
    let b_addr = EndpointAddr {
        id: b.id(),
        addrs: [TransportAddr::Relay(url.clone())].into(),
    };
    tokio::spawn({
        let b = b.clone();
        async move {
            while let Some(incoming) = b.accept().await {
                if let Ok(conn) = incoming.await {
                    tokio::spawn(async move {
                        if let Ok((mut send, mut recv)) = conn.accept_bi().await {
                            let mut buf = vec![0u8; PAYLOAD.len()];
                            if recv.read_exact(&mut buf).await.is_ok() {
                                let _ = send.write_all(&buf).await;
                            }
                        }
                        // Hold the connection until the dialer closes it.
                        conn.closed().await;
                    });
                }
            }
        }
    });

    let data_ok = tokio::time::timeout(Duration::from_secs(20), async {
        let conn = a.connect(b_addr, ALPN).await.context("connect via relay")?;
        let (mut send, mut recv) = conn.open_bi().await.context("open_bi")?;
        send.write_all(PAYLOAD).await?;
        let mut echo = vec![0u8; PAYLOAD.len()];
        recv.read_exact(&mut echo).await.context("read echo")?;
        anyhow::ensure!(echo == PAYLOAD, "echoed payload does not match");
        conn.close(0u32.into(), b"done");
        Ok::<_, anyhow::Error>(())
    })
    .await;
    match &data_ok {
        Ok(Ok(())) => println!("[3] data round-trip through the relay: OK"),
        Ok(Err(e)) => println!("[3] data round-trip through the relay: FAILED — {e:#}"),
        Err(_) => println!("[3] data round-trip through the relay: FAILED — timed out after 20s"),
    }

    // Phase 4: the same relay must reject a client that has no token.
    if token.is_some() {
        let no_token = RelayConfig::from(url.clone());
        let c = bind(no_token).await.context("bind token-less endpoint")?;
        let rejected = tokio::time::timeout(Duration::from_secs(10), c.online())
            .await
            .is_err();
        println!(
            "[4] token-less client rejected: {}",
            if rejected {
                "OK — endpoint without the token never came online"
            } else {
                "FAILED — the relay accepted a client with NO token"
            }
        );
        c.close().await;
    } else {
        println!("[4] token enforcement: skipped (no --token given)");
    }

    a.close().await;
    b.close().await;
    Ok(())
}
