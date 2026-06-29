//! Verifies the geolocation pipeline against the real DB-IP City MMDB: download
//! it, open it, and resolve a well-known public IP. Network + ~60 MB download, so
//! `#[ignore]`. Run with:
//!   cargo test -p ipn-core --test geo_e2e -- --ignored --nocapture

use ipn_core::geo;

#[test]
#[ignore = "downloads ~60 MB from GitHub; run with --ignored"]
fn download_and_lookup() {
    let dir = std::env::temp_dir().join("ipn-geo-e2e");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(geo::DB_FILENAME);

    geo::download(&path).expect("download DB-IP City mmdb");
    let db = geo::GeoDb::open(&path).expect("open mmdb");

    // 8.8.8.8 (Google DNS) and 1.1.1.1 (Cloudflare) should resolve to something.
    for ip in ["8.8.8.8", "1.1.1.1"] {
        let loc = db.lookup(ip.parse().unwrap());
        println!("{ip} -> {loc:?}");
        assert!(loc.is_some(), "{ip} should resolve to a location");
    }
}
