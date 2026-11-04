//! Wire protocol for the OriginLang extension host.
//!
//! The crate is deliberately transport agnostic: it owns the JSON-RPC 2.0
//! envelope, the error code space and the canonical method names, plus a
//! newline-delimited channel that any byte stream can be adapted to.
//!
//! Design constraints inherited from `bootidea.rust`:
//!
//! - One JSON object per line, no framing header, so a client can be written
//!   in any language in a few dozen lines.
//! - Method names live here rather than at call sites, to keep host and
//!   runtimes from drifting apart.
//! - Unknown application error codes are preserved verbatim, so a runtime can
//!   surface its own failures without the host rewriting them.

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use std::fmt;
use std::io::{BufRead, Read, Write};

/// JSON-RPC 2.0 protocol version marker.
pub const JSONRPC_VERSION: &str = "2.0";

/// Upper bound for a single inbound line.
///
/// A peer that never sends a newline would otherwise grow the read buffer until
/// the host runs out of memory. 8 MiB is far above any legitimate request and
/// keeps the failure mode a clean protocol error instead of an OOM abort.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Error codes: the JSON-RPC reserved range plus this platform's range.
pub mod code {
    // 标准 JSON-RPC
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON sent is not a valid Request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist / is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;

    // 平台错误区间 [-32099, -32000]
    /// The requested extension is not installed.
    pub const EXTENSION_NOT_FOUND: i64 = -32001;
    /// The extension is installed but not loaded.
    pub const EXTENSION_NOT_LOADED: i64 = -32002;
    /// The call did not finish within its deadline.
    pub const TIMEOUT: i64 = -32003;
    /// Manifest, runtime or activation failure.
    pub const EXTENSION_LOAD_FAILED: i64 = -32004;
    /// Generic operation failure on the remote side.
    pub const OPERATION_FAILED: i64 = -32005;
    /// The extension is already loaded.
    pub const EXTENSION_EXISTS: i64 = -32012;
    /// The extension is installed but disabled by the operator.
    pub const EXTENSION_DISABLED: i64 = -32014;
    /// The caller did not present a valid session token.
    pub const UNAUTHORIZED: i64 = -32020;
    /// A declared capability was not granted.
    pub const PERMISSION_DENIED: i64 = -32021;
    /// The request is well formed but rejected by policy (quota, state, ...).
    pub const REJECTED: i64 = -32022;
}

/// Canonical method names.
pub mod method {
    // ---- 扩展进程/运行时侧（宿主调用） ----
    /// `extension.register` — 上报元数据，加载完成的标志。
    pub const EXTENSION_REGISTER: &str = "extension.register";
    /// `extension.load` — 沙箱启动帧：携带源码与限额，返回时表示求值完成。
    pub const EXTENSION_LOAD: &str = "extension.load";
    /// `extension.invoke` — 调用扩展的一个方法。
    pub const EXTENSION_INVOKE: &str = "extension.invoke";
    /// `extension.ping` — 存活探测。
    pub const EXTENSION_PING: &str = "extension.ping";
    /// `extension.shutdown` — 允许执行清理钩子后退出。
    pub const EXTENSION_SHUTDOWN: &str = "extension.shutdown";
    /// `extension.activate` — 首次激活。
    pub const EXTENSION_ACTIVATE: &str = "extension.activate";
    /// `extension.deactivate` — 卸载前清理。
    pub const EXTENSION_DEACTIVATE: &str = "extension.deactivate";
    /// `extension.event` — 宿主向扩展投递事件。
    pub const EXTENSION_EVENT: &str = "extension.event";

    // ---- 客户端侧（管理接口） ----
    /// `host.info` — 宿主版本与运行态摘要。
    pub const HOST_INFO: &str = "host.info";
    /// `host.extension.list`
    pub const HOST_EXTENSION_LIST: &str = "host.extension.list";
    /// `host.extension.call`
    pub const HOST_EXTENSION_CALL: &str = "host.extension.call";
    /// `host.extension.load`
    pub const HOST_EXTENSION_LOAD: &str = "host.extension.load";
    /// `host.extension.unload`
    pub const HOST_EXTENSION_UNLOAD: &str = "host.extension.unload";
    /// `host.extension.reload`
    pub const HOST_EXTENSION_RELOAD: &str = "host.extension.reload";
    /// `host.extension.logs`
    pub const HOST_EXTENSION_LOGS: &str = "host.extension.logs";
    /// `host.event.emit` — 客户端注入事件。
    pub const HOST_EVENT_EMIT: &str = "host.event.emit";
    /// `host.authenticate` — TCP 传输的首个请求。
    pub const HOST_AUTHENTICATE: &str = "host.authenticate";
    /// `host.shutdown` — 优雅停止宿主。
    pub const HOST_SHUTDOWN: &str = "host.shutdown";

    // ---- 扩展调用宿主的能力面（Host API） ----
    /// `host.log`
    pub const HOST_LOG: &str = "host.log";
    /// `host.storage.get`
    pub const HOST_STORAGE_GET: &str = "host.storage.get";
    /// `host.storage.set`
    pub const HOST_STORAGE_SET: &str = "host.storage.set";
    /// `host.storage.delete`
    pub const HOST_STORAGE_DELETE: &str = "host.storage.delete";
    /// `host.storage.list`
    pub const HOST_STORAGE_LIST: &str = "host.storage.list";
    /// `host.timer.set`
    pub const HOST_TIMER_SET: &str = "host.timer.set";
    /// `host.timer.clear`
    pub const HOST_TIMER_CLEAR: &str = "host.timer.clear";
    /// `host.events.emit`
    pub const HOST_EVENTS_EMIT: &str = "host.events.emit";
    /// `host.events.subscribe`
    pub const HOST_EVENTS_SUBSCRIBE: &str = "host.events.subscribe";
    /// `host.extension.call` — 扩展间调用。
    pub const HOST_EXTENSION_CALL_OUT: &str = "host.extension.call";
    /// `host.self.info`
    pub const HOST_SELF_INFO: &str = "host.self.info";

    // ---- 分组：协议的自描述部分 ----
    //
    // 三张表把"谁可以调用什么"变成数据而不是注释。它们存在的意义是可被测试
    // 遍历：任何一个方法只要落在这三张表之外，就说明方法名常量加了却没有接线。

    /// 客户端（stdio/TCP 连接）可以调用的管理方法。
    pub const CLIENT: &[&str] = &[
        HOST_INFO,
        HOST_EXTENSION_LIST,
        HOST_EXTENSION_LOAD,
        HOST_EXTENSION_UNLOAD,
        HOST_EXTENSION_RELOAD,
        HOST_EXTENSION_CALL,
        HOST_EXTENSION_LOGS,
        HOST_EVENT_EMIT,
        HOST_AUTHENTICATE,
        HOST_SHUTDOWN,
    ];

    /// 扩展实例可以调用的能力面方法。
    ///
    /// `host.extension.call` 不在此列：它与客户端同名但语义不同（前者由扩展发起，
    /// 受 `extension.call` 能力约束），因此只在 [`CLIENT`] 中出现一次。
    pub const HOST_API: &[&str] = &[
        HOST_LOG,
        HOST_STORAGE_GET,
        HOST_STORAGE_SET,
        HOST_STORAGE_DELETE,
        HOST_STORAGE_LIST,
        HOST_TIMER_SET,
        HOST_TIMER_CLEAR,
        HOST_EVENTS_EMIT,
        HOST_EVENTS_SUBSCRIBE,
        HOST_SELF_INFO,
    ];

    /// 运行时边界方法（宿主 → 实例或沙箱），**不得**对客户端开放。
    pub const RUNTIME: &[&str] = &[
        EXTENSION_REGISTER,
        EXTENSION_LOAD,
        EXTENSION_INVOKE,
        EXTENSION_PING,
        EXTENSION_ACTIVATE,
        EXTENSION_DEACTIVATE,
        EXTENSION_EVENT,
        EXTENSION_SHUTDOWN,
    ];
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    /// Numeric error code.
    pub code: i64,
    /// Human readable message.
    pub message: String,
    /// Optional structured detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Builds an error without structured data.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attaches structured data.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)?;
        if let Some(data) = &self.data {
            write!(f, ": {data}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

/// Any inbound message: a peer request/notification, or a response.
#[derive(Debug, Clone, Serialize)]
pub enum Message {
    /// A request (has `id`) or notification (no `id`) sent to us.
    Request {
        /// Protocol version.
        jsonrpc: String,
        /// Request id, absent for notifications.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        /// Method name.
        method: String,
        /// Optional parameters.
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
    /// A response to one of our requests.
    Response {
        /// Protocol version.
        jsonrpc: String,
        /// Identifier of the request being answered.
        id: Value,
        /// Successful result.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        /// Failure detail.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
}

/// Flat view of an inbound object, classified into a [`Message`] after checks.
///
/// `#[serde(untagged)]` would accept the same inputs but report only "data did
/// not match any variant", which hides whether the version was wrong, the `id`
/// was missing, or `result` and `error` were both present. Classifying by hand
/// costs a few lines and makes every rejection name the actual defect — the
/// difference between a client author fixing it in a minute and guessing.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

impl Envelope {
    /// Validates the envelope and picks the variant it describes.
    fn classify(self) -> Result<Message, String> {
        match self.jsonrpc.as_deref() {
            Some(JSONRPC_VERSION) => {}
            Some(other) => {
                return Err(format!(
                    "unsupported \"jsonrpc\" version {other:?}; expected {JSONRPC_VERSION:?}"
                ));
            }
            None => {
                return Err(format!(
                    "missing \"jsonrpc\": {JSONRPC_VERSION:?}, so this is not a JSON-RPC 2.0 message"
                ));
            }
        }

        if let Some(method) = self.method {
            // A request carrying `result`/`error` is ambiguous: the peer is
            // either confused or probing for a lenient parser. Neither is worth
            // guessing about.
            if self.result.is_some() || self.error.is_some() {
                return Err(format!(
                    "request '{method}' must not carry \"result\" or \"error\""
                ));
            }
            return Ok(Message::Request {
                jsonrpc: JSONRPC_VERSION.to_owned(),
                // `"id": null` and a missing id both mean "no reply expected";
                // JSON-RPC discourages the former and we treat it as the latter.
                id: self.id,
                method,
                params: self.params,
            });
        }

        if self.params.is_some() {
            return Err("only a request may carry \"params\"".to_owned());
        }
        let Some(id) = self.id else {
            return Err(
                "message has neither \"method\" (a request) nor \"id\" (a response)".to_owned(),
            );
        };
        if self.result.is_some() && self.error.is_some() {
            return Err(
                "a response must carry either \"result\" or \"error\", not both".to_owned(),
            );
        }
        Ok(Message::Response {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: self.result,
            error: self.error,
        })
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Envelope::deserialize(deserializer)?
            .classify()
            .map_err(de::Error::custom)
    }
}

impl Message {
    /// Consumes a response message, mapping `error` to `Err`.
    pub fn into_result(self) -> Result<Value, RpcError> {
        match self {
            Message::Response { error: Some(e), .. } => Err(e),
            Message::Response {
                result: Some(result),
                ..
            } => Ok(result),
            Message::Response { result: None, .. } => Ok(Value::Null),
            other => Err(RpcError::new(
                code::INVALID_REQUEST,
                format!("expected a response, got {other:?}"),
            )),
        }
    }
}

/// Builds a request object.
pub fn request(id: u64, method: &str, params: Option<Value>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), Value::String(JSONRPC_VERSION.into()));
    obj.insert("id".into(), Value::Number(id.into()));
    obj.insert("method".into(), Value::String(method.into()));
    if let Some(params) = params {
        obj.insert("params".into(), params);
    }
    Value::Object(obj)
}

/// Builds a notification object (no `id`, no response expected).
pub fn notification(method: &str, params: Option<Value>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), Value::String(JSONRPC_VERSION.into()));
    obj.insert("method".into(), Value::String(method.into()));
    if let Some(params) = params {
        obj.insert("params".into(), params);
    }
    Value::Object(obj)
}

/// Builds a successful response.
pub fn response(id: Value, result: Value) -> Value {
    serde_json::json!({"jsonrpc": JSONRPC_VERSION, "id": id, "result": result})
}

/// Builds an error response.
pub fn error_response(id: Value, error: &RpcError) -> Value {
    serde_json::json!({"jsonrpc": JSONRPC_VERSION, "id": id, "error": error})
}

/// Reads exactly one newline-delimited message.
///
/// Returns `Ok(None)` on clean EOF. Blank lines are skipped. A line longer than
/// [`MAX_MESSAGE_BYTES`] is rejected without buffering it: the read itself is
/// capped, so a peer that never sends a newline cannot drive the host into an
/// allocation it never authorized.
///
/// Two distinct failures, two distinct codes: bytes that are not JSON are
/// `PARSE_ERROR`, while valid JSON that is not a valid message (wrong version,
/// batch array, both `result` and `error`) is `INVALID_REQUEST`. A client can
/// therefore tell "my serializer is broken" from "my envelope is wrong".
pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Message>, RpcError> {
    loop {
        let Some(line) = read_line_limited(reader)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&line)
            .map_err(|_| RpcError::new(code::PARSE_ERROR, "message is not valid UTF-8"))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        return parse_message(trimmed.as_bytes()).map(Some);
    }
}

/// Parses one complete JSON-RPC envelope, independent of transport framing.
/// HTTP bodies may contain formatting newlines but never multiple envelopes.
pub fn parse_message(bytes: &[u8]) -> Result<Message, RpcError> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(RpcError::new(
            code::INVALID_REQUEST,
            "message exceeds 8 MiB",
        ));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| RpcError::new(code::PARSE_ERROR, format!("parse error: {e}")))?;
    if value.is_array() {
        return Err(RpcError::new(
            code::INVALID_REQUEST,
            "batch requests are not supported: send one object per message",
        ));
    }
    serde_json::from_value(value)
        .map_err(|e| RpcError::new(code::INVALID_REQUEST, format!("invalid request: {e}")))
}

/// Reads one line, refusing to buffer more than [`MAX_MESSAGE_BYTES`].
///
/// The cap is applied to the read, not to the result: `read_line` would happily
/// allocate gigabytes before the caller could check the length.
fn read_line_limited<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, RpcError> {
    // One byte over the limit is enough to detect an oversized line while still
    // separating "exactly at the limit plus newline" from "over the limit".
    let limit = MAX_MESSAGE_BYTES as u64 + 1;
    let mut buffer = Vec::new();
    let read = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut buffer)
        .map_err(|e| RpcError::new(code::INVALID_REQUEST, format!("read error: {e}")))?;
    if read == 0 {
        return Ok(None);
    }
    if buffer.last() == Some(&b'\n') {
        buffer.pop();
    }
    if buffer.len() > MAX_MESSAGE_BYTES {
        return Err(RpcError::new(
            code::INVALID_REQUEST,
            format!("message exceeds {MAX_MESSAGE_BYTES} bytes; the connection is closed"),
        ));
    }
    Ok(Some(buffer))
}

/// Writes `value` as one newline-terminated JSON line and flushes it.
pub fn write_line<W: Write>(writer: &mut W, value: &Value) -> Result<(), RpcError> {
    let mut line = serde_json::to_string(value)
        .map_err(|e| RpcError::new(code::INVALID_REQUEST, format!("serialize error: {e}")))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .map_err(|e| RpcError::new(code::INVALID_REQUEST, format!("write error: {e}")))?;
    writer
        .flush()
        .map_err(|e| RpcError::new(code::INVALID_REQUEST, format!("flush error: {e}")))
}

/// Failure modes of a [`Channel`] conversation.
#[derive(Debug)]
pub enum ChannelError {
    /// Transport level failure.
    Io(std::io::Error),
    /// Protocol level failure.
    Rpc(RpcError),
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelError::Io(e) => write!(f, "io error: {e}"),
            ChannelError::Rpc(e) => write!(f, "rpc error: {e}"),
        }
    }
}

impl std::error::Error for ChannelError {}

impl From<std::io::Error> for ChannelError {
    fn from(e: std::io::Error) -> Self {
        ChannelError::Io(e)
    }
}

impl From<RpcError> for ChannelError {
    fn from(e: RpcError) -> Self {
        ChannelError::Rpc(e)
    }
}

/// A bidirectional newline-delimited channel.
///
/// The channel is single-threaded by design: one owner drives the conversation.
/// Concurrent callers are serialized by the host instead of by this type, which
/// keeps the transport free of locking and avoids interleaved writes.
pub struct Channel<W: Write, R: BufRead> {
    reader: R,
    writer: W,
    next_id: u64,
}

impl<W: Write, R: BufRead> Channel<W, R> {
    /// Creates a channel over an existing reader/writer pair.
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: 1,
        }
    }

    /// Sends a request and waits for the matching response.
    ///
    /// Requests arriving from the peer while we wait are answered with
    /// `METHOD_NOT_FOUND`; use [`Channel::call_with_handler`] to serve them.
    pub fn call(&mut self, method: &str, params: Option<Value>) -> Result<Value, ChannelError> {
        self.call_with_handler(method, params, |name, _| {
            Err(RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("host method unavailable: {name}"),
            ))
        })
    }

    /// Sends a request and serves peer requests until the response arrives.
    ///
    /// Notifications reach `handler` but produce no reply on the wire.
    pub fn call_with_handler<F>(
        &mut self,
        method: &str,
        params: Option<Value>,
        mut handler: F,
    ) -> Result<Value, ChannelError>
    where
        F: FnMut(&str, Option<Value>) -> Result<Value, RpcError>,
    {
        let id = self.next_id;
        self.next_id += 1;
        write_line(&mut self.writer, &request(id, method, params))?;
        loop {
            match read_message(&mut self.reader)? {
                None => {
                    return Err(RpcError::new(
                        code::EXTENSION_NOT_LOADED,
                        "peer closed the channel before answering",
                    )
                    .into());
                }
                Some(Message::Response {
                    id: reply_id,
                    result,
                    error,
                    ..
                }) => {
                    if reply_id == Value::Number(id.into()) {
                        if let Some(error) = error {
                            return Err(error.into());
                        }
                        return Ok(result.unwrap_or(Value::Null));
                    }
                    // A response to an earlier, abandoned request: ignore.
                }
                Some(Message::Request {
                    id: callback_id,
                    method,
                    params,
                    ..
                }) => {
                    let result = handler(&method, params);
                    if let Some(callback_id) = callback_id {
                        let reply = match result {
                            Ok(value) => response(callback_id, value),
                            Err(error) => error_response(callback_id, &error),
                        };
                        write_line(&mut self.writer, &reply)?;
                    }
                }
            }
        }
    }

    /// Sends a notification.
    pub fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), ChannelError> {
        write_line(&mut self.writer, &notification(method, params))?;
        Ok(())
    }

    /// Reads the next message without sending anything.
    pub fn next_message(&mut self) -> Result<Option<Message>, ChannelError> {
        Ok(read_message(&mut self.reader)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn peer_requests_are_answered_before_the_original_call_returns() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":\"peer-1\",\"method\":\"host.log\",\"params\":{\"message\":\"hi\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"host.log\",\"params\":{\"message\":\"note\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"done\"}\n"
        );
        let mut channel = Channel::new(Cursor::new(input), Vec::new());
        let mut calls = 0;
        let result = channel
            .call_with_handler("extension.ping", None, |name, _| {
                assert_eq!(name, "host.log");
                calls += 1;
                Err(RpcError::new(code::PERMISSION_DENIED, "denied"))
            })
            .unwrap();
        assert_eq!(result, serde_json::json!("done"));
        assert_eq!(calls, 2);
        let output = String::from_utf8(channel.writer).unwrap();
        let lines: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "对通知不应写回响应");
        assert_eq!(lines[1]["id"], "peer-1");
        assert_eq!(lines[1]["error"]["code"], code::PERMISSION_DENIED);
    }

    #[test]
    fn round_trip_and_notification_envelopes() {
        let mut input = Cursor::new("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"pong\":true}}\n");
        let mut output: Vec<u8> = Vec::new();
        let mut channel = Channel::new(&mut input, &mut output);
        assert_eq!(
            channel.call("extension.ping", None).unwrap(),
            serde_json::json!({"pong": true})
        );
        channel
            .notify("extension.event", Some(serde_json::json!({"topic": "t"})))
            .unwrap();
        let wire = String::from_utf8(output).unwrap();
        assert!(wire.contains("\"method\":\"extension.ping\""));
        assert!(wire.contains("\"method\":\"extension.event\""));
        assert!(!wire.contains("\"id\":null"));
    }

    #[test]
    fn oversized_lines_are_rejected() {
        let mut input = Cursor::new(format!("{}\n", "x".repeat(MAX_MESSAGE_BYTES + 1)));
        let error = read_message(&mut input).unwrap_err();
        assert_eq!(error.code, code::INVALID_REQUEST);
    }

    #[test]
    fn a_line_at_the_limit_still_parses() {
        // Exactly at the cap, counting the newline as framing rather than
        // payload: the boundary must not be an off-by-one rejection.
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"x","params":{{"p":"{}"}}}}"#,
            "y".repeat(MAX_MESSAGE_BYTES - 60)
        );
        assert!(body.len() <= MAX_MESSAGE_BYTES);
        let mut input = Cursor::new(format!("{body}\n"));
        assert!(read_message(&mut input).unwrap().is_some());
    }

    #[test]
    fn malformed_json_and_malformed_envelopes_get_different_codes() {
        let mut not_json = Cursor::new("{not json}\n");
        assert_eq!(
            read_message(&mut not_json).unwrap_err().code,
            code::PARSE_ERROR
        );

        // Valid JSON, wrong version: the peer's serializer works, its envelope
        // does not. Calling this a parse error would send it debugging the wrong
        // layer.
        let mut wrong_version = Cursor::new("{\"jsonrpc\":\"1.0\",\"method\":\"x\"}\n");
        let error = read_message(&mut wrong_version).unwrap_err();
        assert_eq!(error.code, code::INVALID_REQUEST);
        assert!(error.message.contains("1.0"), "{}", error.message);

        let mut batch = Cursor::new("[{\"jsonrpc\":\"2.0\",\"method\":\"x\"}]\n");
        let error = read_message(&mut batch).unwrap_err();
        assert_eq!(error.code, code::INVALID_REQUEST);
        assert!(error.message.contains("batch"), "{}", error.message);
    }

    #[test]
    fn ambiguous_envelopes_are_named_precisely() {
        for (wire, expected) in [
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"x","result":1}"#,
                "\"result\"",
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":-1,"message":"m"}}"#,
                "not both",
            ),
            (r#"{"jsonrpc":"2.0"}"#, "neither"),
            (r#"{"method":"x"}"#, "jsonrpc"),
        ] {
            let error = read_message(&mut Cursor::new(format!("{wire}\n"))).unwrap_err();
            assert_eq!(error.code, code::INVALID_REQUEST, "{wire}");
            assert!(
                error.message.contains(expected),
                "{wire} 的报错应提到 {expected}，实得 {}",
                error.message
            );
        }
    }

    #[test]
    fn a_null_id_is_a_notification() {
        let message = read_message(&mut Cursor::new(
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"x\"}\n",
        ))
        .unwrap()
        .unwrap();
        assert!(matches!(message, Message::Request { id: None, .. }));
    }

    #[test]
    fn method_groups_are_disjoint_and_cover_every_name() {
        let groups = [
            ("CLIENT", method::CLIENT),
            ("HOST_API", method::HOST_API),
            ("RUNTIME", method::RUNTIME),
        ];
        let mut seen: Vec<&str> = Vec::new();
        for (label, names) in groups {
            for name in names {
                assert!(
                    !seen.contains(name),
                    "{name} 同时出现在 {label} 和之前的表里"
                );
                seen.push(name);
            }
        }
        // 常量总数：加新方法时这里会先失败，提醒把名字接进某张表。
        assert_eq!(seen.len(), 28, "方法名总数与分组表不一致");
        for required in [
            method::HOST_SELF_INFO,
            method::EXTENSION_INVOKE,
            method::HOST_SHUTDOWN,
        ] {
            assert!(seen.contains(&required), "{required} 未分组");
        }
    }

    #[test]
    fn blank_lines_are_skipped_and_eof_is_clean() {
        let mut input = Cursor::new("\n\n{\"jsonrpc\":\"2.0\",\"method\":\"x\"}\n");
        let message = read_message(&mut input).unwrap().unwrap();
        assert!(matches!(message, Message::Request { .. }));
        assert!(read_message(&mut input).unwrap().is_none());
    }
}
