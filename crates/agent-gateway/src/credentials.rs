//! Keys never cross the Host boundary. No file, environment or in-memory fallback keystore.
use zeroize::Zeroizing;

const SERVICE: &str = "dev.coloride.desktop.provider-api-key";

fn entry(provider: &str) -> Result<keyring::Entry, String> {
    if agent_registry::profiles::provider_profile(provider).is_none() {
        return Err("Unknown provider".into());
    }
    keyring::Entry::new(SERVICE, provider).map_err(|_| "OS credential store is unavailable".into())
}

pub fn store_key(provider: &str, key: Zeroizing<String>) -> Result<(), String> {
    if key.is_empty() || key.len() > 8192 || key.contains(['\r', '\n', '\0']) {
        return Err("Invalid provider API key".into());
    }
    entry(provider)?
        .set_password(&key)
        .map_err(|_| "Could not save key in OS credential store".into())
}

pub fn read_key(provider: &str) -> Result<Option<Zeroizing<String>>, String> {
    match entry(provider)?.get_password() {
        Ok(key) => Ok(Some(Zeroizing::new(key))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err("Could not read key from OS credential store".into()),
    }
}

pub fn remove_key(provider: &str) -> Result<(), String> {
    match entry(provider)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err("Could not remove key from OS credential store".into()),
    }
}
