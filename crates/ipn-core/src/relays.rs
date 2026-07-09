//! User-configured **custom relay servers** (self-hosted iroh relays), with an
//! optional per-relay auth token and a policy for how they combine with the
//! public number-0 relays.
//!
//! Two policies:
//! - [`RelayPolicy::Preferred`] — the endpoint runs on the custom relays alone;
//!   if none of them is reachable the engine's watchdog *adds* the public n0
//!   relays as a fallback, and removes them again once a custom relay is back
//!   (see `relay_watchdog` in [`crate::engine`]). Best of both: your relay
//!   carries traffic whenever it's up, but a dead relay never strands you.
//! - [`RelayPolicy::Only`] — custom relays only, never fall back. For networks
//!   that must not touch third-party infrastructure.
//!
//! With no custom relays configured the endpoint uses the iroh defaults and the
//! policy is irrelevant.
//!
//! The module also provides [`PreferMyRelaySelector`], a [`PathSelector`] that
//! mirrors iroh's default biased-RTT policy but slots relay paths through one
//! of the *user's* relays above relay paths through anything else. Direct
//! (hole-punched) paths still always win over any relay.
//!
//! Settings are per-device (`relays.cbor` in the data dir) and are **not**
//! distributed through the roster: every member that should use the relay has
//! to configure it (a relay with a token rejects clients that don't have it,
//! which would also break relay-assisted hole-punching with unconfigured
//! peers).

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use iroh::{
    endpoint::transports::{
        FourTuple, PathSelection, PathSelectionContext, PathSelector,
    },
    RelayConfig, RelayUrl,
};
use serde::{Deserialize, Serialize};

const RELAYS_FILE: &str = "relays.cbor";

/// One user-configured relay server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayServer {
    /// `https://host[:port]` of the relay.
    pub url: String,
    /// Optional access token, sent as `Authorization: Bearer <token>` when
    /// connecting. Stored locally in `relays.cbor` (see `docs/security.md`).
    #[serde(default)]
    pub token: Option<String>,
}

/// How custom relays combine with the public iroh relays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayPolicy {
    /// Use the custom relays; fall back to the public relays only while none
    /// of the custom ones is reachable.
    #[default]
    Preferred,
    /// Use the custom relays exclusively — never contact the public relays.
    Only,
}

/// The persisted relay configuration for this device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySettings {
    #[serde(default)]
    pub servers: Vec<RelayServer>,
    #[serde(default)]
    pub mode: RelayPolicy,
}

impl RelaySettings {
    /// Whether any custom relay is configured (otherwise the iroh defaults apply).
    pub fn is_custom(&self) -> bool {
        !self.servers.is_empty()
    }

    /// Parses the configured servers into iroh [`RelayConfig`]s (with tokens
    /// attached), validating as it goes. An empty list is valid and means
    /// "use the defaults".
    pub fn relay_configs(&self) -> Result<Vec<RelayConfig>> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::with_capacity(self.servers.len());
        for s in &self.servers {
            let url: RelayUrl = s
                .url
                .parse()
                .with_context(|| format!("invalid relay URL {:?}", s.url))?;
            if !seen.insert(url.clone()) {
                bail!("duplicate relay URL {url}");
            }
            let mut cfg = RelayConfig::from(url);
            if let Some(token) = &s.token {
                // The token travels in an HTTP header; reject values that
                // can't (control chars / non-ASCII) up front rather than
                // failing every connection attempt later.
                if token.is_empty() || !token.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
                    bail!(
                        "relay token for {} must be non-empty printable ASCII without spaces",
                        s.url
                    );
                }
                cfg = cfg.with_auth_token(token.clone());
            }
            out.push(cfg);
        }
        Ok(out)
    }

    /// The parsed URLs of the configured servers (order preserved).
    pub fn urls(&self) -> Result<Vec<RelayUrl>> {
        Ok(self.relay_configs()?.into_iter().map(|c| c.url).collect())
    }
}

/// The public iroh relays used as the default map and as the fallback set.
pub(crate) fn default_relay_configs() -> Vec<Arc<RelayConfig>> {
    iroh::endpoint::default_relay_mode().relay_map().relays()
}

pub fn load_relay_settings(data_dir: &Path) -> RelaySettings {
    std::fs::read(data_dir.join(RELAYS_FILE))
        .ok()
        .and_then(|b| ciborium::from_reader(b.as_slice()).ok())
        .unwrap_or_default()
}

pub fn save_relay_settings(data_dir: &Path, settings: &RelaySettings) -> Result<()> {
    std::fs::create_dir_all(data_dir).ok();
    let mut buf = Vec::new();
    ciborium::into_writer(settings, &mut buf).context("encode relay settings")?;
    std::fs::write(data_dir.join(RELAYS_FILE), buf).context("write relay settings")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Path selection
// ---------------------------------------------------------------------------

/// Shared, live-updatable set of relay URLs the selector should prefer. The
/// engine updates it when the user edits relay settings; the endpoint holds
/// the selector (and thus this handle) for its whole lifetime.
#[derive(Clone, Debug, Default)]
pub struct PreferredRelays(Arc<RwLock<BTreeSet<RelayUrl>>>);

impl PreferredRelays {
    pub fn set(&self, urls: BTreeSet<RelayUrl>) {
        *self.0.write().expect("poisoned") = urls;
    }

    fn contains(&self, url: &RelayUrl) -> bool {
        self.0.read().expect("poisoned").contains(url)
    }
}

/// Mirrors iroh's default path selection (IPv6 3ms ahead of IPv4, lowest
/// biased RTT wins, 5ms stickiness against flapping) with one change: relay
/// paths are split into two tiers, and a path through one of the user's own
/// relays always beats a path through any other relay. Direct paths keep
/// beating every relay, so this only matters while traffic is relayed.
const IPV6_RTT_ADVANTAGE: Duration = Duration::from_millis(3);
const RTT_SWITCHING_MIN: Duration = Duration::from_millis(5);

/// Preference tier of a path: lower wins regardless of RTT. `0` = direct
/// (IP or custom transport), `1` = one of the user's relays, `2` = any other
/// relay. Mirrors iroh's Primary/Backup split with the relay tier divided.
fn path_tier(path: &FourTuple, preferred: &PreferredRelays) -> u8 {
    match path {
        FourTuple::Relay { url, .. } => {
            if preferred.contains(url) {
                1
            } else {
                2
            }
        }
        _ => 0,
    }
}

/// Biased RTT in nanoseconds: the tie-breaker within a tier.
fn biased_rtt(path: &FourTuple, rtt: Duration) -> i128 {
    let mut ns = rtt.as_nanos() as i128;
    if matches!(path, FourTuple::Ip { remote, .. } if remote.is_ipv6()) {
        ns -= IPV6_RTT_ADVANTAGE.as_nanos() as i128;
    }
    ns
}

/// Whether a path with key `best` should replace the current path with key
/// `current`: immediately across tiers, only on a clear RTT win within one.
fn should_switch(current: (u8, i128), best: (u8, i128)) -> bool {
    if best.0 != current.0 {
        best.0 < current.0
    } else {
        best.1 + RTT_SWITCHING_MIN.as_nanos() as i128 <= current.1
    }
}

#[derive(Debug)]
pub struct PreferMyRelaySelector {
    preferred: PreferredRelays,
}

impl PreferMyRelaySelector {
    pub fn new(preferred: PreferredRelays) -> Self {
        Self { preferred }
    }
}

impl PathSelector for PreferMyRelaySelector {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let current = ctx.current();
        let mut best: Option<(iroh::endpoint::transports::PathSelectionData<'_>, (u8, i128))> =
            None;
        let mut current_key: Option<(u8, i128)> = None;

        for psd in ctx.paths() {
            let path = psd.network_path();
            // A path whose stats are gone was closed concurrently; skip it.
            let Some(stats) = psd.stats() else { continue };
            let key = (
                path_tier(path, &self.preferred),
                biased_rtt(path, stats.rtt),
            );
            if Some(path) == current && current_key.is_none_or(|c| key < c) {
                current_key = Some(key);
            }
            if best.as_ref().is_none_or(|(_, b)| key < *b) {
                best = Some((psd, key));
            }
        }

        let mut selection = PathSelection::none();
        let Some((best_psd, best_key)) = best else {
            return selection;
        };
        match current_key {
            // No current path (or no stats for it): take the best one.
            None => selection.set(&best_psd),
            Some(cur) if should_switch(cur, best_key) => selection.set(&best_psd),
            Some(_) => {}
        }
        selection
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn relay_path(url: &str) -> FourTuple {
        FourTuple::from_remote(iroh::endpoint::transports::Addr::Relay(
            url.parse().unwrap(),
            iroh::EndpointId::from_bytes(&[0u8; 32]).unwrap(),
        ))
    }

    fn ip_path(v6: bool) -> FourTuple {
        let addr: std::net::SocketAddr = if v6 {
            "[::1]:9000".parse().unwrap()
        } else {
            "127.0.0.1:9000".parse().unwrap()
        };
        FourTuple::from_remote(iroh::endpoint::transports::Addr::Ip(addr))
    }

    fn preferred(urls: &[&str]) -> PreferredRelays {
        let p = PreferredRelays::default();
        p.set(urls.iter().map(|u| u.parse().unwrap()).collect());
        p
    }

    #[test]
    fn my_relay_beats_faster_foreign_relay() {
        let p = preferred(&["https://mine.example.com"]);
        let mine = relay_path("https://mine.example.com");
        let n0 = relay_path("https://relay.iroh.link");
        // Even a much faster foreign relay stays in a worse tier.
        let mine_key = (path_tier(&mine, &p), biased_rtt(&mine, ms(80)));
        let n0_key = (path_tier(&n0, &p), biased_rtt(&n0, ms(5)));
        assert!(mine_key < n0_key);
        assert!(should_switch(n0_key, mine_key));
        // ...but a direct path still beats my relay.
        let direct_key = (path_tier(&ip_path(false), &p), biased_rtt(&ip_path(false), ms(200)));
        assert!(direct_key < mine_key);
    }

    #[test]
    fn without_preferred_relays_matches_default_policy() {
        let p = preferred(&[]);
        let a = relay_path("https://a.example.com");
        let b = relay_path("https://b.example.com");
        // Same tier: RTT + stickiness decide.
        let a_key = (path_tier(&a, &p), biased_rtt(&a, ms(50)));
        let b_fast = (path_tier(&b, &p), biased_rtt(&b, ms(40)));
        let b_close = (path_tier(&b, &p), biased_rtt(&b, ms(47)));
        assert!(should_switch(a_key, b_fast), "5ms better switches");
        assert!(!should_switch(a_key, b_close), "3ms better sticks");
        // IPv6 gets its 3ms advantage over IPv4.
        assert!(
            biased_rtt(&ip_path(true), ms(10)) < biased_rtt(&ip_path(false), ms(9))
        );
    }

    #[test]
    fn live_update_of_preferred_set() {
        let p = preferred(&[]);
        let mine = relay_path("https://mine.example.com");
        assert_eq!(path_tier(&mine, &p), 2);
        p.set(["https://mine.example.com".parse().unwrap()].into());
        assert_eq!(path_tier(&mine, &p), 1);
    }

    #[test]
    fn settings_validation() {
        let ok = RelaySettings {
            servers: vec![RelayServer {
                url: "https://relay.example.com:8443".into(),
                token: Some("s3cret-Token_123".into()),
            }],
            mode: RelayPolicy::Preferred,
        };
        assert_eq!(ok.relay_configs().unwrap().len(), 1);
        assert!(ok.relay_configs().unwrap()[0].auth_token.is_some());

        for bad in [
            RelaySettings {
                servers: vec![RelayServer { url: "not a url".into(), token: None }],
                ..Default::default()
            },
            RelaySettings {
                servers: vec![RelayServer {
                    url: "https://a.example.com".into(),
                    token: Some("has space".into()),
                }],
                ..Default::default()
            },
            RelaySettings {
                servers: vec![
                    RelayServer { url: "https://a.example.com".into(), token: None },
                    RelayServer { url: "https://a.example.com".into(), token: None },
                ],
                ..Default::default()
            },
        ] {
            assert!(bad.relay_configs().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn settings_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ipn-relays-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = RelaySettings {
            servers: vec![RelayServer {
                url: "https://relay.example.com:8443".into(),
                token: Some("tok".into()),
            }],
            mode: RelayPolicy::Only,
        };
        save_relay_settings(&dir, &s).unwrap();
        assert_eq!(load_relay_settings(&dir), s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
