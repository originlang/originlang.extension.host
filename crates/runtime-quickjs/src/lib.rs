//! In-process QuickJS backend.
//!
//! One worker thread per extension, each owning its own `Engine`. Contexts never
//! cross thread boundaries: the host talks to the worker through a channel, which
//! also serializes execution.
//!
//! Isolation properties, stated precisely because they bound what the capability
//! model can promise:
//!
//! - No Node, no `require`, no `process`, no filesystem or network bindings.
//!   The only exit is the `__oextHost` callback, which routes into
//!   [`HostBridge`] where capabilities are enforced.
//! - Heap, stack and wall-clock limits are applied per instance. A timed-out
//!   invocation interrupts the engine and retires the worker, because an
//!   interrupted engine must not be reused.
//! - The engine still runs in-process: a QuickJS memory-safety bug is a host
//!   bug. Use the sandbox backend (`originlang-ext-runtime-process`) when that
//!   matters; this one is kept for trusted packages and for tests that need to
//!   observe the engine directly.

mod engine;

pub use engine::{Engine, HostFn};

use originlang_ext_protocol::{RpcError, code};
use originlang_ext_runtime::{
    CallChain, Event, ExtensionInfo, HostBridge, Runtime, RuntimeLimits, RuntimeProvider,
    RuntimeSpec,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

type Reply = Result<Value, RpcError>;

struct Request {
    method: String,
    params: Option<Value>,
    chain: Vec<String>,
    reply: mpsc::SyncSender<Reply>,
}

/// Backend identifier reported by diagnostics.
pub const KIND: &str = "quickjs";

/// In-process QuickJS [`RuntimeProvider`].
#[derive(Debug, Default, Clone, Copy)]
pub struct QuickJsProvider;

impl QuickJsProvider {
    /// Creates a provider.
    pub fn new() -> Self {
        Self
    }
}

impl RuntimeProvider for QuickJsProvider {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn start(&self, spec: RuntimeSpec<'_>) -> Result<Box<dyn Runtime>, RpcError> {
        QuickJsRuntime::load(spec).map(|runtime| Box::new(runtime) as Box<dyn Runtime>)
    }
}

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new(code::OPERATION_FAILED, message)
}

/// One live QuickJS instance.
pub struct QuickJsRuntime {
    sender: mpsc::Sender<Request>,
    alive: Arc<AtomicBool>,
    timeout: Duration,
}

impl QuickJsRuntime {
    /// Starts the worker and waits for the entry script to finish evaluating.
    fn load(spec: RuntimeSpec<'_>) -> Result<Self, RpcError> {
        let source = std::fs::read(spec.entry)
            .map_err(|e| error(format!("read entry {}: {e}", spec.entry.display())))?;
        let limits = spec.limits;
        let bridge = spec.bridge.clone();
        let id = spec.id.to_owned();

        let (sender, receiver) = mpsc::channel::<Request>();
        let (ready_sender, ready_receiver) = mpsc::sync_channel::<Result<(), RpcError>>(1);
        let alive = Arc::new(AtomicBool::new(true));
        let worker_alive = alive.clone();

        std::thread::Builder::new()
            .name(format!("quickjs-{id}"))
            .spawn(move || worker(source, bridge, limits, worker_alive, ready_sender, receiver))
            .map_err(|e| error(format!("start QuickJS worker: {e}")))?;

        // The worker reports initialization once, after evaluating the entry
        // script, so a syntax error surfaces as a load failure rather than as a
        // mysterious first-call failure.
        match ready_receiver.recv_timeout(limits.timeout + Duration::from_secs(1)) {
            Ok(result) => result?,
            Err(e) => return Err(error(format!("initialize QuickJS worker: {e}"))),
        }

        Ok(Self {
            sender,
            alive,
            timeout: limits.timeout,
        })
    }

    fn send(
        &self,
        method: &str,
        params: Option<Value>,
        chain: &CallChain,
    ) -> Result<Reply, RpcError> {
        let (reply, receiver) = mpsc::sync_channel::<Reply>(1);
        self.sender
            .send(Request {
                method: method.to_owned(),
                params,
                chain: chain.to_vec(),
                reply,
            })
            .map_err(|_| RpcError::new(code::EXTENSION_NOT_LOADED, "QuickJS worker stopped"))?;
        // The extra second over the engine deadline covers the worker's own
        // bookkeeping; beyond that the worker is considered wedged.
        let reply = receiver
            .recv_timeout(self.timeout + Duration::from_secs(1))
            .map_err(|e| RpcError::new(code::TIMEOUT, format!("QuickJS worker: {e}")))?;
        Ok(reply)
    }
}

impl Runtime for QuickJsRuntime {
    fn register(&mut self) -> Result<ExtensionInfo, RpcError> {
        let value = self.send("extension.register", None, &[])??;
        serde_json::from_value(value)
            .map_err(|e| error(format!("invalid registration payload: {e}")))
    }

    fn call(
        &mut self,
        method: &str,
        params: Option<Value>,
        chain: &CallChain,
    ) -> Result<Value, RpcError> {
        self.send(method, params, chain)?
    }

    fn emit(&mut self, event: Event, chain: &CallChain) -> Result<(), RpcError> {
        let params = serde_json::to_value(&event).map_err(|e| error(e.to_string()))?;
        self.call(
            originlang_ext_protocol::method::EXTENSION_EVENT,
            Some(params),
            chain,
        )
        .map(|_| ())
    }

    fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn shutdown(&mut self) -> Result<(), RpcError> {
        self.call(
            originlang_ext_protocol::method::EXTENSION_SHUTDOWN,
            None,
            &[],
        )
        .map(|_| ())
    }
}

fn worker(
    source: Vec<u8>,
    bridge: Arc<dyn HostBridge>,
    limits: RuntimeLimits,
    alive: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<(), RpcError>>,
    receiver: mpsc::Receiver<Request>,
) {
    struct Alive(Arc<AtomicBool>);
    impl Drop for Alive {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let _alive = Alive(alive);

    let host: HostFn = Box::new(move |method, params, chain| bridge.call(method, params, chain));
    let engine = match Engine::start(&source, limits, host) {
        Ok(engine) => {
            let _ = ready.send(Ok(()));
            engine
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    for request in receiver {
        let result = engine.invoke(&request.method, request.params, &request.chain);
        let retire = request.method == originlang_ext_protocol::method::EXTENSION_SHUTDOWN
            || result
                .as_ref()
                .err()
                .is_some_and(|error| error.code == code::TIMEOUT);
        let _ = request.reply.send(result);
        if retire {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use originlang_ext_runtime::RuntimeLimits;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Source(PathBuf);

    impl Source {
        fn new(text: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "oext-js-{}-{}-{}.js",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, text).unwrap();
            Self(path)
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    struct NoHost;
    impl HostBridge for NoHost {
        fn call(
            &self,
            _method: &str,
            _params: Option<Value>,
            _chain: &CallChain,
        ) -> Result<Value, RpcError> {
            Err(RpcError::new(code::PERMISSION_DENIED, "denied"))
        }
        fn log(&self, _level: &str, _message: &str) {}
    }

    fn start(path: &Path, limits: RuntimeLimits) -> Result<Box<dyn Runtime>, RpcError> {
        QuickJsProvider.start(RuntimeSpec {
            id: "test",
            backend: "quickjs",
            entry: path,
            limits,
            bridge: Arc::new(NoHost),
        })
    }

    #[test]
    fn runs_async_handlers_and_keeps_module_state() {
        let source = Source::new(
            r#"let calls = 0;
               Origin.define({
                 id: "test",
                 version: "0.1.0",
                 methods: {
                   echo: async (params) => {
                     calls += 1;
                     return {
                       params,
                       calls,
                       node: typeof process,
                       require: typeof require,
                     };
                   },
                 },
               });"#,
        );
        let mut runtime = start(&source.0, RuntimeLimits::default()).unwrap();
        let info = runtime.register().unwrap();
        assert_eq!(info.id, "test");
        assert_eq!(info.methods, vec!["echo"]);

        let first = runtime
            .call("echo", Some(json!({"value": 1})), &[])
            .unwrap();
        assert_eq!(first["calls"], 1);
        assert_eq!(first["node"], "undefined", "沙箱内不应存在 Node 全局对象");
        assert_eq!(first["require"], "undefined");
        assert_eq!(runtime.call("echo", None, &[]).unwrap()["calls"], 2);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn host_denials_cross_the_js_boundary_with_their_code() {
        let source = Source::new(
            r#"Origin.define({
                 id: "test",
                 version: "0.1.0",
                 methods: { touch: (_, host) => host.log("nope") },
               });"#,
        );
        let mut runtime = start(&source.0, RuntimeLimits::default()).unwrap();
        let error = runtime.call("touch", None, &[]).unwrap_err();
        assert_eq!(error.code, code::PERMISSION_DENIED);
    }

    #[test]
    fn the_host_sees_the_caller_stack() {
        // The chain must reach the bridge unchanged: it is the only way the host
        // can reject a cycle after the call crossed a thread boundary.
        let source = Source::new(
            r#"Origin.define({
                 id: "test",
                 version: "0.1.0",
                 methods: { probe: () => Origin.host.call("host.self.info", null) },
               });"#,
        );
        struct ChainProbe;
        impl HostBridge for ChainProbe {
            fn call(
                &self,
                _method: &str,
                _params: Option<Value>,
                chain: &CallChain,
            ) -> Result<Value, RpcError> {
                Ok(json!(chain))
            }
            fn log(&self, _level: &str, _message: &str) {}
        }
        let mut runtime = QuickJsProvider
            .start(RuntimeSpec {
                id: "test",
                backend: "quickjs",
                entry: &source.0,
                limits: RuntimeLimits::default(),
                bridge: Arc::new(ChainProbe),
            })
            .unwrap();
        let chain = runtime
            .call("probe", None, &["outer".to_owned(), "test".to_owned()])
            .unwrap();
        assert_eq!(chain, json!(["outer", "test"]));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn unsupported_methods_report_method_not_found() {
        let source = Source::new(r#"Origin.define({id: "test", version: "0.1.0"});"#);
        let mut runtime = start(&source.0, RuntimeLimits::default()).unwrap();
        let error = runtime.call("missing", None, &[]).unwrap_err();
        assert_eq!(error.code, code::METHOD_NOT_FOUND);
    }

    #[test]
    fn infinite_loop_is_interrupted_within_the_deadline() {
        let source = Source::new(
            r#"Origin.define({
                 id: "test",
                 version: "0.1.0",
                 methods: { spin: () => { while (true) {} } },
               });"#,
        );
        let limits = RuntimeLimits {
            timeout: Duration::from_millis(150),
            ..RuntimeLimits::default()
        };
        let mut runtime = start(&source.0, limits).unwrap();
        let started = std::time::Instant::now();
        let error = runtime.call("spin", None, &[]).unwrap_err();
        assert_eq!(error.code, code::TIMEOUT);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "超时中断应立即生效，实际耗时 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn syntax_errors_fail_at_load_time() {
        let source = Source::new("Origin.define({ this is not javascript");
        let error = match start(&source.0, RuntimeLimits::default()) {
            Err(error) => error,
            Ok(_) => panic!("语法错误应在加载阶段失败"),
        };
        assert_eq!(error.code, code::OPERATION_FAILED);
        assert!(error.message.contains("initialize JS extension"));
    }

    #[test]
    fn extension_without_define_cannot_register() {
        let source = Source::new("globalThis.noop = true;");
        let mut runtime = start(&source.0, RuntimeLimits::default()).unwrap();
        let error = runtime.register().unwrap_err();
        assert!(error.message.contains("Origin.define"));
    }
}
