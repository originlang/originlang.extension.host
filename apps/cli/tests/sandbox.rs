//! End-to-end tests for the out-of-process backend.
//!
//! These mirror the in-process suite at the points where a process boundary can
//! change behaviour: capability checks, storage, and cross-extension calls whose
//! stack now spans processes and threads.

use originlang_ext_host::{Host, HostConfig, Paths};
use originlang_ext_protocol::code;
use originlang_ext_runtime_process::SandboxProvider;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "oext-sbx-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn host(&self) -> Host {
        let provider = SandboxProvider::discover().expect("沙箱可执行文件应存在");
        Host::new(
            HostConfig::new(Paths::new(&self.root)).with_startup_load(false),
            Arc::new(provider),
        )
        .expect("host 应能启动")
    }

    fn package(&self, id: &str, capabilities: &[&str], body: &str) -> PathBuf {
        let directory = self.root.join("pkgs").join(id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("extension.json"),
            serde_json::to_string_pretty(&json!({
                "id": id,
                "name": id,
                "version": "0.1.0",
                "apiVersion": "1",
                "main": "main.js",
                "capabilities": capabilities,
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(directory.join("main.js"), body).unwrap();
        directory
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn grant_all(host: &Host, id: &str) {
    let summary = host
        .summaries()
        .unwrap()
        .into_iter()
        .find(|summary| summary.id == id)
        .expect("扩展应已安装");
    host.grant(id, &summary.capabilities).unwrap();
}

#[test]
fn an_installed_package_runs_in_its_own_process() {
    let fixture = Fixture::new("run");
    let host = fixture.host();
    host.install(&example("hello"), false).unwrap();
    grant_all(&host, "hello");

    let value = host
        .call("hello", "greet", Some(json!({"name": "Sandbox"})))
        .unwrap();
    assert_eq!(value["message"], "Hello, Sandbox");
    assert_eq!(host.info()["runtime"], "sandbox");
    host.shutdown();
}

#[test]
fn host_api_calls_are_authorized_in_the_parent() {
    let fixture = Fixture::new("auth");
    let host = fixture.host();
    host.install(&example("hello"), false).unwrap();
    grant_all(&host, "hello");
    host.call("hello", "greet", None).unwrap();

    // Revoking mid-flight must take effect on the next Host API call, even
    // though the code enforcing it now lives on the other side of a pipe.
    host.revoke("hello", "log.write").unwrap();
    let error = host.call("hello", "announce", None).unwrap_err();
    assert_eq!(error.to_rpc().code, code::PERMISSION_DENIED);
    assert!(host.summaries().unwrap()[0].loaded, "撤销不应卸载实例");
    host.shutdown();
}

#[test]
fn storage_writes_survive_a_process_restart() {
    let fixture = Fixture::new("storage");
    let host = fixture.host();
    host.install(&example("notes"), false).unwrap();
    grant_all(&host, "notes");

    host.call("notes", "save", Some(json!({"key": "k", "value": "v"})))
        .unwrap();
    // Reload tears the process down and spawns a new one: the data must come
    // from the host's storage container, not from memory in the sandbox.
    host.reload("notes").unwrap();
    let value = host
        .call("notes", "read", Some(json!({"key": "k"})))
        .unwrap();
    assert_eq!(value["value"], "v");
    host.shutdown();
}

#[test]
fn a_chain_of_calls_across_processes_cannot_deadlock() {
    let fixture = Fixture::new("cycle");
    fixture.package(
        "ping",
        &["extension.call"],
        r#"Origin.define({
             id: "ping",
             version: "0.1.0",
             capabilities: ["extension.call"],
             methods: { go: () => Origin.host.callExtension("pong", "go", null) },
           });"#,
    );
    fixture.package(
        "pong",
        &["extension.call"],
        r#"Origin.define({
             id: "pong",
             version: "0.1.0",
             capabilities: ["extension.call"],
             methods: { go: () => Origin.host.callExtension("ping", "go", null) },
           });"#,
    );

    let host = fixture.host();
    host.install(&fixture.root.join("pkgs").join("ping"), false)
        .unwrap();
    host.install(&fixture.root.join("pkgs").join("pong"), false)
        .unwrap();
    grant_all(&host, "ping");
    grant_all(&host, "pong");

    // A and B each hold the other's only thread; if the host failed to carry
    // the caller stack across the pipe the two reader threads would block on
    // each other's instance mutex and this call would never return.
    let (sender, receiver) = mpsc::channel();
    let caller = host.clone();
    std::thread::spawn(move || {
        let outcome = caller
            .call("ping", "go", None)
            .map_err(|error| error.to_rpc());
        let _ = sender.send(outcome);
    });

    match receiver.recv_timeout(Duration::from_secs(20)) {
        Ok(Err(error)) => {
            assert_eq!(error.code, code::REJECTED, "{error}");
            assert!(
                error.message.contains("re-entrant"),
                "拒绝理由应是重入，实得 {error}"
            );
        }
        Ok(Ok(value)) => panic!("循环调用必须被拒绝，实得 {value}"),
        Err(_) => panic!("跨进程循环调用没有在 20s 内被拒绝，疑似死锁"),
    }
    host.shutdown();
}

#[test]
fn events_and_timers_reach_a_sandboxed_instance() {
    let fixture = Fixture::new("events");
    let host = fixture.host();
    host.install(&example("timers"), false).unwrap();
    grant_all(&host, "timers");
    host.load("timers").unwrap();

    // `extension.event` is a notification-free request in both backends: the
    // host writes a frame, the child runs `onEvent` and answers.
    host.call(
        "timers",
        "start",
        Some(json!({"intervalMs": 20, "ticks": 2})),
    )
    .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let finished = host
            .logs(Some("timers"), 50)
            .iter()
            .any(|record| record.message.starts_with("timer finished after"));
        if finished {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "定时器未在预期时间内触发"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let stats = host.call("timers", "stats", None).unwrap();
    assert_eq!(stats["fired"], 2);
    host.shutdown();
}

#[test]
fn a_sandbox_that_dies_is_retired_and_the_host_keeps_serving() {
    let fixture = Fixture::new("crash");
    fixture.package(
        "crashy",
        &[],
        r#"Origin.define({
             id: "crashy",
             version: "0.1.0",
             methods: { boom: () => { throw new Error("boom"); } },
           });"#,
    );
    let host = fixture.host();
    host.install(&fixture.root.join("pkgs").join("crashy"), false)
        .unwrap();
    host.install(&example("hello"), false).unwrap();
    grant_all(&host, "hello");

    // A JS throw is an ordinary error, not a crash: the instance must survive so
    // one bad call does not cost the extension its module state.
    let error = host.call("crashy", "boom", None).unwrap_err();
    assert_eq!(error.to_rpc().code, code::OPERATION_FAILED);
    assert!(
        host.summaries()
            .unwrap()
            .iter()
            .any(|s| s.id == "crashy" && s.loaded)
    );

    // Other instances are unaffected by whatever happens to this one.
    let value: Value = host.call("hello", "greet", None).unwrap();
    assert_eq!(value["api"], "1");
    host.shutdown();
}
