//! Management commands and the development runner.

use crate::{ExtCommand, TrustCommand};
use anyhow::{Context, Result, bail};
use originlang_ext_host::{Host, HostConfig, Paths};
use originlang_ext_manifest::Manifest;
use originlang_ext_permission::validate_identity;
use originlang_ext_signing::{KeyFile, sign_package};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// Executes one `oext ext ...` subcommand.
pub fn ext(host: &Host, command: ExtCommand) -> Result<()> {
    match command {
        ExtCommand::List => list(host),
        ExtCommand::Info { id } => info(host, &id),
        ExtCommand::Install { path, force, grant } => install(host, &path, force, &grant),
        ExtCommand::Uninstall { id } => {
            host.uninstall(&id)?;
            println!("uninstalled {id}");
            Ok(())
        }
        ExtCommand::Enable { id } => {
            let summary = host.set_enabled(&id, true)?;
            println!("enabled {} v{}", summary.id, summary.version);
            Ok(())
        }
        ExtCommand::Disable { id } => {
            let summary = host.set_enabled(&id, false)?;
            println!("disabled {} v{}", summary.id, summary.version);
            Ok(())
        }
        ExtCommand::Grant { id, capabilities } => {
            let summary = find(host, &id)?;
            ensure_declared(&summary.capabilities, &capabilities)?;
            let granted = host.grant(&id, &capabilities)?;
            println!("{}: {}", id, granted.join(", "));
            Ok(())
        }
        ExtCommand::Revoke { id, capability } => {
            let granted = host.revoke(&id, &capability)?;
            println!(
                "{}: {}",
                id,
                if granted.is_empty() {
                    "-".to_owned()
                } else {
                    granted.join(", ")
                }
            );
            Ok(())
        }
        ExtCommand::Logs { id, limit } => {
            let records = host.logs(id.as_deref(), limit);
            if records.is_empty() {
                println!("no log records");
            }
            for record in records {
                println!(
                    "{:>13} {:<5} {:<16} {}",
                    record.at_ms,
                    record.level,
                    record.extension.as_deref().unwrap_or("-"),
                    record.message
                );
            }
            Ok(())
        }
        ExtCommand::Init { id, dir, name } => init(&id, dir, name),
    }
}

fn list(host: &Host) -> Result<()> {
    let summaries = host.summaries()?;
    if summaries.is_empty() {
        println!(
            "no extensions installed in {}",
            host.paths().extensions_dir().display()
        );
        return Ok(());
    }
    println!(
        "{:<24} {:<10} {:<9} {:<7} {:<20} CAPABILITIES",
        "ID", "VERSION", "STATE", "LOADED", "SIGNED"
    );
    for summary in summaries {
        // A trailing `*` marks a publisher whose key is in the trust store, so a
        // bare name is visibly "signed, but by someone unverified".
        let signed = match (&summary.publisher, summary.trusted) {
            (Some(publisher), true) => format!("{publisher} *"),
            (Some(publisher), false) => publisher.clone(),
            (None, _) => "-".to_owned(),
        };
        println!(
            "{:<24} {:<10} {:<9} {:<7} {:<20} {}",
            summary.id,
            summary.version,
            if summary.enabled {
                "enabled"
            } else {
                "disabled"
            },
            if summary.loaded { "yes" } else { "no" },
            signed,
            if summary.capabilities.is_empty() {
                "-".to_owned()
            } else {
                summary.capabilities.join(", ")
            }
        );
    }
    Ok(())
}

fn info(host: &Host, id: &str) -> Result<()> {
    let summary = find(host, id)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn install(host: &Host, path: &Path, force: bool, grant: &[String]) -> Result<()> {
    let installed = host.install(path, force)?;
    if !grant.is_empty() {
        ensure_declared(&installed.capabilities, grant)?;
        host.grant(&installed.id, grant)?;
    }
    // Re-read after granting so the report reflects the stored policy rather
    // than the snapshot taken at install time.
    let summary = find(host, &installed.id)?;
    println!(
        "installed {} v{} into {}",
        summary.id,
        summary.version,
        host.paths().extensions_dir().join(&summary.id).display()
    );
    match (&summary.publisher, summary.trusted) {
        (Some(publisher), true) => println!("signed by: {publisher} (trusted)"),
        (Some(publisher), false) => println!(
            "signed by: {publisher} (untrusted key; `oext trust add` to accept the publisher)"
        ),
        (None, _) => println!("unsigned: no signature was found in the package"),
    }
    if !summary.granted.is_empty() {
        println!("granted: {}", summary.granted.join(", "));
    } else if !summary.capabilities.is_empty() {
        println!(
            "not granted yet: {} (use `oext ext grant {} ...`)",
            summary.capabilities.join(", "),
            summary.id
        );
    }
    Ok(())
}

fn find(host: &Host, id: &str) -> Result<originlang_ext_host::ExtensionSummary> {
    host.summaries()?
        .into_iter()
        .find(|summary| summary.id == id)
        .with_context(|| format!("extension '{id}' is not installed"))
}

/// Rejects grants for capabilities the manifest does not declare.
///
/// Granting an undeclared capability would be dead weight at best; at worst it
/// hides the fact that the package needs a manifest update.
fn ensure_declared(declared: &[String], requested: &[String]) -> Result<()> {
    for capability in requested {
        if !declared.iter().any(|candidate| candidate == capability) {
            bail!(
                "capability '{capability}' is not declared by the manifest (declared: {})",
                if declared.is_empty() {
                    "none".to_owned()
                } else {
                    declared.join(", ")
                }
            );
        }
    }
    Ok(())
}

/// Runs one method of a local package inside a throwaway installation.
pub fn run(
    _paths: Paths,
    dir: &Path,
    method: &str,
    params: &str,
    grant_all: bool,
    backend: crate::Backend,
    require_signature: bool,
) -> Result<()> {
    let params: Value = serde_json::from_str(params).context("--params must be valid JSON")?;
    let source = fs::canonicalize(dir)
        .with_context(|| format!("cannot resolve package directory {}", dir.display()))?;

    let scratch = ScratchHome::new()?;
    let host = Host::new(
        HostConfig::new(Paths::new(scratch.path()))
            .with_startup_load(false)
            .with_require_signature(require_signature),
        crate::provider(backend)?,
    )?;

    let summary = host.install(&source, false)?;
    if grant_all {
        host.grant(&summary.id, &summary.capabilities)?;
    }
    let result = host.call(&summary.id, method, Some(params));
    host.shutdown();
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

/// Writes a key file the operator can sign packages with.
pub fn keygen(out: &Path, publisher: &str) -> Result<()> {
    if out.exists() {
        bail!(
            "{} already exists; refusing to overwrite a signing key",
            out.display()
        );
    }
    let file = KeyFile::generate(publisher).map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    file.save(out).map_err(|error| anyhow::anyhow!("{error}"))?;
    println!("publisher:  {}", file.publisher);
    println!("public key: {}", file.public_key);
    println!("key file:   {}", out.display());
    println!();
    println!(
        "Trust this key on this host:  oext trust add {} --name {}",
        file.public_key, file.publisher
    );
    println!("Keep the key file private; it is the package signing credential.");
    Ok(())
}

/// Signs a package directory.
pub fn sign(dir: &Path, key_path: &Path, publisher: Option<&str>) -> Result<()> {
    let key_file = KeyFile::load(key_path)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", key_path.display()))?;
    let publisher = publisher.unwrap_or(&key_file.publisher);
    let key = key_file
        .secret()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let document = sign_package(dir, &key, publisher)
        .map_err(|error| anyhow::anyhow!("cannot sign {}: {error}", dir.display()))?;
    println!(
        "signed {} as '{}' ({})",
        dir.display(),
        document.publisher,
        &document.public_key[..16]
    );
    Ok(())
}

/// Reads or edits the publisher trust store.
pub fn trust(host: &Host, command: TrustCommand) -> Result<()> {
    match command {
        TrustCommand::List => {
            let entries = host.trusted_publishers();
            if entries.is_empty() {
                println!(
                    "no trusted publishers in {}",
                    host.paths().publishers_path().display()
                );
                return Ok(());
            }
            println!("{:<20} KEY", "NAME");
            for entry in entries {
                println!("{:<20} {}", entry.name, entry.public_key);
            }
            Ok(())
        }
        TrustCommand::Add { public_key, name } => {
            host.trust_publisher(&name, &public_key)?;
            println!("trusted '{name}'");
            Ok(())
        }
        TrustCommand::Remove { name } => {
            if host.untrust_publisher(&name)? {
                println!("untrusted '{name}'");
            } else {
                println!("no publisher named '{name}'");
            }
            Ok(())
        }
    }
}

/// Writes a starter package.
fn init(id: &str, dir: Option<PathBuf>, name: Option<String>) -> Result<()> {
    validate_identity(id).map_err(|error| anyhow::anyhow!(error))?;
    let target = dir.unwrap_or_else(|| PathBuf::from(id));
    if target.exists()
        && target
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true)
    {
        bail!("{} already exists and is not empty", target.display());
    }
    fs::create_dir_all(&target)?;

    let name = name.unwrap_or_else(|| id.to_owned());
    let manifest = json!({
        "id": id,
        "name": name,
        "version": "0.1.0",
        "apiVersion": "1",
        "main": "main.js",
        "description": "An OriginLang extension",
        "capabilities": ["log.write"],
        "activationEvents": ["onStartup"]
    });
    fs::write(
        target.join(originlang_ext_manifest::MANIFEST_FILE),
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;
    fs::write(target.join("main.js"), MAIN_JS.replace("__ID__", id))?;
    fs::write(target.join("README.md"), README.replace("__ID__", id))?;

    // Validate what we just wrote: a template that fails its own manifest check
    // is worse than no template at all.
    Manifest::read_dir(&target)?;
    println!("created extension '{id}' in {}", target.display());
    println!(
        "next: oext ext install {} --grant log.write",
        target.display()
    );
    Ok(())
}

/// Temporary installation root, removed on drop.
struct ScratchHome(PathBuf);

impl ScratchHome {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "oext-run-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&root)?;
        Ok(Self(root))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const MAIN_JS: &str = r#"// Entry script. Everything an extension can do goes through `Origin.host`,
// which the host authorizes against the manifest's declared capabilities.
Origin.define({
  id: "__ID__",
  version: "0.1.0",
  capabilities: ["log.write"],

  activate() {
    Origin.host.log("extension activated", "debug");
  },

  deactivate() {
    Origin.host.log("extension deactivated", "debug");
  },

  methods: {
    hello(params) {
      return { message: `Hello, ${params?.name ?? "world"}` };
    },
  },

  onEvent(event) {
    Origin.host.log(`event received: ${event.topic}`, "debug");
  },
});
"#;

const README: &str = r#"# __ID__

An OriginLang extension.

## Layout

| Path | Purpose |
| --- | --- |
| `extension.json` | Manifest: identity, entry point, declared capabilities. |
| `main.js` | Entry script; registers methods through `Origin.define`. |

## Try it

```sh
oext run . --method hello --params '{"name":"OriginLang"}' --grant-all
oext ext install . --grant log.write
oext ext list
```

## Host API

| Call | Capability |
| --- | --- |
| `Origin.host.log(message, level)` | `log.write` |
| `Origin.host.storage.get/set/remove/list` | `storage.read` / `storage.write` |
| `Origin.host.timers.set/clear` | `timer.schedule` |
| `Origin.host.events.emit` / `subscribe` | `events.emit` / `events.subscribe` |
| `Origin.host.callExtension(id, method, params)` | `extension.call` |

`onEvent(event)` receives events published by other extensions. Declare
`onEvent:<topic>` in `activationEvents` to load on demand.
"#;
