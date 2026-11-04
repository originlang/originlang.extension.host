//! Directory layout for one host installation.
//!
//! ```text
//! <root>/
//! ├── extensions/<id>/extension.json   installed packages (read-only at runtime)
//! ├── data/storage/                    namespaced extension state
//! ├── state/policy.json                capability grants
//! ├── state/registry.json              enable/disable + installed content hashes
//! └── logs/host.log                    rolling host log
//! ```
//!
//! `OEXT_HOME` overrides the root, which is what tests and portable installs use.

use std::env;
use std::io;
use std::path::{Path, PathBuf};

/// Environment variable overriding the installation root.
pub const HOME_ENV: &str = "OEXT_HOME";

/// Default root for the current platform.
pub fn default_home() -> PathBuf {
    if let Some(root) = env::var_os(HOME_ENV) {
        return PathBuf::from(root);
    }
    #[cfg(windows)]
    {
        let base = env::var_os("APPDATA")
            .or_else(|| env::var_os("LOCALAPPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("originlang-ext")
    }
    #[cfg(target_os = "macos")]
    {
        home().join("Library/Application Support/originlang-ext")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"));
        base.join("originlang-ext")
    }
}

#[cfg(not(windows))]
fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolved installation layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Builds the layout for an explicit root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Builds the layout from `OEXT_HOME` or the platform default.
    pub fn from_env() -> Self {
        Self::new(default_home())
    }

    /// Installation root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Installed extension packages.
    pub fn extensions_dir(&self) -> PathBuf {
        self.root.join("extensions")
    }

    /// Mutable host data.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Host state (policy, registry).
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Host logs.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Capability grant document.
    pub fn policy_path(&self) -> PathBuf {
        self.state_dir().join("policy.json")
    }

    /// Registry metadata document.
    pub fn registry_path(&self) -> PathBuf {
        self.state_dir().join("registry.json")
    }

    /// Publisher trust store.
    pub fn publishers_path(&self) -> PathBuf {
        self.state_dir().join("publishers.json")
    }

    /// Storage container directory.
    pub fn storage_dir(&self) -> PathBuf {
        self.data_dir().join("storage")
    }

    /// Rolling text log.
    pub fn log_path(&self) -> PathBuf {
        self.logs_dir().join("host.log")
    }

    /// Creates every directory the host writes to.
    pub fn ensure(&self) -> io::Result<()> {
        for dir in [
            self.extensions_dir(),
            self.data_dir(),
            self.state_dir(),
            self.logs_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_derived_from_the_root() {
        let paths = Paths::new("/tmp/oext");
        assert_eq!(paths.extensions_dir(), Path::new("/tmp/oext/extensions"));
        assert_eq!(
            paths.policy_path(),
            Path::new("/tmp/oext/state/policy.json")
        );
        assert_eq!(paths.storage_dir(), Path::new("/tmp/oext/data/storage"));
        assert_eq!(paths.log_path(), Path::new("/tmp/oext/logs/host.log"));
    }
}
