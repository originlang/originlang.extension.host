//! JSON-RPC transports.
//!
//! Stdio and TCP use NDJSON; HTTP carries one JSON-RPC envelope per body.
//! Network transports require a session token.

mod http;

use anyhow::{Context, Result, bail};
use originlang_ext_host::{Host, HostResult};
use originlang_ext_protocol::{
    Message, RpcError, code, error_response, method, read_message, response, write_line,
};
use originlang_ext_session as session;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::BufReader;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Which transport to serve on.
pub enum Transport {
    /// Newline-delimited JSON-RPC on stdin/stdout.
    Stdio,
    /// JSON-RPC over HTTP POST /rpc with per-request bearer authentication.
    Http { host: String, port: u16 },
    /// Newline-delimited JSON-RPC over TCP with token authentication.
    Tcp {
        /// Bind address.
        host: String,
        /// Bind port; 0 selects a free port.
        port: u16,
    },
}

/// Serve configuration.
pub struct ServeOptions {
    /// Transport selection.
    pub transport: Transport,
}

/// Result of handling one request.
struct Outcome {
    result: Result<Value, RpcError>,
    stop: bool,
}

/// Starts serving on the configured transport.
pub fn serve(host: Host, options: ServeOptions) -> Result<()> {
    match options.transport {
        Transport::Stdio => serve_stdio(&host),
        Transport::Tcp { host: bind, port } => serve_network(host, &bind, port, false),
        Transport::Http { host: bind, port } => serve_network(host, &bind, port, true),
    }
}

fn serve_stdio(host: &Host) -> Result<()> {
    host.startup()?;
    eprintln!(
        "oext {} serving JSON-RPC on stdio (runtime: {})",
        env!("CARGO_PKG_VERSION"),
        host.runtime_kind()
    );

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                // The stream is no longer trustworthy: report and stop rather
                // than guessing where the next frame begins.
                let _ = write_line(&mut writer, &error_response(Value::Null, &error));
                break;
            }
        };
        let Message::Request {
            id, method, params, ..
        } = message
        else {
            continue;
        };
        let outcome = dispatch(host, &method, params);
        if let Some(id) = id {
            let reply = match outcome.result {
                Ok(value) => response(id, value),
                Err(error) => error_response(id, &error),
            };
            write_line(&mut writer, &reply).map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        if outcome.stop {
            break;
        }
    }

    host.shutdown();
    Ok(())
}

fn serve_network(host: Host, bind: &str, port: u16, http: bool) -> Result<()> {
    let listener =
        TcpListener::bind((bind, port)).with_context(|| format!("cannot bind {bind}:{port}"))?;
    // Non-blocking accept with a short poll keeps shutdown bounded: a stop
    // request arrives on a connection thread, and the accept loop must notice.
    listener
        .set_nonblocking(true)
        .context("cannot switch the listener to non-blocking mode")?;

    let address = listener
        .local_addr()
        .context("cannot read listen address")?;
    // A wildcard bind is not a connectable address for clients; advertise the
    // loopback address that the socket also accepts.
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

    let token = session::generate_token().context("cannot generate a session token")?;
    // A predecessor that was killed left both files behind; clear them before
    // contending for the lock so the takeover does not depend on luck.
    session::prune_stale();
    let _lock = match session::acquire_lock() {
        Ok(guard) => guard,
        Err(error) => bail!("{error}"),
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
    session::write_state(&state).context("cannot write the discovery file")?;

    if let Err(error) = host.startup() {
        session::remove_state();
        return Err(error.into());
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
        http::serve(listener, &host, &token, &stopping)
    } else {
        accept_loop(&listener, &host, &token, &stopping)
    };
    stopping.store(true, Ordering::SeqCst);

    host.shutdown();
    session::remove_state();
    outcome
}

fn accept_loop(
    listener: &TcpListener,
    host: &Host,
    token: &str,
    stopping: &Arc<AtomicBool>,
) -> Result<()> {
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
                    .context("cannot spawn a connection thread")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("accept failed"),
        }
    }
}

fn handle_connection(
    stream: std::net::TcpStream,
    host: Host,
    token: Arc<String>,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    // The listener is non-blocking so the accept loop can observe shutdown.
    // Accepted sockets inherit that flag on some platforms (Windows, BSD), which
    // would turn every read into a spurious WouldBlock; force blocking mode.
    stream
        .set_nonblocking(false)
        .context("cannot switch the connection to blocking mode")?;
    let _ = stream.set_nodelay(true);
    // Idle clients are dropped after five minutes instead of pinning a thread
    // for the lifetime of the process.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let reader_stream = stream.try_clone().context("cannot clone the socket")?;
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;
    let mut authenticated = false;

    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            // A clean EOF is a client going away; nothing to report.
            Ok(None) => return Ok(()),
            Err(error) => {
                // A protocol error leaves the stream un-framed: refuse to guess
                // where the next message starts. Say what broke first, then
                // close — silently dropping it looks like a network glitch and
                // costs the caller a debugging session. The same policy as
                // stdio keeps the two transports telling the same story.
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
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
            }
            continue;
        }

        let outcome = dispatch(&host, &method, params);
        if let (Some(id), result) = (id, outcome.result) {
            let reply = match result {
                Ok(value) => response(id, value),
                Err(error) => error_response(id, &error),
            };
            write_line(&mut writer, &reply).map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        if outcome.stop {
            stopping.store(true, Ordering::SeqCst);
            return Ok(());
        }
    }
}

fn dispatch(host: &Host, method_name: &str, params: Option<Value>) -> Outcome {
    let failure = |error: RpcError| Outcome {
        result: Err(error),
        stop: false,
    };
    let outcome = match method_name {
        method::HOST_INFO => Ok(host.info()),
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

fn map(result: HostResult<Value>) -> Result<Value, RpcError> {
    result.map_err(|error| error.to_rpc())
}

fn to_value<T: Serialize>(result: HostResult<T>) -> Result<Value, RpcError> {
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

    /// Every name in the protocol tables must reach a real handler.
    ///
    /// Behaviour is covered by the integration tests; this covers *wiring*. A
    /// method constant added without a match arm is otherwise invisible until a
    /// client happens to call it, and the two dispatch tables (`serve` here,
    /// `api` for the Host API) drift apart silently.
    #[test]
    fn method_tables_match_the_dispatcher() {
        let dir = std::env::temp_dir().join(format!("oext-wiring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let host = Host::new(
            originlang_ext_host::HostConfig::new(originlang_ext_host::Paths::new(&dir))
                .with_startup_load(false),
            Arc::new(originlang_ext_runtime_quickjs::QuickJsProvider::new()),
        )
        .unwrap();

        for name in method::CLIENT {
            let outcome = dispatch(&host, name, None);
            assert!(
                !matches!(&outcome.result, Err(error) if error.code == code::METHOD_NOT_FOUND),
                "客户端方法 {name} 没有接入分发器"
            );
        }

        // The runtime surface is host→instance only. Reaching it from a client
        // would hand an attacker the primitives the host uses internally.
        for name in method::RUNTIME {
            let outcome = dispatch(&host, name, None);
            assert!(
                matches!(&outcome.result, Err(error) if error.code == code::METHOD_NOT_FOUND),
                "运行时方法 {name} 不应对客户端开放"
            );
        }

        host.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
