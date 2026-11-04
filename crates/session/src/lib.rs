//! Host session state: bearer token, discovery file, liveness probe, singleton.
//!
//! A host that listens on a TCP port is reachable by every local process, so the
//! token is a mandatory credential rather than an optional hardening step. The
//! discovery file (`host.json`) is what lets a client find a running host; the
//! lock file (`host.lock`) is what keeps two hosts from serving the same user
//! data concurrently.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Discovery file format version; bump when field semantics change.
pub const STATE_VERSION: u32 = 1;

/// Discovery file name.
pub const STATE_FILE_NAME: &str = "host.json";

/// Singleton lock file name.
pub const LOCK_FILE_NAME: &str = "host.lock";

/// Environment variable overriding the state directory.
pub const STATE_DIR_ENV: &str = "OEXT_STATE_DIR";

const TOKEN_BYTES: usize = 32;

/// Liveness probe timeout. Short on purpose: callers usually wait for this
/// synchronously when a user asks to open a client.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

/// What a client needs in order to connect to a running host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    /// Format version.
    pub version: u32,
    /// Network transport; older discovery files default to TCP.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Host process id. Reused by the OS, so never sufficient on its own.
    pub pid: u32,
    /// Advertised listen address (wildcard addresses are normalized to loopback).
    pub host: String,
    /// Listen port.
    pub port: u16,
    /// Bearer token required by every request.
    pub token: String,
    /// Host start time, Unix seconds.
    pub started_at: u64,
}

fn default_transport() -> String {
    "tcp".to_owned()
}

/// Generates a 256-bit token from the OS CSPRNG, hex encoded.
///
/// A predictable token would let any local process drive the host, so this uses
/// `getrandom` rather than a hand-rolled PRNG.
pub fn generate_token() -> io::Result<String> {
    let mut buffer = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut buffer)
        .map_err(|e| io::Error::other(format!("getrandom failed: {e}")))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// State directory: `OEXT_STATE_DIR` when set, else `<home>/.originlang-ext`.
///
/// The token lives here, so this directory must be as protected as the
/// extension install directory.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".originlang-ext")
}

/// Path of the discovery file.
pub fn state_path() -> PathBuf {
    state_dir().join(STATE_FILE_NAME)
}

/// Path of the singleton lock file.
pub fn lock_path() -> PathBuf {
    state_dir().join(LOCK_FILE_NAME)
}

/// Writes the discovery file atomically.
///
/// Write-then-rename means a concurrent reader sees either the previous file or
/// the complete new one, never a half-written JSON document.
pub fn write_state(state: &SessionState) -> io::Result<PathBuf> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let path = state_path();
    let temporary = dir.join(format!("{STATE_FILE_NAME}.tmp"));
    let body = serde_json::to_vec_pretty(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&temporary, body)?;
    replace(&temporary, &path)?;
    restrict_permissions(&path)?;
    Ok(path)
}

#[cfg(unix)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    // std::fs::rename refuses an existing destination on Windows.
    if to.exists() {
        fs::remove_file(to)?;
    }
    fs::rename(from, to)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn restrict_permissions(_path: &Path) -> io::Result<()> {
    // Windows has no Unix mode bits; the per-user state directory ACL applies.
    Ok(())
}

/// Reads the discovery file, rejecting unknown versions and corruption.
pub fn read_state() -> Option<SessionState> {
    let body = fs::read(state_path()).ok()?;
    let state: SessionState = serde_json::from_slice(&body).ok()?;
    if state.version != STATE_VERSION {
        return None;
    }
    Some(state)
}

/// Removes the discovery file; missing files are ignored.
pub fn remove_state() {
    let _ = fs::remove_file(state_path());
}

/// Why acquiring the singleton lock failed.
#[derive(Debug)]
pub enum LockError {
    /// A live host already holds the lock; carries its pid.
    Held(u32),
    /// The lock directory or file could not be used.
    Io(io::Error),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Held(pid) => write!(f, "another host is already running (pid {pid})"),
            LockError::Io(e) => write!(f, "cannot acquire host lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// Held singleton lock; the file is removed on drop when still ours.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Only delete our own lock: after a crash the file may already have been
        // cleaned up and re-created by a newer host, whose lock we must not remove.
        let owned = fs::read_to_string(&self.path)
            .map(|body| body.trim() == std::process::id().to_string())
            .unwrap_or(false);
        if owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Acquires the singleton lock, clearing stale locks from dead processes.
pub fn acquire_lock() -> Result<LockGuard, LockError> {
    let dir = state_dir();
    fs::create_dir_all(&dir).map_err(LockError::Io)?;
    let path = lock_path();
    let me = std::process::id().to_string();

    // Retries cover the race where two processes both decide the same lock is
    // stale: only the faster unlink can create, the slower retries.
    for _ in 0..4 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(me.as_bytes()).map_err(LockError::Io)?;
                return Ok(LockGuard { path });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match lock_owner(&path) {
                Some(pid) if process_alive(pid) => return Err(LockError::Held(pid)),
                _ => {
                    let _ = fs::remove_file(&path);
                }
            },
            Err(e) => return Err(LockError::Io(e)),
        }
    }
    Err(LockError::Io(io::Error::new(
        io::ErrorKind::WouldBlock,
        "host lock remains contended after clearing stale locks",
    )))
}

/// Reads the holder pid from the lock file.
///
/// A freshly created lock may still be empty; that must not be treated as stale,
/// or a second host would delete the lock of a host that is still initializing.
fn lock_owner(path: &Path) -> Option<u32> {
    for _ in 0..4 {
        let body = fs::read_to_string(path).ok()?;
        let trimmed = body.trim();
        if trimmed.is_empty() {
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        return trimmed.parse().ok();
    }
    None
}

/// Current Unix time in seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when both the process and the port look alive.
///
/// Both checks are required: pids are recycled (a dead host's pid may belong to
/// an unrelated process), and ports are reused (a connectable port may not be
/// our host). Only when both pass do we consider the session usable.
pub fn is_alive(state: &SessionState) -> bool {
    process_alive(state.pid) && port_open(&state.host, state.port)
}

/// Process existence probe.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // pid <= 1 belongs to init-class processes: seeing one means a broken file.
    if pid <= 1 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // Signal 0 only probes; EPERM (1) means "exists but not ours".
    unsafe { kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) }
}

/// Process existence probe.
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) }; // SYNCHRONIZE
    if handle.is_null() {
        // Access denied still means the process exists.
        return std::io::Error::last_os_error().raw_os_error() == Some(5);
    }
    let status = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    status == 258 // WAIT_TIMEOUT: not yet terminated
}

/// True when a TCP connection to `host:port` succeeds within the probe timeout.
pub fn port_open(host: &str, port: u16) -> bool {
    let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// Returns a live host session, clearing the discovery file when it is stale.
pub fn discover() -> Option<SessionState> {
    let state = read_state()?;
    if is_alive(&state) {
        return Some(state);
    }
    // Leaving a stale file makes every future connect repeat a failing probe
    // with a timeout, and hides the fact that the last exit was abnormal.
    remove_state();
    // The lock outlives the discovery file for the same reason: a host that was
    // killed or crashed never ran `LockGuard::drop`. Clear both together so a
    // dead session leaves nothing behind for the next start to trip over.
    clear_stale_lock();
    None
}

/// Removes leftovers from a host that died without running its shutdown path.
///
/// A `SIGKILL`ed or crashed host cannot run destructors, so both the discovery
/// file and the singleton lock survive it. Every entry point that is about to
/// trust either file calls this first; otherwise a client keeps probing a dead
/// port and a new host keeps seeing a lock owned by a recycled pid.
pub fn prune_stale() {
    if matches!(read_state(), Some(state) if !is_alive(&state)) {
        remove_state();
    }
    clear_stale_lock();
}

/// Deletes the lock file when its owner is provably gone.
///
/// An empty or unparseable lock is left untouched on purpose: a host that has
/// just created the file but not yet written its pid is indistinguishable from a
/// corrupt one, and deleting a live host's lock would let a second host serve the
/// same data. `acquire_lock` resolves that ambiguity itself, under the
/// create-new race that makes the decision safe.
fn clear_stale_lock() {
    if let Some(pid) = lock_owner(&lock_path())
        && !process_alive(pid)
    {
        let _ = fs::remove_file(lock_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // The state directory is process-global through an environment variable, so
    // tests touching it must not run concurrently.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    struct TempStateDir(PathBuf);

    impl TempStateDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "oext-session-{tag}-{}-{}",
                std::process::id(),
                now_unix()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            // SAFETY: guarded by `env_lock`, single-threaded by construction.
            unsafe { std::env::set_var(STATE_DIR_ENV, &dir) };
            Self(dir)
        }
    }

    impl Drop for TempStateDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
            // SAFETY: paired with the `set_var` above.
            unsafe { std::env::remove_var(STATE_DIR_ENV) };
        }
    }

    fn sample(port: u16) -> SessionState {
        SessionState {
            version: STATE_VERSION,
            transport: "tcp".to_owned(),
            pid: std::process::id(),
            host: "127.0.0.1".into(),
            port,
            token: "tok".into(),
            started_at: 1,
        }
    }

    #[test]
    fn tokens_are_unique_and_long_enough() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_ne!(a, b, "CSPRNG 输出重复，token 不可用");
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn state_round_trips_and_rejects_unknown_versions() {
        let _guard = env_lock();
        let _dir = TempStateDir::new("roundtrip");
        let state = sample(54321);
        write_state(&state).unwrap();
        assert_eq!(read_state().unwrap(), state);

        let mut future = state.clone();
        future.version = STATE_VERSION + 1;
        write_state(&future).unwrap();
        assert!(read_state().is_none(), "未知版本必须被识别为不可用");

        fs::write(state_path(), b"{not json").unwrap();
        assert!(read_state().is_none());
    }

    #[test]
    fn missing_state_file_is_none() {
        let _guard = env_lock();
        let _dir = TempStateDir::new("missing");
        assert!(read_state().is_none());
    }

    #[test]
    fn liveness_requires_process_and_port() {
        let state = sample(1);
        assert!(process_alive(state.pid));
        assert!(!is_alive(&state), "端口不可连时整体判为不存活");
    }

    #[test]
    fn discover_keeps_live_state_and_clears_stale_state() {
        let _guard = env_lock();
        let _dir = TempStateDir::new("discover");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        write_state(&sample(port)).unwrap();
        assert_eq!(discover().map(|state| state.port), Some(port));
        assert!(state_path().exists());

        write_state(&sample(1)).unwrap();
        assert!(discover().is_none());
        assert!(!state_path().exists(), "陈旧状态文件应被清理");
    }

    /// Reproduces the state left behind by a host that was killed: a discovery
    /// file for a dead pid and a lock file bearing the same pid.
    #[test]
    fn prune_stale_clears_a_killed_host_entirely() {
        let _guard = env_lock();
        let _dir = TempStateDir::new("prune");

        let mut dead = sample(65535);
        // A pid that cannot exist: probes must treat it as gone on every platform.
        dead.pid = u32::MAX;
        write_state(&dead).unwrap();
        fs::write(lock_path(), u32::MAX.to_string()).unwrap();

        prune_stale();

        assert!(!state_path().exists(), "陈旧发现文件应被清理");
        assert!(!lock_path().exists(), "死进程持有的锁应被清理");
    }

    #[test]
    fn prune_stale_keeps_a_live_session() {
        let _guard = env_lock();
        let _dir = TempStateDir::new("prune-live");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        write_state(&sample(port)).unwrap();
        let guard = acquire_lock().unwrap();

        prune_stale();

        assert!(state_path().exists(), "存活会话不得被清理");
        assert!(lock_path().exists(), "存活的锁不得被清理");
        drop(guard);
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let _guard = env_lock();
        let _dir = TempStateDir::new("lock");

        let held = acquire_lock().unwrap();
        assert_eq!(
            fs::read_to_string(lock_path()).unwrap().trim(),
            std::process::id().to_string()
        );
        match acquire_lock() {
            Err(LockError::Held(pid)) => assert_eq!(pid, std::process::id()),
            other => panic!("应报 Held，实得 {other:?}"),
        }
        drop(held);
        assert!(!lock_path().exists());
        assert!(acquire_lock().is_ok(), "释放后应可再次加锁");
    }

    #[cfg(unix)]
    #[test]
    fn stale_lock_from_dead_process_is_cleared() {
        let _guard = env_lock();
        let _dir = TempStateDir::new("stale-lock");
        fs::create_dir_all(state_dir()).unwrap();
        fs::write(lock_path(), "999999999").unwrap();
        let guard = acquire_lock().expect("陈旧锁应被清理");
        assert_eq!(
            fs::read_to_string(lock_path()).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(guard);
    }
}
