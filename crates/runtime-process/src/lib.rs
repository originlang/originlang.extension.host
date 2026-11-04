//! Out-of-process extension backend.
//!
//! Each instance is a separate `oext-sandbox` process. The parent keeps three
//! things the child cannot be trusted with — the policy, the storage container
//! and the package directory — and exposes only the Host API over a pipe. A bug
//! or a deliberate attack inside the sandbox therefore has to cross a process
//! boundary to reach anything, and the crossing point is exactly where
//! capabilities are checked.
//!
//! Consequences worth stating explicitly:
//!
//! - **Capability checks do not move.** The child asks; the parent decides. The
//!   child cannot grant itself anything, because it has no copy of the policy.
//! - **Crash isolation.** A native crash inside the sandbox kills the sandbox.
//!   The host sees a closed pipe, retires the instance and keeps serving.
//! - **Timeout isolation.** The engine interrupts itself first; if it cannot
//!   (a wedged native frame), the parent kills the process, which always works.
//! - **No orphans.** On Windows the child is placed in a Job Object with
//!   kill-on-close, so a crashed host takes its sandboxes with it. On Linux the
//!   child asks for `SIGKILL` when its parent dies.

mod job;

use originlang_ext_protocol::{
    Message, RpcError, code, error_response, method, read_message, request, response, write_line,
};
use originlang_ext_runtime::{
    CallChain, Event, ExtensionInfo, HostBridge, Runtime, RuntimeProvider, RuntimeSpec,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

type Reply = Result<Value, RpcError>;

/// The parent's half of the pipe.
///
/// Shared because two threads write to it: the caller (requests) and the reader
/// thread (answers to Host API calls). Taking the writer out drops the pipe,
/// which is how the child is told to stop.
type Wire = Arc<Mutex<Option<BufWriter<ChildStdin>>>>;

/// Backend identifier reported by diagnostics.
pub const KIND: &str = "sandbox";

/// Environment variable overriding the sandbox executable path.
pub const BIN_ENV: &str = "OEXT_SANDBOX_BIN";

/// Extra wall-clock allowance on top of the engine deadline. The child enforces
/// its own deadline; this is the backstop for a child that cannot enforce it.
const BACKSTOP: Duration = Duration::from_secs(2);

/// Spawns sandbox processes.
#[derive(Debug, Clone)]
pub struct SandboxProvider {
    binary: PathBuf,
}

impl SandboxProvider {
    /// Creates a provider for an explicit executable path.
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// Locates the sandbox executable.
    ///
    /// Lookup order is `OEXT_SANDBOX_BIN`, then a sibling of the running
    /// executable. Both are plain paths, not a `PATH` search: the sandbox is a
    /// trusted component of this installation, and resolving it through `PATH`
    /// would let any directory earlier in the list supply the binary.
    pub fn discover() -> Result<Self, RpcError> {
        if let Some(path) = std::env::var_os(BIN_ENV) {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Ok(Self::new(path));
            }
            return Err(missing(&format!(
                "{BIN_ENV} points at {}, which is not a file",
                path.display()
            )));
        }
        let current = std::env::current_exe().map_err(|e| {
            RpcError::new(code::OPERATION_FAILED, format!("locate host binary: {e}"))
        })?;
        let name = if cfg!(windows) {
            "oext-sandbox.exe"
        } else {
            "oext-sandbox"
        };
        for candidate in siblings(&current, name) {
            if candidate.is_file() {
                return Ok(Self::new(candidate));
            }
        }
        Err(missing(&format!(
            "no sandbox binary next to {} (build it with `cargo build -p originlang-ext-sandbox`, \
             or set {BIN_ENV})",
            current.display()
        )))
    }

    /// Path of the sandbox executable.
    pub fn binary(&self) -> &Path {
        &self.binary
    }
}
fn missing(message: &str) -> RpcError {
    RpcError::new(code::EXTENSION_LOAD_FAILED, message)
}

/// Candidate locations of the sandbox binary relative to `current`.
///
/// The second entry matters for tests: a `cargo test` binary lives in
/// `target/debug/deps/`, while binaries live one level up in `target/debug/`.
/// `PATH` is never consulted — see [`SandboxProvider::discover`].
fn siblings(current: &Path, name: &str) -> Vec<PathBuf> {
    let mut candidates = vec![current.with_file_name(name)];
    if let Some(dir) = current.parent()
        && dir.file_name().is_some_and(|dir| dir == "deps")
        && let Some(parent) = dir.parent()
    {
        candidates.push(parent.join(name));
    }
    candidates
}

impl RuntimeProvider for SandboxProvider {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn start(&self, spec: RuntimeSpec<'_>) -> Result<Box<dyn Runtime>, RpcError> {
        SandboxRuntime::start(&self.binary, spec)
            .map(|runtime| Box::new(runtime) as Box<dyn Runtime>)
    }
}

/// One sandboxed extension instance.
pub struct SandboxRuntime {
    child: Child,
    wire: Wire,
    pending: Arc<Mutex<HashMap<u64, mpsc::SyncSender<Reply>>>>,
    next_id: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    /// Caller stack of the invocation currently in flight. Read by the reader
    /// thread when the child calls back, so the host sees one authoritative
    /// stack even though the call now runs on a different thread.
    chain: Arc<Mutex<Vec<String>>>,
    timeout: Duration,
    id: String,
    /// Keeps the Windows Job Object alive for exactly as long as the child.
    _job: Option<job::Job>,
}

impl SandboxRuntime {
    /// Starts a sandbox for `spec` using the executable at `binary`.
    pub fn start(binary: &Path, spec: RuntimeSpec<'_>) -> Result<Self, RpcError> {
        let source = std::fs::read(spec.entry)
            .map_err(|e| missing(&format!("read entry {}: {e}", spec.entry.display())))?;
        // Lossy conversion would silently alter the program; a JavaScript source
        // file is UTF-8 or it is broken.
        let source = String::from_utf8(source).map_err(|_| {
            missing(&format!(
                "entry {} is not valid UTF-8",
                spec.entry.display()
            ))
        })?;
        let limits = spec.limits;

        let mut command = Command::new(binary);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is diagnostic output for a human; keeping it separate means
            // a panic in the sandbox never corrupts the protocol stream.
            .stderr(Stdio::inherit());
        job::configure(&mut command);

        let mut child = command
            .spawn()
            .map_err(|e| missing(&format!("cannot start sandbox {}: {e}", binary.display())))?;

        let job = match job::Job::attach(child.id(), limits.memory_bytes) {
            Ok(job) => job,
            Err(error) => {
                // Refusing to run would turn a hardening step into an outage:
                // the deadline, the memory limit inside QuickJS and `Drop` all
                // still apply. Say so loudly instead of failing silently.
                spec.bridge.log(
                    "warn",
                    &format!(
                        "sandbox for '{}' is not confined by the OS ({error}); \
                         it will not be reaped automatically if the host dies",
                        spec.id
                    ),
                );
                None
            }
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| missing("sandbox has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| missing("sandbox has no stdout"))?;

        let wire: Wire = Arc::new(Mutex::new(Some(BufWriter::new(stdin))));
        let pending: Arc<Mutex<HashMap<u64, mpsc::SyncSender<Reply>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let chain = Arc::new(Mutex::new(Vec::new()));
        let next_id = Arc::new(AtomicU64::new(1));

        let reader = Reader {
            stdout: BufReader::new(stdout),
            wire: wire.clone(),
            pending: pending.clone(),
            alive: alive.clone(),
            chain: chain.clone(),
            bridge: spec.bridge.clone(),
            id: spec.id.to_owned(),
        };
        std::thread::Builder::new()
            .name(format!("sandbox-io-{}", spec.id))
            .spawn(move || reader.run())
            .map_err(|e| {
                let _ = child.kill();
                missing(&format!("cannot start the sandbox reader: {e}"))
            })?;

        let mut runtime = Self {
            child,
            wire,
            pending,
            next_id,
            alive,
            chain,
            timeout: limits.timeout,
            id: spec.id.to_owned(),
            _job: job,
        };

        // The child evaluates the entry script before answering this frame, so a
        // syntax error surfaces here rather than on the first call.
        let loaded = runtime.send(
            method::EXTENSION_LOAD,
            Some(json!({
                "id": spec.id,
                "source": source,
                "limits": {
                    "memoryBytes": limits.memory_bytes,
                    "stackBytes": limits.stack_bytes,
                    "timeoutMs": limits.timeout.as_millis() as u64,
                },
            })),
            Duration::from_millis(0),
        )?;
        drop(loaded);
        Ok(runtime)
    }

    /// Sends one request and waits for its reply.
    ///
    /// `extra` widens the deadline; evaluation and shutdown are allowed more
    /// slack than a normal call because they are not bounded by the engine's own
    /// interrupt handler.
    fn send(&mut self, method: &str, params: Option<Value>, extra: Duration) -> Reply {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(RpcError::new(
                code::EXTENSION_NOT_LOADED,
                "sandbox process is gone",
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (reply, receiver) = mpsc::sync_channel::<Reply>(1);
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, reply);
        }
        if let Err(error) = write(&self.wire, &request(id, method, params)) {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&id);
            }
            self.alive.store(false, Ordering::SeqCst);
            return Err(error);
        }
        match receiver.recv_timeout(self.timeout + BACKSTOP + extra) {
            Ok(reply) => reply,
            Err(_) => {
                // A child that missed its own deadline cannot be trusted to
                // finish this call, and an interrupted engine is not reusable:
                // kill it, which is the one mechanism that always works.
                self.kill();
                Err(RpcError::new(
                    code::TIMEOUT,
                    format!("sandbox for '{}' exceeded {:?}", self.id, self.timeout),
                ))
            }
        }
    }

    fn kill(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Operating-system process id of the sandbox.
    ///
    /// Exposed for diagnostics and for tests that need to simulate a crash; the
    /// host itself never uses it.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    fn begin_call(&self, chain: &CallChain) {
        if let Ok(mut current) = self.chain.lock() {
            current.clear();
            current.extend_from_slice(chain);
        }
    }
}

impl Runtime for SandboxRuntime {
    fn register(&mut self) -> Result<ExtensionInfo, RpcError> {
        let value = self.send(method::EXTENSION_REGISTER, None, Duration::from_secs(0))?;
        serde_json::from_value(value).map_err(|e| {
            RpcError::new(
                code::OPERATION_FAILED,
                format!("invalid registration payload: {e}"),
            )
        })
    }

    fn call(
        &mut self,
        target: &str,
        params: Option<Value>,
        chain: &CallChain,
    ) -> Result<Value, RpcError> {
        self.begin_call(chain);
        self.send(
            method::EXTENSION_INVOKE,
            Some(json!({ "method": target, "params": params, "chain": chain })),
            Duration::from_secs(0),
        )
    }

    fn emit(&mut self, event: Event, chain: &CallChain) -> Result<(), RpcError> {
        self.begin_call(chain);
        let mut params = serde_json::to_value(&event)
            .map_err(|e| RpcError::new(code::OPERATION_FAILED, format!("encode event: {e}")))?;
        if let Some(object) = params.as_object_mut() {
            object.insert("chain".into(), json!(chain));
        }
        self.send(
            method::EXTENSION_EVENT,
            Some(params),
            Duration::from_secs(0),
        )
        .map(|_| ())
    }

    fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn shutdown(&mut self) -> Result<(), RpcError> {
        let outcome = self
            .send(method::EXTENSION_SHUTDOWN, None, Duration::from_secs(1))
            .map(|_| ());
        // Closing stdin is the polite half of shutdown: the child's read loop
        // sees EOF and exits, so a clean exit needs no signal.
        close(&self.wire);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.alive.store(false, Ordering::SeqCst);
                    return outcome;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        self.kill();
        outcome
    }
}

impl Drop for SandboxRuntime {
    fn drop(&mut self) {
        // `shutdown` normally ran; if it did not (an error path, a panic) the
        // process would outlive its instance. Kill it rather than leak it.
        if self.alive.load(Ordering::SeqCst) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Reads replies and serves Host API calls until the pipe closes.
struct Reader {
    stdout: BufReader<ChildStdout>,
    wire: Wire,
    pending: Arc<Mutex<HashMap<u64, mpsc::SyncSender<Reply>>>>,
    alive: Arc<AtomicBool>,
    chain: Arc<Mutex<Vec<String>>>,
    bridge: Arc<dyn HostBridge>,
    id: String,
}

impl Reader {
    fn run(mut self) {
        loop {
            match read_message(&mut self.stdout) {
                Ok(Some(Message::Response {
                    id, result, error, ..
                })) => {
                    let Some(id) = id.as_u64() else { continue };
                    let reply = match error {
                        Some(error) => Err(error),
                        None => Ok(result.unwrap_or(Value::Null)),
                    };
                    if let Some(sender) = self
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id)
                    {
                        let _ = sender.send(reply);
                    }
                }
                Ok(Some(Message::Request {
                    id, method, params, ..
                })) => {
                    let chain = self
                        .chain
                        .lock()
                        .map(|chain| chain.clone())
                        .unwrap_or_default();
                    // Authorization happens here, in the parent, on a request
                    // the child cannot forge an identity for.
                    let outcome = self.bridge.call(&method, params, &chain);
                    if let Some(id) = id {
                        let message = match outcome {
                            Ok(value) => response(id, value),
                            Err(error) => error_response(id, &error),
                        };
                        let _ = write(&self.wire, &message);
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    // A malformed frame means we can no longer find frame
                    // boundaries: stop reading instead of guessing.
                    self.bridge.log(
                        "error",
                        &format!("sandbox for '{}' broke the protocol: {error}", self.id),
                    );
                    break;
                }
            }
        }
        self.alive.store(false, Ordering::SeqCst);
        // Nobody will answer these, and a caller waiting on a dead pipe would
        // otherwise sit until its timeout.
        let stalled: Vec<_> = self
            .pending
            .lock()
            .map(|mut pending| pending.drain().map(|(_, sender)| sender).collect())
            .unwrap_or_default();
        for sender in stalled {
            let _ = sender.send(Err(RpcError::new(
                code::EXTENSION_NOT_LOADED,
                "sandbox process exited",
            )));
        }
    }
}

fn write(wire: &Wire, message: &Value) -> Result<(), RpcError> {
    let mut guard = wire
        .lock()
        .map_err(|_| RpcError::new(code::INTERNAL_ERROR, "sandbox writer lock is poisoned"))?;
    let writer = guard
        .as_mut()
        .ok_or_else(|| RpcError::new(code::EXTENSION_NOT_LOADED, "sandbox channel is closed"))?;
    write_line(writer, message)
}

/// Drops the writer, which closes the child's stdin and ends its read loop.
fn close(wire: &Wire) {
    if let Ok(mut guard) = wire.lock()
        && let Some(mut writer) = guard.take()
    {
        let _ = writer.flush();
    }
}
