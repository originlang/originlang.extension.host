//! `oext-sandbox` — one extension instance, isolated in its own process.
//!
//! The process is deliberately dumb: it owns a JavaScript engine and a pipe, and
//! nothing else. It cannot read the install directory, the policy file or the
//! storage container, because none of that code is linked in — the binary's
//! dependency tree is `protocol` + `runtime` + `runtime-quickjs`. Every side
//! effect an extension wants is a `host.*` request written to stdout and answered
//! by the parent, which is where capabilities are enforced.
//!
//! Protocol (one JSON object per line, same envelope as the client-facing
//! transport, so there is only one wire format in the project):
//!
//! ```text
//! parent -> child   extension.load     {source, limits}      (first frame)
//! parent -> child   extension.invoke   {method, params}
//! parent -> child   extension.event    {topic, payload, from}
//! parent -> child   extension.shutdown
//! child  -> parent  host.<anything>    {..}                  (Host API call)
//! ```
//!
//! A child never decides whether it may do something. It asks, and the parent
//! answers with a result or an error; an error crosses back into JavaScript as a
//! thrown `Error` with the host's code attached.

use originlang_ext_protocol::{
    Message, RpcError, code, error_response, method, read_message, request, response, write_line,
};
use originlang_ext_runtime::RuntimeLimits;
use originlang_ext_runtime_quickjs::{Engine, HostFn};
use serde_json::{Value, json};
use std::io::{BufReader, BufWriter};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Exit code for a fatal protocol error; the parent reports it as a load failure.
const EXIT_PROTOCOL: i32 = 2;

/// Exit code for a clean shutdown.
const EXIT_OK: i32 = 0;

fn main() {
    let code = match run() {
        Ok(()) => EXIT_OK,
        Err(error) => {
            // stderr belongs to the host's console, not to the protocol.
            eprintln!("oext-sandbox: {error}");
            EXIT_PROTOCOL
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), RpcError> {
    let mut link = Link::new();
    let load = match link.next()? {
        Some(Message::Request {
            method, params, id, ..
        }) if method == method::EXTENSION_LOAD => (id, params),
        Some(_) => return Err(protocol("first frame must be extension.load")),
        None => return Ok(()),
    };
    let (load_id, load_params) = load;
    let limits = parse_limits(&load_params)?;
    let source = load_params
        .as_ref()
        .and_then(|value| value.get("source"))
        .and_then(Value::as_str)
        .ok_or_else(|| protocol("extension.load requires a 'source' string"))?
        .to_owned();

    let shared = Arc::new(Mutex::new(link));
    let host_link = shared.clone();
    let host: HostFn = Box::new(move |call_method, call_params, _chain| {
        host_link
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .host_call(call_method, call_params)
    });

    // Evaluating the entry script happens here, so a syntax error is reported as
    // a failed load rather than as a mysterious first-call failure.
    let engine = match Engine::start(source.as_bytes(), limits, host) {
        Ok(engine) => engine,
        Err(error) => {
            let mut link = shared.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(id) = load_id {
                link.reply(id, Err(error))?;
            }
            return Ok(());
        }
    };
    {
        let mut link = shared.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = load_id {
            link.reply(id, Ok(json!({ "pid": std::process::id() })))?;
        }
    }

    loop {
        let message = shared.lock().unwrap_or_else(|e| e.into_inner()).next()?;
        let Some(Message::Request {
            id, method, params, ..
        }) = message
        else {
            // Notifications carry no reply; a response is either stale or a bug.
            continue;
        };
        let outcome = dispatch(&engine, &method, params);
        // An interrupted engine is not reusable: report the timeout, then exit
        // so the parent starts a fresh process for the next call.
        let stop = method == method::EXTENSION_SHUTDOWN
            || matches!(&outcome, Err(error) if error.code == code::TIMEOUT);
        if let Some(id) = id {
            shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reply(id, outcome)?;
        }
        if stop {
            return Ok(());
        }
    }
}

fn dispatch(engine: &Engine, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
    match method {
        method::EXTENSION_INVOKE => {
            let call = params.as_ref().ok_or_else(|| invalid_params("invoke"))?;
            let target = call
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("invoke.method"))?;
            let call_params = call.get("params").cloned();
            let chain = parse_chain(call);
            engine.invoke(target, call_params, &chain)
        }
        // `extension.register`, `extension.event`, `extension.shutdown` and
        // `extension.ping` are handled by the JS bootstrap itself.
        other => {
            let chain = params.as_ref().map(parse_chain).unwrap_or_default();
            engine.invoke(other, params, &chain)
        }
    }
}

fn parse_chain(value: &Value) -> Vec<String> {
    value
        .get("chain")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_limits(params: &Option<Value>) -> Result<RuntimeLimits, RpcError> {
    let default = RuntimeLimits::default();
    let limits = params.as_ref().and_then(|value| value.get("limits"));
    let number = |name: &str| -> Option<u64> {
        limits
            .and_then(|value| value.get(name))
            .and_then(Value::as_u64)
    };
    Ok(RuntimeLimits {
        memory_bytes: number("memoryBytes")
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(default.memory_bytes),
        stack_bytes: number("stackBytes")
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(default.stack_bytes),
        timeout: number("timeoutMs")
            .map(Duration::from_millis)
            .unwrap_or(default.timeout),
    })
}

/// Newline-delimited JSON-RPC over the process's own stdio.
///
/// Single-threaded by construction and guarded by one mutex: the engine calls
/// back into it during an invocation, and the main loop uses it between
/// invocations. The two never overlap.
struct Link {
    reader: BufReader<std::io::Stdin>,
    writer: BufWriter<std::io::Stdout>,
    next_id: u64,
}

impl Link {
    fn new() -> Self {
        Self {
            reader: BufReader::new(std::io::stdin()),
            writer: BufWriter::new(std::io::stdout()),
            next_id: 1,
        }
    }

    fn next(&mut self) -> Result<Option<Message>, RpcError> {
        read_message(&mut self.reader)
    }

    fn send(&mut self, value: &Value) -> Result<(), RpcError> {
        write_line(&mut self.writer, value)
    }

    fn reply(&mut self, id: Value, outcome: Result<Value, RpcError>) -> Result<(), RpcError> {
        let message = match outcome {
            Ok(value) => response(id, value),
            Err(error) => error_response(id, &error),
        };
        self.send(&message)
    }

    /// Performs one Host API call and waits for the parent's answer.
    ///
    /// Frames that are not the awaited response are refused with `REJECTED`
    /// instead of being executed: re-entering the engine while a call is already
    /// on the stack is exactly the reentrancy the host forbids, and answering
    /// lets the parent see why instead of hanging.
    fn host_call(&mut self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&request(id, method, params))?;
        loop {
            match self.next()? {
                None => {
                    return Err(RpcError::new(
                        code::EXTENSION_NOT_LOADED,
                        "host closed the sandbox channel",
                    ));
                }
                Some(Message::Response {
                    id: reply_id,
                    result,
                    error,
                    ..
                }) if reply_id.as_u64() == Some(id) => {
                    if let Some(error) = error {
                        return Err(error);
                    }
                    return Ok(result.unwrap_or(Value::Null));
                }
                Some(Message::Response { .. }) => continue,
                Some(Message::Request {
                    id: inbound_id,
                    method: inbound_method,
                    ..
                }) => {
                    // The parent serializes invocations, so this only happens if
                    // it is broken or hostile; refuse without touching the engine.
                    if let Some(inbound_id) = inbound_id {
                        let error = RpcError::new(
                            code::REJECTED,
                            format!("sandbox is busy; '{inbound_method}' was not started"),
                        );
                        self.send(&error_response(inbound_id, &error))?;
                    }
                }
            }
        }
    }
}

fn protocol(message: impl Into<String>) -> RpcError {
    RpcError::new(code::INVALID_REQUEST, message)
}

fn invalid_params(name: &str) -> RpcError {
    RpcError::new(code::INVALID_PARAMS, format!("invalid {name} frame"))
}
