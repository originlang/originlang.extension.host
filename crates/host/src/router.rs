//! Dispatches each extension to the backend its manifest asks for.
//!
//! The host used to take a single provider, which made "which backend" a
//! property of the binary rather than of the package. That is wrong once more
//! than one backend exists: a first-party plugin wants the dynamic-library
//! backend and a third-party plugin wants a sandbox, in the same process, at
//! the same time.
//!
//! A router is itself a [`RuntimeProvider`], so the host keeps its single
//! provider field and learns nothing new. The only behaviour worth noting is
//! what it does *not* do: it never falls back to another backend. Substituting
//! QuickJS for a requested `dylib` would fail later with a confusing "main must
//! be .js" error, so an unregistered backend is rejected with the list of ones
//! that are registered.

use originlang_ext_protocol::{RpcError, code};
use originlang_ext_runtime::{Runtime, RuntimeProvider, RuntimeSpec};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Sends each instance to the provider matching its declared backend.
#[derive(Default)]
pub struct Router {
    providers: BTreeMap<&'static str, Arc<dyn RuntimeProvider>>,
}

impl Router {
    /// A router with no backend; [`Router::with`] adds them.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one backend, keyed by the name manifests use.
    ///
    /// A later registration with the same name replaces the earlier one, which
    /// is what lets a test swap a real backend for a fake without changing the
    /// host.
    pub fn with(mut self, provider: Arc<dyn RuntimeProvider>) -> Self {
        self.providers.insert(provider.kind(), provider);
        self
    }

    /// Names of the registered backends, sorted.
    pub fn kinds(&self) -> Vec<&'static str> {
        self.providers.keys().copied().collect()
    }
}

impl RuntimeProvider for Router {
    fn kind(&self) -> &'static str {
        "router"
    }

    fn available(&self) -> Vec<&'static str> {
        self.kinds()
    }

    fn start(&self, spec: RuntimeSpec<'_>) -> Result<Box<dyn Runtime>, RpcError> {
        let Some(provider) = self.providers.get(spec.backend) else {
            let known = if self.providers.is_empty() {
                "none".to_owned()
            } else {
                self.kinds().join(", ")
            };
            return Err(RpcError::new(
                code::EXTENSION_LOAD_FAILED,
                format!(
                    "extension '{}' requires backend '{}'; this host provides {known}",
                    spec.id, spec.backend
                ),
            ));
        };
        provider.start(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use originlang_ext_runtime::{ExtensionInfo, HostBridge, RuntimeLimits};
    use serde_json::Value;
    use std::path::Path;

    struct FakeProvider(&'static str);

    struct FakeRuntime(&'static str);

    impl RuntimeProvider for FakeProvider {
        fn kind(&self) -> &'static str {
            self.0
        }

        fn start(&self, _spec: RuntimeSpec<'_>) -> Result<Box<dyn Runtime>, RpcError> {
            Ok(Box::new(FakeRuntime(self.0)))
        }
    }

    impl Runtime for FakeRuntime {
        fn register(&mut self) -> Result<ExtensionInfo, RpcError> {
            Ok(ExtensionInfo {
                id: self.0.to_owned(),
                version: "0.0.0".to_owned(),
                methods: Vec::new(),
                capabilities: Vec::new(),
            })
        }
        fn call(
            &mut self,
            _method: &str,
            _params: Option<Value>,
            _chain: &[String],
        ) -> Result<Value, RpcError> {
            Ok(Value::Null)
        }
        fn emit(
            &mut self,
            _event: originlang_ext_runtime::Event,
            _chain: &[String],
        ) -> Result<(), RpcError> {
            Ok(())
        }
        fn is_running(&self) -> bool {
            true
        }
        fn shutdown(&mut self) -> Result<(), RpcError> {
            Ok(())
        }
    }

    struct NoBridge;

    impl HostBridge for NoBridge {
        fn call(
            &self,
            _method: &str,
            _params: Option<Value>,
            _chain: &[String],
        ) -> Result<Value, RpcError> {
            Ok(Value::Null)
        }
        fn log(&self, _level: &str, _message: &str) {}
    }

    fn spec<'a>(id: &'a str, backend: &'a str) -> RuntimeSpec<'a> {
        RuntimeSpec {
            id,
            backend,
            entry: Path::new("entry"),
            limits: RuntimeLimits::default(),
            bridge: Arc::new(NoBridge),
        }
    }

    #[test]
    fn dispatches_to_the_declared_backend() {
        let router = Router::new().with(Arc::new(FakeProvider("quickjs")));
        let mut runtime = router.start(spec("a", "quickjs")).expect("start");
        assert_eq!(runtime.register().expect("register").id, "quickjs");
    }

    #[test]
    fn an_unregistered_backend_is_rejected_and_lists_the_others() {
        let router = Router::new().with(Arc::new(FakeProvider("quickjs")));
        let error = start_error(&router, spec("a", "dylib"));
        assert_eq!(error.code, code::EXTENSION_LOAD_FAILED);
        assert!(error.message.contains("quickjs"), "{}", error.message);
        assert!(!error.message.contains("none"), "{}", error.message);
    }

    #[test]
    fn available_reports_every_registered_backend() {
        let router = Router::new()
            .with(Arc::new(FakeProvider("quickjs")))
            .with(Arc::new(FakeProvider("dylib")));
        assert_eq!(router.available(), vec!["dylib", "quickjs"]);
    }

    #[test]
    fn an_empty_router_says_so() {
        let router = Router::new();
        let error = start_error(&router, spec("a", "quickjs"));
        assert!(error.message.contains("provides none"), "{}", error.message);
    }

    /// `Box<dyn Runtime>` is not `Debug`, so `expect_err` cannot be used here.
    fn start_error(router: &Router, spec: RuntimeSpec<'_>) -> RpcError {
        match router.start(spec) {
            Ok(_) => panic!("start should have failed"),
            Err(error) => error,
        }
    }
}
