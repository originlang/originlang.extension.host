//! Dynamic-library backend for first-party extensions.
//!
//! This backend loads a `cdylib` into the host process and adapts it to
//! [`Runtime`]. It exists for one reason: first-party plugins are trusted, and
//! paying a sandbox for trusted code buys nothing. Everything the host protects
//! an extension *from* — a runaway loop, a corrupted heap, a hostile author —
//! is out of scope here, because the plugin is written by the same people who
//! write the host.
//!
//! # What this backend deliberately does not do
//!
//! Three properties are traded away. They are listed here rather than
//! discovered in production:
//!
//! **A call cannot be interrupted.** QuickJS installs an interrupt handler, so
//! [`RuntimeLimits::timeout`] is enforced by stopping the interpreter. A native
//! function cannot be stopped from another thread without killing the process.
//! When the deadline passes this backend therefore *retires* the instance: it
//! stops accepting calls and reports the timeout, but the offending call keeps
//! running until it returns. A plugin that blocks forever will take up a host
//! thread forever. First-party plugins are expected to honour their own
//! deadlines; the check here exists to stop *later* calls, not to preempt.
//!
//! **A library is never unloaded.** `FreeLibrary` on a Rust `cdylib` that has
//! started a thread, registered a TLS callback, or owns a static with a
//! destructor leaves dangling pointers behind. Unloading is only safe under
//! conditions this backend cannot check, so it does not unload at all. The cost
//! is that a plugin cannot be hot-swapped: replacing the file on disk has no
//! effect until the host restarts. The benefit is that a load is never followed
//! by a crash at shutdown.
//!
//! **Capabilities are advisory.** A plugin shares the host's address space and
//! can open a socket or read a file without asking. Routing its requests
//! through [`originlang_ext_plugin_abi::HostVTable`] keeps one auditable path
//! and one place for log attribution, but it is a convention, not a boundary.
//! If that convention ever needs to become a boundary, the answer is a sandbox
//! backend (WASM), not more checks here.
//!
//! # Memory
//!
//! Two allocators live in this address space once a plugin is loaded. Nothing
//! allocated on one side is freed on the other: the host frees what
//! [`originlang_ext_plugin_abi::HostVTable::call`] returned, and the plugin
//! frees what it returned through its own `string_free`.

use libloading::Library;
use originlang_ext_plugin_abi::{
    ABI_VERSION, AbiVersionFn, CallFn, CreateFn, EmitFn, Handle, HostVTable, IsRunningFn,
    RegisterFn, SYM_ABI_VERSION, SYM_CALL, SYM_CREATE, SYM_EMIT, SYM_IS_RUNNING, SYM_REGISTER,
    SYM_SHUTDOWN, SYM_STRING_FREE, ShutdownFn, StringFreeFn, free_c_string, read_c_str, read_json,
};
use originlang_ext_protocol::{RpcError, code};
use originlang_ext_runtime::{
    CallChain, Event, ExtensionInfo, Runtime, RuntimeProvider, RuntimeSpec,
};
use serde_json::Value;
use std::ffi::{CString, c_char, c_void};
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

/// Loads first-party `cdylib` extensions.
#[derive(Debug, Default, Clone, Copy)]
pub struct DylibProvider;

impl DylibProvider {
    /// A provider with no state; every instance is independent.
    pub fn new() -> Self {
        Self
    }
}

impl RuntimeProvider for DylibProvider {
    fn kind(&self) -> &'static str {
        "dylib"
    }

    fn start(&self, spec: RuntimeSpec<'_>) -> Result<Box<dyn Runtime>, RpcError> {
        Ok(Box::new(DylibRuntime::open(spec)?))
    }
}

/// Function pointers resolved once at load time.
///
/// Storing plain pointers rather than `Symbol` values decouples the handles
/// from the library's borrow, which matters because the library is deliberately
/// leaked (see the module docs).
#[derive(Clone, Copy)]
struct Symbols {
    create: CreateFn,
    register: RegisterFn,
    call: CallFn,
    emit: EmitFn,
    is_running: IsRunningFn,
    shutdown: ShutdownFn,
    string_free: StringFreeFn,
}

/// State the host lends to one plugin instance.
///
/// Boxed and never moved: the plugin keeps a raw pointer to it for its whole
/// lifetime.
struct HostContext {
    /// The bridge already knows which extension it belongs to, so no id is
    /// carried here; adding one would create a second source of truth for log
    /// attribution.
    bridge: std::sync::Arc<dyn originlang_ext_runtime::HostBridge>,
}

/// Keeps the vtable and the context it points at at stable addresses.
///
/// Field order is irrelevant because `ctx` is already boxed, but the invariant
/// matters: `table.ctx` must keep pointing at the same allocation for as long
/// as the plugin can call back.
struct VTableSlot {
    #[allow(dead_code)]
    ctx: Box<HostContext>,
    table: HostVTable,
}

/// One loaded plugin instance.
pub struct DylibRuntime {
    /// Leaked on purpose; see the module docs on unloading.
    _library: ManuallyDrop<Library>,
    symbols: Symbols,
    handle: Handle,
    /// Kept alive for the instance's lifetime; `table` inside is handed to the
    /// plugin and must not move. Never read directly, which is exactly why it
    /// is prefixed: holding it *is* the point.
    _slot: Box<VTableSlot>,
    timeout: Duration,
    /// Cleared once the instance is retired, either by timeout or shutdown.
    alive: bool,
}

// SAFETY: `handle` is an opaque pointer into plugin state that the host only
// touches through `&mut self`. `Runtime` already documents that calls are
// serialized by the owner, so no two threads ever use the handle at once.
unsafe impl Send for DylibRuntime {}

impl DylibRuntime {
    fn open(spec: RuntimeSpec<'_>) -> Result<Self, RpcError> {
        let path = spec.entry;
        // SAFETY: loading arbitrary native code is the entire point of this
        // backend. The signature check below rejects a mismatched build before
        // any other symbol is called.
        let library = unsafe { Library::new(path) }.map_err(|error| {
            RpcError::new(
                code::EXTENSION_LOAD_FAILED,
                format!("cannot load '{}': {error}", path.display()),
            )
        })?;

        let abi_version: AbiVersionFn = bind(&library, SYM_ABI_VERSION)?;
        // SAFETY: the symbol has the declared signature; the plugin exports it.
        let declared = unsafe { abi_version() };
        if declared != ABI_VERSION {
            return Err(RpcError::new(
                code::EXTENSION_LOAD_FAILED,
                format!(
                    "'{}' targets plugin ABI {declared}, this host implements {ABI_VERSION}; rebuild the plugin",
                    path.display()
                ),
            ));
        }

        let symbols = Symbols {
            create: bind(&library, SYM_CREATE)?,
            register: bind(&library, SYM_REGISTER)?,
            call: bind(&library, SYM_CALL)?,
            emit: bind(&library, SYM_EMIT)?,
            is_running: bind(&library, SYM_IS_RUNNING)?,
            shutdown: bind(&library, SYM_SHUTDOWN)?,
            string_free: bind(&library, SYM_STRING_FREE)?,
        };

        let limits = spec.limits;
        let ctx = Box::new(HostContext {
            bridge: spec.bridge.clone(),
        });
        let ctx_ptr = (&*ctx as *const HostContext).cast_mut().cast::<c_void>();
        let slot = Box::new(VTableSlot {
            ctx,
            table: HostVTable {
                abi_version: ABI_VERSION,
                call: Some(host_call),
                string_free: Some(host_string_free),
                log: Some(host_log),
                ctx: ctx_ptr,
            },
        });

        // SAFETY: the vtable lives in a box that outlives the instance.
        let handle = unsafe { (symbols.create)(&slot.table) };
        if handle.is_null() {
            // SAFETY: the library is valid and the symbol was bound above.
            unsafe { (symbols.shutdown)(handle) };
            return Err(RpcError::new(
                code::EXTENSION_LOAD_FAILED,
                format!("'{}' refused to create an instance", path.display()),
            ));
        }

        Ok(Self {
            _library: ManuallyDrop::new(library),
            symbols,
            handle,
            _slot: slot,
            timeout: limits.timeout,
            alive: true,
        })
    }

    /// Runs one plugin export and reads its envelope back.
    fn exchange(
        &mut self,
        body: impl FnOnce(&Symbols, Handle) -> *mut c_char,
    ) -> Result<Value, RpcError> {
        if !self.alive {
            return Err(RpcError::new(
                code::EXTENSION_NOT_LOADED,
                "plugin instance has been retired",
            ));
        }

        let started = Instant::now();
        let raw = body(&self.symbols, self.handle);
        let text = {
            // SAFETY: the plugin returned a NUL-terminated string it allocated.
            let text = unsafe { read_c_str(raw) };
            // SAFETY: same allocation; ownership returns to the plugin.
            unsafe { (self.symbols.string_free)(raw) };
            text
        };

        // The call already finished, so this only decides whether the *next*
        // one is allowed. See the module docs on preemption.
        if started.elapsed() > self.timeout {
            self.alive = false;
            return Err(RpcError::new(
                code::TIMEOUT,
                format!(
                    "call exceeded {} ms; a dynamic plugin cannot be interrupted, so it has been retired",
                    self.timeout.as_millis()
                ),
            ));
        }

        let text = text.ok_or_else(|| {
            RpcError::new(
                code::OPERATION_FAILED,
                "plugin returned a null or non-UTF-8 response",
            )
        })?;
        originlang_ext_plugin_abi::decode(&text)
            .map_err(|(code, message)| RpcError::new(i64::from(code), message))
    }
}

impl Runtime for DylibRuntime {
    fn register(&mut self) -> Result<ExtensionInfo, RpcError> {
        let value = self.exchange(|symbols, handle| {
            // SAFETY: handle was returned by this library's `create`.
            unsafe { (symbols.register)(handle) }
        })?;
        serde_json::from_value(value).map_err(|error| {
            RpcError::new(
                code::EXTENSION_LOAD_FAILED,
                format!("plugin reported malformed metadata: {error}"),
            )
        })
    }

    fn call(
        &mut self,
        method: &str,
        params: Option<Value>,
        chain: &CallChain,
    ) -> Result<Value, RpcError> {
        let method = CString::new(method)
            .map_err(|_| RpcError::new(code::INVALID_PARAMS, "method name contains a NUL byte"))?;
        // A serialized JSON value never contains a bare NUL — it is escaped — so
        // `ok()` cannot silently drop arguments in practice.
        let params = params.and_then(|value| CString::new(value.to_string()).ok());
        let chain = chain_json(chain);
        let params_ptr = params
            .as_ref()
            .map(|value| value.as_ptr())
            .unwrap_or(std::ptr::null());
        let chain_ptr = chain.as_ptr();

        self.exchange(|symbols, handle| {
            // SAFETY: all pointers outlive the call; handle came from `create`.
            unsafe { (symbols.call)(handle, method.as_ptr(), params_ptr, chain_ptr) }
        })
    }

    fn emit(&mut self, event: Event, chain: &CallChain) -> Result<(), RpcError> {
        let event = CString::new(serde_json::to_string(&event).unwrap_or_default())
            .unwrap_or_else(|_| CString::new("{}").expect("static string"));
        let chain = chain_json(chain);

        self.exchange(|symbols, handle| {
            // SAFETY: both pointers outlive the call; handle came from `create`.
            unsafe { (symbols.emit)(handle, event.as_ptr(), chain.as_ptr()) }
        })
        .map(|_| ())
    }

    fn is_running(&self) -> bool {
        self.alive
            && !self.handle.is_null()
            // SAFETY: handle is valid while `alive` is set and shutdown has not run.
            && unsafe { (self.symbols.is_running)(self.handle) } != 0
    }

    fn shutdown(&mut self) -> Result<(), RpcError> {
        if !self.alive {
            return Ok(());
        }
        // SAFETY: handle came from `create` and shutdown runs at most once.
        let raw = unsafe { (self.symbols.shutdown)(self.handle) };
        let text = unsafe { read_c_str(raw) };
        // SAFETY: same allocation; ownership returns to the plugin.
        unsafe { (self.symbols.string_free)(raw) };
        self.alive = false;

        match text {
            Some(text) => originlang_ext_plugin_abi::decode(&text)
                .map(|_| ())
                .map_err(|(code, message)| RpcError::new(i64::from(code), message)),
            None => Ok(()),
        }
    }
}

/// Binds one export, naming it in the error so a partial build is obvious.
fn bind<T: Copy>(library: &Library, name: &str) -> Result<T, RpcError> {
    // SAFETY: the type is the one documented for this symbol in `plugin-abi`.
    let symbol = unsafe { library.get::<T>(name.as_bytes()) }.map_err(|error| {
        RpcError::new(
            code::EXTENSION_LOAD_FAILED,
            format!("missing export '{name}': {error}"),
        )
    })?;
    Ok(*symbol)
}

/// Serializes the caller's stack; a failure yields an empty stack rather than
/// aborting the call, because losing cycle detection is better than failing it.
fn chain_json(chain: &CallChain) -> CString {
    let encoded = serde_json::to_string(chain).unwrap_or_else(|_| "[]".to_owned());
    CString::new(encoded).unwrap_or_else(|_| CString::new("[]").expect("static string"))
}

/// Host services, reachable from the plugin's thread of execution.
///
/// Never panics: a panic here would abort the host, so every path is wrapped.
unsafe extern "C" fn host_call(
    ctx: *mut c_void,
    method: *const c_char,
    params_json: *const c_char,
    chain_json: *const c_char,
) -> *mut c_char {
    originlang_ext_plugin_abi::guard(|| {
        if ctx.is_null() {
            return Err((
                code::INTERNAL_ERROR as i32,
                "host context is missing".to_owned(),
            ));
        }
        // SAFETY: `ctx` was set to a boxed `HostContext` that outlives the plugin.
        let host = unsafe { &*(ctx as *const HostContext) };
        // SAFETY: the plugin passes NUL-terminated strings for the call duration.
        let method = unsafe { read_c_str(method) }
            .ok_or((code::INVALID_PARAMS as i32, "method is null".to_owned()))?;
        // SAFETY: same contract as above.
        let params = unsafe { read_json(params_json) };
        // SAFETY: same contract as above.
        let chain = match unsafe { read_json(chain_json) } {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };

        host.bridge
            .call(&method, params, &chain)
            .map_err(|error| (error.code as i32, error.message))
    })
}

/// Frees a string the host handed to the plugin.
unsafe extern "C" fn host_string_free(ptr: *mut c_char) {
    // SAFETY: the pointer came from `plugin_abi::to_c_string` on this side.
    unsafe { free_c_string(ptr) }
}

/// Records a log line. Best effort: a failing logger must not fail the call.
unsafe extern "C" fn host_log(ctx: *mut c_void, level: *const c_char, message: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() {
            return;
        }
        // SAFETY: `ctx` was set to a boxed `HostContext` that outlives the plugin.
        let host = unsafe { &*(ctx as *const HostContext) };
        // SAFETY: the plugin passes NUL-terminated strings for the call duration.
        let level = unsafe { read_c_str(level) }.unwrap_or_else(|| "info".to_owned());
        // SAFETY: same contract as above.
        let message = unsafe { read_c_str(message) }.unwrap_or_default();
        host.bridge.log(&level, &message);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use originlang_ext_runtime::{HostBridge, RuntimeLimits};
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Arc;

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

    fn spec(entry: &Path) -> RuntimeSpec<'_> {
        RuntimeSpec {
            id: "test",
            backend: "dylib",
            entry,
            limits: RuntimeLimits::default(),
            bridge: Arc::new(NoBridge),
        }
    }

    /// `Box<dyn Runtime>` is not `Debug`, so `expect_err` cannot be used.
    fn start_error(entry: &Path) -> RpcError {
        match DylibProvider.start(spec(entry)) {
            Ok(_) => panic!("start should have failed"),
            Err(error) => error,
        }
    }

    #[test]
    fn a_missing_library_is_reported_as_a_load_failure() {
        let error = start_error(Path::new("no-such-plugin.dll"));
        assert_eq!(error.code, code::EXTENSION_LOAD_FAILED);
        assert!(error.message.contains("cannot load"), "{}", error.message);
    }

    #[test]
    fn a_file_that_is_not_a_library_is_rejected() {
        // A text file with the right extension still fails the loader's own
        // format check, which is what keeps a corrupted download from being
        // mistaken for a plugin.
        let path = std::env::temp_dir().join(format!("oext-not-a-lib-{}.dll", std::process::id()));
        std::fs::write(&path, b"not a portable executable").expect("write fixture");

        let error = start_error(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(error.code, code::EXTENSION_LOAD_FAILED);
        assert!(error.message.contains("cannot load"), "{}", error.message);
    }
}
