//! Installed-extension registry.
//!
//! The registry owns the install directory and a small metadata document that
//! records two things per extension: whether it is enabled, and the content hash
//! captured at install time. The hash is the reason a locally modified package
//! cannot silently load: the host re-computes it before every load.

use originlang_ext_manifest::{MANIFEST_FILE, Manifest, ManifestError};
use originlang_ext_signing::{TrustStore, verify_package};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{HostError, HostResult};

/// Metadata document version.
pub const REGISTRY_VERSION: u32 = 1;

/// Maximum files accepted in one package.
const MAX_FILES: usize = 1024;

/// Maximum total bytes accepted in one package.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// What an install is willing to accept.
pub struct SignaturePolicy<'a> {
    /// Reject packages that are unsigned or signed by an untrusted key.
    pub require_signature: bool,
    /// Publishers the operator has accepted.
    pub trust: &'a TrustStore,
}

impl<'a> SignaturePolicy<'a> {
    /// Accepts unsigned packages and records whether a signature was trusted.
    pub fn permissive(trust: &'a TrustStore) -> Self {
        Self {
            require_signature: false,
            trust,
        }
    }
}

/// Provenance recorded at install time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// Publisher name carried by the signature, absent when unsigned.
    pub publisher: Option<String>,
    /// Whether the signing key is in the operator's trust store.
    pub trusted: bool,
}

/// One installed package, validated against disk at scan time.
#[derive(Debug, Clone)]
pub struct InstalledExtension {
    /// Parsed manifest.
    pub manifest: Manifest,
    /// Package root.
    pub root: PathBuf,
    /// Hash recorded at install (or adopted on first scan).
    pub hash: String,
    /// Operator switch.
    pub enabled: bool,
    /// Who signed it, if anyone.
    pub provenance: Provenance,
}

/// Outcome of a directory scan.
#[derive(Debug, Default)]
pub struct Scan {
    /// Valid packages, sorted by id.
    pub extensions: Vec<InstalledExtension>,
    /// Human-readable problems for directories that could not be used.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateEntry {
    enabled: bool,
    hash: String,
    #[serde(rename = "installedAt")]
    installed_at: u64,
    /// Publisher recorded at install; absent for unsigned packages and for
    /// entries written by older versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publisher: Option<String>,
    /// Whether the signing key was trusted when the package was installed.
    #[serde(default)]
    trusted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateDocument {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, StateEntry>,
}

impl Default for StateDocument {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// Install directory plus metadata document.
pub struct Registry {
    extensions_dir: PathBuf,
    state_path: PathBuf,
    state: Mutex<StateDocument>,
}

impl Registry {
    /// Opens the registry, creating the install directory when missing.
    pub fn open(extensions_dir: PathBuf, state_path: PathBuf) -> HostResult<Self> {
        fs::create_dir_all(&extensions_dir)
            .map_err(|e| HostError::io("create extensions directory", e))?;
        let state = match fs::read_to_string(&state_path) {
            Ok(text) => {
                let document: StateDocument = serde_json::from_str(&text)
                    .map_err(|e| HostError::Io(format!("invalid registry document: {e}")))?;
                if document.version != REGISTRY_VERSION {
                    return Err(HostError::Io(format!(
                        "unsupported registry version {} (expected {REGISTRY_VERSION})",
                        document.version
                    )));
                }
                document
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StateDocument::default(),
            Err(e) => return Err(HostError::io("read registry document", e)),
        };
        Ok(Self {
            extensions_dir,
            state_path,
            state: Mutex::new(state),
        })
    }

    /// Scans the install directory.
    ///
    /// Directories that fail validation are reported as warnings instead of
    /// aborting the scan: one broken package must not hide the others.
    pub fn scan(&self) -> HostResult<Scan> {
        let mut scan = Scan::default();
        let mut adopted = false;
        let mut state = self.state.lock().expect("registry state lock");

        let entries = match fs::read_dir(&self.extensions_dir) {
            Ok(entries) => entries,
            Err(e) => return Err(HostError::io("read extensions directory", e)),
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !path.is_dir() || name.starts_with('.') {
                continue;
            }
            let manifest = match Manifest::read_dir(&path) {
                Ok(manifest) => manifest,
                Err(error) => {
                    scan.warnings.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let hash = match state.entries.get(&manifest.id) {
                Some(entry) => entry.hash.clone(),
                None => {
                    // Manually copied package: adopt its current content as the
                    // baseline (trust on first use). Reinstalling through the
                    // CLI records an explicit hash instead.
                    let hash = match manifest.content_hash(&path) {
                        Ok(hash) => hash,
                        Err(error) => {
                            scan.warnings.push(format!("{}: {error}", path.display()));
                            continue;
                        }
                    };
                    state.entries.insert(
                        manifest.id.clone(),
                        StateEntry {
                            enabled: true,
                            hash: hash.clone(),
                            installed_at: now_unix(),
                            publisher: None,
                            trusted: false,
                        },
                    );
                    adopted = true;
                    hash
                }
            };
            let entry = state.entries.get(&manifest.id);
            let enabled = entry.map(|entry| entry.enabled).unwrap_or(true);
            let provenance = entry
                .map(|entry| Provenance {
                    publisher: entry.publisher.clone(),
                    trusted: entry.trusted,
                })
                .unwrap_or_default();
            scan.extensions.push(InstalledExtension {
                manifest,
                root: path,
                hash,
                enabled,
                provenance,
            });
        }

        if adopted {
            let document = state.clone();
            drop(state);
            self.save(&document)?;
        }
        scan.extensions
            .sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        Ok(scan)
    }

    /// Returns one installed extension.
    pub fn get(&self, id: &str) -> HostResult<InstalledExtension> {
        self.scan()?
            .extensions
            .into_iter()
            .find(|extension| extension.manifest.id == id)
            .ok_or_else(|| HostError::NotFound(id.to_owned()))
    }

    /// Copies `source` into the install directory.
    ///
    /// The copy goes to a staging directory, is validated, hashed and (when
    /// signed) verified, and only then is renamed into place. A failed install
    /// therefore never leaves a half-populated package behind, and a bad
    /// signature is rejected while the previous install is still intact.
    pub fn install(
        &self,
        source: &Path,
        force: bool,
        policy: &SignaturePolicy<'_>,
    ) -> HostResult<InstalledExtension> {
        let source =
            fs::canonicalize(source).map_err(|e| HostError::io("resolve package directory", e))?;
        let manifest = Manifest::read_dir(&source)?;
        let target = self.extensions_dir.join(&manifest.id);

        if target.exists() && !force {
            return Err(HostError::Rejected(format!(
                "extension '{}' is already installed; reinstall with force to replace it",
                manifest.id
            )));
        }

        let staging = self.extensions_dir.join(format!(
            ".staging-{}-{}-{}",
            manifest.id,
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&staging);
        if let Err(error) = copy_tree(&source, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        let staged = (|| -> HostResult<InstalledExtension> {
            let staged_manifest = Manifest::read_dir(&staging)?;
            if staged_manifest.id != manifest.id {
                return Err(HostError::Manifest(ManifestError::Invalid(
                    "package identity changed during install".into(),
                )));
            }
            let hash = staged_manifest.content_hash(&staging)?;
            // Verified on the staged copy, against the hash of the staged bytes:
            // checking the source and copying afterwards would leave a window in
            // which the two could differ.
            let provenance = check_provenance(&staging, &staged_manifest, &hash, policy)?;
            if target.exists() {
                fs::remove_dir_all(&target)
                    .map_err(|e| HostError::io("remove previous install", e))?;
            }
            fs::rename(&staging, &target).map_err(|e| HostError::io("publish package", e))?;
            let mut state = self.state.lock().expect("registry state lock");
            state.entries.insert(
                manifest.id.clone(),
                StateEntry {
                    enabled: true,
                    hash: hash.clone(),
                    installed_at: now_unix(),
                    publisher: provenance.publisher.clone(),
                    trusted: provenance.trusted,
                },
            );
            let document = state.clone();
            drop(state);
            self.save(&document)?;
            Ok(InstalledExtension {
                manifest: staged_manifest,
                root: target.clone(),
                hash,
                enabled: true,
                provenance,
            })
        })();

        if staged.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        staged
    }

    /// Removes a package directory and its metadata.
    pub fn uninstall(&self, id: &str) -> HostResult<()> {
        let installed = self.get(id)?;
        fs::remove_dir_all(&installed.root)
            .map_err(|e| HostError::io("remove package directory", e))?;
        let mut state = self.state.lock().expect("registry state lock");
        state.entries.remove(id);
        let document = state.clone();
        drop(state);
        self.save(&document)
    }

    /// Enables or disables a package without touching its directory.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> HostResult<InstalledExtension> {
        let installed = self.get(id)?;
        let mut state = self.state.lock().expect("registry state lock");
        let entry = state
            .entries
            .get_mut(id)
            .ok_or_else(|| HostError::NotFound(id.to_owned()))?;
        entry.enabled = enabled;
        let document = state.clone();
        drop(state);
        self.save(&document)?;
        Ok(InstalledExtension {
            enabled,
            ..installed
        })
    }

    /// Path of the install directory.
    pub fn extensions_dir(&self) -> &Path {
        &self.extensions_dir
    }

    fn save(&self, document: &StateDocument) -> HostResult<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent).map_err(|e| HostError::io("create state directory", e))?;
        }
        let body = serde_json::to_vec_pretty(document)
            .map_err(|e| HostError::Io(format!("serialize registry: {e}")))?;
        let temporary = self.state_path.with_extension("json.tmp");
        fs::write(&temporary, body).map_err(|e| HostError::io("write registry", e))?;
        replace(&temporary, &self.state_path).map_err(|e| HostError::io("publish registry", e))
    }
}

/// Verifies a staged package's signature and applies the operator's policy.
///
/// Three cases, in order of severity:
///
/// - A signature that does not verify is always fatal. It means the bytes
///   changed after signing, which no policy can make acceptable.
/// - A missing signature is fatal only when signatures are required.
/// - A valid signature from an untrusted key is recorded, not rejected, unless
///   signatures are required: the operator can then decide to trust that key
///   without reinstalling.
fn check_provenance(
    dir: &Path,
    manifest: &Manifest,
    hash: &str,
    policy: &SignaturePolicy<'_>,
) -> HostResult<Provenance> {
    let signed = verify_package(dir, manifest, hash)
        .map_err(|error| HostError::Rejected(format!("signature check failed: {error}")))?;
    let Some(signed) = signed else {
        if policy.require_signature {
            return Err(HostError::Rejected(format!(
                "extension '{}' is not signed and signatures are required",
                manifest.id
            )));
        }
        return Ok(Provenance::default());
    };
    let trusted = policy.trust.name_of(&signed.public_key).is_some();
    if policy.require_signature && !trusted {
        return Err(HostError::Rejected(format!(
            "extension '{}' is signed by '{}' ({}), which is not a trusted publisher",
            manifest.id,
            signed.publisher,
            &signed.public_key[..16.min(signed.public_key.len())]
        )));
    }
    Ok(Provenance {
        publisher: Some(signed.publisher),
        trusted,
    })
}

/// Recursively copies a package, rejecting links and enforcing size limits.
fn copy_tree(from: &Path, to: &Path) -> HostResult<()> {
    let mut budget = Budget { files: 0, bytes: 0 };
    copy_dir(from, to, &mut budget)
}

struct Budget {
    files: usize,
    bytes: u64,
}

fn copy_dir(from: &Path, to: &Path, budget: &mut Budget) -> HostResult<()> {
    fs::create_dir_all(to).map_err(|e| HostError::io("create package directory", e))?;
    for entry in fs::read_dir(from)
        .map_err(|e| HostError::io("read package directory", e))?
        .flatten()
    {
        let path = entry.path();
        let target = to.join(entry.file_name());
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| HostError::io("inspect package entry", e))?;
        if metadata.file_type().is_symlink() {
            return Err(HostError::Rejected(format!(
                "package contains a symlink: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            copy_dir(&path, &target, budget)?;
        } else if metadata.is_file() {
            budget.files += 1;
            budget.bytes = budget.bytes.saturating_add(metadata.len());
            if budget.files > MAX_FILES {
                return Err(HostError::Rejected(format!(
                    "package exceeds {MAX_FILES} files"
                )));
            }
            if budget.bytes > MAX_BYTES {
                return Err(HostError::Rejected(format!(
                    "package exceeds {} MiB",
                    MAX_BYTES / (1024 * 1024)
                )));
            }
            fs::copy(&path, &target).map_err(|e| HostError::io("copy package file", e))?;
        } else {
            return Err(HostError::Rejected(format!(
                "package contains a non-regular file: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        fs::remove_file(to)?;
    }
    fs::rename(from, to)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Name of the manifest file, re-exported for callers writing templates.
pub const MANIFEST_NAME: &str = MANIFEST_FILE;

#[cfg(test)]
mod tests {
    use super::*;
    use originlang_ext_signing::{KeyFile, sign_package};

    /// Trust store with nobody accepted, shared by tests that do not care.
    fn empty_trust() -> &'static TrustStore {
        static TRUST: std::sync::OnceLock<TrustStore> = std::sync::OnceLock::new();
        TRUST.get_or_init(TrustStore::default)
    }

    /// Signature policy that accepts unsigned packages and trusts nobody.
    fn accept() -> SignaturePolicy<'static> {
        SignaturePolicy::permissive(empty_trust())
    }

    fn workspace(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "oext-registry-{tag}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn package(root: &Path, id: &str, body: &str) -> PathBuf {
        let dir = root.join(format!("src-{id}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(MANIFEST_FILE),
            format!(
                r#"{{"id":"{id}","name":"{id}","version":"0.1.0","apiVersion":"1","main":"main.js","capabilities":["log.write"]}}"#
            ),
        )
        .unwrap();
        fs::write(dir.join("main.js"), body).unwrap();
        dir
    }

    #[test]
    fn install_scan_uninstall_round_trip() {
        let root = workspace("roundtrip");
        let extensions = root.join("extensions");
        let registry =
            Registry::open(extensions.clone(), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({});");

        let installed = registry.install(&source, false, &accept()).unwrap();
        assert_eq!(installed.manifest.id, "hello");
        assert!(extensions.join("hello").join(MANIFEST_FILE).exists());
        assert!(
            registry.install(&source, false, &accept()).is_err(),
            "重复安装应失败，除非显式 force"
        );

        let scan = registry.scan().unwrap();
        assert_eq!(scan.extensions.len(), 1);
        assert!(scan.warnings.is_empty());

        registry.uninstall("hello").unwrap();
        assert!(registry.scan().unwrap().extensions.is_empty());
        assert!(!extensions.join("hello").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn modified_packages_keep_their_install_hash() {
        let root = workspace("hash");
        let registry =
            Registry::open(root.join("extensions"), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({v:1});");
        let installed = registry.install(&source, false, &accept()).unwrap();

        // 安装后原地修改入口文件：记录的哈希不应随之改变，加载方才能发现篡改
        fs::write(installed.root.join("main.js"), "Origin.define({v:2});").unwrap();
        let rescanned = registry.get("hello").unwrap();
        assert_eq!(rescanned.hash, installed.hash);
        let current = rescanned.manifest.content_hash(&rescanned.root).unwrap();
        assert_ne!(current, rescanned.hash);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_state_survives_rescan_and_force_replaces() {
        let root = workspace("enable");
        let registry =
            Registry::open(root.join("extensions"), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({});");
        registry.install(&source, false, &accept()).unwrap();

        registry.set_enabled("hello", false).unwrap();
        assert!(!registry.get("hello").unwrap().enabled);

        fs::write(source.join("main.js"), "Origin.define({v:2});").unwrap();
        let replaced = registry.install(&source, true, &accept()).unwrap();
        assert!(replaced.enabled, "重新安装应恢复为启用状态");
        assert_eq!(
            fs::read_to_string(root.join("extensions/hello/main.js")).unwrap(),
            "Origin.define({v:2});"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_packages_are_rejected() {
        let root = workspace("symlink");
        let registry =
            Registry::open(root.join("extensions"), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({});");
        std::os::unix::fs::symlink("/etc/passwd", source.join("leak")).unwrap();
        assert!(registry.install(&source, false, &accept()).is_err());
        assert!(registry.scan().unwrap().extensions.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manually_copied_packages_are_adopted_on_first_scan() {
        let root = workspace("adopt");
        let extensions = root.join("extensions");
        let registry =
            Registry::open(extensions.clone(), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({});");
        fs::create_dir_all(extensions.join("hello")).unwrap();
        fs::copy(
            source.join(MANIFEST_FILE),
            extensions.join("hello/extension.json"),
        )
        .unwrap();
        fs::copy(source.join("main.js"), extensions.join("hello/main.js")).unwrap();

        let scan = registry.scan().unwrap();
        assert_eq!(scan.extensions.len(), 1);
        assert_eq!(scan.extensions[0].hash.len(), 64);
        assert!(scan.extensions[0].enabled);
        assert_eq!(
            scan.extensions[0].provenance,
            Provenance::default(),
            "手工复制的包没有发布者"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_signed_package_records_its_publisher_and_trust_decision() {
        let root = workspace("signed");
        let registry =
            Registry::open(root.join("extensions"), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({});");
        let key = KeyFile::generate("acme").unwrap();
        sign_package(&source, &key.secret().unwrap(), "acme").unwrap();

        // Untrusted but valid: accepted, flagged, so the operator can decide.
        let installed = registry
            .install(&source, false, &SignaturePolicy::permissive(empty_trust()))
            .unwrap();
        assert_eq!(installed.provenance.publisher.as_deref(), Some("acme"));
        assert!(!installed.provenance.trusted);

        // Same package, now with the key trusted.
        let mut trust = TrustStore::default();
        trust.trust("acme", &key.public_key);
        let installed = registry
            .install(&source, true, &SignaturePolicy::permissive(&trust))
            .unwrap();
        assert!(installed.provenance.trusted);

        // Provenance survives a rescan, which reads the state document.
        let rescanned = registry.get("hello").unwrap();
        assert_eq!(rescanned.provenance.publisher.as_deref(), Some("acme"));
        assert!(rescanned.provenance.trusted);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_signatures_reject_unsigned_and_untrusted_packages() {
        let root = workspace("required");
        let registry =
            Registry::open(root.join("extensions"), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({});");
        let trust = empty_trust();

        let strict = SignaturePolicy {
            require_signature: true,
            trust,
        };
        let error = registry.install(&source, false, &strict).unwrap_err();
        assert!(
            matches!(&error, HostError::Rejected(message) if message.contains("not signed")),
            "未签名应被拒绝，实得 {error:?}"
        );

        let key = KeyFile::generate("acme").unwrap();
        sign_package(&source, &key.secret().unwrap(), "acme").unwrap();
        let error = registry.install(&source, false, &strict).unwrap_err();
        assert!(
            matches!(&error, HostError::Rejected(message) if message.contains("not a trusted publisher")),
            "未受信发布者应被拒绝，实得 {error:?}"
        );
        assert!(
            !root.join("extensions/hello").exists(),
            "被拒绝的安装不应落盘"
        );

        let mut trust = TrustStore::default();
        trust.trust("acme", &key.public_key);
        let accepted = SignaturePolicy {
            require_signature: true,
            trust: &trust,
        };
        assert!(
            registry
                .install(&source, false, &accepted)
                .unwrap()
                .provenance
                .trusted
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_invalid_signature_is_rejected_even_when_signatures_are_optional() {
        let root = workspace("badsig");
        let registry =
            Registry::open(root.join("extensions"), root.join("state/registry.json")).unwrap();
        let source = package(&root, "hello", "Origin.define({});");
        let key = KeyFile::generate("acme").unwrap();
        sign_package(&source, &key.secret().unwrap(), "acme").unwrap();
        // Change the code after signing: the hash no longer matches the payload.
        fs::write(source.join("main.js"), "Origin.define({v:2});").unwrap();

        let error = registry
            .install(&source, false, &SignaturePolicy::permissive(empty_trust()))
            .unwrap_err();
        assert!(matches!(error, HostError::Rejected(_)), "实得 {error:?}");
        fs::remove_dir_all(root).unwrap();
    }
}
