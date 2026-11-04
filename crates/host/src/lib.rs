//! OriginLang extension host.
//!
//! The crate is the orchestration layer: it discovers packages, authorizes
//! declared capabilities, starts runtime instances, routes Host API calls,
//! persists per-extension state, schedules timers and fans out events.
//!
//! Deliberate boundaries:
//!
//! - It does not implement a runtime; it talks to [`originlang_ext_runtime`].
//!   The QuickJS backend lives in its own crate, so a second backend can be
//!   wired in without changing this one.
//! - It is not a process supervisor. Extension code runs in-process; the
//!   capability check is a host-level gate, not an OS sandbox.
//! - It exposes no network transport. `oext serve` and library embedders own
//!   the transport; this crate owns the semantics.

pub mod api;
pub mod error;
pub mod host;
pub mod logs;
pub mod paths;
pub mod registry;
pub mod router;

pub use api::{capability, required_capability};
pub use error::{HostError, HostResult};
pub use host::{
    ExtensionSummary, HOST_VERSION, Host, HostConfig, MAX_TIMER_DELAY_MS, validate_topic,
};
pub use logs::{LogBuffer, LogRecord};
pub use paths::Paths;
pub use registry::{InstalledExtension, Registry};
pub use router::Router;
