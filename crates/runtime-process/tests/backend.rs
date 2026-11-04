//! Backend-level tests: the properties a process boundary is supposed to buy.
//!
//! Each test drives a real `oext-sandbox` process, so they are slower than the
//! in-process backend's tests and deliberately so: the point of this crate is
//! the boundary, and a mock would not have one.

use originlang_ext_protocol::{RpcError, code};
use originlang_ext_runtime::{
    CallChain, HostBridge, Runtime, RuntimeLimits, RuntimeProvider, RuntimeSpec,
};
use originlang_ext_runtime_process::{SandboxProvider, SandboxRuntime};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A JavaScript entry point written to a temporary file.
struct Source(PathBuf);

impl Source {
    fn new(text: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "oext-sandbox-{}-{}-{}.js",
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Records what the child asked for and refuses everything else.
struct Recorder {
    asked: Mutex<Vec<String>>,
    chains: Mutex<Vec<Vec<String>>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
            chains: Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl HostBridge for Recorder {
    fn call(
        &self,
        method: &str,
        params: Option<Value>,
        chain: &CallChain,
    ) -> Result<Value, RpcError> {
        self.asked.lock().unwrap().push(method.to_owned());
        self.chains.lock().unwrap().push(chain.to_vec());
        match method {
            "host.self.info" => Ok(json!({ "id": "test", "params": params })),
            _ => Err(RpcError::new(code::PERMISSION_DENIED, "denied by test")),
        }
    }

    fn log(&self, _level: &str, _message: &str) {}
}

fn start(
    source: &Source,
    limits: RuntimeLimits,
    bridge: Arc<dyn HostBridge>,
) -> Result<SandboxRuntime, RpcError> {
    let provider = SandboxProvider::discover().expect("沙箱可执行文件应存在");
    SandboxRuntime::start(
        provider.binary(),
        RuntimeSpec {
            id: "test",
            backend: "sandbox",
            entry: source.path(),
            limits,
            bridge,
        },
    )
}

#[test]
fn provider_reports_the_sandbox_kind_and_a_usable_binary() {
    let provider = SandboxProvider::discover().expect("沙箱可执行文件应存在");
    assert_eq!(provider.kind(), "sandbox");
    assert!(
        provider.binary().is_file(),
        "{}",
        provider.binary().display()
    );
}

#[test]
fn calls_round_trip_and_host_requests_cross_the_boundary() {
    let source = Source::new(
        r#"let calls = 0;
           Origin.define({
             id: "test",
             version: "0.1.0",
             methods: {
               echo: (params) => {
                 calls += 1;
                 return { params, calls, node: typeof process };
               },
               whoami: () => Origin.host.info(),
             },
           });"#,
    );
    let recorder = Arc::new(Recorder::new());
    let mut runtime = start(&source, RuntimeLimits::default(), recorder.clone()).unwrap();

    let info = runtime.register().unwrap();
    assert_eq!(info.methods, vec!["echo", "whoami"]);

    let first = runtime
        .call("echo", Some(json!({"value": 1})), &["test".to_owned()])
        .unwrap();
    assert_eq!(first["calls"], 1);
    assert_eq!(first["node"], "undefined", "沙箱内不应有 Node 全局对象");

    let second = runtime.call("echo", None, &[]).unwrap();
    assert_eq!(second["calls"], 2, "同一实例应保留模块状态");

    let who = runtime.call("whoami", None, &["test".to_owned()]).unwrap();
    assert_eq!(who["id"], "test", "Host API 应由父进程应答");
    assert_eq!(recorder.asked(), vec!["host.self.info".to_owned()]);
    assert_eq!(
        recorder.chains.lock().unwrap().last().cloned(),
        Some(vec!["test".to_owned()]),
        "父进程应收到调用方栈"
    );

    runtime.shutdown().unwrap();
    assert!(!runtime.is_running());
}

#[test]
fn capability_denials_come_back_as_errors_inside_the_sandbox() {
    let source = Source::new(
        r#"Origin.define({
             id: "test",
             version: "0.1.0",
             methods: {
               write: (_, host) => {
                 try {
                   host.log("hello");
                   return { denied: false };
                 } catch (error) {
                   return { denied: true, code: error.code };
                 }
               },
             },
           });"#,
    );
    let mut runtime = start(&source, RuntimeLimits::default(), Arc::new(Recorder::new())).unwrap();
    let value = runtime.call("write", None, &[]).unwrap();
    assert_eq!(value["denied"], true, "父进程的拒绝必须穿过进程边界到达 JS");
    assert_eq!(value["code"], code::PERMISSION_DENIED);
    runtime.shutdown().unwrap();
}

#[test]
fn a_runaway_invocation_is_stopped_and_the_instance_retired() {
    let source = Source::new(
        r#"Origin.define({
             id: "test",
             version: "0.1.0",
             methods: { spin: () => { while (true) {} } },
           });"#,
    );
    let limits = RuntimeLimits {
        timeout: Duration::from_millis(200),
        ..RuntimeLimits::default()
    };
    let mut runtime = start(&source, limits, Arc::new(Recorder::new())).unwrap();
    let started = Instant::now();
    let error = runtime.call("spin", None, &[]).unwrap_err();
    assert_eq!(error.code, code::TIMEOUT);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "超时应在预算内结束，实际 {:?}",
        started.elapsed()
    );
    // The worker exits after an interrupt, so the host must see a dead instance
    // and retire it instead of reusing a possibly-broken engine.
    std::thread::sleep(Duration::from_millis(100));
    assert!(!runtime.is_running(), "超时后实例应被判定为不可用");
}

#[test]
fn a_crashed_sandbox_is_detected_instead_of_hanging_the_caller() {
    let source = Source::new(r#"Origin.define({id: "test", version: "0.1.0"});"#);
    let mut runtime = start(&source, RuntimeLimits::default(), Arc::new(Recorder::new())).unwrap();
    runtime.register().unwrap();

    kill(runtime.pid());
    // The reader thread notices the closed pipe and fails whatever is pending.
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.is_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!runtime.is_running(), "进程退出应被观测到");

    let error = runtime.call("anything", None, &[]).unwrap_err();
    assert_eq!(error.code, code::EXTENSION_NOT_LOADED);
}

#[test]
fn a_syntax_error_fails_the_load_without_leaving_a_process() {
    let source = Source::new("Origin.define({ this is not javascript");
    let error = match start(&source, RuntimeLimits::default(), Arc::new(Recorder::new())) {
        Err(error) => error,
        Ok(_) => panic!("语法错误应在加载阶段失败"),
    };
    assert_eq!(error.code, code::OPERATION_FAILED);
    assert!(error.message.contains("initialize JS extension"));
}

/// Kills a process by pid, so a test can simulate a native crash.
fn kill(pid: u32) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("taskkill 应可执行");
        assert!(status.success(), "taskkill 失败");
    }
    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("kill 应可执行");
        assert!(status.success(), "kill 失败");
    }
}
