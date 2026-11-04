//! Byte-oriented key/value storage with per-extension namespacing.
//!
//! Two backends share one trait: [`MemoryStorage`] for tests and ephemeral
//! hosts, [`FileStorage`] for the real host. [`ScopedStorage`] enforces the
//! namespace boundary, which is what makes "an extension cannot read another
//! extension's keys" a property of this crate rather than a convention in the
//! host API.

mod file;
mod scoped;

pub use file::FileStorage;
pub use scoped::ScopedStorage;

use std::collections::BTreeMap;
use std::sync::RwLock;

/// Maximum accepted key length in bytes.
pub const MAX_KEY_BYTES: usize = 512;

/// Storage failure modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No value stored under the key.
    NotFound(String),
    /// The store was closed; every operation fails afterwards.
    Closed,
    /// A lock was poisoned by a panicking writer.
    Poisoned,
    /// Backend I/O failure.
    Io(String),
    /// On-disk data failed its integrity checks.
    Corrupt,
    /// Key violates [`validate_key`].
    InvalidKey(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound(key) => write!(f, "key not found: {key}"),
            Error::Closed => write!(f, "storage is closed"),
            Error::Poisoned => write!(f, "storage lock poisoned"),
            Error::Io(message) => write!(f, "storage io: {message}"),
            Error::Corrupt => write!(f, "storage data is corrupt"),
            Error::InvalidKey(key) => write!(f, "invalid key: {key}"),
        }
    }
}

impl std::error::Error for Error {}

/// One key/value pair returned by a prefix listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Key, without the namespace prefix.
    pub key: String,
    /// Raw value.
    pub value: Vec<u8>,
}

/// Keys must be non-empty, bounded and free of NUL bytes.
///
/// Path traversal is not a concern here: keys are never used as file names, the
/// file backend serializes them into a single container file.
pub fn validate_key(key: &str) -> Result<(), Error> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES || key.contains('\0') {
        return Err(Error::InvalidKey(key.chars().take(64).collect()));
    }
    Ok(())
}

/// Backend-agnostic storage contract.
pub trait Storage: Send + Sync {
    /// Reads a value.
    fn get(&self, key: &str) -> Result<Vec<u8>, Error>;
    /// Writes a value, replacing any previous one.
    fn set(&self, key: &str, value: &[u8]) -> Result<(), Error>;
    /// Deletes a key; deleting a missing key succeeds.
    fn delete(&self, key: &str) -> Result<(), Error>;
    /// Lists entries whose key starts with `prefix`, in ascending key order.
    fn list(&self, prefix: &str) -> Result<Vec<Entry>, Error>;
    /// Releases resources; idempotent.
    fn close(&self) -> Result<(), Error>;
}

/// In-memory backend; data is lost when dropped.
pub struct MemoryStorage {
    data: RwLock<Option<BTreeMap<String, Vec<u8>>>>,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStorage {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self {
            data: RwLock::new(Some(BTreeMap::new())),
        }
    }
}

impl Storage for MemoryStorage {
    fn get(&self, key: &str) -> Result<Vec<u8>, Error> {
        validate_key(key)?;
        self.data
            .read()
            .map_err(|_| Error::Poisoned)?
            .as_ref()
            .ok_or(Error::Closed)?
            .get(key)
            .cloned()
            .ok_or_else(|| Error::NotFound(key.to_owned()))
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), Error> {
        validate_key(key)?;
        self.data
            .write()
            .map_err(|_| Error::Poisoned)?
            .as_mut()
            .ok_or(Error::Closed)?
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        validate_key(key)?;
        self.data
            .write()
            .map_err(|_| Error::Poisoned)?
            .as_mut()
            .ok_or(Error::Closed)?
            .remove(key);
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>, Error> {
        let guard = self.data.read().map_err(|_| Error::Poisoned)?;
        let data = guard.as_ref().ok_or(Error::Closed)?;
        Ok(data
            .range(prefix.to_owned()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| Entry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect())
    }

    fn close(&self) -> Result<(), Error> {
        *self.data.write().map_err(|_| Error::Poisoned)? = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_owned_and_prefix_is_literal() {
        let store = MemoryStorage::new();
        let mut original = vec![1, 2, 3];
        store.set("a/two", &original).unwrap();
        store.set("a/one", &[4]).unwrap();
        store.set("ab", &[5]).unwrap();
        original[0] = 9;

        let mut returned = store.get("a/two").unwrap();
        returned[0] = 8;
        assert_eq!(store.get("a/two").unwrap(), vec![1, 2, 3]);
        assert_eq!(
            store
                .list("a/")
                .unwrap()
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            vec!["a/one", "a/two"]
        );
        store.delete("missing").unwrap();
        store.delete("ab").unwrap();
        assert_eq!(store.get("ab"), Err(Error::NotFound("ab".into())));
    }

    #[test]
    fn close_is_idempotent_and_everything_fails_afterwards() {
        let store = MemoryStorage::new();
        store.close().unwrap();
        store.close().unwrap();
        assert_eq!(store.get("x"), Err(Error::Closed));
        assert_eq!(store.set("x", &[]), Err(Error::Closed));
        assert_eq!(store.delete("x"), Err(Error::Closed));
        assert_eq!(store.list(""), Err(Error::Closed));
    }

    #[test]
    fn invalid_keys_are_rejected_by_every_operation() {
        let store = MemoryStorage::new();
        assert!(matches!(store.get(""), Err(Error::InvalidKey(_))));
        assert!(matches!(store.set("a\0b", b"x"), Err(Error::InvalidKey(_))));
        let long = "k".repeat(MAX_KEY_BYTES + 1);
        assert!(matches!(store.set(&long, b"x"), Err(Error::InvalidKey(_))));
    }
}
