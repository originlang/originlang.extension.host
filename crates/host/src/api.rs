//! Host API: the only surface an extension may call.
//!
//! Two rules hold for every handler:
//!
//! 1. The calling identity is supplied by the host, never by the request. An
//!    extension therefore cannot act as another extension or as the client.
//! 2. A handler is reachable only with the matching capability, checked before
//!    any work happens. Unknown methods fail with `METHOD_NOT_FOUND` rather than
//!    a generic error, so a typo and a missing grant are distinguishable.

use crate::host::{HOST_VERSION, Inner, with_chain};
use originlang_ext_manifest::API_VERSION;
use originlang_ext_protocol::{RpcError, code, method};
use originlang_ext_runtime::{CallChain, HostBridge};
use originlang_ext_storage::Storage;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Capability identifiers understood by the host.
pub mod capability {
    /// Write to the host log.
    pub const LOG_WRITE: &str = "log.write";
    /// Read the extension's own storage namespace.
    pub const STORAGE_READ: &str = "storage.read";
    /// Write the extension's own storage namespace.
    pub const STORAGE_WRITE: &str = "storage.write";
    /// Create and cancel timers.
    pub const TIMER_SCHEDULE: &str = "timer.schedule";
    /// Publish events.
    pub const EVENTS_EMIT: &str = "events.emit";
    /// Subscribe to events.
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// Call other extensions.
    pub const EXTENSION_CALL: &str = "extension.call";
}

/// Largest value accepted by `host.storage.set`.
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// Largest number of entries returned by `host.storage.list`.
pub const MAX_LIST_ENTRIES: usize = 512;

/// Capability required by a Host API method, if any.
///
/// Methods absent from this table are either unprivileged (`host.self.info`) or
/// unknown; the distinction is made by the dispatcher.
pub fn required_capability(method: &str) -> Option<&'static str> {
    match method {
        method::HOST_LOG => Some(capability::LOG_WRITE),
        method::HOST_STORAGE_GET | method::HOST_STORAGE_LIST => Some(capability::STORAGE_READ),
        method::HOST_STORAGE_SET | method::HOST_STORAGE_DELETE => Some(capability::STORAGE_WRITE),
        method::HOST_TIMER_SET | method::HOST_TIMER_CLEAR => Some(capability::TIMER_SCHEDULE),
        method::HOST_EVENTS_EMIT => Some(capability::EVENTS_EMIT),
        method::HOST_EVENTS_SUBSCRIBE => Some(capability::EVENTS_SUBSCRIBE),
        method::HOST_EXTENSION_CALL_OUT => Some(capability::EXTENSION_CALL),
        _ => None,
    }
}

/// Bridge handed to one extension instance.
pub(crate) struct ExtensionBridge {
    inner: Arc<Inner>,
    id: String,
    /// Capabilities declared by the manifest, fixed at load time.
    declared: Arc<std::collections::BTreeSet<String>>,
}

impl ExtensionBridge {
    pub(crate) fn new(
        inner: Arc<Inner>,
        id: &str,
        declared: Arc<std::collections::BTreeSet<String>>,
    ) -> Self {
        Self {
            inner,
            id: id.to_owned(),
            declared,
        }
    }
}

impl HostBridge for ExtensionBridge {
    fn call(
        &self,
        method: &str,
        params: Option<Value>,
        chain: &CallChain,
    ) -> Result<Value, RpcError> {
        // The chain is normalized rather than trusted: a backend that reports a
        // stack not ending with its own id still cannot hide its identity from
        // the cycle check, and a backend that reports nothing is treated as the
        // innermost frame it necessarily is.
        let mut chain = chain.to_vec();
        if chain.last().map(String::as_str) != Some(self.id.as_str()) {
            chain.push(self.id.clone());
        }
        with_chain(&chain, || {
            self.inner
                .dispatch_host_call(&self.id, &self.declared, method, params)
        })
    }

    fn log(&self, level: &str, message: &str) {
        self.inner.record_log(level, Some(&self.id), message);
    }
}

impl Inner {
    /// Dispatches one Host API call on behalf of `extension`.
    ///
    /// Two gates apply in order: the capability must be *declared* in the
    /// manifest, then *granted* by the operator. Declaration alone is not
    /// consent, and a grant for an undeclared capability is not authority.
    pub(crate) fn dispatch_host_call(
        &self,
        extension: &str,
        declared: &std::collections::BTreeSet<String>,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, RpcError> {
        if let Some(capability) = required_capability(method) {
            if !declared.contains(capability) {
                return Err(RpcError::new(
                    code::PERMISSION_DENIED,
                    format!("extension '{extension}' did not declare capability '{capability}'"),
                ));
            }
            let allowed = self
                .policy
                .read()
                .map(|policy| policy.allows(extension, capability))
                .unwrap_or(false);
            if !allowed {
                return Err(RpcError::new(
                    code::PERMISSION_DENIED,
                    format!("extension '{extension}' lacks capability '{capability}'"),
                ));
            }
        }

        match method {
            method::HOST_LOG => self.host_log(extension, params),
            method::HOST_STORAGE_GET => self.host_storage_get(extension, params),
            method::HOST_STORAGE_SET => self.host_storage_set(extension, params),
            method::HOST_STORAGE_DELETE => self.host_storage_delete(extension, params),
            method::HOST_STORAGE_LIST => self.host_storage_list(extension, params),
            method::HOST_TIMER_SET => self.host_timer_set(extension, params),
            method::HOST_TIMER_CLEAR => self.host_timer_clear(extension, params),
            method::HOST_EVENTS_EMIT => self.host_events_emit(extension, params),
            method::HOST_EVENTS_SUBSCRIBE => self.host_events_subscribe(extension, params),
            method::HOST_EXTENSION_CALL_OUT => self.host_extension_call(extension, params),
            method::HOST_SELF_INFO => Ok(json!({
                "id": extension,
                "hostVersion": HOST_VERSION,
                "apiVersion": API_VERSION,
                "runtime": self.provider.kind(),
                "capabilities": self.grants(extension),
            })),
            other => Err(RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("unsupported host method: {other}"),
            )),
        }
    }

    fn grants(&self, extension: &str) -> Vec<String> {
        self.policy
            .read()
            .map(|policy| policy.grants_for(extension))
            .unwrap_or_default()
    }

    fn host_log(&self, extension: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let message = require_string(&params, "message")?;
        let level = optional_string(&params, "level")?.unwrap_or_else(|| "info".into());
        self.record_log(&level, Some(extension), &message);
        Ok(Value::Null)
    }

    fn host_storage_get(&self, extension: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let key = require_string(&params, "key")?;
        let storage = self.scoped_storage(extension);
        match storage.get(&key) {
            Ok(bytes) => Ok(Value::String(decode(&bytes)?)),
            // A missing key is a normal outcome for a cache-like store; callers
            // test for null instead of catching an exception.
            Err(originlang_ext_storage::Error::NotFound(_)) => Ok(Value::Null),
            Err(error) => Err(storage_error(error)),
        }
    }

    fn host_storage_set(&self, extension: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let key = require_string(&params, "key")?;
        let value = require_string(&params, "value")?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                format!("value exceeds {MAX_VALUE_BYTES} bytes"),
            ));
        }
        self.scoped_storage(extension)
            .set(&key, value.as_bytes())
            .map_err(storage_error)?;
        Ok(Value::Null)
    }

    fn host_storage_delete(
        &self,
        extension: &str,
        params: Option<Value>,
    ) -> Result<Value, RpcError> {
        let key = require_string(&params, "key")?;
        self.scoped_storage(extension)
            .delete(&key)
            .map_err(storage_error)?;
        Ok(Value::Null)
    }

    fn host_storage_list(&self, extension: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let prefix = optional_string(&params, "prefix")?.unwrap_or_default();
        let entries = self
            .scoped_storage(extension)
            .list(&prefix)
            .map_err(storage_error)?;
        let mut items = Vec::with_capacity(entries.len().min(MAX_LIST_ENTRIES));
        for entry in entries.into_iter().take(MAX_LIST_ENTRIES) {
            items.push(json!({
                "key": entry.key,
                "value": decode(&entry.value)?,
            }));
        }
        Ok(Value::Array(items))
    }

    fn host_timer_set(&self, extension: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let delay_ms = require_u64(&params, "delayMs")?;
        let repeat = optional_bool(&params, "repeat")?.unwrap_or(false);
        if delay_ms == 0 {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "delayMs must be greater than zero",
            ));
        }
        let id = self
            .schedule_timer(extension, delay_ms, repeat)
            .map_err(|error| error.to_rpc())?;
        Ok(json!({ "id": id }))
    }

    fn host_timer_clear(&self, extension: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let id = require_string(&params, "id")?;
        let cancelled = self
            .clear_timer(extension, &id)
            .map_err(|error| error.to_rpc())?;
        Ok(json!({ "cancelled": cancelled }))
    }

    fn host_events_emit(&self, extension: &str, params: Option<Value>) -> Result<Value, RpcError> {
        let topic = require_string(&params, "topic")?;
        let payload = param(&params, "payload").cloned().unwrap_or(Value::Null);
        let delivered = self
            .emit(&topic, payload, Some(extension))
            .map_err(|error| error.to_rpc())?;
        Ok(json!({ "delivered": delivered }))
    }

    fn host_events_subscribe(
        &self,
        extension: &str,
        params: Option<Value>,
    ) -> Result<Value, RpcError> {
        let topic = require_string(&params, "topic")?;
        self.subscribe(extension, &topic)
            .map_err(|error| error.to_rpc())?;
        Ok(Value::Null)
    }

    fn host_extension_call(
        &self,
        extension: &str,
        params: Option<Value>,
    ) -> Result<Value, RpcError> {
        let target = require_string(&params, "id")?;
        let target_method = require_string(&params, "method")?;
        if target == extension {
            return Err(RpcError::new(
                code::REJECTED,
                "an extension cannot call itself through host.extension.call",
            ));
        }
        let call_params = param(&params, "params").cloned();
        self.call(&target, &target_method, call_params)
            .map_err(|error| error.to_rpc())
    }
}

fn decode(bytes: &[u8]) -> Result<String, RpcError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| RpcError::new(code::OPERATION_FAILED, "stored value is not valid UTF-8"))
}

fn storage_error(error: originlang_ext_storage::Error) -> RpcError {
    let code = match error {
        originlang_ext_storage::Error::InvalidKey(_) => code::INVALID_PARAMS,
        _ => code::OPERATION_FAILED,
    };
    RpcError::new(code, error.to_string())
}

fn param<'a>(params: &'a Option<Value>, name: &str) -> Option<&'a Value> {
    params.as_ref().and_then(|value| value.get(name))
}

fn require_string(params: &Option<Value>, name: &str) -> Result<String, RpcError> {
    match param(params, name) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("'{name}' must be a string"),
        )),
        None => Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("missing parameter '{name}'"),
        )),
    }
}

fn optional_string(params: &Option<Value>, name: &str) -> Result<Option<String>, RpcError> {
    match param(params, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("'{name}' must be a string"),
        )),
    }
}

fn require_u64(params: &Option<Value>, name: &str) -> Result<u64, RpcError> {
    param(params, name).and_then(Value::as_u64).ok_or_else(|| {
        RpcError::new(
            code::INVALID_PARAMS,
            format!("'{name}' must be a non-negative integer"),
        )
    })
}

fn optional_bool(params: &Option<Value>, name: &str) -> Result<Option<bool>, RpcError> {
    match param(params, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(RpcError::new(
            code::INVALID_PARAMS,
            format!("'{name}' must be a boolean"),
        )),
    }
}

/// Renders a params object for diagnostics; unused parameters fail loudly in tests.
#[allow(dead_code)]
fn as_object(params: &Option<Value>) -> Option<&Map<String, Value>> {
    params.as_ref().and_then(Value::as_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_privileged_method_names_a_capability() {
        for method in [
            method::HOST_LOG,
            method::HOST_STORAGE_GET,
            method::HOST_STORAGE_SET,
            method::HOST_STORAGE_DELETE,
            method::HOST_STORAGE_LIST,
            method::HOST_TIMER_SET,
            method::HOST_TIMER_CLEAR,
            method::HOST_EVENTS_EMIT,
            method::HOST_EVENTS_SUBSCRIBE,
            method::HOST_EXTENSION_CALL_OUT,
        ] {
            assert!(
                required_capability(method).is_some(),
                "{method} 必须有对应能力"
            );
        }
        assert_eq!(required_capability(method::HOST_SELF_INFO), None);
        assert_eq!(required_capability("host.unknown"), None);
    }

    #[test]
    fn parameter_helpers_reject_wrong_types() {
        let params = Some(json!({"key": 7, "flag": "yes"}));
        assert!(require_string(&params, "key").is_err());
        assert!(require_string(&params, "absent").is_err());
        assert!(optional_bool(&params, "flag").is_err());
        assert!(require_u64(&params, "flag").is_err());

        let ok = Some(json!({"key": "a", "n": 3, "flag": true}));
        assert_eq!(require_string(&ok, "key").unwrap(), "a");
        assert_eq!(require_u64(&ok, "n").unwrap(), 3);
        assert_eq!(optional_bool(&ok, "flag").unwrap(), Some(true));
        assert_eq!(optional_string(&ok, "absent").unwrap(), None);
    }
}
