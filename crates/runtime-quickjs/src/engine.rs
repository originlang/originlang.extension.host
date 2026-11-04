//! The QuickJS engine itself, independent of how its host calls travel.
//!
//! `Engine` owns one `JS Runtime` + `Context` and must stay on the thread that
//! created it: `rquickjs` handles are not `Send`. Both backends respect that —
//! the in-process provider parks an engine on a dedicated worker thread, the
//! sandbox binary owns one on its main thread.
//!
//! Everything an extension can reach is the callback passed to
//! [`Engine::start`]: `__oextHost(method, json)` in JavaScript. Whether that
//! callback is a direct call into the host process or a round-trip over a pipe
//! is decided by the caller, not here.

use originlang_ext_protocol::{RpcError, code};
use originlang_ext_runtime::{CallChain, RuntimeLimits};
use rquickjs::{Context, Function, Promise, Runtime as JsRuntime};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Callback `__oextHost` routes to. Receives the method, the parameters and the
/// caller's stack, and returns either a result or an error to surface in JS.
pub type HostFn =
    Box<dyn Fn(&str, Option<Value>, &CallChain) -> Result<Value, RpcError> + Send + Sync>;

/// One initialized QuickJS instance.
pub struct Engine {
    // Field order matters for drop order: the context must go before the
    // runtime it was created from.
    context: Context,
    _runtime: JsRuntime,
    deadline: Arc<Mutex<Instant>>,
    chain: Arc<Mutex<Vec<String>>>,
    timeout: Duration,
}

impl Engine {
    /// Creates the engine, installs the host callback and evaluates `source`.
    ///
    /// A syntax error surfaces here rather than on the first invocation, so a
    /// broken package fails at load time with a usable message.
    pub fn start(source: &[u8], limits: RuntimeLimits, host: HostFn) -> Result<Self, RpcError> {
        let runtime =
            JsRuntime::new().map_err(|e| error(format!("create QuickJS runtime: {e}")))?;
        runtime.set_memory_limit(limits.memory_bytes);
        runtime.set_max_stack_size(limits.stack_bytes);

        // The interrupt handler only compares against a timestamp: it runs at
        // arbitrary bytecode boundaries and must not allocate or lock widely.
        let deadline = Arc::new(Mutex::new(Instant::now() + limits.timeout));
        let limit = deadline.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            Instant::now() >= *limit.lock().unwrap_or_else(|e| e.into_inner())
        })));

        let chain = Arc::new(Mutex::new(Vec::new()));
        let context =
            Context::full(&runtime).map_err(|e| error(format!("create QuickJS context: {e}")))?;

        let callback_chain = chain.clone();
        context
            .with(|ctx| -> rquickjs::Result<()> {
                let callback = Function::new(
                    ctx.clone(),
                    move |method: String, params: String| -> String {
                        let chain = callback_chain
                            .lock()
                            .map(|chain| chain.clone())
                            .unwrap_or_default();
                        let result = serde_json::from_str::<Value>(&params)
                            .map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))
                            .and_then(|params| host(&method, Some(params), &chain));
                        match result {
                            Ok(value) => json!({"result": value}).to_string(),
                            Err(error) => json!({"error": error}).to_string(),
                        }
                    },
                )?;
                ctx.globals().set("__oextHost", callback)?;
                ctx.eval::<(), _>(include_str!("bootstrap.js"))?;
                ctx.eval::<(), _>(source)?;
                Ok(())
            })
            .map_err(|e| error(format!("initialize JS extension: {e}")))?;

        Ok(Self {
            context,
            _runtime: runtime,
            deadline,
            chain,
            timeout: limits.timeout,
        })
    }

    /// Wall-clock ceiling of one invocation.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Invokes one method, giving it a fresh deadline and the caller's stack.
    pub fn invoke(
        &self,
        method: &str,
        params: Option<Value>,
        chain: &CallChain,
    ) -> Result<Value, RpcError> {
        {
            let mut current = self.chain.lock().unwrap_or_else(|e| e.into_inner());
            current.clear();
            current.extend_from_slice(chain);
        }
        *self.deadline.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now() + self.timeout;
        self.evaluate(method, params)
    }

    fn expired(&self) -> bool {
        Instant::now() >= *self.deadline.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn evaluate(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        self.context.with(|ctx| -> Result<Value, RpcError> {
            if self.expired() {
                return Err(timeout_error());
            }
            // Method and params are both passed as JSON strings: the JS side
            // parses them, so no value is ever spliced into source text.
            let method = serde_json::to_string(method).map_err(|e| error(e.to_string()))?;
            let payload = serde_json::to_string(&params.unwrap_or(Value::Null))
                .map_err(|e| error(e.to_string()))?;
            let payload = serde_json::to_string(&payload).map_err(|e| error(e.to_string()))?;
            let promise: Promise = ctx
                .eval(format!("__oextInvoke({method}, {payload})"))
                .map_err(|e| {
                    if self.expired() {
                        timeout_error()
                    } else {
                        error(format!("JS invocation: {e}"))
                    }
                })?;

            // A promise with no pending job that has not settled can never
            // settle inside this engine: report that instead of spinning.
            let reply: String = loop {
                if self.expired() {
                    return Err(timeout_error());
                }
                if let Some(result) = promise.result::<String>() {
                    break result.map_err(|e| error(format!("JS promise rejected: {e}")))?;
                }
                if !ctx.execute_pending_job() {
                    return Err(error("JS promise never settled and has no pending job"));
                }
            };

            let reply: Value = serde_json::from_str(&reply).map_err(|e| error(e.to_string()))?;
            if self.expired() {
                return Err(timeout_error());
            }
            if let Some(error_value) = reply.get("error") {
                return Err(serde_json::from_value(error_value.clone())
                    .map_err(|e| error(e.to_string()))?);
            }
            Ok(reply.get("result").cloned().unwrap_or(Value::Null))
        })
    }
}

fn error(message: impl Into<String>) -> RpcError {
    RpcError::new(code::OPERATION_FAILED, message)
}

fn timeout_error() -> RpcError {
    RpcError::new(code::TIMEOUT, "JS execution timed out")
}
