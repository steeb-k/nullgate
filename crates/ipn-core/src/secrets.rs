//! At-rest storage for 32-byte secrets (the device key, the network secret, and
//! the originator master key).
//!
//! Each secret is stored in the **OS keystore** (Windows Credential Manager,
//! macOS Keychain, Linux Secret Service) when that's available, and otherwise in
//! a `0600` file under `<data_dir>/secrets/` — because a headless/`systemd`
//! daemon often has no Secret Service.
//!
//! The hazard with a fallback is *silently regenerating* a secret when the
//! keystore is temporarily unavailable (which would change this device's identity
//! and kick it off the network). To prevent that, a secret stored in the keystore
//! also drops a `<name>.in-keystore` **marker** next to where its file would be:
//! if the marker is present we *require* the keystore and return an **error** when
//! it's unreachable, rather than reporting "not found" and letting the caller mint
//! a new one.
//!
//! Set `IPN_SECRETS_FILE_ONLY=1` to force the file backend (used by tests, where
//! many in-process engines would otherwise collide on the same global keystore
//! keys; with files they're isolated per data dir).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

const SERVICE: &str = "iroh-private-network";

fn file_only() -> bool {
    std::env::var_os("IPN_SECRETS_FILE_ONLY").is_some()
}

fn secrets_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("secrets")
}
fn key_file(data_dir: &Path, name: &str) -> PathBuf {
    secrets_dir(data_dir).join(format!("{name}.key"))
}
fn marker_file(data_dir: &Path, name: &str) -> PathBuf {
    secrets_dir(data_dir).join(format!("{name}.in-keystore"))
}

/// Store a 32-byte secret under `name`. Prefers the OS keystore; falls back to a
/// `0600` file.
pub fn store(data_dir: &Path, name: &str, bytes: &[u8; 32]) -> Result<()> {
    let _ = std::fs::create_dir_all(secrets_dir(data_dir));
    let hex = data_encoding::HEXLOWER.encode(bytes);

    if !file_only() {
        if let Ok(entry) = keyring::Entry::new(SERVICE, name) {
            if entry.set_password(&hex).is_ok() {
                // Mark it as keystore-backed and drop any prior plaintext file.
                std::fs::write(marker_file(data_dir, name), b"").ok();
                let _ = std::fs::remove_file(key_file(data_dir, name));
                return Ok(());
            }
        }
    }
    // File fallback (no marker → load() will read the file).
    let _ = std::fs::remove_file(marker_file(data_dir, name));
    write_secret_file(&key_file(data_dir, name), &hex)
}

/// Load the secret under `name`, or `None` if it was never stored.
pub fn load(data_dir: &Path, name: &str) -> Result<Option<[u8; 32]>> {
    if marker_file(data_dir, name).exists() {
        // It's keystore-backed: require the keystore (don't fall back / regenerate).
        let entry = keyring::Entry::new(SERVICE, name).context("open keystore entry")?;
        return match entry.get_password() {
            Ok(hex) => Ok(Some(parse_hex(&hex)?)),
            Err(e) => Err(anyhow!(
                "secret '{name}' is stored in the OS keystore but it's unavailable ({e}) — \
                 unlock your login keyring or run in a desktop session"
            )),
        };
    }
    match std::fs::read_to_string(key_file(data_dir, name)) {
        Ok(hex) => Ok(Some(parse_hex(hex.trim())?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("read secret file"),
    }
}

/// Remove a secret from both the keystore and the file fallback.
pub fn delete(data_dir: &Path, name: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, name) {
        let _ = entry.delete_credential();
    }
    let _ = std::fs::remove_file(marker_file(data_dir, name));
    let _ = std::fs::remove_file(key_file(data_dir, name));
}

fn parse_hex(hex: &str) -> Result<[u8; 32]> {
    let bytes = data_encoding::HEXLOWER
        .decode(hex.as_bytes())
        .context("secret is not valid hex")?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("secret must be 32 bytes"))
}

fn write_secret_file(path: &Path, hex: &str) -> Result<()> {
    std::fs::write(path, hex).context("write secret file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("ipn-secrets-test").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // Test the file backend directly (no env / no real keystore), so it's
    // hermetic and parallel-safe.
    #[test]
    fn file_backend_roundtrips() {
        let dir = scratch("roundtrip");
        let secret = [7u8; 32];
        // Simulate the file-fallback path explicitly.
        let _ = std::fs::create_dir_all(secrets_dir(&dir));
        write_secret_file(&key_file(&dir, "node-key"), &data_encoding::HEXLOWER.encode(&secret)).unwrap();
        assert_eq!(load(&dir, "node-key").unwrap(), Some(secret));
    }

    #[test]
    fn missing_secret_is_none() {
        let dir = scratch("missing");
        assert_eq!(load(&dir, "network-secret").unwrap(), None);
    }

    #[test]
    fn delete_removes_file() {
        let dir = scratch("delete");
        let _ = std::fs::create_dir_all(secrets_dir(&dir));
        write_secret_file(&key_file(&dir, "x"), &data_encoding::HEXLOWER.encode(&[1u8; 32])).unwrap();
        assert!(load(&dir, "x").unwrap().is_some());
        delete(&dir, "x");
        assert!(load(&dir, "x").unwrap().is_none());
    }
}
