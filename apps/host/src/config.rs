//! Command-line parsing and layered configuration.
//!
//! Precedence (high → low): command line > environment variables > config file
//! > built-in defaults. See `service-contract.md` §1.1 and §2.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Host binary version, from the crate version.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Transport selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Tcp,
    Http,
}

/// Backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Sandbox,
    Quickjs,
    Dylib,
}

impl Backend {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Backend::Auto),
            "sandbox" => Ok(Backend::Sandbox),
            "quickjs" => Ok(Backend::Quickjs),
            "dylib" => Ok(Backend::Dylib),
            other => Err(format!(
                "invalid backend '{other}' (expected auto|sandbox|quickjs|dylib)"
            )),
        }
    }
}

/// Log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            other => Err(format!(
                "invalid log level '{other}' (expected error|warn|info|debug|trace)"
            )),
        }
    }

    /// Whether `debug`-level diagnostics should be emitted.
    pub fn is_debug(&self) -> bool {
        matches!(self, LogLevel::Debug | LogLevel::Trace)
    }
}

/// Raw command-line arguments.
pub struct Args {
    pub version: bool,
    pub help: bool,
    pub transport: Option<String>,
    pub host: Option<String>,
    pub port: Option<String>,
    pub home: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub backend: Option<String>,
    pub require_signature: bool,
    pub log_level: Option<String>,
    pub no_startup: bool,
}

/// Merged runtime settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub transport: Transport,
    pub host: String,
    pub port: u16,
    pub home: PathBuf,
    pub backend: Backend,
    pub require_signature: bool,
    pub log_level: LogLevel,
    pub startup: bool,
}

/// TOML config file keys (flat, single level).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ConfigFile {
    home: Option<PathBuf>,
    backend: Option<String>,
    require_signature: Option<bool>,
    log_level: Option<String>,
}

pub fn print_usage() {
    println!(
        "oext-host {} — independent extension host service

USAGE:
    oext-host [OPTIONS]

OPTIONS:
    --transport <stdio|tcp|http>   Transport to serve (default: stdio; alias 'std')
    --host <IP|hostname>           Bind address for tcp/http (default: 127.0.0.1)
    --port <0..65535>              Bind port for tcp/http (default: 0 = auto)
    --home <PATH>                  Installation root
    --config <PATH>                TOML config file
    --backend <auto|sandbox|quickjs|dylib>   Extension backend (default: auto)
    --require-signature            Refuse unsigned or untrusted packages
    --log-level <LEVEL>            error|warn|info|debug|trace (default: info)
    --no-startup                   Do not load onStartup extensions
    --version                      Print version and exit
    --help                         Print this usage and exit

ENVIRONMENT:
    OEXT_HOME, OEXT_CONFIG, OEXT_STATE_DIR, OEXT_SANDBOX_BIN,
    OEXT_BACKEND, OEXT_REQUIRE_SIGNATURE, OEXT_LOG_LEVEL
",
        HOST_VERSION
    );
}

impl Args {
    pub fn parse() -> Result<Self, String> {
        let mut args = Args {
            version: false,
            help: false,
            transport: None,
            host: None,
            port: None,
            home: None,
            config: None,
            backend: None,
            require_signature: false,
            log_level: None,
            no_startup: false,
        };

        let mut raw = std::env::args().skip(1).peekable();
        while let Some(arg) = raw.next() {
            let mut value = |name: &str| -> Result<String, String> {
                raw.next()
                    .ok_or_else(|| format!("option '{name}' requires a value"))
            };
            match arg.as_str() {
                "--version" => args.version = true,
                "--help" | "-h" => args.help = true,
                "--transport" => args.transport = Some(value("--transport")?),
                "--host" => args.host = Some(value("--host")?),
                "--port" => args.port = Some(value("--port")?),
                "--home" => args.home = Some(PathBuf::from(value("--home")?)),
                "--config" => args.config = Some(PathBuf::from(value("--config")?)),
                "--backend" => args.backend = Some(value("--backend")?),
                "--require-signature" => args.require_signature = true,
                "--log-level" => args.log_level = Some(value("--log-level")?),
                "--no-startup" => args.no_startup = true,
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        Ok(args)
    }
}

/// Loads and merges settings.
pub fn load(args: &Args) -> Result<Settings, String> {
    let config = load_config_file(args)?;

    // transport: only from the command line, never from config/env (§1.1).
    let transport = match args.transport.as_deref() {
        None | Some("stdio") | Some("std") => Transport::Stdio,
        Some("tcp") => Transport::Tcp,
        Some("http") => Transport::Http,
        Some(other) => return Err(format!("invalid transport '{other}'")),
    };

    // host: --host > default 127.0.0.1 (no env/config).
    let host = args
        .host
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_owned());

    // port: --port > default 0 (no env/config).
    let port = match args.port.as_deref() {
        None => 0,
        Some(value) => value
            .parse::<u16>()
            .map_err(|_| format!("invalid port '{value}'"))?,
    };

    // home: --home > OEXT_HOME > config.home > platform default.
    let home = args
        .home
        .clone()
        .or_else(|| std::env::var_os("OEXT_HOME").map(PathBuf::from))
        .or_else(|| config.home.clone())
        .unwrap_or_else(|| originlang_ext_host::Paths::from_env().root().to_path_buf());

    // backend: --backend > OEXT_BACKEND > config.backend > auto.
    let backend = match args
        .backend
        .as_deref()
        .or(std::env::var("OEXT_BACKEND").ok().as_deref())
        .or(config.backend.as_deref())
    {
        Some(value) => Backend::parse(value)?,
        None => Backend::Auto,
    };

    // require_signature: --require-signature > OEXT_REQUIRE_SIGNATURE > config.
    let require_signature = args.require_signature
        || env_flag("OEXT_REQUIRE_SIGNATURE")
        || config.require_signature.unwrap_or(false);

    // log_level: --log-level > OEXT_LOG_LEVEL > config > info.
    let log_level = match args
        .log_level
        .as_deref()
        .or(std::env::var("OEXT_LOG_LEVEL").ok().as_deref())
        .or(config.log_level.as_deref())
    {
        Some(value) => LogLevel::parse(value)?,
        None => LogLevel::Info,
    };

    Ok(Settings {
        transport,
        host,
        port,
        home,
        backend,
        require_signature,
        log_level,
        startup: !args.no_startup,
    })
}

/// Loads the TOML config file; an explicit path must exist, the default is
/// optional. Unknown keys are rejected (deny_unknown_fields).
fn load_config_file(args: &Args) -> Result<ConfigFile, String> {
    let explicit = args.config.clone().or_else(|| std::env::var_os("OEXT_CONFIG").map(PathBuf::from));
    if let Some(path) = explicit {
        if !path.is_file() {
            return Err(format!("config file not found: {}", path.display()));
        }
        return parse_config(&path);
    }
    if let Some(path) = default_config_path() {
        if path.is_file() {
            return parse_config(&path);
        }
    }
    Ok(ConfigFile::default())
}

fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| Path::new(&home).join(".config"))
        })?;
    Some(base.join("oext").join("config.toml"))
}

fn parse_config(path: &Path) -> Result<ConfigFile, String> {
    let body = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read config {}: {error}", path.display()))?;
    toml::from_str(&body)
        .map_err(|error| format!("invalid config {}: {error}", path.display()))
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_and_level_names_parse() {
        assert_eq!(Backend::parse("auto").unwrap(), Backend::Auto);
        assert_eq!(Backend::parse("sandbox").unwrap(), Backend::Sandbox);
        assert!(Backend::parse("nope").is_err());
        assert_eq!(LogLevel::parse("debug").unwrap(), LogLevel::Debug);
        assert!(LogLevel::parse("nope").is_err());
    }

    #[test]
    fn config_file_rejects_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("oext-host-config-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(&path, "home = \"/tmp/x\"\nunknown_key = 1\n").unwrap();
        assert!(parse_config(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
