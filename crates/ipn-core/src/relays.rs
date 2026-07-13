//! User-configured **custom relay servers** (self-hosted iroh relays), with an
//! optional per-relay auth token and a policy for how they combine with the
//! public number-0 relays.
//!
//! Two policies:
//! - [`RelayPolicy::Preferred`] — the relay map holds the custom relays **and**
//!   the public n0 relays, with [`PreferMyRelaySelector`] biasing traffic onto
//!   the custom ones. The device stays reachable on the public relays, so a
//!   relay that is down — or a peer that simply hasn't been configured with it
//!   yet — never partitions the network.
//! - [`RelayPolicy::Only`] — custom relays alone, never contact the public
//!   relays. For networks that must not touch third-party infrastructure.
//!   A peer without the relay (or without its token) then cannot reach this
//!   device at all: that is the point, and the cost.
//!
//! With no custom relays configured the endpoint uses the iroh defaults and the
//! policy is irrelevant.
//!
//! `Preferred` used to mean "custom relays *alone*, with a watchdog that adds
//! the public relays back if none of them answers". That watchdog could only
//! ever observe *our* reachability to the relay, never a peer's — so a
//! token-gated relay configured on some devices and not others made those
//! groups mutually invisible while the relay was perfectly healthy. Keeping
//! both sets in the map removes the failure mode instead of trying to detect
//! it. The trade-off: the custom relay may lose the home-relay election to a
//! lower-latency public one, so some inbound traffic can still land on a public
//! relay. Reachability beats relay purity; `Only` remains for anyone who
//! disagrees.
//!
//! The module also provides [`PreferMyRelaySelector`], a [`PathSelector`] that
//! mirrors iroh's default biased-RTT policy but slots relay paths through one
//! of the *user's* relays above relay paths through anything else. Direct
//! (hole-punched) paths still always win over any relay. Under `Preferred` the
//! map now genuinely holds both tiers, which is the case the selector was
//! written for.
//!
//! Settings are per-device (`relays.cbor` in the data dir) and are **not**
//! distributed through the roster: every member that should use the relay has
//! to configure it, with the same URL and token.

use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use iroh::{
    endpoint::transports::{FourTuple, PathSelection, PathSelectionContext, PathSelector},
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
    /// Keep the custom relays **and** the public relays in the map, preferring
    /// the custom ones for traffic. Stays reachable to peers that don't have
    /// the relay configured (or lack its token).
    #[default]
    Preferred,
    /// Use the custom relays exclusively — never contact the public relays.
    /// Peers without the relay cannot reach this device.
    Only,
}

/// How far a relay-settings change has got in reaching the **live** endpoint.
///
/// The settings are saved and reported the moment the user asks for them, but
/// pushing them into a running endpoint means talking to iroh's socket actor,
/// which can stall (see [`crate::engine::Engine::set_relay_settings`]). This is
/// how the CLI and GUI say what actually happened instead of assuming success.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayApply {
    /// The endpoint has the new relay map and has been told to re-probe.
    #[default]
    Applied,
    /// Saved; still being pushed into the endpoint.
    Pending,
    /// The endpoint's socket actor never acknowledged the change. The relay map
    /// itself was still updated (iroh mutates it synchronously), but nothing
    /// re-probed it, so a daemon restart is the reliable way to make it take
    /// effect.
    Failed { reason: String },
}

/// The relay configuration plus how far it has got in being applied — what
/// `GetRelays` answers with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayStatus {
    #[serde(default)]
    pub settings: RelaySettings,
    #[serde(default)]
    pub apply: RelayApply,
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

    /// The relay map this device should be running: exactly what belongs in the
    /// endpoint, for both startup ([`crate::node`]) and a live edit
    /// ([`crate::engine::Engine::set_relay_settings`]).
    ///
    /// - no custom servers → the public defaults
    /// - [`RelayPolicy::Only`] → the custom servers alone
    /// - [`RelayPolicy::Preferred`] → the custom servers **plus** every public
    ///   default that isn't already one of them
    ///
    /// Custom entries come first and win on a URL collision, so a custom relay
    /// that happens to be a public one keeps its auth token.
    pub fn desired_relay_configs(&self) -> Result<Vec<Arc<RelayConfig>>> {
        let custom = self.relay_configs()?;
        if custom.is_empty() {
            return Ok(default_relay_configs());
        }
        let custom_urls: BTreeSet<RelayUrl> = custom.iter().map(|c| c.url.clone()).collect();
        let mut out: Vec<Arc<RelayConfig>> = custom.into_iter().map(Arc::new).collect();
        if self.mode == RelayPolicy::Preferred {
            out.extend(
                default_relay_configs()
                    .into_iter()
                    .filter(|c| !custom_urls.contains(&c.url)),
            );
        }
        Ok(out)
    }
}

/// The public iroh relays — the default map, and (under
/// [`RelayPolicy::Preferred`]) the always-reachable half of a custom one.
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

/// Check that a relay server accepts us **with these credentials**, before the
/// setting is saved and pushed into the live endpoint.
///
/// A relay does not answer "is this token good?" over plain HTTP: the token is
/// read during the websocket upgrade and the access check runs *after* it, so a
/// rejected client gets a `101` like everyone else and is dropped inside the
/// stream. The only way to ask the question is to be a relay client — bind an
/// endpoint whose relay map holds nothing but this relay and see whether it
/// comes online (`examples/relay_probe.rs` proves both halves of this against a
/// real token-gated relay, including that a token-less client is refused).
///
/// This binds its **own** endpoint, so it never touches the running engine's —
/// it cannot wedge the live socket actor the way an `insert_relay` against a
/// relay that answers `401` does (see the gotchas in `CLAUDE.md`).
///
/// A wrong token and an unreachable relay are indistinguishable from out here —
/// both are simply "never came online" — so the error says both.
pub async fn probe_relay(server: &RelayServer, timeout: Duration) -> Result<()> {
    // Reuse the real parse/validate path: a malformed URL or a token that can't
    // ride in an HTTP header fails here, with no socket opened.
    let settings = RelaySettings {
        servers: vec![server.clone()],
        mode: RelayPolicy::Only,
    };
    let cfg = settings
        .relay_configs()?
        .into_iter()
        .next()
        .context("no relay to probe")?;

    // Minimal preset: no discovery services, and a map of exactly one relay, so
    // coming online can only mean *this* relay accepted us.
    let ep = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Custom(iroh::RelayMap::from(cfg)))
        .bind()
        .await
        .context("bind a probe endpoint")?;
    let online = tokio::time::timeout(timeout, ep.online()).await.is_ok();
    ep.close().await;

    if !online {
        let secs = timeout.as_secs();
        if server.token.is_some() {
            bail!(
                "{} did not accept this token within {secs}s — the token may be wrong, \
                 or the relay unreachable",
                server.url
            );
        }
        bail!(
            "no relay connection to {} within {secs}s — it may be unreachable, \
             or it may require an access token",
            server.url
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path selection
// ---------------------------------------------------------------------------

/// Shared, live-updatable set of relay URLs the selector should prefer. The
/// engine updates it when the user edits relay settings; the endpoint holds
/// the selector (and thus this handle) for its whole lifetime.
///
/// [`ArcSwap`] rather than a lock: [`contains`](Self::contains) runs on the
/// send path (the selector is consulted per datagram), and a `std::sync::RwLock`
/// write there would park a whole tokio worker thread — the writer is
/// `set_relay_settings`, called from inside an async fn.
#[derive(Clone, Debug, Default)]
pub struct PreferredRelays(Arc<ArcSwap<BTreeSet<RelayUrl>>>);

impl PreferredRelays {
    pub fn set(&self, urls: BTreeSet<RelayUrl>) {
        self.0.store(Arc::new(urls));
    }

    fn contains(&self, url: &RelayUrl) -> bool {
        self.0.load().contains(url)
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

    /// The regression this whole rework exists for: under `Preferred` the map
    /// must hold the public relays *as well as* the custom one, so a peer that
    /// hasn't been given the relay (or its token) can still reach us.
    #[test]
    fn desired_map_matrix() {
        let defaults: BTreeSet<RelayUrl> =
            default_relay_configs().iter().map(|c| c.url.clone()).collect();
        assert!(!defaults.is_empty(), "iroh should ship default relays");

        let urls = |s: &RelaySettings| -> BTreeSet<RelayUrl> {
            s.desired_relay_configs()
                .unwrap()
                .iter()
                .map(|c| c.url.clone())
                .collect()
        };
        let custom = |mode| RelaySettings {
            servers: vec![RelayServer {
                url: "https://mine.example.com:8443".into(),
                token: Some("tok".into()),
            }],
            mode,
        };
        let mine: RelayUrl = "https://mine.example.com:8443".parse().unwrap();

        // No custom relays: the defaults, whatever the mode says.
        for mode in [RelayPolicy::Preferred, RelayPolicy::Only] {
            let s = RelaySettings { servers: vec![], mode };
            assert_eq!(urls(&s), defaults);
        }
        // Only: the custom relay alone.
        assert_eq!(urls(&custom(RelayPolicy::Only)), [mine.clone()].into());
        // Preferred: the custom relay *and* every default.
        let want: BTreeSet<RelayUrl> = defaults
            .iter()
            .cloned()
            .chain([mine.clone()])
            .collect();
        assert_eq!(urls(&custom(RelayPolicy::Preferred)), want);
    }

    /// A custom relay that *is* one of the public ones must appear once, and
    /// keep its auth token (the custom entry wins the collision).
    #[test]
    fn desired_map_dedups_a_custom_url_that_is_a_default() {
        let dup = default_relay_configs()[0].url.clone();
        let s = RelaySettings {
            servers: vec![RelayServer {
                url: dup.to_string(),
                token: Some("tok".into()),
            }],
            mode: RelayPolicy::Preferred,
        };
        let cfgs = s.desired_relay_configs().unwrap();
        let matching: Vec<_> = cfgs.iter().filter(|c| c.url == dup).collect();
        assert_eq!(matching.len(), 1, "the shared URL must not be listed twice");
        assert!(
            matching[0].auth_token.is_some(),
            "the custom entry (with its token) must win"
        );
        assert_eq!(cfgs.len(), default_relay_configs().len());
    }

    /// Under the new `Preferred` the selector finally sees the mixed map it was
    /// written for: our relay outranks a public one that is in the map with it.
    #[test]
    fn my_relay_wins_in_a_mixed_map() {
        let s = RelaySettings {
            servers: vec![RelayServer {
                url: "https://mine.example.com".into(),
                token: None,
            }],
            mode: RelayPolicy::Preferred,
        };
        let map = s.desired_relay_configs().unwrap();
        assert!(map.len() > 1, "Preferred keeps the public relays too");

        // The selector prefers the custom URLs only — not the defaults that
        // share the map with them (see `node.rs`).
        let p = preferred(&["https://mine.example.com"]);
        let public = map.iter().find(|c| c.url.as_str() != "https://mine.example.com/").unwrap();
        let mine_key = (path_tier(&relay_path("https://mine.example.com"), &p), biased_rtt(&relay_path("https://mine.example.com"), ms(90)));
        let public_path = relay_path(public.url.as_str());
        let public_key = (path_tier(&public_path, &p), biased_rtt(&public_path, ms(10)));
        assert!(mine_key < public_key, "my relay outranks a faster public one");
        assert!(should_switch(public_key, mine_key));
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

    /// The probe rejects input it could never connect with *before* it binds an
    /// endpoint — the caller (the CLI's token prompt) re-asks on failure, and
    /// making it wait out a timeout to be told the token has a space in it would
    /// be a poor way to spend 15 seconds. The connecting half is e2e-only.
    #[tokio::test]
    async fn probe_rejects_unusable_input_without_dialing() {
        let started = std::time::Instant::now();
        for bad in [
            RelayServer { url: "not a url".into(), token: Some("tok".into()) },
            RelayServer {
                url: "https://relay.example.com".into(),
                token: Some("has space".into()),
            },
        ] {
            assert!(probe_relay(&bad, Duration::from_secs(15)).await.is_err());
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the pre-flight checks must fail without opening a socket"
        );
    }
}
