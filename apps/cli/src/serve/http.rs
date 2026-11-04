//! HTTP adaptation of the shared management dispatcher.
use super::*;
use originlang_ext_protocol::{MAX_MESSAGE_BYTES, parse_message};
use std::io::Read;
use tiny_http::{Header, Method, Request, Response, Server};

pub(super) fn serve(
    listener: TcpListener,
    host: &Host,
    token: &str,
    stopping: &Arc<AtomicBool>,
) -> Result<()> {
    listener.set_nonblocking(false)?;
    let server = Server::from_listener(listener, None).map_err(|e| anyhow::anyhow!(e))?;
    while !stopping.load(Ordering::SeqCst) {
        if let Some(request) = server.recv_timeout(Duration::from_millis(50))? {
            let host = host.clone();
            let token = token.to_owned();
            let stopping = stopping.clone();
            std::thread::Builder::new()
                .name("oext-http".to_owned())
                .spawn(move || {
                    if let Err(error) = handle(request, &host, &token, &stopping) {
                        eprintln!("HTTP request ended: {error}");
                    }
                })?;
        }
    }
    Ok(())
}

fn reply(request: Request, status: u16, value: Option<Value>) -> Result<()> {
    let body = value.map(|v| v.to_string()).unwrap_or_default();
    let mut response = Response::from_string(body).with_status_code(status);
    for (name, value) in [
        ("Content-Type", "application/json"),
        ("Cache-Control", "no-store"),
    ] {
        response.add_header(Header::from_bytes(name, value).unwrap());
    }
    if status == 401 {
        response.add_header(Header::from_bytes("WWW-Authenticate", "Bearer").unwrap());
    }
    if status == 405 {
        response.add_header(Header::from_bytes("Allow", "POST").unwrap());
    }
    request.respond(response)?;
    Ok(())
}

fn reject(request: Request, status: u16, code: i64, message: &str) -> Result<()> {
    reply(
        request,
        status,
        Some(error_response(Value::Null, &RpcError::new(code, message))),
    )
}

fn handle(mut request: Request, host: &Host, token: &str, stopping: &AtomicBool) -> Result<()> {
    let authorization: Vec<_> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Authorization"))
        .collect();
    let authenticated = authorization.len() == 1
        && authorization[0]
            .value
            .as_str()
            .split_once(' ')
            .is_some_and(|(scheme, supplied)| {
                scheme.eq_ignore_ascii_case("Bearer")
                    && constant_time_eq(supplied.as_bytes(), token.as_bytes())
            });
    if !authenticated {
        return reject(
            request,
            401,
            code::UNAUTHORIZED,
            "missing or invalid bearer token",
        );
    }
    if request.url() != "/rpc" {
        return reject(request, 404, code::INVALID_REQUEST, "use /rpc");
    }
    if request.method() != &Method::Post {
        return reject(request, 405, code::INVALID_REQUEST, "use POST");
    }
    if !request.headers().iter().any(|h| {
        h.field.equiv("Content-Type")
            && h.value
                .as_str()
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
    }) {
        return reject(request, 415, code::INVALID_REQUEST, "use application/json");
    }
    if request
        .body_length()
        .is_some_and(|len| len > MAX_MESSAGE_BYTES)
    {
        return reject(request, 413, code::INVALID_REQUEST, "message exceeds 8 MiB");
    }
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_MESSAGE_BYTES as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > MAX_MESSAGE_BYTES {
        return reject(request, 413, code::INVALID_REQUEST, "message exceeds 8 MiB");
    }
    let message = match parse_message(&body) {
        Ok(message) => message,
        Err(error) => return reply(request, 400, Some(error_response(Value::Null, &error))),
    };
    let Message::Request {
        id, method, params, ..
    } = message
    else {
        return reject(
            request,
            400,
            code::INVALID_REQUEST,
            "expected a request or notification",
        );
    };
    if stopping.load(Ordering::SeqCst) {
        return reject(request, 503, code::REJECTED, "host is stopping");
    }
    let outcome = dispatch(host, &method, params);
    let result = if let Some(id) = id {
        let value = match outcome.result {
            Ok(value) => response(id, value),
            Err(error) => error_response(id, &error),
        };
        reply(request, 200, Some(value))
    } else {
        reply(request, 204, None)
    };
    // Shutdown remains effective even if the requesting client disconnected.
    if outcome.stop {
        stopping.store(true, Ordering::SeqCst);
    }
    result
}
