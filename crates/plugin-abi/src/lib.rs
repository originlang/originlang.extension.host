//! C ABI shared by the host and first-party dynamic plugins.
//!
//! A first-party plugin ships as a `cdylib` and is loaded into the host
//! process. Rust has no stable ABI, so nothing but C types may cross that
//! boundary: every value is either a NUL-terminated UTF-8 string holding JSON,
//! or an opaque handle. That costs one serialization per call — the same price
//! the out-of-process backend pays for a pipe write — and keeps the contract
//! inspectable in a debugger rather than hidden behind `rustc`'s layout
//! decisions.
//!
//! # Why the host is a table of function pointers
//!
//! A plugin cannot call [`originlang_ext_runtime::HostBridge`] directly for the
//! same reason it cannot pass a `String`: trait objects are laid out by the
//! compiler, not by the ABI, so a `Box<dyn HostBridge>` created in the host and
//! used in the plugin is undefined behaviour the moment the two are built by
//! different `rustc` versions. Instead the host hands over [`HostVTable`] —
//! plain function pointers plus an opaque context — and every request travels
//! through [`HostVTable::call`], which lands on the same dispatcher that
//! enforces capabilities.
//!
//! A dynamic plugin is trusted code and could bypass that dispatcher by calling
//! `std::fs` directly. Routing it through the bridge anyway buys three things
//! worth more than the convention: one auditable path, one place where
//! per-call deadlines are applied, and log attribution that works the same way
//! for every backend.
//!
//! # Memory ownership
//!
//! Whoever allocates a string frees it. The host frees strings it returned
//! through [`HostVTable::string_free`]; the plugin frees strings it returns
//! through its own `oext_plugin_string_free`. Neither side ever calls `free`
//! on memory it did not allocate, which is the only rule that survives two
//! allocators in one address space.
//!
//! # Panics
//!
//! Since Rust 1.81 a panic crossing an `extern "C"` boundary aborts the
//! process, and an abort in a plugin kills the host. Every export must
//! therefore run inside [`guard`], which turns a panic into a normal error
//! envelope.

use serde_json::{Value, json};
use std::any::Any;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// ABI revision understood by this build.
///
/// The host refuses to load a plugin reporting a different value. Bumping it is
/// a breaking change: existing plugins must be rebuilt, which is acceptable for
/// first-party code that ships alongside the host.
pub const ABI_VERSION: u32 = 1;

/// Opaque per-instance state owned by the plugin.
pub type Handle = *mut c_void;

/// Symbol exporting [`ABI_VERSION`]. Called before any other symbol is bound.
pub const SYM_ABI_VERSION: &str = "oext_plugin_abi_version";
/// Symbol constructing an instance from a host table.
pub const SYM_CREATE: &str = "oext_plugin_create";
/// Symbol reporting plugin metadata, as a JSON envelope.
pub const SYM_REGISTER: &str = "oext_plugin_register";
/// Symbol invoking one method.
pub const SYM_CALL: &str = "oext_plugin_call";
/// Symbol delivering one event.
pub const SYM_EMIT: &str = "oext_plugin_emit";
/// Symbol reporting liveness.
pub const SYM_IS_RUNNING: &str = "oext_plugin_is_running";
/// Symbol running the cleanup hook and destroying the instance.
pub const SYM_SHUTDOWN: &str = "oext_plugin_shutdown";
/// Symbol freeing a string the plugin returned.
pub const SYM_STRING_FREE: &str = "oext_plugin_string_free";

/// Reads the plugin's ABI revision.
pub type AbiVersionFn = unsafe extern "C" fn() -> u32;

/// Builds an instance, borrowing the host table for the instance's lifetime.
///
/// The table must stay valid until the instance is shut down, so the host
/// allocates it once per instance and never moves it.
pub type CreateFn = unsafe extern "C" fn(host: *const HostVTable) -> Handle;

/// Reports metadata. A null handle means "no instance".
pub type RegisterFn = unsafe extern "C" fn(handle: Handle) -> *mut c_char;

/// Invokes `method`. `params` and `chain` are JSON, either may be null.
pub type CallFn = unsafe extern "C" fn(
    handle: Handle,
    method: *const c_char,
    params_json: *const c_char,
    chain_json: *const c_char,
) -> *mut c_char;

/// Delivers one event. `event` and `chain` are JSON, either may be null.
pub type EmitFn = unsafe extern "C" fn(
    handle: Handle,
    event_json: *const c_char,
    chain_json: *const c_char,
) -> *mut c_char;

/// Returns non-zero while the instance can still serve calls.
pub type IsRunningFn = unsafe extern "C" fn(handle: Handle) -> i32;

/// Runs the cleanup hook and releases the instance.
pub type ShutdownFn = unsafe extern "C" fn(handle: Handle) -> *mut c_char;

/// Frees a string produced by the plugin.
pub type StringFreeFn = unsafe extern "C" fn(ptr: *mut c_char);

/// Services the host lends to one plugin instance.
///
/// `#[repr(C)]` and `Copy`: the struct may be copied by value into the plugin,
/// but the pointers inside stay valid for the instance's lifetime, so a copy is
/// a view of the same services rather than a new set.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostVTable {
    /// Mirrors [`ABI_VERSION`]; lets a plugin reject an incompatible host.
    pub abi_version: u32,
    /// Dispatches one Host API call. Never null.
    pub call: Option<
        unsafe extern "C" fn(
            ctx: *mut c_void,
            method: *const c_char,
            params_json: *const c_char,
            chain_json: *const c_char,
        ) -> *mut c_char,
    >,
    /// Frees a string returned by [`HostVTable::call`]. Never null.
    pub string_free: Option<unsafe extern "C" fn(ptr: *mut c_char)>,
    /// Records a log line attributed to this plugin. May be null.
    pub log: Option<
        unsafe extern "C" fn(ctx: *mut c_void, level: *const c_char, message: *const c_char),
    >,
    /// Host state passed back verbatim on every callback.
    pub ctx: *mut c_void,
}

/// Envelope wrapping a successful result: `{"result": <value>}`.
const KEY_RESULT: &str = "result";
/// Envelope wrapping a failure: `{"error": {"code": .., "message": ..}}`.
const KEY_ERROR: &str = "error";

/// Encodes a successful result for the boundary.
pub fn encode_ok(value: &Value) -> *mut c_char {
    let envelope = json!({ KEY_RESULT: value });
    to_c_string(envelope.to_string())
}

/// Encodes a failure for the boundary.
pub fn encode_err(code: i32, message: impl Into<String>) -> *mut c_char {
    let envelope = json!({ KEY_ERROR: { "code": code, "message": message.into() } });
    to_c_string(envelope.to_string())
}

/// Encodes either arm; the shape returned is what [`decode`] expects.
pub fn encode(result: Result<Value, (i32, String)>) -> *mut c_char {
    match result {
        Ok(value) => encode_ok(&value),
        Err((code, message)) => encode_err(code, message),
    }
}

/// Decodes an envelope into a result.
///
/// Unknown keys are ignored so that a newer peer can add fields without
/// breaking an older one; a payload that is neither arm is reported as a
/// protocol error rather than silently treated as success.
pub fn decode(text: &str) -> Result<Value, (i32, String)> {
    let envelope: Value = serde_json::from_str(text)
        .map_err(|error| (-32700, format!("malformed envelope: {error}")))?;
    if let Some(error) = envelope.get(KEY_ERROR) {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603) as i32;
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("plugin reported an error")
            .to_owned();
        return Err((code, message));
    }
    match envelope.get(KEY_RESULT) {
        Some(value) => Ok(value.clone()),
        None => Err((-32700, "envelope carries neither result nor error".into())),
    }
}

/// Runs `body`, converting a panic into an error envelope.
///
/// Every exported function must be wrapped: a panic that escapes an `extern
/// "C"` boundary aborts the process, and here that process is the host.
pub fn guard(body: impl FnOnce() -> Result<Value, (i32, String)>) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => encode(result),
        Err(payload) => encode_err(
            -32603,
            format!("plugin panicked: {}", panic_message(&payload)),
        ),
    }
}

/// Best-effort text for a panic payload.
fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown payload".to_owned()
    }
}

/// Copies a NUL-terminated string, treating null and non-UTF-8 as absent.
///
/// # Safety
///
/// `ptr` must be null or point to a NUL-terminated byte sequence valid for the
/// duration of the call.
pub unsafe fn read_c_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Copies a JSON string argument, treating null and malformed text as `None`.
///
/// # Safety
///
/// Same contract as [`read_c_str`].
pub unsafe fn read_json(ptr: *const c_char) -> Option<Value> {
    // SAFETY: forwarded to `read_c_str`.
    let text = unsafe { read_c_str(ptr) }?;
    serde_json::from_str(&text).ok()
}

/// Releases a string the plugin allocated.
///
/// # Safety
///
/// `ptr` must come from [`to_c_string`] in the same library instance, and must
/// not have been freed already.
pub unsafe fn free_c_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: the caller guarantees provenance from `CString::into_raw`.
    drop(unsafe { CString::from_raw(ptr) });
}

/// Moves `text` to the boundary.
///
/// An interior NUL cannot survive JSON encoding — it is escaped — so the
/// replacement branch is unreachable for well-formed input and exists only so
/// this function cannot panic.
fn to_c_string(text: String) -> *mut c_char {
    match CString::new(text) {
        Ok(value) => value.into_raw(),
        Err(_) => CString::new("{\"error\":{\"code\":-32700,\"message\":\"interior NUL\"}}")
            .expect("static string has no NUL")
            .into_raw(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_round_trips() {
        let pointer = encode_ok(&json!({"saved": true}));
        // SAFETY: freshly allocated above.
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: same allocation, not yet freed.
        unsafe { free_c_string(pointer) };

        assert_eq!(decode(&text), Ok(json!({"saved": true})));
    }

    #[test]
    fn error_round_trips_with_its_code() {
        let pointer = encode_err(-32021, "missing capability");
        // SAFETY: freshly allocated above.
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: same allocation, not yet freed.
        unsafe { free_c_string(pointer) };

        assert_eq!(
            decode(&text),
            Err((-32021, "missing capability".to_owned()))
        );
    }

    #[test]
    fn malformed_envelopes_are_protocol_errors() {
        assert_eq!(decode("not json").unwrap_err().0, -32700);
        assert_eq!(decode(r#"{"unrelated":1}"#).unwrap_err().0, -32700);
    }

    #[test]
    fn an_error_without_a_code_still_decodes() {
        let error = decode(r#"{"error":{"message":"boom"}}"#).unwrap_err();
        assert_eq!(error.0, -32603);
        assert_eq!(error.1, "boom");
    }

    #[test]
    fn guard_converts_a_panic_into_an_error_envelope() {
        let pointer = guard(|| panic!("deliberate"));
        // SAFETY: freshly allocated above.
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: same allocation, not yet freed.
        unsafe { free_c_string(pointer) };

        let error = decode(&text).unwrap_err();
        assert_eq!(error.0, -32603);
        assert!(error.1.contains("deliberate"), "{}", error.1);
    }

    #[test]
    fn null_and_missing_arguments_read_as_absent() {
        // SAFETY: null is explicitly allowed.
        assert_eq!(unsafe { read_json(std::ptr::null()) }, None);
        // SAFETY: null is explicitly allowed.
        assert_eq!(unsafe { read_c_str(std::ptr::null()) }, None);
    }
}
