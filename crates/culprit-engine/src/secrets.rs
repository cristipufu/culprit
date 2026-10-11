//! Where secrets live: connection passwords (account = connection name) and the Microsoft Entra ID refresh token from
//! device-code sign-in (account = `entra-refresh:<tenant>:<client>`). The OS keychain in normal use; memory for tests,
//! embedders that keep state out of the global config, and `CULPRIT_KEYCHAIN=off`.

use std::collections::HashMap;
use std::sync::Mutex;

use culprit_core::{Error, ErrorCode, Result};

const KEYCHAIN_SERVICE: &str = "culprit";

pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    /// Deleting something that isn't there is not an error.
    fn delete(&self, account: &str) -> Result<()>;
    /// Drop any copy kept in this process (the server rejected it), so the next `get` reads the backing store again.
    fn forget(&self, _account: &str) {}
}

/// The OS keychain (macOS Keychain, Windows Credential Manager, Secret Service on Linux), service `culprit`.
///
/// Every scan, plan and live refresh opens connections, and on macOS each keychain read by a binary the person hasn't
/// "Always Allow"-ed is a password prompt. So each secret is read at most once per process and kept in memory (wiped
/// when dropped or replaced); the database drivers hold their own copies while connections are open anyway.
#[derive(Default)]
pub struct Keychain(Mutex<HashMap<String, zeroize::Zeroizing<String>>>);

impl Keychain {
    fn read(&self) -> std::sync::MutexGuard<'_, HashMap<String, zeroize::Zeroizing<String>>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// What to do when there is no keychain to talk to (a headless server, a container, CI) or it refused access.
const UNAVAILABLE_HINT: &str = if cfg!(target_os = "macos") {
    "Unlock the login keychain, or pass the password with --password-env NAME (and set CULPRIT_KEYCHAIN=off to skip the keychain)"
} else if cfg!(windows) {
    "Culprit keeps passwords in Windows Credential Manager; pass the password with --password-env NAME instead (and set CULPRIT_KEYCHAIN=off to skip it)"
} else {
    "Culprit keeps passwords in the Secret Service (GNOME Keyring, KWallet). Without one, as on servers, containers and CI, pass the password with --password-env NAME and set CULPRIT_KEYCHAIN=off"
};

fn keychain_error(error: keyring::Error) -> Error {
    let unavailable = matches!(
        error,
        keyring::Error::NoDefaultStore | keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_)
    );
    let e = Error::new(ErrorCode::Internal, format!("OS keychain: {error}"));
    if unavailable { e.with_hint(UNAVAILABLE_HINT) } else { e }
}

impl SecretStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<String>> {
        if let Some(secret) = self.read().get(account) {
            return Ok(Some(secret.to_string()));
        }
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(keychain_error)?;
        match entry.get_password() {
            Ok(secret) => {
                self.read().insert(account.to_string(), zeroize::Zeroizing::new(secret.clone()));
                Ok(Some(secret))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        keyring::Entry::new(KEYCHAIN_SERVICE, account).and_then(|e| e.set_password(secret)).map_err(keychain_error)?;
        self.read().insert(account.to_string(), zeroize::Zeroizing::new(secret.to_string()));
        Ok(())
    }

    fn forget(&self, account: &str) {
        self.read().remove(account);
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.read().remove(account);
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(keychain_error)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keychain_error(e)),
        }
    }
}

/// Secrets that live as long as the process. Nothing touches the OS keychain.
#[derive(Default)]
pub struct Memory(Mutex<HashMap<String, String>>);

impl SecretStore for Memory {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.lock().map_err(|_| Error::internal("secret store poisoned"))?.get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.0.lock().map_err(|_| Error::internal("secret store poisoned"))?.insert(account.into(), secret.into());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().map_err(|_| Error::internal("secret store poisoned"))?.remove(account);
        Ok(())
    }
}

/// The keychain, unless `CULPRIT_KEYCHAIN=off` (tests, CI, scratch configs): then memory only, and passwords come from
/// `--password-env`.
pub fn default_store() -> std::sync::Arc<dyn SecretStore> {
    let off = std::env::var("CULPRIT_KEYCHAIN")
        .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "off" | "0" | "false"));
    if off { std::sync::Arc::new(Memory::default()) } else { std::sync::Arc::new(Keychain::default()) }
}
