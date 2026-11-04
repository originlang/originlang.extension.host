use super::{Entry, Error, Storage, validate_key};
use std::sync::Arc;

/// Storage view restricted to one extension's key namespace.
///
/// Every key is stored as `<namespace>/<key>`. Because the namespace is fixed at
/// construction, an extension cannot escape it by crafting a key: there is no
/// path traversal to exploit and the prefix is never taken from the caller.
pub struct ScopedStorage {
    inner: Arc<dyn Storage>,
    namespace: String,
}

impl ScopedStorage {
    /// Wraps `inner` for `namespace`; the namespace must be a validated identity.
    pub fn new(inner: Arc<dyn Storage>, namespace: impl Into<String>) -> Self {
        Self {
            inner,
            namespace: namespace.into(),
        }
    }

    fn full(&self, key: &str) -> Result<String, Error> {
        validate_key(key)?;
        Ok(format!("{}/{}", self.namespace, key))
    }

    fn prefix(&self) -> String {
        format!("{}/", self.namespace)
    }

    /// Namespace this view is bound to.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

impl Storage for ScopedStorage {
    fn get(&self, key: &str) -> Result<Vec<u8>, Error> {
        self.inner.get(&self.full(key)?)
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), Error> {
        self.inner.set(&self.full(key)?, value)
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(&self.full(key)?)
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>, Error> {
        // An empty prefix is legitimate here: it lists the whole namespace.
        if prefix.len() > super::MAX_KEY_BYTES || prefix.contains('\0') {
            return Err(Error::InvalidKey(prefix.chars().take(64).collect()));
        }
        let full = format!("{}{}", self.prefix(), prefix);
        Ok(self
            .inner
            .list(&full)?
            .into_iter()
            .map(|entry| Entry {
                key: entry.key[self.prefix().len()..].to_owned(),
                value: entry.value,
            })
            .collect())
    }

    fn close(&self) -> Result<(), Error> {
        // The underlying store may be shared with other namespaces; a scoped
        // view must not close it for everyone.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStorage;

    #[test]
    fn namespaces_are_isolated_and_keys_are_returned_unprefixed() {
        let store: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let alpha = ScopedStorage::new(store.clone(), "alpha");
        let beta = ScopedStorage::new(store.clone(), "beta");

        alpha.set("settings/theme", b"dark").unwrap();
        beta.set("settings/theme", b"light").unwrap();
        alpha.set("state/counter", b"1").unwrap();

        assert_eq!(alpha.get("settings/theme").unwrap(), b"dark");
        assert_eq!(beta.get("settings/theme").unwrap(), b"light");

        let keys: Vec<String> = alpha
            .list("settings/")
            .unwrap()
            .into_iter()
            .map(|entry| entry.key)
            .collect();
        assert_eq!(keys, vec!["settings/theme"]);
        assert_eq!(alpha.list("").unwrap().len(), 2);
        assert_eq!(beta.list("").unwrap().len(), 1);

        alpha.delete("settings/theme").unwrap();
        assert_eq!(
            alpha.get("settings/theme"),
            Err(Error::NotFound("alpha/settings/theme".into()))
        );
        assert_eq!(beta.get("settings/theme").unwrap(), b"light");
    }

    #[test]
    fn scoped_close_does_not_close_the_shared_backend() {
        let store: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let view = ScopedStorage::new(store.clone(), "alpha");
        view.set("k", b"v").unwrap();
        view.close().unwrap();
        assert_eq!(view.get("k").unwrap(), b"v");
    }
}
