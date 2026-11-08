//! Extension host: lifecycle, capability enforcement, storage, timers, events.
//!
//! Threading model:
//!
//! - Calls into one extension are serialized by a per-slot mutex, which matches
//!   the single-threaded nature of a scripting runtime.
//! - Calls between extensions are checked against a thread-local call chain, so
//!   a cycle (`a -> b -> a`) is rejected instead of deadlocking.
//! - Timers and event fan-out use `try_lock`: a busy extension skips the event
//!   and the skip is logged, rather than stalling the timer thread.

use crate::api::ExtensionBridge;
use crate::error::{HostError, HostResult};
use crate::logs::{LogBuffer, LogRecord};
use crate::paths::Paths;
use crate::registry::{InstalledExtension, Provenance, Registry, SignaturePolicy};
use originlang_ext_manifest::{API_VERSION, Manifest};
use originlang_ext_permission::{Policy, validate_identity};
use originlang_ext_plugin_abi::ABI_VERSION;
use originlang_ext_protocol::PROTOCOL_VERSION;
use originlang_ext_runtime::{
    Event, ExtensionInfo, Runtime, RuntimeLimits, RuntimeProvider, RuntimeSpec,
};
use originlang_ext_scheduler::{Scheduler, Task};
use originlang_ext_signing::{PublicKey, TrustEntry, TrustStore};
use originlang_ext_storage::{FileStorage, ScopedStorage, Storage};
use serde::Serialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Host crate version, reported by `host.info`.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Largest accepted timer delay: 24 hours.
pub const MAX_TIMER_DELAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Identifies the discovered sandbox executable, reported by `host.hello`.
#[derive(Debug, Clone, Serialize)]
pub struct SandboxInfo {
    /// Sandbox executable version.
    pub version: String,
    /// Absolute path of the sandbox executable.
    pub path: String,
}

/// Public snapshot of one extension, returned by list/load/install.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionSummary {
    /// Stable identity.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Extension version.
    pub version: String,
    /// Manifest API version.
    pub api_version: String,
    /// Optional description.
    pub description: Option<String>,
    /// Operator switch.
    pub enabled: bool,
    /// Whether a live instance exists.
    pub loaded: bool,
    /// Methods reported by the runtime; empty when not loaded.
    pub methods: Vec<String>,
    /// Unix seconds when the current instance was loaded.
    pub loaded_at: Option<u64>,
    /// Capabilities declared by the manifest.
    pub capabilities: Vec<String>,
    /// Capabilities actually granted.
    pub granted: Vec<String>,
    /// Activation triggers.
    pub activation_events: Vec<String>,
    /// Publisher that signed the package, absent when unsigned.
    pub publisher: Option<String>,
    /// Whether the signing key is in the operator's trust store.
    pub trusted: bool,
    /// Backend identifier.
    pub runtime: String,
    /// Contribution points, forwarded verbatim.
    pub contributes: Option<Value>,
}

/// Construction options.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Installation layout.
    pub paths: Paths,
    /// Load `onStartup` extensions during [`Host::startup`].
    pub startup_load: bool,
    /// Log records retained in memory.
    pub max_log_lines: usize,
    /// Refuse unsigned packages and packages signed by untrusted keys.
    pub require_signature: bool,
}

impl HostConfig {
    /// Defaults for an explicit layout.
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            startup_load: true,
            max_log_lines: 2000,
            require_signature: false,
        }
    }

    /// Defaults for the platform installation root.
    pub fn from_env() -> Self {
        Self::new(Paths::from_env())
    }

    /// Overrides startup loading.
    pub fn with_startup_load(mut self, enabled: bool) -> Self {
        self.startup_load = enabled;
        self
    }

    /// Overrides the in-memory log capacity.
    pub fn with_max_log_lines(mut self, lines: usize) -> Self {
        self.max_log_lines = lines.max(1);
        self
    }

    /// Requires every package to be signed by a trusted publisher.
    pub fn with_require_signature(mut self, required: bool) -> Self {
        self.require_signature = required;
        self
    }
}

/// One live instance.
pub(crate) struct Slot {
    #[allow(dead_code)]
    pub(crate) manifest: Manifest,
    pub(crate) info: ExtensionInfo,
    pub(crate) runtime: Mutex<Box<dyn Runtime>>,
    pub(crate) loaded_at: u64,
}

/// Shared host state.
pub(crate) struct Inner {
    pub(crate) paths: Paths,
    pub(crate) policy: RwLock<Policy>,
    pub(crate) trust: RwLock<TrustStore>,
    pub(crate) require_signature: bool,
    pub(crate) registry: Registry,
    pub(crate) provider: Arc<dyn RuntimeProvider>,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) scheduler: Mutex<Scheduler>,
    pub(crate) slots: RwLock<BTreeMap<String, Arc<Slot>>>,
    pub(crate) logs: Mutex<LogBuffer>,
    pub(crate) subscriptions: Mutex<BTreeMap<String, BTreeSet<String>>>,
    pub(crate) timers: Mutex<BTreeMap<String, BTreeSet<String>>>,
    pub(crate) timer_seq: AtomicU64,
    pub(crate) shutting_down: AtomicBool,
    pub(crate) started_at: u64,
    pub(crate) self_ref: OnceLock<Weak<Inner>>,
}

impl Inner {
    pub(crate) fn this(&self) -> Arc<Inner> {
        self.self_ref
            .get()
            .and_then(Weak::upgrade)
            .expect("host inner must outlive its Host")
    }

    /// Backend an installed extension asks for.
    ///
    /// Falls back to the provider's own name only when the manifest is gone,
    /// which cannot happen for a loaded instance; the fallback exists so that
    /// diagnostics never panic on a partially torn-down host.
    pub(crate) fn runtime_of(&self, id: &str) -> String {
        self.registry
            .get(id)
            .map(|installed| installed.manifest.runtime_kind().to_owned())
            .unwrap_or_else(|_| self.provider.kind().to_owned())
    }

    pub(crate) fn record_log(&self, level: &str, extension: Option<&str>, message: &str) {
        if let Ok(mut logs) = self.logs.lock() {
            logs.record(level, extension, message);
        }
    }

    pub(crate) fn slot(&self, id: &str) -> Option<Arc<Slot>> {
        self.slots.read().ok()?.get(id).cloned()
    }

    pub(crate) fn is_loaded(&self, id: &str) -> bool {
        self.slots
            .read()
            .map(|slots| slots.contains_key(id))
            .unwrap_or(false)
    }

    pub(crate) fn scoped_storage(&self, id: &str) -> ScopedStorage {
        ScopedStorage::new(self.storage.clone(), id)
    }

    pub(crate) fn summarize(&self, installed: &InstalledExtension) -> ExtensionSummary {
        let id = installed.manifest.id.as_str();
        let slot = self.slot(id);
        ExtensionSummary {
            id: installed.manifest.id.clone(),
            name: installed.manifest.name.clone(),
            version: installed.manifest.version.clone(),
            api_version: installed.manifest.api_version.clone(),
            description: installed.manifest.description.clone(),
            enabled: installed.enabled,
            loaded: slot.is_some(),
            methods: slot
                .as_ref()
                .map(|slot| slot.info.methods.clone())
                .unwrap_or_default(),
            loaded_at: slot.as_ref().map(|slot| slot.loaded_at),
            capabilities: installed.manifest.capabilities.clone(),
            granted: self
                .policy
                .read()
                .map(|policy| policy.grants_for(id))
                .unwrap_or_default(),
            activation_events: installed.manifest.activation_events.clone(),
            publisher: installed.provenance.publisher.clone(),
            trusted: installed.provenance.trusted,
            // Per-extension, not per-binary: with more than one backend the
            // provider's own name no longer describes this extension.
            runtime: installed.manifest.runtime_kind().to_owned(),
            contributes: installed.manifest.contributes.clone(),
        }
    }

    fn ensure_loaded(&self, id: &str) -> HostResult<()> {
        if self.is_loaded(id) {
            return Ok(());
        }
        self.load(id).map(|_| ())
    }

    pub(crate) fn load(&self, id: &str) -> HostResult<ExtensionSummary> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(HostError::Rejected("host is shutting down".into()));
        }
        if self.is_loaded(id) {
            return Err(HostError::AlreadyLoaded(id.to_owned()));
        }

        let installed = self.registry.get(id)?;
        if !installed.enabled {
            return Err(HostError::Disabled(id.to_owned()));
        }

        // Integrity: a package edited after install must not load silently.
        let actual = installed.manifest.content_hash(&installed.root)?;
        if actual != installed.hash {
            return Err(HostError::LoadFailed(format!(
                "content hash mismatch for '{id}': reinstall the package"
            )));
        }

        // Authorization happens before any code is evaluated, so a denied
        // extension never gets a chance to run or to call the host.
        self.policy
            .read()
            .map_err(|_| HostError::Rejected("policy lock poisoned".into()))?
            .authorize(id, &installed.manifest.capabilities)
            .map_err(HostError::PermissionDenied)?;

        let entry = installed.manifest.entry_path(&installed.root)?;
        let limits = installed.manifest.effective_limits();
        let declared: Arc<BTreeSet<String>> =
            Arc::new(installed.manifest.capabilities.iter().cloned().collect());
        let bridge = Arc::new(ExtensionBridge::new(self.this(), id, declared));
        let mut runtime = self.provider.start(RuntimeSpec {
            id,
            backend: installed.manifest.runtime_kind(),
            entry: &entry,
            limits: RuntimeLimits {
                memory_bytes: limits.memory_bytes,
                stack_bytes: limits.stack_bytes,
                timeout: Duration::from_millis(limits.timeout_ms),
            },
            bridge,
        })?;

        let info = match runtime.register() {
            Ok(info) => info,
            Err(error) => {
                let _ = runtime.shutdown();
                return Err(HostError::LoadFailed(format!(
                    "registration failed: {error}"
                )));
            }
        };
        if info.id != installed.manifest.id {
            let _ = runtime.shutdown();
            return Err(HostError::LoadFailed(format!(
                "extension declared id '{}' but is installed as '{}'",
                info.id, installed.manifest.id
            )));
        }

        let slot = Arc::new(Slot {
            manifest: installed.manifest.clone(),
            info,
            runtime: Mutex::new(runtime),
            loaded_at: now_unix(),
        });
        {
            let mut slots = self
                .slots
                .write()
                .map_err(|_| HostError::Rejected("slot lock poisoned".into()))?;
            if slots.contains_key(id) {
                return Err(HostError::AlreadyLoaded(id.to_owned()));
            }
            slots.insert(id.to_owned(), slot);
        }
        self.record_log(
            "info",
            Some(id),
            &format!(
                "loaded {} v{} on {}",
                installed.manifest.name,
                installed.manifest.version,
                installed.manifest.runtime_kind()
            ),
        );
        Ok(self.summarize(&installed))
    }

    pub(crate) fn unload(&self, id: &str) -> HostResult<()> {
        let slot = self
            .slots
            .write()
            .map_err(|_| HostError::Rejected("slot lock poisoned".into()))?
            .remove(id)
            .ok_or_else(|| HostError::NotLoaded(id.to_owned()))?;

        // Timers survive in the scheduler independently of the slot; they must
        // be dropped before the runtime goes away or the next tick targets a
        // dead instance.
        let timers = self
            .timers
            .lock()
            .map(|mut timers| timers.remove(id).unwrap_or_default())
            .unwrap_or_default();
        if let Ok(scheduler) = self.scheduler.lock() {
            for timer in timers {
                let _ = scheduler.cancel(&timer);
            }
        }
        if let Ok(mut subscriptions) = self.subscriptions.lock() {
            subscriptions.retain(|_, subscribers| {
                subscribers.remove(id);
                !subscribers.is_empty()
            });
        }

        let outcome = slot
            .runtime
            .lock()
            .map_err(|_| HostError::Rejected("runtime lock poisoned".into()))
            .and_then(|mut runtime| runtime.shutdown().map_err(HostError::Runtime));
        match &outcome {
            Ok(()) => self.record_log("info", Some(id), "unloaded"),
            Err(error) => self.record_log(
                "warn",
                Some(id),
                &format!("unloaded after cleanup error: {error}"),
            ),
        }
        outcome
    }

    pub(crate) fn reload(&self, id: &str) -> HostResult<ExtensionSummary> {
        if self.is_loaded(id) {
            self.unload(id)?;
        }
        self.load(id)
    }

    pub(crate) fn call(&self, id: &str, method: &str, params: Option<Value>) -> HostResult<Value> {
        self.ensure_loaded(id)?;
        let _chain = ChainGuard::enter(id)?;
        // Snapshot after entering, so the stack handed to the backend ends with
        // `id` and a backend that runs elsewhere can reproduce it exactly.
        let chain = chain_snapshot();
        let slot = self
            .slot(id)
            .ok_or_else(|| HostError::NotLoaded(id.to_owned()))?;
        let outcome = {
            let mut runtime = slot
                .runtime
                .lock()
                .map_err(|_| HostError::Rejected("runtime lock poisoned".into()))?;
            runtime
                .call(method, params, &chain)
                .map_err(|error| (error, runtime.is_running()))
        };
        match outcome {
            Ok(value) => Ok(value),
            Err((error, alive)) => {
                if !alive {
                    self.retire(id, "runtime stopped");
                }
                Err(HostError::Runtime(error))
            }
        }
    }

    pub(crate) fn retire(&self, id: &str, reason: &str) {
        self.record_log("warn", Some(id), &format!("retiring instance: {reason}"));
        let _ = self.unload(id);
    }

    /// Delivers one event; a busy extension is skipped, not queued.
    pub(crate) fn deliver(&self, id: &str, event: Event) -> HostResult<()> {
        let slot = self
            .slot(id)
            .ok_or_else(|| HostError::NotLoaded(id.to_owned()))?;
        let mut runtime = match slot.runtime.try_lock() {
            Ok(runtime) => runtime,
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(HostError::Rejected("extension is busy".into()));
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(HostError::Rejected("runtime lock poisoned".into()));
            }
        };
        let mut chain = chain_snapshot();
        if chain.last().map(String::as_str) != Some(id) {
            chain.push(id.to_owned());
        }
        runtime.emit(event, &chain).map_err(HostError::Runtime)
    }

    pub(crate) fn emit(
        &self,
        topic: &str,
        payload: Value,
        from: Option<&str>,
    ) -> HostResult<usize> {
        validate_topic(topic)?;

        let mut targets = self
            .subscriptions
            .lock()
            .map(|subscriptions| subscriptions.get(topic).cloned().unwrap_or_default())
            .unwrap_or_default();

        // Activation events are the declarative half of subscription: an
        // extension declaring `onEvent:<topic>` is loaded when the topic fires.
        let scan = self.registry.scan()?;
        for warning in &scan.warnings {
            self.record_log("warn", None, warning);
        }
        for installed in &scan.extensions {
            if installed.enabled && installed.manifest.activation_topics().contains(&topic) {
                targets.insert(installed.manifest.id.clone());
            }
        }
        if let Some(from) = from {
            // Never echo an event back to its publisher: that is the shortest
            // path to an infinite publish loop.
            targets.remove(from);
        }

        let mut delivered = 0;
        for id in targets {
            if !self.is_loaded(&id)
                && let Err(error) = self.load(&id)
            {
                self.record_log(
                    "error",
                    Some(&id),
                    &format!("activation for '{topic}' failed: {error}"),
                );
                continue;
            }
            let event = match from {
                Some(from) => Event::from_extension(topic, payload.clone(), from),
                None => Event::host(topic, payload.clone()),
            };
            match self.deliver(&id, event) {
                Ok(()) => delivered += 1,
                Err(error) => self.record_log(
                    "warn",
                    Some(&id),
                    &format!("event '{topic}' not delivered: {error}"),
                ),
            }
        }
        Ok(delivered)
    }

    pub(crate) fn schedule_timer(
        &self,
        extension: &str,
        delay_ms: u64,
        repeat: bool,
    ) -> HostResult<String> {
        let delay = Duration::from_millis(delay_ms.clamp(1, MAX_TIMER_DELAY_MS));
        let sequence = self.timer_seq.fetch_add(1, Ordering::SeqCst);
        let timer_id = format!("timer:{extension}:{sequence}");

        let weak = Arc::downgrade(&self.this());
        let owner = extension.to_owned();
        let fired = timer_id.clone();
        let action: originlang_ext_scheduler::Action = Box::new(move || {
            let Some(inner) = weak.upgrade() else {
                return Ok(());
            };
            if !repeat {
                inner.forget_timer(&owner, &fired);
            }
            inner
                .deliver(&owner, Event::host("timer", json!({ "id": fired })))
                .map_err(|error| error.to_string())
        });

        let task = if repeat {
            Task::recurring(&timer_id, Instant::now() + delay, delay, action)
        } else {
            Task::once(&timer_id, Instant::now() + delay, action)
        };
        self.scheduler
            .lock()
            .map_err(|_| HostError::Rejected("scheduler lock poisoned".into()))?
            .add(task)
            .map_err(|error| HostError::Rejected(format!("schedule timer: {error:?}")))?;
        if let Ok(mut timers) = self.timers.lock() {
            timers
                .entry(extension.to_owned())
                .or_default()
                .insert(timer_id.clone());
        }
        Ok(timer_id)
    }

    pub(crate) fn clear_timer(&self, extension: &str, timer_id: &str) -> HostResult<bool> {
        let cancelled = self
            .scheduler
            .lock()
            .map_err(|_| HostError::Rejected("scheduler lock poisoned".into()))?
            .cancel(timer_id)
            .unwrap_or(false);
        if let Ok(mut timers) = self.timers.lock()
            && let Some(owned) = timers.get_mut(extension)
        {
            owned.remove(timer_id);
        }
        Ok(cancelled)
    }

    fn forget_timer(&self, extension: &str, timer_id: &str) {
        if let Ok(mut timers) = self.timers.lock()
            && let Some(owned) = timers.get_mut(extension)
        {
            owned.remove(timer_id);
        }
    }

    pub(crate) fn subscribe(&self, extension: &str, topic: &str) -> HostResult<()> {
        validate_topic(topic)?;
        self.subscriptions
            .lock()
            .map_err(|_| HostError::Rejected("subscription lock poisoned".into()))?
            .entry(topic.to_owned())
            .or_default()
            .insert(extension.to_owned());
        Ok(())
    }

    fn save_policy(&self, policy: &Policy) -> HostResult<()> {
        policy
            .save(&self.paths.policy_path())
            .map_err(|e| HostError::io("write policy", e))
    }

    pub(crate) fn save_trust(&self, trust: &TrustStore) -> HostResult<()> {
        trust
            .save(&self.paths.publishers_path())
            .map_err(|error| HostError::Rejected(format!("cannot write trust store: {error}")))
    }

    pub(crate) fn grant(&self, id: &str, capabilities: &[String]) -> HostResult<Vec<String>> {
        validate_identity(id).map_err(HostError::Rejected)?;
        if !self.registry_path_exists(id) {
            return Err(HostError::NotFound(id.to_owned()));
        }
        let mut policy = self
            .policy
            .write()
            .map_err(|_| HostError::Rejected("policy lock poisoned".into()))?;
        // Validate the whole batch first: a partial grant would leave the
        // operator believing a capability was applied when it was not.
        for capability in capabilities {
            originlang_ext_permission::validate_capability(capability)
                .map_err(HostError::Rejected)?;
        }
        for capability in capabilities {
            policy.grant(id, capability).map_err(HostError::Rejected)?;
        }
        let granted = policy.grants_for(id);
        let snapshot = policy.clone();
        drop(policy);
        self.save_policy(&snapshot)?;
        self.record_log(
            "info",
            Some(id),
            &format!("granted capabilities: {}", capabilities.join(", ")),
        );
        Ok(granted)
    }

    pub(crate) fn revoke(&self, id: &str, capability: &str) -> HostResult<Vec<String>> {
        let mut policy = self
            .policy
            .write()
            .map_err(|_| HostError::Rejected("policy lock poisoned".into()))?;
        policy.revoke(id, capability);
        let granted = policy.grants_for(id);
        let snapshot = policy.clone();
        drop(policy);
        self.save_policy(&snapshot)?;
        self.record_log(
            "info",
            Some(id),
            &format!("revoked capability: {capability}"),
        );
        Ok(granted)
    }

    fn registry_path_exists(&self, id: &str) -> bool {
        self.paths.extensions_dir().join(id).is_dir()
    }

    fn purge_storage(&self, id: &str) {
        let storage = self.scoped_storage(id);
        if let Ok(entries) = storage.list("") {
            for entry in entries {
                let _ = storage.delete(&entry.key);
            }
        }
    }
}

/// The extension host.
///
/// Cheap to clone: every clone shares the same state and the same live
/// instances. Call [`Host::shutdown`] once when the process is done.
#[derive(Clone)]
pub struct Host {
    inner: Arc<Inner>,
}

impl Host {
    /// Opens (creating when missing) the installation described by `config`.
    pub fn new(config: HostConfig, provider: Arc<dyn RuntimeProvider>) -> HostResult<Self> {
        config
            .paths
            .ensure()
            .map_err(|e| HostError::io("create installation directories", e))?;
        let policy = Policy::load(&config.paths.policy_path())
            .map_err(|e| HostError::io("load policy", e))?;
        let trust = TrustStore::load(&config.paths.publishers_path())
            .map_err(|error| HostError::Rejected(format!("cannot read trust store: {error}")))?;
        let require_signature = config.require_signature;
        let registry = Registry::open(config.paths.extensions_dir(), config.paths.registry_path())?;
        let storage: Arc<dyn Storage> = Arc::new(
            FileStorage::open(&config.paths.storage_dir())
                .map_err(|e| HostError::Storage(e.to_string()))?,
        );
        let logs = LogBuffer::with_file(config.max_log_lines, &config.paths.log_path());
        let inner = Arc::new(Inner {
            paths: config.paths,
            policy: RwLock::new(policy),
            trust: RwLock::new(trust),
            require_signature,
            registry,
            provider,
            storage,
            scheduler: Mutex::new(Scheduler::new()),
            slots: RwLock::new(BTreeMap::new()),
            logs: Mutex::new(logs),
            subscriptions: Mutex::new(BTreeMap::new()),
            timers: Mutex::new(BTreeMap::new()),
            timer_seq: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            started_at: now_unix(),
            self_ref: OnceLock::new(),
        });
        let _ = inner.self_ref.set(Arc::downgrade(&inner));
        inner.record_log("info", None, &format!("host {HOST_VERSION} starting"));
        Ok(Self { inner })
    }

    /// Loads every enabled extension declaring `onStartup`.
    ///
    /// Failures are logged and skipped: one broken package must not prevent the
    /// host from starting.
    pub fn startup(&self) -> HostResult<usize> {
        let scan = self.inner.registry.scan()?;
        for warning in &scan.warnings {
            self.inner.record_log("warn", None, warning);
        }
        let mut loaded = 0;
        for installed in scan.extensions {
            if !installed.enabled || !installed.manifest.activates_on_startup() {
                continue;
            }
            match self.inner.load(&installed.manifest.id) {
                Ok(_) => loaded += 1,
                Err(error) => self.inner.record_log(
                    "error",
                    Some(&installed.manifest.id),
                    &format!("startup load failed: {error}"),
                ),
            }
        }
        Ok(loaded)
    }

    /// Installed extensions with their runtime state.
    pub fn summaries(&self) -> HostResult<Vec<ExtensionSummary>> {
        let scan = self.inner.registry.scan()?;
        Ok(scan
            .extensions
            .iter()
            .map(|installed| self.inner.summarize(installed))
            .collect())
    }

    /// `{ "extensions": [...] }`, shaped for `host.extension.list`.
    pub fn list_json(&self) -> HostResult<Value> {
        Ok(json!({ "extensions": self.summaries()? }))
    }

    /// Loads an extension.
    pub fn load(&self, id: &str) -> HostResult<ExtensionSummary> {
        self.inner.load(id)
    }

    /// Unloads an extension.
    pub fn unload(&self, id: &str) -> HostResult<()> {
        self.inner.unload(id)
    }

    /// Reloads an extension, picking up a reinstalled package.
    pub fn reload(&self, id: &str) -> HostResult<ExtensionSummary> {
        self.inner.reload(id)
    }

    /// Invokes a method, loading the extension on demand.
    pub fn call(&self, id: &str, method: &str, params: Option<Value>) -> HostResult<Value> {
        self.inner.call(id, method, params)
    }

    /// Publishes a host-originated event; returns the number of deliveries.
    pub fn emit(&self, topic: &str, payload: Value) -> HostResult<usize> {
        self.inner.emit(topic, payload, None)
    }

    /// Recent log records, optionally filtered to one extension.
    pub fn logs(&self, id: Option<&str>, limit: usize) -> Vec<LogRecord> {
        self.inner
            .logs
            .lock()
            .map(|logs| logs.snapshot(id, limit))
            .unwrap_or_default()
    }

    /// Installs a package directory.
    pub fn install(&self, source: &std::path::Path, force: bool) -> HostResult<ExtensionSummary> {
        let trust = self
            .inner
            .trust
            .read()
            .map_err(|_| HostError::Rejected("trust store lock poisoned".into()))?;
        let policy = SignaturePolicy {
            require_signature: self.inner.require_signature,
            trust: &trust,
        };
        let installed = self.inner.registry.install(source, force, &policy)?;
        drop(trust);
        let Provenance { publisher, trusted } = &installed.provenance;
        self.inner.record_log(
            "info",
            Some(&installed.manifest.id),
            &match publisher {
                Some(publisher) if *trusted => {
                    format!(
                        "installed v{} signed by '{publisher}' (trusted)",
                        installed.manifest.version
                    )
                }
                Some(publisher) => format!(
                    "installed v{} signed by '{publisher}' (untrusted key)",
                    installed.manifest.version
                ),
                None => format!("installed v{} (unsigned)", installed.manifest.version),
            },
        );
        Ok(self.inner.summarize(&installed))
    }

    /// Publishers the operator has accepted.
    pub fn trusted_publishers(&self) -> Vec<TrustEntry> {
        self.inner
            .trust
            .read()
            .map(|trust| trust.entries())
            .unwrap_or_default()
    }

    /// Accepts a publisher public key under a name.
    pub fn trust_publisher(&self, name: &str, public_key: &str) -> HostResult<()> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(HostError::Rejected(
                "publisher name must be 1..=64 characters".into(),
            ));
        }
        // Parse before storing: an unusable key in the trust store would only
        // fail later, at install time, where the cause is much harder to see.
        PublicKey::from_hex(public_key).map_err(|error| HostError::Rejected(error.to_string()))?;
        let mut trust = self
            .inner
            .trust
            .write()
            .map_err(|_| HostError::Rejected("trust store lock poisoned".into()))?;
        trust.trust(name, public_key);
        let snapshot = trust.clone();
        drop(trust);
        self.inner.save_trust(&snapshot)?;
        self.inner
            .record_log("info", None, &format!("trusted publisher '{name}'"));
        Ok(())
    }

    /// Removes a publisher from the trust store.
    pub fn untrust_publisher(&self, name: &str) -> HostResult<bool> {
        let mut trust = self
            .inner
            .trust
            .write()
            .map_err(|_| HostError::Rejected("trust store lock poisoned".into()))?;
        let removed = trust.untrust(name);
        let snapshot = trust.clone();
        drop(trust);
        if removed {
            self.inner.save_trust(&snapshot)?;
            self.inner
                .record_log("info", None, &format!("untrusted publisher '{name}'"));
        }
        Ok(removed)
    }

    /// Unloads, clears storage and removes a package.
    pub fn uninstall(&self, id: &str) -> HostResult<()> {
        if self.inner.is_loaded(id) {
            self.inner.unload(id)?;
        }
        self.inner.purge_storage(id);
        self.inner.registry.uninstall(id)?;
        self.inner.record_log("info", Some(id), "uninstalled");
        Ok(())
    }

    /// Enables or disables a package; disabling unloads it first.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> HostResult<ExtensionSummary> {
        if !enabled && self.inner.is_loaded(id) {
            self.inner.unload(id)?;
        }
        let installed = self.inner.registry.set_enabled(id, enabled)?;
        self.inner.record_log(
            "info",
            Some(id),
            if enabled { "enabled" } else { "disabled" },
        );
        Ok(self.inner.summarize(&installed))
    }

    /// Capabilities granted to one extension.
    pub fn grants(&self, id: &str) -> Vec<String> {
        self.inner
            .policy
            .read()
            .map(|policy| policy.grants_for(id))
            .unwrap_or_default()
    }

    /// Grants capabilities (all-or-nothing per call).
    pub fn grant(&self, id: &str, capabilities: &[String]) -> HostResult<Vec<String>> {
        self.inner.grant(id, capabilities)
    }

    /// Revokes one capability.
    pub fn revoke(&self, id: &str, capability: &str) -> HostResult<Vec<String>> {
        self.inner.revoke(id, capability)
    }

    /// `host.info` payload.
    pub fn info(&self) -> Value {
        let (installed, loaded) = match self.summaries() {
            Ok(summaries) => (
                summaries.len(),
                summaries.iter().filter(|s| s.loaded).count(),
            ),
            Err(_) => (0, 0),
        };
        json!({
            "hostVersion": HOST_VERSION,
            "apiVersion": API_VERSION,
            "runtime": self.inner.provider.kind(),
            // What this binary can actually run, as opposed to the name of the
            // provider holding them; empty when the provider is not a router.
            "backends": self.inner.provider.available(),
            "root": self.inner.paths.root().to_string_lossy(),
            "startedAt": self.inner.started_at,
            "extensions": { "installed": installed, "loaded": loaded },
        })
    }

    /// `host.hello` payload: the negotiation handshake on every connection.
    ///
    /// Superset of [`Host::info`] plus the service protocol and plugin ABI
    /// versions a client must compare before driving the host. `sandbox` is
    /// `None` when the sandbox backend is unavailable, in which case the field
    /// serializes to `null` so a client can tell "no sandbox" from "not asked".
    pub fn hello(&self, sandbox: Option<SandboxInfo>) -> Value {
        let mut payload = self.info();
        let object = payload.as_object_mut().expect("info() is an object");
        object.insert(
            "protocolVersion".to_owned(),
            Value::String(PROTOCOL_VERSION.to_owned()),
        );
        object.insert("abiVersion".to_owned(), Value::Number(ABI_VERSION.into()));
        object.insert(
            "sandbox".to_owned(),
            sandbox
                .map(|info| json!({ "version": info.version, "path": info.path }))
                .unwrap_or(Value::Null),
        );
        payload
    }

    /// Stops every instance and the scheduler. Idempotent.
    pub fn shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let ids: Vec<String> = self
            .inner
            .slots
            .read()
            .map(|slots| slots.keys().cloned().collect())
            .unwrap_or_default();
        for id in ids {
            let _ = self.inner.unload(&id);
        }
        if let Ok(mut scheduler) = self.inner.scheduler.lock() {
            let _ = scheduler.stop();
        }
        self.inner.record_log("info", None, "host stopped");
    }

    /// Installation layout.
    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    /// Runtime backend identifier.
    pub fn runtime_kind(&self) -> &'static str {
        self.inner.provider.kind()
    }
}

thread_local! {
    /// Extension ids currently on this thread's call stack.
    static CALL_CHAIN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Snapshot of this thread's call stack, innermost last.
pub(crate) fn chain_snapshot() -> Vec<String> {
    CALL_CHAIN.with(|chain| chain.borrow().clone())
}

/// Runs `f` with `chain` installed as this thread's call stack.
///
/// A process backend answers Host API calls on its own reader thread, so the
/// stack that led to the call has to be installed there explicitly. Without it a
/// nested call would start from an empty stack and a cycle would block on a slot
/// mutex instead of being rejected.
pub(crate) fn with_chain<R>(chain: &[String], f: impl FnOnce() -> R) -> R {
    struct Restore(Vec<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CALL_CHAIN.with(|chain| {
                let mut chain = chain.borrow_mut();
                chain.clear();
                chain.extend(self.0.iter().cloned());
            });
        }
    }
    let saved = CALL_CHAIN.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), chain.to_vec()));
    let _restore = Restore(saved);
    f()
}

/// Rejects re-entrant calls that would deadlock on a slot mutex.
struct ChainGuard;

impl ChainGuard {
    fn enter(id: &str) -> HostResult<Self> {
        CALL_CHAIN.with(|chain| {
            let mut chain = chain.borrow_mut();
            if chain.iter().any(|entry| entry == id) {
                return Err(HostError::Rejected(format!(
                    "re-entrant call into '{id}' would deadlock and is not allowed"
                )));
            }
            chain.push(id.to_owned());
            Ok(Self)
        })
    }
}

impl Drop for ChainGuard {
    fn drop(&mut self) {
        CALL_CHAIN.with(|chain| {
            chain.borrow_mut().pop();
        });
    }
}

/// Validates an event topic: lower-case segments separated by `.`, `-`, `_`, `:`.
pub fn validate_topic(topic: &str) -> HostResult<()> {
    let valid = !topic.is_empty()
        && topic.len() <= 128
        && !topic.starts_with('.')
        && !topic.contains("..")
        && topic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'));
    if valid {
        Ok(())
    } else {
        Err(HostError::Rejected(format!("invalid event topic: {topic}")))
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
