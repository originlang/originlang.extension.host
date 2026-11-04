//! End-to-end coverage for package signatures and the publisher trust store.

use originlang_ext_host::{Host, HostConfig, HostError, Paths};
use originlang_ext_runtime_quickjs::QuickJsProvider;
use originlang_ext_signing::{KeyFile, sign_package};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "oext-sign-it-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn host(&self, require_signature: bool) -> Host {
        Host::new(
            HostConfig::new(Paths::new(&self.root))
                .with_startup_load(false)
                .with_require_signature(require_signature),
            Arc::new(QuickJsProvider::new()),
        )
        .expect("host 应能启动")
    }

    /// Copies an example so a test can sign it without touching the repo.
    fn package(&self, name: &str) -> PathBuf {
        self.package_example(name)
    }

    /// Same, under an explicit name that reads better at call sites.
    fn package_example(&self, name: &str) -> PathBuf {
        let source = example(name);
        let target = self.root.join("pkgs").join(name);
        fs::create_dir_all(&target).unwrap();
        for entry in fs::read_dir(&source).unwrap().flatten() {
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
        target
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_signed_package_records_its_publisher_and_becomes_trusted_on_request() {
    let fixture = Fixture::new("trust");
    let host = fixture.host(false);
    let package = fixture.package("hello");
    let key = KeyFile::generate("acme").unwrap();
    sign_package(&package, &key.secret().unwrap(), "acme").unwrap();

    let installed = host.install(&package, false).unwrap();
    assert_eq!(installed.publisher.as_deref(), Some("acme"));
    assert!(!installed.trusted, "未入信任库的密钥不应被视为受信");

    // Trusting the key changes the decision for the next install, and the
    // operator can see the public key before accepting it.
    assert!(host.trusted_publishers().is_empty());
    host.trust_publisher("acme", &key.public_key).unwrap();
    assert_eq!(host.trusted_publishers().len(), 1);

    let reinstalled = host.install(&package, true).unwrap();
    assert!(reinstalled.trusted);
    host.shutdown();
}

#[test]
fn required_signatures_gate_install() {
    let fixture = Fixture::new("required");
    let package = fixture.package("hello");
    let key = KeyFile::generate("acme").unwrap();

    // Signed by a key nobody trusts: rejected, and nothing lands on disk.
    // The host is dropped before the next one opens the same directory, because
    // the storage container is exclusively locked per process.
    sign_package(&package, &key.secret().unwrap(), "acme").unwrap();
    {
        let host = fixture.host(true);
        let error = host.install(&package, false).unwrap_err();
        assert!(
            matches!(&error, HostError::Rejected(message) if message.contains("not a trusted publisher")),
            "实得 {error:?}"
        );
        assert!(!host.paths().extensions_dir().join("hello").exists());
        host.shutdown();
    }

    // Same package, trusted publisher: accepted. A fresh host reads the trust
    // store from disk, which is what a restart would do.
    let host = fixture.host(true);
    host.trust_publisher("acme", &key.public_key).unwrap();
    assert!(host.install(&package, false).unwrap().trusted);

    // Unsigned packages are refused outright under the same policy.
    let unsigned = fixture.package_example("notes");
    let error = host.install(&unsigned, false).unwrap_err();
    assert!(
        matches!(&error, HostError::Rejected(message) if message.contains("not signed")),
        "实得 {error:?}"
    );
    host.shutdown();
}

#[test]
fn editing_code_after_signing_breaks_the_install() {
    let fixture = Fixture::new("tamper");
    let host = fixture.host(false);
    let package = fixture.package("hello");
    let key = KeyFile::generate("acme").unwrap();
    sign_package(&package, &key.secret().unwrap(), "acme").unwrap();

    let entry = package.join("main.js");
    let mut body = fs::read_to_string(&entry).unwrap();
    body.push_str("\n// edited after signing\n");
    fs::write(&entry, body).unwrap();

    let error = host.install(&package, false).unwrap_err();
    assert!(
        matches!(&error, HostError::Rejected(message) if message.contains("signature")),
        "签名与内容不符必须拒绝，实得 {error:?}"
    );
    assert!(!host.paths().extensions_dir().join("hello").exists());
    host.shutdown();
}

#[test]
fn an_untrusted_publisher_is_reported_but_usable_when_not_required() {
    let fixture = Fixture::new("untrusted");
    let host = fixture.host(false);
    let package = fixture.package("hello");
    let key = KeyFile::generate("acme").unwrap();
    sign_package(&package, &key.secret().unwrap(), "acme").unwrap();

    host.install(&package, false).unwrap();
    host.grant("hello", &["log.write".to_owned()]).unwrap();
    // Installing an untrusted package is allowed by default; running it is a
    // separate decision (capabilities), and both are visible in the summary.
    let value = host
        .call(
            "hello",
            "greet",
            Some(serde_json::json!({"name": "Signed"})),
        )
        .unwrap();
    assert_eq!(value["message"], "Hello, Signed");
    let summary = &host.summaries().unwrap()[0];
    assert_eq!(summary.publisher.as_deref(), Some("acme"));
    assert!(!summary.trusted);
    host.shutdown();
}
