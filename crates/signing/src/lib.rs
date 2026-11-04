//! Ed25519 package signing and a publisher trust store.
//!
//! Why this exists next to the content hash: the hash answers "is this the same
//! bytes I installed?", which stops an in-place edit but says nothing about who
//! produced the bytes. A signature answers "did the holder of this key produce
//! this package for this id and version?", and the trust store turns "some key"
//! into "a publisher the operator accepted".
//!
//! What is signed:
//!
//! ```text
//! oext-signature-v1\n<publisher>\n<id>\n<version>\n<content hash>\n
//! ```
//!
//! The domain line keeps a signature from this scheme out of any other protocol
//! that might sign the same fields. The publisher name is inside the payload
//! rather than outside it: it is shown to the operator, so it must not be
//! editable by anyone who can write `extension.sig`. The content hash already
//! covers `extension.json` and the entry script, so binding the hash binds the
//! manifest — including the capability list — and the code together: a package
//! cannot be re-signed by swapping in a manifest that asks for more.
//!
//! What is *not* covered: additional files in the package directory. Only the
//! manifest and the entry script are hashed, and only they are loaded, so an
//! unsigned asset cannot become code.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use originlang_ext_manifest::Manifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;

/// Signature document written next to the manifest.
pub const SIGNATURE_FILE: &str = "extension.sig";

/// Signature document format version.
pub const SIGNATURE_VERSION: u32 = 1;

/// Domain separator mixed into every signed payload.
const DOMAIN: &str = "oext-signature-v1";

/// Failure modes of this crate.
#[derive(Debug)]
pub enum Error {
    /// Filesystem failure.
    Io(String),
    /// Malformed key, signature or document.
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(message) => write!(f, "{message}"),
            Error::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

fn io(context: &str, error: std::io::Error) -> Error {
    Error::Io(format!("{context}: {error}"))
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// A 32-byte Ed25519 secret seed.
pub struct SecretKey(SigningKey);

impl SecretKey {
    /// Generates a key from the OS CSPRNG.
    pub fn generate() -> Result<Self, Error> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed)
            .map_err(|e| io("getrandom failed", std::io::Error::other(e.to_string())))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// Restores a key from its 32-byte seed.
    pub fn from_bytes(seed: &[u8]) -> Result<Self, Error> {
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| invalid("an Ed25519 secret key is exactly 32 bytes"))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// Raw seed, hex encoded.
    pub fn to_hex(&self) -> String {
        hex(&self.0.to_bytes())
    }

    /// Matching public key.
    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// Signs an arbitrary payload, returning a hex signature.
    pub fn sign(&self, payload: &[u8]) -> String {
        hex(&self.0.sign(payload).to_bytes())
    }
}

/// A 32-byte Ed25519 public key.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// Parses a hex public key.
    pub fn from_hex(text: &str) -> Result<Self, Error> {
        let bytes = unhex(text)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| invalid("an Ed25519 public key is exactly 32 bytes"))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|e| invalid(format!("invalid public key: {e}")))?;
        Ok(Self(key))
    }

    /// Hex encoding, the canonical text form in key files and the trust store.
    pub fn to_hex(&self) -> String {
        hex(&self.0.to_bytes())
    }

    /// Short stable identifier for display; not a security boundary.
    pub fn fingerprint(&self) -> String {
        self.to_hex().chars().take(16).collect()
    }

    /// Verifies a hex signature over `payload`.
    pub fn verify(&self, payload: &[u8], signature_hex: &str) -> Result<(), Error> {
        let bytes = unhex(signature_hex)?;
        let bytes: [u8; 64] = bytes
            .try_into()
            .map_err(|_| invalid("an Ed25519 signature is exactly 64 bytes"))?;
        let signature = Signature::from_bytes(&bytes);
        // `verify_strict` rejects the malleable and small-order edge cases that
        // the permissive variant accepts; there is no reason to allow them here.
        self.0
            .verify_strict(payload, &signature)
            .map_err(|_| invalid("signature does not match the package"))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint())
    }
}

/// `extension.sig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignatureFile {
    /// Document version.
    pub version: u32,
    /// Human-readable publisher name; informational, the key is authoritative.
    pub publisher: String,
    /// Hex public key that produced `signature`.
    pub public_key: String,
    /// Hex signature over the canonical payload.
    pub signature: String,
}

impl SignatureFile {
    /// Reads `extension.sig` from a package directory, if present.
    pub fn read(dir: &Path) -> Result<Option<Self>, Error> {
        let path = dir.join(SIGNATURE_FILE);
        let body = match fs::read(&path) {
            Ok(body) => body,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&format!("read {}", path.display()), e)),
        };
        let document: Self = serde_json::from_slice(&body)
            .map_err(|e| invalid(format!("invalid {}: {e}", path.display())))?;
        if document.version != SIGNATURE_VERSION {
            return Err(invalid(format!(
                "unsupported signature version {} (expected {SIGNATURE_VERSION})",
                document.version
            )));
        }
        Ok(Some(document))
    }

    /// Writes `extension.sig` into a package directory.
    pub fn write(&self, dir: &Path) -> Result<(), Error> {
        let path = dir.join(SIGNATURE_FILE);
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| invalid(format!("serialize signature: {e}")))?;
        fs::write(&path, body).map_err(|e| io(&format!("write {}", path.display()), e))
    }

    /// Parses the embedded public key.
    pub fn key(&self) -> Result<PublicKey, Error> {
        PublicKey::from_hex(&self.public_key)
    }

    /// Verifies this signature against a package.
    pub fn verify(&self, manifest: &Manifest, content_hash: &str) -> Result<(), Error> {
        let payload = payload(manifest, content_hash, &self.publisher);
        self.key()?.verify(&payload, &self.signature)
    }
}

/// The canonical bytes a signature covers.
///
/// The publisher name is included because it is displayed to the operator: a
/// label outside the signature would be free for anyone to rewrite.
pub fn payload(manifest: &Manifest, content_hash: &str, publisher: &str) -> Vec<u8> {
    format!(
        "{DOMAIN}\n{publisher}\n{}\n{}\n{}\n",
        manifest.id, manifest.version, content_hash
    )
    .into_bytes()
}

/// Signs a package directory, writing `extension.sig`.
///
/// The package must already be internally consistent: the hash is computed from
/// the files on disk, so signing a directory that fails validation would produce
/// a signature over something the host will refuse to install.
pub fn sign_package(dir: &Path, key: &SecretKey, publisher: &str) -> Result<SignatureFile, Error> {
    let manifest =
        Manifest::read_dir(dir).map_err(|e| Error::Invalid(format!("cannot read package: {e}")))?;
    let hash = manifest
        .content_hash(dir)
        .map_err(|e| Error::Invalid(format!("cannot hash package: {e}")))?;
    let document = SignatureFile {
        version: SIGNATURE_VERSION,
        publisher: publisher.to_owned(),
        public_key: key.public().to_hex(),
        signature: key.sign(&payload(&manifest, &hash, publisher)),
    };
    document.write(dir)?;
    Ok(document)
}

/// A signed package whose signature verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// Publisher name taken from the signature document.
    pub publisher: String,
    /// Key that produced the signature, hex encoded.
    pub public_key: String,
}

/// Verifies the signature of an installed-or-staged package.
///
/// Returns `Ok(None)` when the package carries no signature: absence is not an
/// error here, because the policy that decides whether absence is acceptable
/// belongs to the caller, not to the parser.
pub fn verify_package(
    dir: &Path,
    manifest: &Manifest,
    content_hash: &str,
) -> Result<Option<Signed>, Error> {
    let Some(document) = SignatureFile::read(dir)? else {
        return Ok(None);
    };
    // Verify before reporting anything the document claims: a forged document
    // must not be able to name a publisher, even in an error message.
    document.verify(manifest, content_hash)?;
    Ok(Some(Signed {
        publisher: document.publisher,
        public_key: document.public_key,
    }))
}

/// `key.json` written by `oext keygen`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeyFile {
    /// Document version.
    pub version: u32,
    /// Publisher name copied into signatures.
    pub publisher: String,
    /// Hex public key.
    pub public_key: String,
    /// Hex secret seed. Never copy this into a package.
    pub secret_key: String,
}

impl KeyFile {
    /// Builds a key file around a freshly generated key.
    pub fn generate(publisher: &str) -> Result<Self, Error> {
        let key = SecretKey::generate()?;
        Ok(Self {
            version: 1,
            publisher: publisher.to_owned(),
            public_key: key.public().to_hex(),
            secret_key: key.to_hex(),
        })
    }

    /// Reads a key file.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let body = fs::read(path).map_err(|e| io(&format!("read {}", path.display()), e))?;
        let file: Self = serde_json::from_slice(&body)
            .map_err(|e| invalid(format!("invalid {}: {e}", path.display())))?;
        if file.version != 1 {
            return Err(invalid(format!(
                "unsupported key file version {}",
                file.version
            )));
        }
        Ok(file)
    }

    /// Writes a key file, restricting permissions where the OS allows it.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| invalid(format!("serialize key file: {e}")))?;
        fs::write(path, body).map_err(|e| io(&format!("write {}", path.display()), e))?;
        restrict(path)?;
        Ok(())
    }

    /// Reconstructs the signing key.
    pub fn secret(&self) -> Result<SecretKey, Error> {
        SecretKey::from_bytes(&unhex(&self.secret_key)?)
    }
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| io(&format!("restrict {}", path.display()), e))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), Error> {
    Ok(())
}

/// One accepted publisher.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustEntry {
    /// Operator-facing name.
    pub name: String,
    /// Hex public key.
    pub public_key: String,
}

/// `state/publishers.json`: keys the operator has accepted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustStore {
    /// Document version.
    pub version: u32,
    /// Accepted publishers keyed by name.
    #[serde(default)]
    pub publishers: BTreeMap<String, String>,
}

impl Default for TrustStore {
    fn default() -> Self {
        Self {
            version: 1,
            publishers: BTreeMap::new(),
        }
    }
}

impl TrustStore {
    /// Reads the store, treating a missing file as an empty trust set.
    pub fn load(path: &Path) -> Result<Self, Error> {
        match fs::read(path) {
            Ok(body) => {
                let store: Self = serde_json::from_slice(&body)
                    .map_err(|e| invalid(format!("invalid {}: {e}", path.display())))?;
                if store.version != 1 {
                    return Err(invalid(format!(
                        "unsupported trust store version {}",
                        store.version
                    )));
                }
                Ok(store)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(io(&format!("read {}", path.display()), e)),
        }
    }

    /// Writes the store atomically.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| io(&format!("create {}", parent.display()), e))?;
        }
        let body =
            serde_json::to_vec_pretty(self).map_err(|e| invalid(format!("serialize: {e}")))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, body)
            .map_err(|e| io(&format!("write {}", temporary.display()), e))?;
        fs::rename(&temporary, path).map_err(|e| io(&format!("publish {}", path.display()), e))
    }

    /// Accepts a publisher, replacing any entry with the same name.
    pub fn trust(&mut self, name: &str, public_key: &str) {
        self.publishers
            .insert(name.to_owned(), public_key.to_lowercase());
    }

    /// Forgets a publisher by name; returns whether it was present.
    pub fn untrust(&mut self, name: &str) -> bool {
        self.publishers.remove(name).is_some()
    }

    /// Name under which `public_key` is accepted, if any.
    pub fn name_of(&self, public_key: &str) -> Option<&str> {
        let wanted = public_key.to_lowercase();
        self.publishers
            .iter()
            .find(|(_, key)| **key == wanted)
            .map(|(name, _)| name.as_str())
    }

    /// Entries in name order, which is also the order they were written.
    pub fn entries(&self) -> Vec<TrustEntry> {
        self.publishers
            .iter()
            .map(|(name, public_key)| TrustEntry {
                name: name.clone(),
                public_key: public_key.clone(),
            })
            .collect()
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn unhex(text: &str) -> Result<Vec<u8>, Error> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return Err(invalid("hex text has an odd length"));
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let raw = text.as_bytes();
    for pair in raw.chunks(2) {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| invalid("hex text contains a non-hex character"))?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| invalid("hex text contains a non-hex character"))?;
        bytes.push((high * 16 + low) as u8);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn package(tag: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oext-signing-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("extension.json"),
            r#"{"id":"demo","name":"Demo","version":"0.1.0","apiVersion":"1","main":"main.js","capabilities":["log.write"]}"#,
        )
        .unwrap();
        fs::write(dir.join("main.js"), body).unwrap();
        dir
    }

    #[test]
    fn signatures_verify_and_bind_identity_version_and_content() {
        let dir = package("roundtrip", "Origin.define({id:'demo',version:'0.1.0'});");
        let key = SecretKey::generate().unwrap();
        let document = sign_package(&dir, &key, "acme").unwrap();
        assert_eq!(document.public_key, key.public().to_hex());

        let manifest = Manifest::read_dir(&dir).unwrap();
        let hash = manifest.content_hash(&dir).unwrap();
        let signed = verify_package(&dir, &manifest, &hash).unwrap().unwrap();
        assert_eq!(signed.publisher, "acme");
        assert_eq!(signed.public_key, key.public().to_hex());

        // A different version or a different body must not verify: the payload
        // covers both, so a signature cannot be replayed onto another release.
        let mut other = manifest.clone();
        other.version = "9.9.9".into();
        assert!(verify_package(&dir, &other, &hash).is_err());
        assert!(verify_package(&dir, &manifest, &"0".repeat(64)).is_err());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_tampered_entry_script_invalidates_the_signature() {
        let dir = package("tamper", "Origin.define({id:'demo',version:'0.1.0'});");
        let key = SecretKey::generate().unwrap();
        sign_package(&dir, &key, "acme").unwrap();

        fs::write(
            dir.join("main.js"),
            "Origin.define({id:'demo',version:'0.1.0'}); // edited",
        )
        .unwrap();
        let manifest = Manifest::read_dir(&dir).unwrap();
        let hash = manifest.content_hash(&dir).unwrap();
        let error = verify_package(&dir, &manifest, &hash).unwrap_err();
        assert!(
            error.to_string().contains("signature does not match"),
            "{error}"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_forged_document_cannot_name_a_publisher() {
        let dir = package("forge", "Origin.define({id:'demo',version:'0.1.0'});");
        let key = SecretKey::generate().unwrap();
        let mut document = sign_package(&dir, &key, "acme").unwrap();
        // Keep the signature, claim to be someone else: verification must fail
        // before the claimed name is reported anywhere.
        document.publisher = "trusted-inc".into();
        document.write(&dir).unwrap();

        let manifest = Manifest::read_dir(&dir).unwrap();
        let hash = manifest.content_hash(&dir).unwrap();
        assert!(verify_package(&dir, &manifest, &hash).is_err());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unsigned_packages_report_no_signature() {
        let dir = package("unsigned", "Origin.define({id:'demo',version:'0.1.0'});");
        let manifest = Manifest::read_dir(&dir).unwrap();
        let hash = manifest.content_hash(&dir).unwrap();
        assert!(verify_package(&dir, &manifest, &hash).unwrap().is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trust_store_round_trips_and_matches_by_key() {
        let dir = package("trust", "Origin.define({id:'demo',version:'0.1.0'});");
        let path = dir.join("state/publishers.json");
        let key = SecretKey::generate().unwrap();
        let mut store = TrustStore::load(&path).unwrap();
        assert!(store.entries().is_empty());

        store.trust("acme", &key.public().to_hex());
        store.save(&path).unwrap();

        let reloaded = TrustStore::load(&path).unwrap();
        assert_eq!(
            reloaded.name_of(&key.public().to_hex()),
            Some("acme"),
            "应按公钥而非名字匹配"
        );
        assert_eq!(reloaded.name_of(&"0".repeat(64)), None);

        let mut store = reloaded;
        assert!(store.untrust("acme"));
        assert!(!store.untrust("acme"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn key_files_round_trip_and_keep_the_seed_out_of_the_signature() {
        let dir = package("keyfile", "Origin.define({id:'demo',version:'0.1.0'});");
        let path = dir.join("key.json");
        let file = KeyFile::generate("acme").unwrap();
        file.save(&path).unwrap();

        let reloaded = KeyFile::load(&path).unwrap();
        assert_eq!(reloaded.public_key, file.public_key);
        let key = reloaded.secret().unwrap();
        assert_eq!(key.public().to_hex(), file.public_key);

        let document = sign_package(&dir, &key, "acme").unwrap();
        let body = fs::read_to_string(dir.join(SIGNATURE_FILE)).unwrap();
        assert!(!body.contains(&file.secret_key), "签名文件不得包含私钥");
        assert_eq!(document.public_key, file.public_key);
        fs::remove_dir_all(dir).unwrap();
    }
}
