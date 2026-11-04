//! Capability grants scoped to an extension identity.
//!
//! The policy is an allow-list: an extension without an explicit grant for a
//! capability is denied. There are no wildcards and no hierarchy, which keeps
//! the authorization decision a single set lookup with no interpretation step.
//!
//! This is an in-process check. It bounds what an extension can do *through the
//! host API*; it is not an OS sandbox. The QuickJS runtime is what supplies the
//! isolation; see `docs/security.md`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// On-disk policy format version.
pub const POLICY_VERSION: u32 = 1;

/// Maximum length of a capability string.
const MAX_CAPABILITY_LEN: usize = 128;

/// Validates a capability identifier.
///
/// A capability is one or more dot-separated segments of `[A-Za-z0-9_-]`.
/// Wildcards, empty segments and whitespace are rejected: silently accepting
/// `fs.*` would turn a typo into a privileged grant.
pub fn validate_capability(capability: &str) -> Result<(), String> {
    let valid = capability.len() <= MAX_CAPABILITY_LEN
        && capability.contains('.')
        && capability.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(format!("invalid capability: {capability}"))
    }
}

/// Validates an extension identity.
///
/// The identity is used as a directory name and as a storage namespace, so it
/// must not contain separators, whitespace or path traversal sequences.
pub fn validate_identity(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && id.split('.').all(|part| !part.is_empty());
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid extension id '{id}': expected lowercase [a-z0-9.-], no leading dot, no '..'"
        ))
    }
}

/// Serialized policy document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Document {
    version: u32,
    #[serde(default)]
    grants: BTreeMap<String, BTreeSet<String>>,
}

/// Grant set for all installed extensions.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    grants: BTreeMap<String, BTreeSet<String>>,
}

impl Policy {
    /// Empty policy: every capability check fails.
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants `capability` to `extension`.
    pub fn grant(&mut self, extension: &str, capability: &str) -> Result<(), String> {
        validate_identity(extension)?;
        validate_capability(capability)?;
        self.grants
            .entry(extension.to_owned())
            .or_default()
            .insert(capability.to_owned());
        Ok(())
    }

    /// Revokes `capability`; revoking an absent grant is a no-op.
    pub fn revoke(&mut self, extension: &str, capability: &str) -> bool {
        self.grants
            .get_mut(extension)
            .is_some_and(|set| set.remove(capability))
    }

    /// Returns true when the exact capability is granted.
    pub fn allows(&self, extension: &str, capability: &str) -> bool {
        self.grants
            .get(extension)
            .is_some_and(|set| set.contains(capability))
    }

    /// Checks every requested capability, stopping at the first denial.
    pub fn authorize(&self, extension: &str, requested: &[String]) -> Result<(), String> {
        for capability in requested {
            validate_capability(capability)?;
            if !self.allows(extension, capability) {
                return Err(format!(
                    "permission denied: extension '{extension}' requires '{capability}'"
                ));
            }
        }
        Ok(())
    }

    /// Sorted capabilities granted to one extension.
    pub fn grants_for(&self, extension: &str) -> Vec<String> {
        self.grants
            .get(extension)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// All extensions with at least one grant, sorted.
    pub fn extensions(&self) -> Vec<String> {
        self.grants.keys().cloned().collect()
    }

    /// Serializes to the on-disk document.
    pub fn to_json(&self) -> String {
        let document = Document {
            version: POLICY_VERSION,
            grants: self.grants.clone(),
        };
        serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".into())
    }

    /// Parses the on-disk document, rejecting versions this build cannot read.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let document: Document =
            serde_json::from_str(text).map_err(|e| format!("invalid policy document: {e}"))?;
        if document.version != POLICY_VERSION {
            return Err(format!(
                "unsupported policy version {} (expected {POLICY_VERSION})",
                document.version
            ));
        }
        for (extension, capabilities) in &document.grants {
            validate_identity(extension)?;
            for capability in capabilities {
                validate_capability(capability)?;
            }
        }
        Ok(Self {
            grants: document.grants,
        })
    }

    /// Reads a policy file; a missing file yields an empty (deny-all) policy.
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                Self::from_json(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e),
        }
    }

    /// Writes the policy atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_path(path);
        fs::write(&temporary, self.to_json())?;
        replace(&temporary, path)?;
        Ok(())
    }
}

/// Sibling temporary path used for atomic replacement.
pub fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(unix)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // REPLACE_EXISTING | WRITE_THROUGH
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_by_default_then_exact_grant() {
        let mut policy = Policy::new();
        assert!(!policy.allows("hello", "log.write"));
        assert!(policy.authorize("hello", &["log.write".into()]).is_err());

        policy.grant("hello", "log.write").unwrap();
        assert!(policy.authorize("hello", &["log.write".into()]).is_ok());
        assert!(policy.authorize("other", &["log.write".into()]).is_err());
        assert!(
            policy
                .authorize("hello", &["log.write".into(), "storage.read".into()])
                .is_err(),
            "一个未授予的能力应使整次授权失败"
        );
    }

    #[test]
    fn wildcards_and_malformed_capabilities_are_rejected() {
        for capability in ["log", "log.*", "*", ".read", "fs..write", "fs.read ", ""] {
            assert!(
                validate_capability(capability).is_err(),
                "{capability} 不应通过校验"
            );
        }
    }

    #[test]
    fn identities_cannot_escape_the_install_root() {
        for id in ["../evil", "a/b", ".hidden", "A.B", "", "a..b", "a b"] {
            assert!(validate_identity(id).is_err(), "{id} 不应通过校验");
        }
        for id in ["hello", "com.example.lint", "a1-b2"] {
            assert!(validate_identity(id).is_ok(), "{id} 应通过校验");
        }
    }

    #[test]
    fn policy_round_trips_and_rejects_future_versions() {
        let mut policy = Policy::new();
        policy.grant("hello", "log.write").unwrap();
        policy.grant("hello", "storage.read").unwrap();
        policy.grant("other", "timer.schedule").unwrap();

        let parsed = Policy::from_json(&policy.to_json()).unwrap();
        assert_eq!(
            parsed.grants_for("hello"),
            vec!["log.write", "storage.read"]
        );
        assert_eq!(parsed.extensions(), vec!["hello", "other"]);

        assert!(Policy::from_json(r#"{"version":99,"grants":{}}"#).is_err());
        assert!(Policy::from_json(r#"{"version":1,"grants":{"Bad":{"log.write":true}}}"#).is_err());
    }

    #[test]
    fn revoke_is_idempotent() {
        let mut policy = Policy::new();
        policy.grant("hello", "log.write").unwrap();
        assert!(policy.revoke("hello", "log.write"));
        assert!(!policy.revoke("hello", "log.write"));
        assert!(!policy.allows("hello", "log.write"));
    }

    #[test]
    fn missing_policy_file_loads_as_deny_all() {
        let path = std::env::temp_dir().join(format!("oext-policy-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        assert!(Policy::load(&path).unwrap().extensions().is_empty());
    }
}
