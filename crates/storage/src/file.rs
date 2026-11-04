use super::{Entry, Error, Storage, validate_key};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAGIC: &[u8; 8] = b"OEXTST01";

fn io(error: std::io::Error) -> Error {
    Error::Io(error.to_string())
}

struct State {
    values: BTreeMap<String, Vec<u8>>,
    /// Held for the lifetime of the store; the OS releases it on drop.
    _lock: File,
}

/// Single-file backend guarded by an exclusive lock.
///
/// The whole map is kept in memory and rewritten on every mutation. That is a
/// deliberate trade-off: extension state is small (kilobytes, not gigabytes),
/// and a full rewrite plus rename gives atomic durability without a journal.
pub struct FileStorage {
    root: PathBuf,
    state: Mutex<Option<State>>,
}

impl FileStorage {
    /// Opens (creating if needed) the store under `root`.
    ///
    /// Returns `Error::Io` when another process holds the directory lock.
    pub fn open(root: &Path) -> Result<Self, Error> {
        fs::create_dir_all(root).map_err(io)?;
        let root = fs::canonicalize(root).map_err(io)?;
        // Refuse symlinked or non-regular container files: a plugin-controlled
        // symlink here would redirect writes outside the data directory.
        for name in ["storage.lock", "storage.bin", "storage.tmp"] {
            if let Ok(meta) = fs::symlink_metadata(root.join(name))
                && (!meta.is_file() || meta.file_type().is_symlink())
            {
                return Err(Error::Io(format!(
                    "{name} is not a regular file in {}",
                    root.display()
                )));
            }
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("storage.lock"))
            .map_err(io)?;
        lock.try_lock()
            .map_err(|e| Error::Io(format!("storage is already open by another process: {e}")))?;

        let values = match fs::read(root.join("storage.bin")) {
            Ok(bytes) => decode(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(io(e)),
        };
        Ok(Self {
            root,
            state: Mutex::new(Some(State {
                values,
                _lock: lock,
            })),
        })
    }

    /// Serializes `values` to a temporary file and renames it into place.
    fn persist(&self, values: &BTreeMap<String, Vec<u8>>) -> Result<(), Error> {
        let temporary = self.root.join("storage.tmp");
        // A leftover temporary file can only belong to a previous owner of the
        // lock, which we hold exclusively now.
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        let outcome = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(io)?;
            file.write_all(MAGIC).map_err(io)?;
            file.write_all(&(values.len() as u64).to_le_bytes())
                .map_err(io)?;
            for (key, value) in values {
                file.write_all(&(key.len() as u64).to_le_bytes())
                    .map_err(io)?;
                file.write_all(&(value.len() as u64).to_le_bytes())
                    .map_err(io)?;
                file.write_all(key.as_bytes()).map_err(io)?;
                file.write_all(value).map_err(io)?;
            }
            file.sync_all().map_err(io)?;
            drop(file);
            replace(&temporary, &self.root.join("storage.bin")).map_err(io)
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        outcome
    }
}

impl Storage for FileStorage {
    fn get(&self, key: &str) -> Result<Vec<u8>, Error> {
        validate_key(key)?;
        self.state
            .lock()
            .map_err(|_| Error::Poisoned)?
            .as_ref()
            .ok_or(Error::Closed)?
            .values
            .get(key)
            .cloned()
            .ok_or_else(|| Error::NotFound(key.into()))
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<(), Error> {
        validate_key(key)?;
        let mut guard = self.state.lock().map_err(|_| Error::Poisoned)?;
        let state = guard.as_mut().ok_or(Error::Closed)?;
        let mut next = state.values.clone();
        next.insert(key.into(), value.to_vec());
        // Persist before committing in memory: a failed write must not leave
        // the process serving data that is absent on disk.
        self.persist(&next)?;
        state.values = next;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), Error> {
        validate_key(key)?;
        let mut guard = self.state.lock().map_err(|_| Error::Poisoned)?;
        let state = guard.as_mut().ok_or(Error::Closed)?;
        if !state.values.contains_key(key) {
            return Ok(());
        }
        let mut next = state.values.clone();
        next.remove(key);
        self.persist(&next)?;
        state.values = next;
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>, Error> {
        let guard = self.state.lock().map_err(|_| Error::Poisoned)?;
        Ok(guard
            .as_ref()
            .ok_or(Error::Closed)?
            .values
            .range(prefix.to_owned()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| Entry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect())
    }

    fn close(&self) -> Result<(), Error> {
        *self.state.lock().map_err(|_| Error::Poisoned)? = None;
        Ok(())
    }
}

fn decode(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], Error> {
        if len > bytes.len() {
            return Err(Error::Corrupt);
        }
        let (value, rest) = bytes.split_at(len);
        *bytes = rest;
        Ok(value)
    }
    fn size(bytes: &mut &[u8]) -> Result<usize, Error> {
        usize::try_from(u64::from_le_bytes(
            take(bytes, 8)?.try_into().map_err(|_| Error::Corrupt)?,
        ))
        .map_err(|_| Error::Corrupt)
    }

    let mut bytes = bytes;
    if take(&mut bytes, MAGIC.len())? != MAGIC {
        return Err(Error::Corrupt);
    }
    let count = size(&mut bytes)?;
    // Each entry needs at least 16 bytes of length prefixes.
    if count > bytes.len() / 16 {
        return Err(Error::Corrupt);
    }
    let mut values = BTreeMap::new();
    for _ in 0..count {
        let key_len = size(&mut bytes)?;
        let value_len = size(&mut bytes)?;
        let key =
            String::from_utf8(take(&mut bytes, key_len)?.to_vec()).map_err(|_| Error::Corrupt)?;
        let value = take(&mut bytes, value_len)?.to_vec();
        if values.insert(key, value).is_some() {
            return Err(Error::Corrupt);
        }
    }
    if !bytes.is_empty() {
        return Err(Error::Corrupt);
    }
    Ok(values)
}

#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // REPLACE_EXISTING | WRITE_THROUGH
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "oext-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn persists_across_reopen_and_excludes_second_owner() {
        let root = directory("persist");
        let store = FileStorage::open(&root).unwrap();
        store.set("../../outside", b"one").unwrap();
        store.set("../../outside", b"two").unwrap();
        store.set("中文/键", b"three").unwrap();
        assert_eq!(store.list("../").unwrap().len(), 1);
        assert!(
            FileStorage::open(&root).is_err(),
            "同一目录只允许一个持有者"
        );

        store.close().unwrap();
        assert_eq!(store.get("../../outside"), Err(Error::Closed));

        let reopened = FileStorage::open(&root).unwrap();
        assert_eq!(reopened.get("../../outside").unwrap(), b"two");
        reopened.delete("../../outside").unwrap();
        reopened.delete("missing").unwrap();
        reopened.close().unwrap();

        let reopened = FileStorage::open(&root).unwrap();
        assert_eq!(reopened.list("").unwrap().len(), 1);
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corruption_is_reported_and_never_silently_replaced() {
        let root = directory("corrupt");
        fs::write(root.join("storage.bin"), b"invalid").unwrap();
        assert!(matches!(FileStorage::open(&root), Err(Error::Corrupt)));
        assert_eq!(fs::read(root.join("storage.bin")).unwrap(), b"invalid");
        fs::remove_dir_all(root).unwrap();
    }
}
