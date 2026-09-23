//! Port of `AIProviderKeychainStore.swift` — API keys live in Windows
//! Credential Manager (encrypted at rest, per-user), never in the config
//! file or any plaintext on disk. Service names mirror the macOS
//! `<bundleID>.<provider>` shape so the mapping is auditable.

use super::providers::Provider;

const BUNDLE_ID: &str = "com.shampoo.donemd";
const ACCOUNT: &str = "default";

fn service_name(provider: Provider) -> String {
    format!("{BUNDLE_ID}.{}", provider.id())
}

fn entry(provider: Provider) -> Result<keyring::Entry, String> {
    keyring::Entry::new(&service_name(provider), ACCOUNT).map_err(|e| e.to_string())
}

pub fn save_key(provider: Provider, key: &str) -> Result<(), String> {
    entry(provider)?.set_password(key).map_err(|e| e.to_string())
}

/// None when no key is stored.
pub fn load_key(provider: Provider) -> Result<Option<String>, String> {
    match entry(provider)?.get_password() {
        Ok(k) => Ok(Some(k)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn clear_key(provider: Provider) -> Result<(), String> {
    match entry(provider)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe the REAL Credential Manager end-to-end (a save that returns Ok
    /// but never persists would explain a later 401: the stream path loads
    /// an empty key). Run explicitly:
    /// `cargo test credential_probe_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore = "touches the real Credential Manager"]
    fn credential_probe_roundtrip() {
        let service = "com.shampoo.donemd.probe";
        let entry = keyring::Entry::new(service, ACCOUNT).unwrap();
        entry.delete_credential().ok();
        entry.set_password("probe-secret-123").unwrap();
        let got = entry.get_password();
        eprintln!("[probe] get_password after set: {got:?}");
        assert_eq!(got.as_deref().ok(), Some("probe-secret-123"));
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }
}
