//! Secrets (passwords, tokens) live only in the system's secret store, never in files.

use crate::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Service name of every Draft Desk entry in the keychain.
pub const SERVICE: &str = "Draft Desk";

/// The system's secret store. Debug builds use the JSON file `$DRAFTDESK_TEST_SECRETS_FILE` instead when it is set,
/// so tests never touch the keychain; release builds know only the keychain.
pub fn system() -> Arc<dyn Secrets + Send + Sync> {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("DRAFTDESK_TEST_SECRETS_FILE") {
        return Arc::new(FileSecrets(std::path::PathBuf::from(p)));
    }
    Arc::new(Keychain)
}

/// A JSON file as secret store, for tests only (debug builds).
#[cfg(debug_assertions)]
pub struct FileSecrets(pub std::path::PathBuf);

#[cfg(debug_assertions)]
impl FileSecrets {
    fn map(&self) -> serde_json::Map<String, serde_json::Value> {
        std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn save(&self, m: serde_json::Map<String, serde_json::Value>) -> Result<()> {
        std::fs::write(&self.0, serde_json::Value::Object(m).to_string())?;
        Ok(())
    }
}

#[cfg(debug_assertions)]
impl Secrets for FileSecrets {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .map()
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(String::from))
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut m = self.map();
        m.insert(key.into(), value.into());
        self.save(m)
    }
    fn delete(&self, key: &str) -> Result<()> {
        let mut m = self.map();
        m.remove(key);
        self.save(m)
    }
}

pub trait Secrets {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    /// Deleting a missing entry is not an error.
    fn delete(&self, key: &str) -> Result<()>;
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl Secrets for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.0.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// The macOS login keychain, as generic passwords under [`SERVICE`].
pub struct Keychain;

#[cfg(target_os = "macos")]
mod keychain {
    use super::{Keychain, SERVICE, Secrets};
    use crate::{Error, Result};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    impl Secrets for Keychain {
        fn get(&self, key: &str) -> Result<Option<String>> {
            match get_generic_password(SERVICE, key) {
                Ok(bytes) => String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| Error(format!("keychain {key}: not UTF-8"))),
                Err(e) if e.code() == NOT_FOUND => Ok(None),
                Err(e) => Err(Error(format!("keychain {key}: {e}"))),
            }
        }
        fn set(&self, key: &str, value: &str) -> Result<()> {
            set_generic_password(SERVICE, key, value.as_bytes())
                .map_err(|e| Error(format!("keychain {key}: {e}")))
        }
        fn delete(&self, key: &str) -> Result<()> {
            match delete_generic_password(SERVICE, key) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == NOT_FOUND => Ok(()),
                Err(e) => Err(Error(format!("keychain {key}: {e}"))),
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Secrets for Keychain {
    fn get(&self, _key: &str) -> Result<Option<String>> {
        Err(crate::Error("no secret store on this platform".into()))
    }
    fn set(&self, _key: &str, _value: &str) -> Result<()> {
        Err(crate::Error("no secret store on this platform".into()))
    }
    fn delete(&self, _key: &str) -> Result<()> {
        Err(crate::Error("no secret store on this platform".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn file_secrets_round_trip() {
        let p = std::env::temp_dir().join(format!("dd-file-secrets-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let s = FileSecrets(p.clone());
        assert_eq!(s.get("a").unwrap(), None);
        s.set("a", "1").unwrap();
        s.set("b", "2").unwrap();
        assert_eq!(
            FileSecrets(p.clone()).get("a").unwrap().as_deref(),
            Some("1")
        );
        s.delete("a").unwrap();
        s.delete("a").unwrap();
        assert_eq!(s.get("a").unwrap(), None);
        assert_eq!(s.get("b").unwrap().as_deref(), Some("2"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn memory_roundtrip() {
        let s = MemorySecrets::default();
        assert_eq!(s.get("a").unwrap(), None);
        s.set("a", "1").unwrap();
        s.set("a", "2").unwrap();
        assert_eq!(s.get("a").unwrap().as_deref(), Some("2"));
        s.delete("a").unwrap();
        s.delete("a").unwrap();
        assert_eq!(s.get("a").unwrap(), None);
    }
}
