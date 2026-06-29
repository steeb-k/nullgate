//! Optional IP geolocation, used **only by the originator**, which looks up each
//! member's advertised public IP and propagates the resulting "City, Country"
//! strings to everyone (members never need the database).
//!
//! Data: DB-IP City Lite (MMDB), CC BY 4.0 — attribution "IP Geolocation by
//! DB-IP" (<https://db-ip.com>) is shown in the UI where results appear. The DB
//! is fetched at runtime (~60 MB) and refreshed periodically.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use anyhow::{Context, Result};

/// IPv4 city DB (the IPv6 file is separate; most members are reachable via IPv4).
const DBIP_CITY_IPV4_URL: &str =
    "https://github.com/sapics/ip-location-db/releases/download/latest/dbip-city-ipv4.mmdb";

/// Filename under the data dir.
pub const DB_FILENAME: &str = "dbip-city-ipv4.mmdb";

/// Required CC BY 4.0 attribution for DB-IP Lite data.
pub const ATTRIBUTION_TEXT: &str = "IP Geolocation by DB-IP";
pub const ATTRIBUTION_URL: &str = "https://db-ip.com/";

pub struct GeoDb {
    reader: maxminddb::Reader<Vec<u8>>,
}

impl GeoDb {
    /// Load an MMDB from disk (reads it into memory; ~60 MB).
    pub fn open(path: &Path) -> Result<Self> {
        let reader = maxminddb::Reader::open_readfile(path).context("open geo mmdb")?;
        Ok(Self { reader })
    }

    /// Resolve an IPv4 address to a `"City, Country"` string (best-effort).
    pub fn lookup(&self, ip: Ipv4Addr) -> Option<String> {
        let v: serde_json::Value = self.reader.lookup(IpAddr::V4(ip)).ok()?;
        // Handle both the flat DB-IP schema (`city`, `country_code`) and the
        // GeoLite2-nested schema (`city.names.en`, `country.iso_code`).
        let city = json_str(&v, &["city", "names", "en"]).or_else(|| json_str(&v, &["city"]));
        let cc =
            json_str(&v, &["country", "iso_code"]).or_else(|| json_str(&v, &["country_code"]));
        let country = cc.as_deref().map(country_name);
        match (city, country) {
            (Some(c), Some(co)) => Some(format!("{c}, {co}")),
            (Some(c), None) => Some(c),
            (None, Some(co)) => Some(co),
            (None, None) => None,
        }
    }
}

/// Pull a (possibly nested) string out of a JSON value by key path.
fn json_str(v: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k)?;
    }
    cur.as_str().map(|s| s.to_string())
}

/// ISO 3166-1 alpha-2 code → English country name (falls back to the code).
fn country_name(alpha2: &str) -> String {
    rust_iso3166::from_alpha2(alpha2)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| alpha2.to_string())
}

/// Download the DB-IP City IPv4 MMDB to `path` (atomic temp + rename). Blocking —
/// call from a blocking context. ~60 MB.
pub fn download(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let resp = ureq::get(DBIP_CITY_IPV4_URL)
        .call()
        .context("download DB-IP City mmdb")?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp).context("create temp geo db")?;
        std::io::copy(&mut resp.into_reader(), &mut f).context("write geo db")?;
    }
    std::fs::rename(&tmp, path).context("install geo db")?;
    Ok(())
}
