//! JSON-RPC transports for the independent host.
//!
//! Stdio and TCP use NDJSON; HTTP carries one envelope per body. Network
//! transports require a session token. This is the server-side transport moved
//! out of the `bootidea` CLI, plus the `host.hello` handshake and protocol
//! negotiation the service contract (§4) requires.

mod http;

use crate::config::{Settings, Transport};
use crate::provider;
use originlang_ext_host::Host;
use originlang_ext_protocol::{
    Message, PROTOCOL_VERSION, RpcError, code, error_response, method, read_message, response,
    write_line,
};
use originlang_ext_session as session;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::BufReader;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Failure modes that map onto the contract's exit codes (§9.3).
///
/// Argument/config errors are reported before serving begins (exit 2), so they
/// are not represented here.
#[derive(Debug)]
pub enum ServeError {
    /// Instance conflict or bind failure → exit 3.
    Conflict(String),
    /// Fatal stdio protocol error → exit 4.
    Protocol(String),
    /// Unclassified failure → exit 1.
    Failure(String),
}

/// Result of handling one request.
struct Outcome {
    result: Result<Value, RpcError>,
    stop: bool,
}

/// Serves on the configured transport.
pub fn serve(settings: &Settings) -> Result<(), ServeError> {
    // §2.1: when debug diagnostics are requested, report the final effective
    // values so a conflicting precedence chain is visible.
    if settings.log_level.is_debug() {
        eprintln!(
            "ol-host effective config: transport={:?} backend={:?} home={} require_signature={} startup={}",
            settings.transport,
            settings.backend,
            settings.home.display(),
            settings.require_signature,
            settings.startup
        );
    }
    match settings.transport {
        Transport::Stdio => serve_stdio(settings),
        Transport::Tcp => serve_network(settings, false),
        Transport::Http => serve_network(settings, true),
    }
}

fn serve_stdio(settings: &Settings) -> Result<(), ServeError> {
    let host = provider::open(settings).map_err(ServeError::Failure)?;
    host.startup().map_err(|error| ServeError::Failure(error.to_string()))?;

    eprintln!(
        "ol-host {} serving JSON-RPC on stdio (runtime: {})",
        crate::config::HOST_VERSION,
        host.runtime_kind()
    );

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    // Handshake state: the first business call on a stdio connection must be
    // `host.hello`; anything else is a policy rejection (-32022).
    let mut handshaken = false;

    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                // The stream is no longer trustworthy: report and stop rather
                // than guessing where the next frame begins.
                let _ = write_line(&mut writer, &error_response(Value::Null, &error));
                host.shutdown();
                return Err(ServeError::Protocol(error.message));
            }
        };
        let Message::Request {
            id, method, params, ..
        } = message
        else {
            continue;
        };

        // Protocol negotiation is the first gate on stdio.
        if !handshaken {
            if method != method::HOST_HELLO {
                let error = RpcError::new(
                    code::REJECTED,
                    "host.hello must be the first call on a connection",
                )
                .with_data(json!({ "reason": "handshake_required" }));
                if let Some(id) = id {
                    let _ = write_line(&mut writer, &error_response(id, &error));
                }
                // A well-formed peer that skipped the handshake is rejected and
                // the connection closes cleanly; this is not a protocol fault.
                break;
            }
            match handshake(&host, &params) {
                Ok(payload) => {
                    if let Some(id) = id {
                        write_line(&mut writer, &response(id, payload))
                            .map_err(|error| ServeError::Protocol(error.message))?;
                    }
                    handshaken = true;
                }
                Err(error) => {
                    // Version mismatch is a rejected negotiation, not a fatal
                    // protocol error: report -32023 and close cleanly (§4.3).
                    if let Some(id) = id {
                        let _ = write_line(&mut writer, &error_response(id, &error));
                    }
                    break;
                }
            }
            continue;
        }

        let outcome = dispatch(&host, &method, params);
        if let Some(id) = id {
            let reply = match outcome.result {
                Ok(value) => response(id, value),
                Err(error) => error_response(id, &error),
            };
            write_line(&mut writer, &reply).map_err(|error| ServeError::Protocol(error.message))?;
        }
        if outcome.stop {
            break;
        }
    }

    host.shutdown();
    Ok(())
}

fn serve_network(settings: &Settings, http: bool) -> Result<(), ServeError> {
    let bind = settings.host.as_str();
    let port = settings.port;
    let listener = TcpListener::bind((bind, port))
        .map_err(|error| ServeError::Conflict(format!("cannot bind {bind}:{port}: {error}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| ServeError::Failure(format!("non-blocking: {error}")))?;

    let address = listener
        .local_addr()
        .map_err(|error| ServeError::Failure(format!("local addr: {error}")))?;
    let advertised = if address.ip().is_unspecified() {
        if address.is_ipv6() {
            "::1"
        } else {
            "127.0.0.1"
        }
        .to_owned()
    } else {
        address.ip().to_string()
    };

    // The discovery file and singleton lock are scoped to the installation root,
    // not the machine (§3.3). A network listener can only claim one root.
    let home = Path::new(&settings.home);
    let token = session::generate_token()
        .map_err(|error| ServeError::Failure(format!("token: {error}")))?;
    session::prune_stale(home);
    let _lock = match session::acquire_lock(home) {
        Ok(guard) => guard,
        Err(error) => return Err(ServeError::Conflict(error.to_string())),
    };
    let state = session::SessionState {
        version: session::STATE_VERSION,
        transport: if http { "http" } else { "tcp" }.to_owned(),
        pid: std::process::id(),
        host: advertised.clone(),
        port: address.port(),
        token: token.clone(),
        started_at: session::now_unix(),
    };
    session::write_state(home, &state)
        .map_err(|error| ServeError::Failure(format!("discovery file: {error}")))?;

    let host = match provider::open(settings) {
        Ok(host) => host,
        Err(error) => {
            session::remove_state(home);
            return Err(ServeError::Failure(error));
        }
    };
    if let Err(error) = host.startup() {
        session::remove_state(home);
        return Err(ServeError::Failure(error.to_string()));
    }

    eprintln!("listening on {advertised}:{}", address.port());
    eprintln!("pairing token: {token}");
    if http {
        eprintln!("HTTP POST /rpc requires Authorization: Bearer <token>");
    } else {
        eprintln!("clients must first send host.authenticate with that token");
    }

    let stopping = Arc::new(AtomicBool::new(false));
    let outcome = if http {
        http::serve(&listener, &host, &token, &stopping)
    } else {
        accept_loop(&listener, &host, &token, &stopping)
    };
    stopping.store(true, Ordering::SeqCst);

    host.shutdown();
    session::remove_state(home);
    outcome.map_err(|error| ServeError::Failure(error.to_string()))
}

fn accept_loop(
    listener: &TcpListener,
    host: &Host,
    token: &str,
    stopping: &Arc<AtomicBool>,
) -> Result<(), std::io::Error> {
    let token = Arc::new(token.to_owned());
    loop {
        if stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                let host = host.clone();
                let token = token.clone();
                let stopping = stopping.clone();
                std::thread::Builder::new()
                    .name(format!("oext-conn-{peer}"))
                    .spawn(move || {
                        if let Err(error) = handle_connection(stream, host, token, stopping) {
                            eprintln!("connection ended: {error}");
                        }
                    })
                    .map_err(|error| std::io::Error::other(format!("spawn: {error}")))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

fn handle_connection(
    stream: std::net::TcpStream,
    host: Host,
    token: Arc<String>,
    stopping: Arc<AtomicBool>,
) -> Result<(), std::io::Error> {
    stream
        .set_nonblocking(false)
        .map_err(|error| std::io::Error::other(format!("blocking mode: {error}")))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let reader_stream = stream
        .try_clone()
        .map_err(|error| std::io::Error::other(format!("clone: {error}")))?;
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;
    let mut authenticated = false;
    let mut handshaken = false;

    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(()),
            Err(error) => {
                let _ = write_line(&mut writer, &error_response(Value::Null, &error));
                return Ok(());
            }
        };
        let Message::Request {
            id, method, params, ..
        } = message
        else {
            continue;
        };

        if !authenticated {
            let supplied = params
                .as_ref()
                .and_then(|value| value.get("token"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if method != method::HOST_AUTHENTICATE
                || !constant_time_eq(supplied.as_bytes(), token.as_bytes())
            {
                let error = RpcError::new(code::UNAUTHORIZED, "missing or invalid session token");
                if let Some(id) = id {
                    let _ = write_line(&mut writer, &error_response(id, &error));
                }
                return Ok(());
            }
            authenticated = true;
            if let Some(id) = id {
                write_line(&mut writer, &response(id, json!({ "authenticated": true })))
                    .map_err(|error| std::io::Error::other(error.message))?;
            }
            continue;
        }

        // After authentication, the first business call must be host.hello.
        if !handshaken {
            if method != method::HOST_HELLO {
                let error = RpcError::new(
                    code::REJECTED,
                    "host.hello must be the first call after authentication",
                )
                .with_data(json!({ "reason": "handshake_required" }));
                if let Some(id) = id {
                    let _ = write_line(&mut writer, &error_response(id, &error));
                }
                return Ok(());
            }
            match handshake(&host, &params) {
                Ok(payload) => {
                    if let Some(id) = id {
                        write_line(&mut writer, &response(id, payload))
                            .map_err(|error| std::io::Error::other(error.message))?;
                    }
                    handshaken = true;
                }
                Err(error) => {
                    if let Some(id) = id {
                        let _ = write_line(&mut writer, &error_response(id, &error));
                    }
                    return Ok(());
                }
            }
            continue;
        }

        let outcome = dispatch(&host, &method, params);
        if let (Some(id), result) = (id, outcome.result) {
            let reply = match result {
                Ok(value) => response(id, value),
                Err(error) => error_response(id, &error),
            };
            write_line(&mut writer, &reply)
                .map_err(|error| std::io::Error::other(error.message))?;
        }
        if outcome.stop {
            stopping.store(true, Ordering::SeqCst);
            return Ok(());
        }
    }
}

/// Performs the `host.hello` negotiation, returning the hello payload on success
/// and `-32023` on a protocol major-version mismatch (§4.2/4.3).
fn handshake(host: &Host, params: &Option<Value>) -> Result<Value, RpcError> {
    let requested = params
        .as_ref()
        .and_then(|value| value.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or("0");

    let client_major = requested.split('.').next().unwrap_or("0");
    let host_major = PROTOCOL_VERSION.split('.').next().unwrap_or("1");
    if client_major != host_major {
        return Err(RpcError::new(
            code::INCOMPATIBLE_PROTOCOL,
            format!(
                "incompatible host protocol: host requires {host_major}, client sent {client_major}"
            ),
        )
        .with_data(json!({
            "reason": "incompatible_protocol",
            "required": PROTOCOL_VERSION,
            "received": requested,
        })));
    }
    Ok(host.hello(provider::sandbox_info()))
}

fn dispatch(host: &Host, method_name: &str, params: Option<Value>) -> Outcome {
    let failure = |error: RpcError| Outcome {
        result: Err(error),
        stop: false,
    };
    let outcome = match method_name {
        method::HOST_INFO => Ok(host.info()),
        method::HOST_HELLO => Ok(host.hello(provider::sandbox_info())),
        method::HOST_EXTENSION_LIST => map(host.list_json()),
        method::HOST_EXTENSION_LOAD => match require_string(&params, "id") {
            Ok(id) => to_value(host.load(&id)),
            Err(error) => Err(error),
        },
        method::HOST_EXTENSION_UNLOAD => match require_string(&params, "id") {
            Ok(id) => map(host.unload(&id).map(|()| json!({ "unloaded": id }))),
            Err(error) => Err(error),
        },
        method::HOST_EXTENSION_RELOAD => match require_string(&params, "id") {
            Ok(id) => to_value(host.reload(&id)),
            Err(error) => Err(error),
        },
        method::HOST_EXTENSION_CALL => match require_string(&params, "id")
            .and_then(|id| require_string(&params, "method").map(|method| (id, method)))
        {
            Ok((id, extension_method)) => {
                let call_params = params
                    .as_ref()
                    .and_then(|value| value.get("params"))
                    .cloned();
                map(host.call(&id, &extension_method, call_params))
            }
            Err(error) => Err(error),
        },
        method::HOST_EXTENSION_LOGS => {
            let id = params
                .as_ref()
                .and_then(|value| value.get("id"))
                .and_then(Value::as_str);
            let limit = params
                .as_ref()
                .and_then(|value| value.get("limit"))
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .clamp(1, 1000) as usize;
            Ok(json!({ "logs": host.logs(id, limit) }))
        }
        method::HOST_EVENT_EMIT => match require_string(&params, "topic") {
            Ok(topic) => {
                let payload = params
                    .as_ref()
                    .and_then(|value| value.get("payload"))
                    .cloned()
                    .unwrap_or(Value::Null);
                to_value(
                    host.emit(&topic, payload)
                        .map(|delivered| json!({ "delivered": delivered })),
                )
            }
            Err(error) => Err(error),
        },
        method::HOST_SHUTDOWN => {
            return Outcome {
                result: Ok(json!({ "stopping": true })),
                stop: true,
            };
        }
        method::HOST_AUTHENTICATE => Err(RpcError::new(
            code::INVALID_REQUEST,
            "host.authenticate is only used on the TCP transport",
        )),
        other => Err(RpcError::new(
            code::METHOD_NOT_FOUND,
            format!("unknown host method: {other}"),
        )),
    };
    match outcome {
        Ok(value) => Outcome {
            result: Ok(value),
            stop: false,
        },
        Err(error) => failure(error),
    }
}

fn map(result: originlang_ext_host::HostResult<Value>) -> Result<Value, RpcError> {
    result.map_err(|error| error.to_rpc())
}

fn to_value<T: Serialize>(result: originlang_ext_host::HostResult<T>) -> Result<Value, RpcError> {
    let value = result.map_err(|error| error.to_rpc())?;
    serde_json::to_value(value)
        .map_err(|error| RpcError::new(code::INTERNAL_ERROR, error.to_string()))
}

fn require_string(params: &Option<Value>, name: &str) -> Result<String, RpcError> {
    params
        .as_ref()
        .and_then(|value| value.get(name))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            RpcError::new(
                code::INVALID_PARAMS,
                format!("missing string parameter '{name}'"),
            )
        })
}

/// Compares two secrets without an early exit on the first differing byte.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_requires_an_exact_match() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn handshake_accepts_matching_major_and_rejects_mismatch() {
        let dir = std::env::temp_dir().join(format!("ol-host-handshake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let host = Host::new(
            originlang_ext_host::HostConfig::new(originlang_ext_host::Paths::new(&dir))
                .with_startup_load(false),
            Arc::new(originlang_ext_runtime_quickjs::QuickJsProvider::new()),
        )
        .unwrap();

        let ok = handshake(&host, &Some(json!({ "protocolVersion": "1" }))).unwrap();
        assert_eq!(ok["protocolVersion"], "1");

        let bad = handshake(&host, &Some(json!({ "protocolVersion": "2" }))).unwrap_err();
        assert_eq!(bad.code, code::INCOMPATIBLE_PROTOCOL);
        assert_eq!(bad.data.as_ref().unwrap()["reason"], "incompatible_protocol");

        host.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
