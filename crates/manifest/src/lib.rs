//! Extension manifest (`extension.json`): schema, validation, content hash.
//!
//! Validation is intentionally strict and happens before any code is loaded:
//! a manifest is the only place where declared capabilities and the entry point
//! are stated, so a malformed manifest must fail at install time, not at the
//! first call.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Manifest file name inside an extension directory.
pub const MANIFEST_FILE: &str = "extension.json";

/// Host API version understood by this build.
pub const API_VERSION: &str = "1";

/// Backend used when a manifest does not declare one.
///
/// QuickJS was the first backend and remains the form every package written
/// before `runtime` existed was authored in, so it is the only safe default.
pub const DEFAULT_RUNTIME: &str = "quickjs";

/// Runtime resource limits, taken from the manifest but clamped by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// JS heap ceiling in bytes.
    pub memory_bytes: usize,
    /// JS stack ceiling in bytes.
    pub stack_bytes: usize,
    /// Wall-clock ceiling for a single call, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 64 * 1024 * 1024,
            stack_bytes: 512 * 1024,
            timeout_ms: 10_000,
        }
    }
}

/// Accepted range for one limit, applied after deserialization.
const MEMORY_RANGE: (usize, usize) = (1024 * 1024, 512 * 1024 * 1024);
const STACK_RANGE: (usize, usize) = (64 * 1024, 8 * 1024 * 1024);
const TIMEOUT_RANGE: (u64, u64) = (100, 600_000);

impl Limits {
    /// Clamps every field into the supported range.
    ///
    /// Clamping rather than rejecting is deliberate: a manifest asking for more
    /// memory than the host allows should still load, at the host's ceiling.
    fn clamped(self) -> Self {
        Self {
            memory_bytes: self.memory_bytes.clamp(MEMORY_RANGE.0, MEMORY_RANGE.1),
            stack_bytes: self.stack_bytes.clamp(STACK_RANGE.0, STACK_RANGE.1),
            timeout_ms: self.timeout_ms.clamp(TIMEOUT_RANGE.0, TIMEOUT_RANGE.1),
        }
    }
}

/// Parsed manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    /// Stable identity; also the install directory and storage namespace.
    pub id: String,
    /// Human readable name.
    pub name: String,
    /// Semantic version of the extension itself.
    pub version: String,
    /// Manifest/host API version, currently `"1"`.
    pub api_version: String,
    /// Relative path of the entry module.
    pub main: String,
    /// Backend that executes `main`. Omitted means [`DEFAULT_RUNTIME`].
    ///
    /// Resolved against the providers the host was built with, so an unknown
    /// value fails at load time with the registered list rather than at
    /// install time: a host that gains a backend later can run packages that
    /// were installed before it could.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Optional one-line description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Capabilities the extension needs; every one must be granted to load.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Activation triggers, see [`Manifest::activation_topics`].
    #[serde(default)]
    pub activation_events: Vec<String>,
    /// Resource limits requested by the author.
    #[serde(default)]
    pub limits: Limits,
    /// Free-form contribution points forwarded to the client as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contributes: Option<serde_json::Value>,
}

/// Manifest problems that must abort install or load.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The directory or entry file could not be read.
    #[error("{0}")]
    Io(String),
    /// The JSON could not be parsed or had unknown fields.
    #[error("invalid {MANIFEST_FILE}: {0}")]
    Parse(String),
    /// A field failed validation.
    #[error("invalid manifest: {0}")]
    Invalid(String),
}

impl Manifest {
    /// Parses and validates manifest text.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest =
            serde_json::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reads and validates `<dir>/extension.json`.
    pub fn read_dir(dir: &Path) -> Result<Self, ManifestError> {
        let path = dir.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path)
            .map_err(|e| ManifestError::Io(format!("read {}: {e}", path.display())))?;
        manifest_with_dir(text.as_str(), dir)
    }

    /// Schema and value validation.
    pub fn validate(&self) -> Result<(), ManifestError> {
        originlang_ext_permission::validate_identity(&self.id).map_err(ManifestError::Invalid)?;

        if self.name.trim().is_empty() || self.name.chars().count() > 128 {
            return Err(ManifestError::Invalid(
                "name must be 1..=128 characters".into(),
            ));
        }
        if !is_semver(&self.version) {
            return Err(ManifestError::Invalid(format!(
                "version '{}' is not major.minor.patch",
                self.version
            )));
        }
        if self.api_version != API_VERSION {
            return Err(ManifestError::Invalid(format!(
                "apiVersion '{}' is not supported (expected {API_VERSION})",
                self.api_version
            )));
        }
        validate_runtime(self.runtime_kind())?;
        validate_main(&self.main, self.runtime_kind())?;

        let mut seen = BTreeSet::new();
        for capability in &self.capabilities {
            originlang_ext_permission::validate_capability(capability)
                .map_err(ManifestError::Invalid)?;
            if !seen.insert(capability) {
                return Err(ManifestError::Invalid(format!(
                    "duplicate capability '{capability}'"
                )));
            }
        }
        for event in &self.activation_events {
            validate_activation(event)?;
        }
        if let Some(description) = &self.description
            && description.chars().count() > 512
        {
            return Err(ManifestError::Invalid(
                "description must be at most 512 characters".into(),
            ));
        }
        Ok(())
    }

    /// Limits after clamping into the host-supported range.
    pub fn effective_limits(&self) -> Limits {
        self.limits.clamped()
    }

    /// Backend this extension asks for, defaulting to [`DEFAULT_RUNTIME`].
    pub fn runtime_kind(&self) -> &str {
        self.runtime.as_deref().unwrap_or(DEFAULT_RUNTIME)
    }

    /// True when the extension should be loaded at host startup.
    pub fn activates_on_startup(&self) -> bool {
        self.activation_events.iter().any(|e| e == "onStartup")
    }

    /// Topics this extension subscribes to via activation events.
    pub fn activation_topics(&self) -> Vec<&str> {
        self.activation_events
            .iter()
            .filter_map(|event| event.strip_prefix("onEvent:"))
            .collect()
    }

    /// Resolves the entry script, guaranteeing it stays inside `root`.
    pub fn entry_path(&self, root: &Path) -> Result<PathBuf, ManifestError> {
        let root = fs::canonicalize(root)
            .map_err(|e| ManifestError::Io(format!("resolve {}: {e}", root.display())))?;
        let entry = root.join(&self.main);
        let entry = fs::canonicalize(&entry)
            .map_err(|e| ManifestError::Io(format!("resolve entry {}: {e}", entry.display())))?;
        if !entry.starts_with(&root) {
            return Err(ManifestError::Invalid(
                "main must resolve inside the extension directory".into(),
            ));
        }
        if !entry.is_file() {
            return Err(ManifestError::Invalid("main is not a file".into()));
        }
        Ok(entry)
    }

    /// SHA-256 over the manifest bytes and the entry script.
    ///
    /// The digest is recorded at install time and re-checked before every load,
    /// so editing files in place after install is detected.
    pub fn content_hash(&self, root: &Path) -> Result<String, ManifestError> {
        let manifest_bytes = fs::read(root.join(MANIFEST_FILE))
            .map_err(|e| ManifestError::Io(format!("read manifest: {e}")))?;
        let entry = self.entry_path(root)?;
        let entry_bytes =
            fs::read(&entry).map_err(|e| ManifestError::Io(format!("read entry: {e}")))?;
        Ok(digest(&[&manifest_bytes, &entry_bytes]))
    }
}

/// Validates a parsed manifest against the directory it lives in.
///
/// Separate from [`Manifest::read_dir`] so tests and the installer can validate
/// text and directory together without re-reading the file.
pub fn manifest_with_dir(text: &str, dir: &Path) -> Result<Manifest, ManifestError> {
    let manifest = Manifest::parse(text)?;
    manifest.entry_path(dir)?;
    Ok(manifest)
}

fn digest(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// Rejects malformed backend identifiers.
///
/// The value is not checked against a fixed list on purpose: the set of
/// backends belongs to the host binary, which may be built with a subset. A
/// typo still surfaces early, because loading reports the backends that *are*
/// registered.
fn validate_runtime(runtime: &str) -> Result<(), ManifestError> {
    if runtime.is_empty()
        || runtime.len() > 32
        || !runtime
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ManifestError::Invalid(
            "runtime must be a lowercase identifier such as 'quickjs' or 'dylib'".into(),
        ));
    }
    Ok(())
}

fn validate_main(main: &str, runtime: &str) -> Result<(), ManifestError> {
    let path = Path::new(main);
    let unsafe_component = path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)));
    if main.is_empty() || path.is_absolute() || unsafe_component {
        return Err(ManifestError::Invalid(
            "main must be a relative path without '.' or '..' components".into(),
        ));
    }
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    // The entry extension is what the backend can actually open, so it follows
    // the backend rather than a single hard-coded list. An unrecognised runtime
    // falls through to the QuickJS set: it will fail at load with a clearer
    // message than "wrong file extension" would give.
    let allowed: &[&str] = match runtime {
        "dylib" => &["dll", "so", "dylib"],
        "wasm" => &["wasm"],
        _ => &["js", "mjs"],
    };
    if !allowed.contains(&extension) {
        return Err(ManifestError::Invalid(format!(
            "main must be one of {} for the '{runtime}' runtime",
            allowed.join(", ")
        )));
    }
    Ok(())
}

fn validate_activation(event: &str) -> Result<(), ManifestError> {
    if event == "onStartup" {
        return Ok(());
    }
    if let Some(topic) = event.strip_prefix("onEvent:")
        && !topic.is_empty()
        && topic.len() <= 128
        && topic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
    {
        return Ok(());
    }
    Err(ManifestError::Invalid(format!(
        "unsupported activation event '{event}' (expected onStartup or onEvent:<topic>)"
    )))
}

/// Lenient semantic version check: `major.minor.patch` with optional
/// pre-release/build suffixes. Range semantics are out of scope for v1.
fn is_semver(version: &str) -> bool {
    let core = version
        .split(['-', '+'])
        .next()
        .unwrap_or_default()
        .to_owned();
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 9
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
        })
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        r#"{
            "id": "hello",
            "name": "Hello",
            "version": "0.1.0",
            "apiVersion": "1",
            "main": "main.js",
            "capabilities": ["log.write", "storage.read"],
            "activationEvents": ["onStartup", "onEvent:workspace.changed"]
        }"#
    }

    #[test]
    fn parses_and_exposes_activation_metadata() {
        let manifest = Manifest::parse(sample()).unwrap();
        assert!(manifest.activates_on_startup());
        assert_eq!(manifest.activation_topics(), vec!["workspace.changed"]);
        assert_eq!(manifest.effective_limits(), Limits::default());
    }

    #[test]
    fn rejects_unknown_fields_and_bad_versions() {
        let unknown = sample().replace("\"main\"", "\"entry\": \"main.js\", \"main\"");
        assert!(matches!(
            Manifest::parse(&unknown),
            Err(ManifestError::Parse(_))
        ));

        let bad_version = sample().replace("\"0.1.0\"", "\"1.0\"");
        assert!(matches!(
            Manifest::parse(&bad_version),
            Err(ManifestError::Invalid(_))
        ));
        let leading_zero = sample().replace("\"0.1.0\"", "\"01.2.3\"");
        assert!(Manifest::parse(&leading_zero).is_err());
    }

    #[test]
    fn rejects_path_traversal_and_unsupported_api() {
        let traversal = sample().replace("\"main\": \"main.js\"", "\"main\": \"../evil.js\"");
        assert!(Manifest::parse(&traversal).is_err());

        let absolute = sample().replace("\"main\": \"main.js\"", "\"main\": \"/tmp/evil.js\"");
        assert!(Manifest::parse(&absolute).is_err());

        let python = sample().replace("\"main\": \"main.js\"", "\"main\": \"main.py\"");
        assert!(Manifest::parse(&python).is_err());

        let api = sample().replace("\"apiVersion\": \"1\"", "\"apiVersion\": \"2\"");
        assert!(Manifest::parse(&api).is_err());
    }

    #[test]
    fn rejects_duplicate_and_malformed_capabilities() {
        let duplicate = sample().replace(
            "[\"log.write\", \"storage.read\"]",
            "[\"log.write\", \"log.write\"]",
        );
        assert!(Manifest::parse(&duplicate).is_err());

        let wildcard = sample().replace("\"log.write\"", "\"log.*\"");
        assert!(Manifest::parse(&wildcard).is_err());

        let bad_event = sample().replace("onEvent:workspace.changed", "onFile");
        assert!(Manifest::parse(&bad_event).is_err());
    }

    #[test]
    fn limits_are_clamped_not_rejected() {
        let text = sample().replace(
            "\"activationEvents\"",
            "\"limits\": {\"memoryBytes\": 999999999999, \"stackBytes\": 1, \"timeoutMs\": 0},\n\"activationEvents\"",
        );
        let manifest = Manifest::parse(&text).unwrap();
        let limits = manifest.effective_limits();
        assert_eq!(limits.memory_bytes, MEMORY_RANGE.1);
        assert_eq!(limits.stack_bytes, STACK_RANGE.0);
        assert_eq!(limits.timeout_ms, TIMEOUT_RANGE.0);
    }

    #[test]
    fn hash_covers_manifest_and_entry_and_entry_must_stay_inside_root() {
        let root = std::env::temp_dir().join(format!("oext-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(MANIFEST_FILE), sample()).unwrap();
        fs::write(root.join("main.js"), "Origin.define({});").unwrap();

        let manifest = Manifest::read_dir(&root).unwrap();
        let before = manifest.content_hash(&root).unwrap();
        assert_eq!(before.len(), 64);

        fs::write(root.join("main.js"), "Origin.define({changed:true});").unwrap();
        assert_ne!(before, manifest.content_hash(&root).unwrap());

        // main 指向目录外（软链或篡改）时必须被拒绝
        fs::write(
            root.join(MANIFEST_FILE),
            sample().replace("main.js", "sub/../main.js"),
        )
        .unwrap();
        assert!(Manifest::read_dir(&root).is_err());

        fs::remove_dir_all(&root).unwrap();
    }
}
