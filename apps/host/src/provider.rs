//! Runtime backend assembly.
//!
//! Mirrors the `bootidea` CLI's provider selection: a [`Router`] dispatches each
//! extension to the backend its manifest declares. The sandbox backend is
//! discovered (never `PATH`-searched) and reported separately so `host.hello`
//! can state whether it is available.

use crate::config::{Backend, Settings};
use originlang_ext_host::{Host, HostConfig, Router, SandboxInfo};
use originlang_ext_runtime::RuntimeProvider;
use originlang_ext_runtime_dylib::DylibProvider;
use originlang_ext_runtime_process::SandboxProvider;
use originlang_ext_runtime_quickjs::QuickJsProvider;
use std::sync::Arc;

/// Opens the host for the given settings.
pub fn open(settings: &Settings) -> Result<Host, String> {
    let config = HostConfig::new(originlang_ext_host::Paths::new(&settings.home))
        .with_startup_load(settings.startup)
        .with_require_signature(settings.require_signature);
    Host::new(config, provider(settings.backend)?).map_err(|error| error.to_string())
}

/// Builds the backend provider.
pub fn provider(backend: Backend) -> Result<Arc<dyn RuntimeProvider>, String> {
    match backend {
        Backend::Auto => {
            let mut router = Router::new()
                .with(Arc::new(QuickJsProvider::new()))
                .with(Arc::new(DylibProvider::new()));
            if let Ok(sandbox) = SandboxProvider::discover() {
                router = router.with(Arc::new(sandbox));
            }
            Ok(Arc::new(router))
        }
        Backend::Sandbox => Ok(Arc::new(
            SandboxProvider::discover().map_err(|error| error.to_string())?,
        )),
        Backend::Quickjs => Ok(Arc::new(QuickJsProvider::new())),
        Backend::Dylib => Ok(Arc::new(DylibProvider::new())),
    }
}

/// Reports the sandbox backend, if discoverable, for `host.hello`.
pub fn sandbox_info() -> Option<SandboxInfo> {
    let provider = SandboxProvider::discover().ok()?;
    Some(SandboxInfo {
        // The sandbox and host ship from the same workspace, so they share a
        // version; the sandbox binary itself is deliberately dumb and exposes no
        // `--version` of its own.
        version: crate::config::HOST_VERSION.to_owned(),
        path: provider.binary().to_string_lossy().into_owned(),
    })
}
