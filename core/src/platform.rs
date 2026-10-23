//! Everything that differs between operating systems: where files live and where the token
//! is kept. The desktop feature (default) uses the OS folders and keychain; a mobile app
//! calls [`configure`] once at startup with its own folder and secret store.

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// Keeps the sign-in token out of the plain-text config (OS keychain, Android Keystore, ...).
pub trait SecretStore: Send + Sync {
    /// Returns false if the secret could not be stored.
    fn set(&self, account: &str, secret: &str) -> bool;
    fn get(&self, account: &str) -> Option<String>;
    fn delete(&self, account: &str);
}

struct Config {
    /// Holds settings and the session (`config_dir/kairo/...`).
    config_dir: PathBuf,
    /// Holds the per-account cache (`data_dir/kairo/...`).
    data_dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static DEVICE_NAME: OnceLock<String> = OnceLock::new();

/// Names this device in the server's session list (default: "Kairo Desktop (host)").
pub fn set_device_name(name: &str) {
    let _ = DEVICE_NAME.set(name.to_string());
}

pub(crate) fn device_name() -> Option<&'static str> {
    DEVICE_NAME.get().map(String::as_str)
}

/// Sets the folders and secret store. Call once, before anything else; later calls are ignored.
pub fn configure(config_dir: PathBuf, data_dir: PathBuf, secrets: Arc<dyn SecretStore>) {
    let _ = CONFIG.set(Config { config_dir, data_dir, secrets });
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    match CONFIG.get() {
        Some(config) => Some(config.config_dir.join("kairo")),
        None => default_config_dir(),
    }
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    match CONFIG.get() {
        Some(config) => Some(config.data_dir.join("kairo")),
        None => default_data_dir(),
    }
}

pub(crate) fn secrets() -> Option<Arc<dyn SecretStore>> {
    match CONFIG.get() {
        Some(config) => Some(config.secrets.clone()),
        None => default_secrets(),
    }
}

#[cfg(feature = "desktop")]
fn default_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("kairo"))
}

#[cfg(feature = "desktop")]
fn default_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("kairo"))
}

#[cfg(feature = "desktop")]
fn default_secrets() -> Option<Arc<dyn SecretStore>> {
    Some(Arc::new(Keychain))
}

#[cfg(not(feature = "desktop"))]
fn default_config_dir() -> Option<PathBuf> {
    None
}

#[cfg(not(feature = "desktop"))]
fn default_data_dir() -> Option<PathBuf> {
    None
}

#[cfg(not(feature = "desktop"))]
fn default_secrets() -> Option<Arc<dyn SecretStore>> {
    None
}

#[cfg(feature = "desktop")]
const KEYRING_SERVICE: &str = "kairo";

#[cfg(feature = "desktop")]
struct Keychain;

#[cfg(feature = "desktop")]
impl SecretStore for Keychain {
    fn set(&self, account: &str, secret: &str) -> bool {
        keyring::Entry::new(KEYRING_SERVICE, account).is_ok_and(|entry| entry.set_password(secret).is_ok())
    }

    fn get(&self, account: &str) -> Option<String> {
        keyring::Entry::new(KEYRING_SERVICE, account).ok()?.get_password().ok()
    }

    fn delete(&self, account: &str) {
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, account) {
            let _ = entry.delete_credential();
        }
    }
}
