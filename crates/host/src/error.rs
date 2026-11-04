//! Host errors and their mapping onto the wire protocol.
//!
//! Every failure that reaches a client has a stable RPC code. The message text is
//! for humans; the code is what a client may branch on.

use originlang_ext_manifest::ManifestError;
use originlang_ext_protocol::{RpcError, code};

/// Host failure modes.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// No extension with this id is installed.
    #[error("extension '{0}' is not installed")]
    NotFound(String),

    /// The extension is installed but disabled by the operator.
    #[error("extension '{0}' is disabled")]
    Disabled(String),

    /// The extension is installed but has no live instance.
    #[error("extension '{0}' is not loaded")]
    NotLoaded(String),

    /// A live instance already exists.
    #[error("extension '{0}' is already loaded")]
    AlreadyLoaded(String),

    /// A declared capability is not granted.
    #[error("{0}")]
    PermissionDenied(String),

    /// The manifest is missing or invalid.
    #[error("manifest: {0}")]
    Manifest(#[from] ManifestError),

    /// Filesystem failure.
    #[error("io: {0}")]
    Io(String),

    /// Storage backend failure.
    #[error("storage: {0}")]
    Storage(String),

    /// The runtime could not start, register or was retired.
    #[error("{0}")]
    LoadFailed(String),

    /// The request was understood but refused by policy.
    #[error("{0}")]
    Rejected(String),

    /// The runtime reported an error, preserved verbatim.
    #[error("{0}")]
    Runtime(#[from] RpcError),
}

impl HostError {
    /// Maps the error onto a stable RPC code.
    pub fn to_rpc(&self) -> RpcError {
        let code = match self {
            HostError::NotFound(_) => code::EXTENSION_NOT_FOUND,
            HostError::Disabled(_) => code::EXTENSION_DISABLED,
            HostError::NotLoaded(_) => code::EXTENSION_NOT_LOADED,
            HostError::AlreadyLoaded(_) => code::EXTENSION_EXISTS,
            HostError::PermissionDenied(_) => code::PERMISSION_DENIED,
            HostError::Manifest(_) => code::EXTENSION_LOAD_FAILED,
            HostError::Io(_) => code::OPERATION_FAILED,
            HostError::Storage(_) => code::OPERATION_FAILED,
            HostError::LoadFailed(_) => code::EXTENSION_LOAD_FAILED,
            HostError::Rejected(_) => code::REJECTED,
            HostError::Runtime(error) => return error.clone(),
        };
        RpcError::new(code, self.to_string())
    }

    /// Convenience constructor for I/O failures that need path context.
    pub fn io(context: &str, error: std::io::Error) -> Self {
        HostError::Io(format!("{context}: {error}"))
    }
}

/// Result alias used across the host.
pub type HostResult<T> = Result<T, HostError>;
