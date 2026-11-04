//! End-to-end tests: install, authorize, invoke, events, timers and integrity.

use originlang_ext_host::{Host, HostConfig, HostError, Paths};
use originlang_ext_protocol::code;
use originlang_ext_runtime_quickjs::QuickJsProvider;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Path of a bundled example package.
fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name)
}

/// Isolated installation root, removed after the test.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "oext-it-{tag}-{}-{}",
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
        Host::new(
            HostConfig::new(Paths::new(&self.root)).with_startup_load(false),
            Arc::new(QuickJsProvider::new()),
        )
        .expect("host 应能启动")
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
fn install_grant_invoke_revoke_and_reload() {
    let fixture = Fixture::new("invoke");
    {
        let host = fixture.host();
        let installed = host.install(&example("hello"), false).unwrap();
        assert_eq!(installed.id, "hello");
        assert!(installed.granted.is_empty(), "安装不应隐含授权");
        assert!(!installed.loaded);

        // 加载是授权门：声明的能力未全部授予时，连不调用 Host API 的方法也进不来
        let error = host.call("hello", "greet", None).unwrap_err();
        assert_eq!(error.to_rpc().code, code::PERMISSION_DENIED);
        assert!(!host.summaries().unwrap()[0].loaded);

        host.grant("hello", &["log.write".to_owned()]).unwrap();
        let value = host
            .call("hello", "greet", Some(json!({"name": "Origin"})))
            .unwrap();
        assert_eq!(value["message"], "Hello, Origin");
        assert_eq!(value["api"], "1");
        assert!(host.summaries().unwrap()[0].loaded);

        host.call("hello", "announce", Some(json!({"message": "granted"})))
            .unwrap();
        assert!(
            host.logs(Some("hello"), 20)
                .iter()
                .any(|record| record.message == "granted"),
            "授权后日志应被记录"
        );

        // 运行期撤销立即生效：能力检查在每次 Host API 调用上执行，而非只在加载时
        host.revoke("hello", "log.write").unwrap();
        let error = host.call("hello", "announce", None).unwrap_err();
        assert_eq!(error.to_rpc().code, code::PERMISSION_DENIED);
        assert!(host.summaries().unwrap()[0].loaded, "撤销不应卸载实例");

        host.grant("hello", &["log.write".to_owned()]).unwrap();
        host.reload("hello").unwrap();
        assert!(host.summaries().unwrap()[0].loaded);
        host.shutdown();
    }
}

#[test]
fn loading_requires_grants_for_every_declared_capability() {
    let fixture = Fixture::new("deny");
    {
        let host = fixture.host();
        host.install(&example("timers"), false).unwrap();
        let error = host.load("timers").unwrap_err();
        assert_eq!(error.to_rpc().code, code::PERMISSION_DENIED);
        assert!(!host.summaries().unwrap()[0].loaded);

        // 补授权后即可加载
        grant_all(&host, "timers");
        host.load("timers").unwrap();
        host.shutdown();
    }
}

#[test]
fn post_install_modification_is_detected_before_load() {
    let fixture = Fixture::new("tamper");
    {
        let host = fixture.host();
        host.install(&example("hello"), false).unwrap();
        grant_all(&host, "hello");

        let entry = host.paths().extensions_dir().join("hello").join("main.js");
        let mut body = fs::read_to_string(&entry).unwrap();
        body.push_str("\n// modified after install\n");
        fs::write(&entry, body).unwrap();

        let error = host.load("hello").unwrap_err();
        assert!(
            matches!(error, HostError::LoadFailed(_)),
            "内容哈希不符必须拒绝加载，实得 {error:?}"
        );
        host.shutdown();
    }
}

#[test]
fn emitting_an_event_activates_a_declared_subscriber() {
    let fixture = Fixture::new("events");
    {
        let host = fixture.host();
        host.install(&example("hello"), false).unwrap();
        grant_all(&host, "hello");
        assert!(!host.summaries().unwrap()[0].loaded);

        let delivered = host.emit("greeting", json!({"text": "hi"})).unwrap();
        assert_eq!(delivered, 1, "onEvent:greeting 应触发加载并投递");
        assert!(host.summaries().unwrap()[0].loaded);

        let logs = host.logs(Some("hello"), 20);
        assert!(
            logs.iter()
                .any(|record| record.message.contains("event: greeting")),
            "扩展应收到事件，实际日志: {logs:?}"
        );
        host.shutdown();
    }
}

#[test]
fn storage_is_namespaced_and_survives_reload() {
    let fixture = Fixture::new("storage");
    {
        let host = fixture.host();
        host.install(&example("notes"), false).unwrap();
        grant_all(&host, "notes");

        host.call(
            "notes",
            "save",
            Some(json!({"key": "theme", "value": "dark"})),
        )
        .unwrap();
        host.reload("notes").unwrap();

        let value = host
            .call("notes", "read", Some(json!({"key": "theme"})))
            .unwrap();
        assert_eq!(value["value"], "dark");

        let missing = host
            .call("notes", "read", Some(json!({"key": "absent"})))
            .unwrap();
        assert!(missing["value"].is_null(), "缺失键应返回 null 而不是异常");

        let listed = host.call("notes", "list", None).unwrap();
        assert_eq!(listed["notes"][0]["key"], "theme");
        host.shutdown();
    }
}

#[test]
fn timers_tick_and_the_extension_can_stop_them() {
    let fixture = Fixture::new("timers");
    {
        let host = fixture.host();
        host.install(&example("timers"), false).unwrap();
        grant_all(&host, "timers");
        host.load("timers").unwrap();

        let started = host
            .call(
                "timers",
                "start",
                Some(json!({"intervalMs": 20, "ticks": 2})),
            )
            .unwrap();
        let timer_id = started["timerId"].as_str().unwrap();
        assert!(timer_id.starts_with("timer:timers:"), "{timer_id}");

        // 等待扩展自身声明的完成日志，避免与定时器投递争抢同一个实例锁
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let finished = host
                .logs(Some("timers"), 50)
                .iter()
                .any(|record| record.message.starts_with("timer finished after"));
            if finished {
                break;
            }
            assert!(Instant::now() < deadline, "定时器未在预期时间内触发");
            std::thread::sleep(Duration::from_millis(20));
        }

        let stats = host.call("timers", "stats", None).unwrap();
        assert!(stats["timerId"].is_null(), "完成计次后应已清除定时器");
        assert_eq!(stats["fired"], 2);
        host.shutdown();
    }
}

#[test]
fn cross_extension_self_call_is_rejected_instead_of_deadlocking() {
    let fixture = Fixture::new("reentrant");
    {
        let source = fixture.root.join("pkg-selfcall");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("extension.json"),
            serde_json::to_string_pretty(&json!({
                "id": "selfcall",
                "name": "Self call",
                "version": "0.1.0",
                "apiVersion": "1",
                "main": "main.js",
                "capabilities": ["extension.call"]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            source.join("main.js"),
            r#"Origin.define({
                 id: "selfcall",
                 version: "0.1.0",
                 capabilities: ["extension.call"],
                 methods: {
                   loop: () => Origin.host.callExtension("selfcall", "loop", null),
                 },
               });"#,
        )
        .unwrap();

        let host = fixture.host();
        host.install(&source, false).unwrap();
        grant_all(&host, "selfcall");

        let error = host.call("selfcall", "loop", None).unwrap_err();
        assert_eq!(error.to_rpc().code, code::REJECTED);
        host.shutdown();
    }
}

#[test]
fn disabled_extensions_do_not_load_and_uninstall_clears_storage() {
    let fixture = Fixture::new("disable");
    {
        let host = fixture.host();
        host.install(&example("notes"), false).unwrap();
        grant_all(&host, "notes");
        host.call("notes", "save", Some(json!({"key": "k", "value": "v"})))
            .unwrap();

        host.set_enabled("notes", false).unwrap();
        assert_eq!(
            host.load("notes").unwrap_err().to_rpc().code,
            code::EXTENSION_DISABLED
        );

        host.set_enabled("notes", true).unwrap();
        host.uninstall("notes").unwrap();
        assert!(host.summaries().unwrap().is_empty());
        assert!(!host.paths().extensions_dir().join("notes").exists());
        host.shutdown();
    }
}

#[test]
fn missing_extensions_and_methods_have_distinct_codes() {
    let fixture = Fixture::new("codes");
    {
        let host = fixture.host();
        assert_eq!(
            host.call("nope", "anything", None)
                .unwrap_err()
                .to_rpc()
                .code,
            code::EXTENSION_NOT_FOUND
        );

        host.install(&example("hello"), false).unwrap();
        grant_all(&host, "hello");
        let error = host.call("hello", "nope", None).unwrap_err();
        assert_eq!(error.to_rpc().code, code::METHOD_NOT_FOUND);
        host.shutdown();
    }
}
