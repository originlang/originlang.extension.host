//! `oext` — OriginLang extension host command line.
//!
//! Two modes matter:
//!
//! - `oext run` executes one method of a local package inside a throwaway
//!   installation. It exists for development, so it grants the capabilities the
//!   manifest declares instead of asking the operator to approve each run.
//! - `oext serve` exposes the host over JSON-RPC. On stdio, stdout carries
//!   protocol frames only; all human output goes to stderr.

mod commands;
mod serve;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use originlang_ext_host::{Host, HostConfig, Paths};
use originlang_ext_runtime::RuntimeProvider;
use originlang_ext_runtime_process::SandboxProvider;
use originlang_ext_runtime_quickjs::QuickJsProvider;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "oext",
    version,
    about = "OriginLang extension host: run, serve and manage extensions"
)]
struct Cli {
    /// Installation root (defaults to OEXT_HOME or the platform data directory)
    #[arg(long, global = true, value_name = "PATH")]
    home: Option<PathBuf>,

    /// Execution backend
    #[arg(long, global = true, value_enum, default_value_t = Backend::Sandbox)]
    backend: Backend,

    /// Refuse unsigned packages and packages signed by untrusted publishers
    #[arg(long, global = true)]
    require_signature: bool,

    #[command(subcommand)]
    command: Command,
}

/// Where extension code runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Backend {
    /// One confined process per extension; a crash or an escape stays inside it
    Sandbox,
    /// In-process QuickJS worker: no process boundary, but easier to debug
    Quickjs,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Show host version, runtime backend and resolved directories
    Info,
    /// Invoke one method of a local package (development only)
    Run {
        /// Package directory containing extension.json
        dir: PathBuf,
        /// Method to invoke
        #[arg(long)]
        method: String,
        /// JSON parameters
        #[arg(long, default_value = "null")]
        params: String,
        /// Grant every capability the manifest declares
        #[arg(long)]
        grant_all: bool,
    },
    /// Serve the JSON-RPC interface
    Serve {
        /// Transport to serve on
        #[arg(long, value_enum, default_value_t = Transport::Stdio)]
        transport: Transport,
        /// Bind address (TCP/HTTP transports only)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Bind port; 0 picks a free port (TCP/HTTP transports only)
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Do not load onStartup extensions
        #[arg(long)]
        no_startup: bool,
    },
    /// Manage installed extensions
    #[command(subcommand)]
    Ext(ExtCommand),
    /// Generate a signing key pair
    Keygen {
        /// Where to write the key file (keep this out of the package directory)
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        /// Publisher name embedded in signatures
        #[arg(long, default_value = "local")]
        publisher: String,
    },
    /// Sign a package directory, writing extension.sig
    Sign {
        /// Package directory containing extension.json
        dir: PathBuf,
        /// Key file produced by `oext keygen`
        #[arg(long, value_name = "PATH")]
        key: PathBuf,
        /// Override the publisher name from the key file
        #[arg(long)]
        publisher: Option<String>,
    },
    /// Manage trusted publishers
    #[command(subcommand)]
    Trust(TrustCommand),
}

#[derive(Subcommand, Debug)]
enum TrustCommand {
    /// List trusted publishers
    List,
    /// Trust a publisher public key
    Add {
        /// Public key, hex encoded
        public_key: String,
        /// Operator-facing name
        #[arg(long)]
        name: String,
    },
    /// Forget a trusted publisher
    Remove {
        /// Name recorded when the key was trusted
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum ExtCommand {
    /// List installed extensions
    List,
    /// Show one extension
    Info { id: String },
    /// Install a package directory
    Install {
        path: PathBuf,
        /// Replace an existing installation
        #[arg(long)]
        force: bool,
        /// Capabilities to grant (comma separated, must be declared)
        #[arg(long = "grant", value_delimiter = ',')]
        grant: Vec<String>,
    },
    /// Unload, clear storage and delete a package
    Uninstall { id: String },
    /// Enable a package
    Enable { id: String },
    /// Disable a package (unloads it)
    Disable { id: String },
    /// Grant declared capabilities
    Grant {
        id: String,
        #[arg(required = true)]
        capabilities: Vec<String>,
    },
    /// Revoke one capability
    Revoke { id: String, capability: String },
    /// Show recent host log records
    Logs {
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Write a starter package
    Init {
        id: String,
        /// Target directory (defaults to ./<id>)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Display name (defaults to the id)
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Transport {
    /// Newline-delimited JSON-RPC on stdin/stdout
    #[value(alias = "std")]
    Stdio,
    /// JSON-RPC over HTTP POST /rpc, authenticated by bearer token
    Http,
    /// Newline-delimited JSON-RPC over TCP, authenticated by token
    Tcp,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = match cli.home {
        Some(root) => Paths::new(root),
        None => Paths::from_env(),
    };

    match cli.command {
        Command::Info => {
            let host = open_host(paths, false, cli.backend, cli.require_signature)?;
            println!("{}", serde_json::to_string_pretty(&host.info())?);
            host.shutdown();
            Ok(())
        }
        Command::Run {
            dir,
            method,
            params,
            grant_all,
        } => commands::run(
            paths,
            &dir,
            &method,
            &params,
            grant_all,
            cli.backend,
            cli.require_signature,
        ),
        Command::Serve {
            transport,
            host,
            port,
            no_startup,
        } => {
            let engine = open_host(paths, !no_startup, cli.backend, cli.require_signature)?;
            serve::serve(
                engine,
                serve::ServeOptions {
                    transport: match transport {
                        Transport::Stdio => serve::Transport::Stdio,
                        Transport::Tcp => serve::Transport::Tcp { host, port },
                        Transport::Http => serve::Transport::Http { host, port },
                    },
                },
            )
        }
        Command::Ext(command) => {
            let host = open_host(paths, false, cli.backend, cli.require_signature)?;
            let outcome = commands::ext(&host, command);
            host.shutdown();
            outcome
        }
        Command::Keygen { out, publisher } => commands::keygen(&out, &publisher),
        Command::Sign {
            dir,
            key,
            publisher,
        } => commands::sign(&dir, &key, publisher.as_deref()),
        Command::Trust(command) => {
            let host = open_host(paths, false, cli.backend, cli.require_signature)?;
            let outcome = commands::trust(&host, command);
            host.shutdown();
            outcome
        }
    }
}

fn open_host(
    paths: Paths,
    startup: bool,
    backend: Backend,
    require_signature: bool,
) -> Result<Host> {
    let config = HostConfig::new(paths)
        .with_startup_load(startup)
        .with_require_signature(require_signature);
    Host::new(config, provider(backend)?).context("cannot open the extension host installation")
}

fn provider(backend: Backend) -> Result<Arc<dyn RuntimeProvider>> {
    match backend {
        // Discovery is deliberately explicit rather than a `PATH` lookup: the
        // sandbox is a trusted part of this installation, and resolving it
        // through `PATH` would let any directory supply the binary.
        Backend::Sandbox => Ok(Arc::new(
            SandboxProvider::discover().map_err(|error| anyhow::anyhow!("{error}"))?,
        )),
        Backend::Quickjs => Ok(Arc::new(QuickJsProvider::new())),
    }
}
