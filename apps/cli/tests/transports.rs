//! Exercise real HTTP and stdio clients against the CLI process.
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    root: PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn start(transport: &str) -> Server {
    let root = std::env::temp_dir().join(format!(
        "oext-transport-{transport}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let child = Command::new(env!("CARGO_BIN_EXE_oext"))
        .args(["--backend", "quickjs", "--home"])
        .arg(root.join("home"))
        .args(["serve", "--transport", transport, "--no-startup"])
        .env("OEXT_STATE_DIR", root.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Server { child, root }
}
fn http(
    state: &Value,
    method: &str,
    path: &str,
    token: &str,
    content_type: &str,
    body: &str,
) -> (u16, String) {
    let mut stream =
        TcpStream::connect(("127.0.0.1", state["port"].as_u64().unwrap() as u16)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body.to_owned(),
    )
}
#[test]
fn http_auth_dispatch_validation_and_shutdown() {
    let mut server = start("http");
    let deadline = Instant::now() + Duration::from_secs(15);
    let state: Value = loop {
        if let Ok(bytes) = std::fs::read(server.root.join("state/host.json")) {
            break serde_json::from_slice(&bytes).unwrap();
        }
        assert!(Instant::now() < deadline, "discovery file not created");
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "server exited early"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(state["transport"], "http");
    let token = state["token"].as_str().unwrap();
    let call = |body: &str| http(&state, "POST", "/rpc", token, "application/json", body);
    let info = r#"{"jsonrpc":"2.0","id":1,"method":"host.info"}"#;
    assert_eq!(
        http(&state, "POST", "/rpc", "wrong", "application/json", info).0,
        401
    );
    assert_eq!(
        http(&state, "GET", "/rpc", token, "application/json", "").0,
        405
    );
    assert_eq!(
        http(&state, "POST", "/wrong", token, "application/json", info).0,
        404
    );
    assert_eq!(
        http(&state, "POST", "/rpc", token, "text/plain", info).0,
        415
    );
    let pretty =
        serde_json::to_string_pretty(&serde_json::from_str::<Value>(info).unwrap()).unwrap();
    let (status, body) = call(&pretty);
    assert_eq!(status, 200);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["id"], 1);
    assert!(value.get("result").is_some());
    for (body, code) in [
        ("{", -32700),
        ("[]", -32600),
        (r#"{"jsonrpc":"1.0","method":"host.info"}"#, -32600),
        (r#"{"jsonrpc":"2.0","id":1,"result":42}"#, -32600),
    ] {
        let (status, body) = call(body);
        assert_eq!(status, 400);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            code
        );
    }
    let (_, body) = call(r#"{"jsonrpc":"2.0","id":2,"method":"extension.invoke"}"#);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
        -32601
    );
    assert_eq!(
        call(r#"{"jsonrpc":"2.0","method":"host.info"}"#),
        (204, String::new())
    );
    assert_eq!(
        call(&" ".repeat(originlang_ext_protocol::MAX_MESSAGE_BYTES + 1)).0,
        413
    );
    let (status, _) = call(&json!({"jsonrpc":"2.0","id":3,"method":"host.shutdown"}).to_string());
    assert_eq!(status, 200);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "shutdown timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!server.root.join("state/host.json").exists());
}
#[test]
fn stdio_and_std_alias_preserve_ndjson() {
    for transport in ["stdio", "std"] {
        let mut server = start(transport);
        let mut stdin = server.child.stdin.take().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"host.info"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":2,"method":"host.shutdown"})
        )
        .unwrap();
        drop(stdin);
        let mut stdout = String::new();
        server
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        assert!(server.child.wait().unwrap().success());
        let messages: Vec<Value> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["id"], 1);
        assert_eq!(messages[1]["result"]["stopping"], true);
    }
}
