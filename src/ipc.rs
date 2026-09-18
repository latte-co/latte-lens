//! Instance-to-instance discovery and command delivery.
//!
//! Every Latte Lens instance binds one Unix-domain socket in the per-user
//! runtime directory ([`runtime_dir`]), named `<pid>-<salt>.sock`. Liveness
//! is proved by handshake, never by file presence: discovery connects to
//! every `*.sock` entry and asks for [`RequestBody::Info`]; stale files left
//! by crashed instances, foreign sockets, and hung servers fail the
//! handshake and are skipped.
//!
//! The protocol is one JSON document per connection, newline-terminated from
//! the client and terminated by connection close in the reply direction.
//! Both directions are bounded ([`MAX_REQUEST_BYTES`]), and a connection
//! carries exactly one request and at most one response.
//!
//! Platform note: the socket types come from `std::os::unix::net` and
//! `std::os::windows::net`, which expose the same
//! `UnixListener`/`UnixStream` surface (the Windows side requires the
//! Winsock AF_UNIX support of Windows 10 1803+).

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use std::os::windows::net::{UnixListener, UnixStream};

/// Wire protocol version answered by this build. Clients reject replies and
/// instances reject requests whose version differs, so an upgraded binary
/// fails loudly against a still-running older instance instead of silently
/// misinterpreting it.
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound on one serialized request or reply, in either direction.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Connections served concurrently; excess clients are shed and time out.
const MAX_ACTIVE_CONNECTIONS: usize = 4;

/// Sockets probed by one discovery pass, in sorted file-name order.
const MAX_DISCOVERED_INSTANCES: usize = 64;

/// Per-I/O-operation deadline for the request/reply exchange.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Extra bytes consumed while draining an oversized request before replying,
/// so the peer can finish writing and read the error without a reset.
const MAX_DRAIN_BYTES: usize = MAX_REQUEST_BYTES;

/// Per-socket deadline while fanning out discovery probes.
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(500);

/// Wake-up cadence of the non-blocking accept loop.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a connection thread waits for the instance main loop to answer
/// a forwarded `open` request. The loop drains its inbox synchronously every
/// frame, so a healthy instance answers well within this budget.
const OPEN_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------

/// Identity of one live instance, served by the `info` handshake.
///
/// `root` is the lossy text form of the workspace path; non-UTF-8 paths are
/// represented with replacement characters rather than failing the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub proto: u32,
    pub pid: u32,
    pub root: String,
    pub version: String,
    pub started_at_ms: u64,
}

/// One request from a client to an instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub proto: u32,
    #[serde(flatten)]
    pub body: RequestBody,
}

/// Request bodies understood by protocol version 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum RequestBody {
    /// Ask an instance to identify itself. Powers discovery and `ps`.
    Info,
    /// Ask an instance to open files in its viewer. Reserved for the
    /// `--attach` milestone; current instances answer with
    /// [`ResponseBody::Error`] so newer clients degrade with a message
    /// instead of hanging.
    Open {
        paths: Vec<String>,
        #[serde(default)]
        focus: Focus,
    },
}

/// Where a successfully opened file should surface in the viewer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Focus {
    /// Reveal the file in the instance's active tab.
    #[default]
    Active,
    /// Open the file in a new tab.
    NewTab,
}

/// One reply from an instance to a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub proto: u32,
    #[serde(flatten)]
    pub body: ResponseBody,
}

/// Reply bodies produced by protocol version 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum ResponseBody {
    Info {
        info: InstanceInfo,
    },
    /// Reserved for the `--attach` milestone.
    Opened {
        opened: Vec<String>,
    },
    Error {
        message: String,
    },
}

/// Failure modes of the client side of the protocol.
#[derive(Debug)]
pub enum IpcError {
    /// The peer could not be reached, or the exchange failed at I/O level.
    Io { path: PathBuf, source: io::Error },
    /// The peer answered outside the protocol contract: a malformed
    /// document, a mismatched protocol version, or an unexpected reply shape.
    Protocol(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "cannot reach instance socket {}: {source}",
                    path.display()
                )
            }
            Self::Protocol(message) => write!(formatter, "instance protocol error: {message}"),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Protocol(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime directory
// ---------------------------------------------------------------------------

/// Resolve the per-user directory that hosts instance sockets:
/// `<runtime-root>/instances`. The runtime root mirrors the
/// agent-observability runtime root and its environment overrides:
/// `LATTE_LENS_RUNTIME_DIR`, then `XDG_RUNTIME_DIR/latte-lens`, then a
/// per-uid temporary directory. The two features share the root but never
/// entries, so one override isolates both in tests.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let root = match std::env::var_os("LATTE_LENS_RUNTIME_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => default_runtime_root()?,
    };
    if !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "runtime root {} is not an absolute, parent-free path",
                root.display()
            ),
        ));
    }
    Ok(root.join("instances"))
}

#[cfg(unix)]
fn default_runtime_root() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("latte-lens"));
    }
    // SAFETY: geteuid has no preconditions and is used only to partition
    // a private temporary runtime directory (mirrors agent::live).
    let uid = unsafe { libc::geteuid() };
    Ok(std::env::temp_dir().join(format!("latte-lens-{uid}")))
}

#[cfg(windows)]
fn default_runtime_root() -> io::Result<PathBuf> {
    Ok(std::env::temp_dir().join("latte-lens"))
}

/// Create `dir` if needed and ensure it is private to the current user.
/// A directory that cannot be made owner-only is refused rather than served
/// from: another local user could otherwise talk to our instances.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(dir)?;
    let mut permissions = std::fs::metadata(dir)?.permissions();
    if permissions.mode() & 0o777 != 0o700 {
        permissions.set_mode(0o700);
        std::fs::set_permissions(dir, permissions)?;
    }
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory {} is accessible beyond its owner",
                dir.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    // The runtime root resolves inside the current user's temp directory,
    // whose default ACLs already exclude other users.
    std::fs::create_dir_all(dir)
}

/// `<pid>-<salt>.sock`, with a salt mixed from the wall clock so a recycled
/// PID cannot collide with a leftover socket from a previous life of the
/// same PID. Deterministic so tests can pin the format.
fn socket_file_name(pid: u32, salt: u32) -> String {
    format!("{pid}-{salt:08x}.sock")
}

fn startup_salt(now: SystemTime) -> u32 {
    match now.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => (elapsed.as_secs() as u32) ^ elapsed.subsec_nanos().rotate_left(16),
        Err(_) => 0,
    }
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Instance side
// ---------------------------------------------------------------------------

/// Static identity one instance serves over the `info` handshake.
#[derive(Debug, Clone)]
struct ServedIdentity {
    info: InstanceInfo,
}

/// One forwarded `open` request waiting for the instance main loop, carrying
/// its one-shot reply channel. The connection thread parks on the channel, so
/// the loop must answer every request (even with an error) or the client sees
/// [`OPEN_REPLY_TIMEOUT`].
pub struct InstanceRequest {
    pub paths: Vec<String>,
    pub focus: Focus,
    pub reply: mpsc::SyncSender<ResponseBody>,
}

/// Where connection threads hand `open` requests to the instance's main loop.
/// The loop drains it synchronously each frame; see the app-side
/// `poll_instance_requests`.
pub type RequestInbox = Arc<Mutex<VecDeque<InstanceRequest>>>;

/// Create the inbox an instance hands to [`IpcServer::start_serving`].
pub fn new_request_inbox() -> RequestInbox {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Route an `open` request either straight to a decline (no inbox attached)
/// or through `inbox` to the instance main loop.
fn forward_open(paths: Vec<String>, focus: Focus, inbox: Option<&RequestInbox>) -> Response {
    let Some(inbox) = inbox else {
        return error_response("this instance does not accept 'open' requests");
    };
    if paths.is_empty() {
        return error_response("open requires at least one path");
    }
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let request = InstanceRequest {
        paths,
        focus,
        reply: reply_tx,
    };
    inbox
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(request);
    match reply_rx.recv_timeout(OPEN_REPLY_TIMEOUT) {
        Ok(body) => Response {
            proto: PROTOCOL_VERSION,
            body,
        },
        Err(_) => error_response("the instance did not answer the open request in time"),
    }
}

/// A bound instance listener serving handshakes on a background thread.
///
/// Dropping the server stops the thread and removes the socket file. If the
/// process dies first the file lingers, but discovery filters dead sockets
/// out by failed handshake, so no separate cleanup pass exists.
pub struct IpcServer {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Bind an instance socket in the process runtime directory and start
    /// serving handshakes. Failures are non-fatal by contract: callers warn
    /// and continue without instance discovery.
    pub fn start(root: &Path) -> io::Result<Self> {
        let dir = runtime_dir()?;
        Self::start_in(&dir, root)
    }

    /// Bind in the process runtime directory and forward `open` requests to
    /// `inbox`, for the instance's main loop to answer.
    pub fn start_serving(root: &Path, inbox: &RequestInbox) -> io::Result<Self> {
        let dir = runtime_dir()?;
        Self::start_in_serving(&dir, root, inbox)
    }

    /// Bind an instance socket in an explicit directory (tests, embedders).
    /// `open` requests are declined without a [`RequestInbox`].
    pub fn start_in(dir: &Path, root: &Path) -> io::Result<Self> {
        Self::bind_and_serve(dir, root, None)
    }

    /// Bind in an explicit directory (tests, embedders) and forward `open`
    /// requests to `inbox`.
    pub fn start_in_serving(dir: &Path, root: &Path, inbox: &RequestInbox) -> io::Result<Self> {
        Self::bind_and_serve(dir, root, Some(Arc::clone(inbox)))
    }

    fn bind_and_serve(dir: &Path, root: &Path, inbox: Option<RequestInbox>) -> io::Result<Self> {
        ensure_private_dir(dir)?;
        let (listener, socket_path) = bind_listener(dir)?;
        let thread_socket_path = socket_path.clone();
        let identity = Arc::new(ServedIdentity {
            info: InstanceInfo {
                proto: PROTOCOL_VERSION,
                pid: std::process::id(),
                root: root.display().to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                started_at_ms: unix_millis_now(),
            },
        });
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("lens-ipc".to_string())
            .spawn(move || {
                serve_until_stop(
                    listener,
                    thread_socket_path,
                    identity,
                    inbox,
                    thread_stop,
                    active,
                )
            })?;
        Ok(Self {
            socket_path,
            stop,
            thread: Some(thread),
        })
    }

    /// Path of the socket this instance serves.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Connect to the socket at `path` with a bounded deadline. `std` no longer
/// offers `UnixStream::connect_timeout`, so the connect runs on a helper
/// thread; a connect that misses the deadline strands that thread until the
/// OS completes it, which is acceptable for the bounded discovery probes and
/// the one startup stale-file probe that use this path.
fn connect_bounded(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let path = path.to_path_buf();
    std::thread::Builder::new()
        .name("lens-ipc-dial".to_string())
        .spawn(move || {
            let _ = sender.send(UnixStream::connect(&path));
        })
        .map_err(|error| io::Error::other(format!("connect worker unavailable: {error}")))?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "connect deadline exceeded",
        )),
        Err(_) => Err(io::Error::other("connect worker failed")),
    }
}

/// Bind a uniquely named socket in `dir`, reclaiming a leftover file only
/// when no live peer answers on it.
fn bind_listener(dir: &Path) -> io::Result<(UnixListener, PathBuf)> {
    let salt = startup_salt(SystemTime::now());
    let path = dir.join(socket_file_name(std::process::id(), salt));
    match UnixListener::bind(&path) {
        Ok(listener) => return Ok((listener, path)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists
            ) => {}
        Err(error) => return Err(error),
    }
    // A leftover entry with our name: a refused connect proves the owner is
    // gone and the name is reclaimable; a live peer answering means the name
    // is genuinely taken and we refuse to fight for it.
    match connect_bounded(&path, DISCOVERY_TIMEOUT) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "instance socket {} is served by another process",
                path.display()
            ),
        )),
        Err(_) => {
            std::fs::remove_file(&path)?;
            let listener = UnixListener::bind(&path)?;
            Ok((listener, path))
        }
    }
}

fn serve_until_stop(
    listener: UnixListener,
    socket_path: PathBuf,
    identity: Arc<ServedIdentity>,
    inbox: Option<RequestInbox>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
) {
    let _ = listener.set_nonblocking(true);
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_ACTIVE_CONNECTIONS {
                    active.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let identity = Arc::clone(&identity);
                let connection_inbox = inbox.clone();
                let connection_active = Arc::clone(&active);
                let spawned = std::thread::Builder::new()
                    .name("lens-ipc-conn".to_string())
                    .spawn(move || {
                        serve_connection(stream, identity, connection_inbox.as_ref());
                        connection_active.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(_) => std::thread::sleep(ACCEPT_POLL_INTERVAL),
        }
    }
    drop(listener);
    // Only reachable once the listener is closed, which matters on Windows:
    // an open socket file cannot be removed there.
    let _ = std::fs::remove_file(&socket_path);
}

fn serve_connection(
    mut stream: UnixStream,
    identity: Arc<ServedIdentity>,
    inbox: Option<&RequestInbox>,
) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let reply = match read_bounded_request(&mut stream) {
        Ok(RequestRead::Complete(payload)) if payload.len() <= MAX_REQUEST_BYTES => {
            respond(&payload, &identity, inbox)
        }
        Ok(RequestRead::Complete(_)) => error_response("request exceeds the protocol size bound"),
        Ok(RequestRead::Oversized) => {
            // The peer is still writing. Consume the rest of the request up
            // to a bounded budget until its terminator, so it can finish and
            // read our reply instead of a connection reset. Past the budget
            // (a hostile stream) the connection drops and the peer sees the
            // reset.
            drain_to_terminator(&mut stream, MAX_DRAIN_BYTES);
            error_response("request exceeds the protocol size bound")
        }
        Err(_) => error_response("request could not be read"),
    };
    let _ = write_response(&mut stream, &reply);
}

/// Outcome of reading one request off the wire.
enum RequestRead {
    /// The request terminator (newline) or EOF arrived within the bound.
    Complete(Vec<u8>),
    /// The bound was exceeded before the terminator arrived.
    Oversized,
}

/// Read one newline-terminated (or EOF-terminated) request document, bounded
/// by [`MAX_REQUEST_BYTES`]. Bytes after the first newline are ignored: a
/// connection carries exactly one request.
fn read_bounded_request(stream: &mut UnixStream) -> io::Result<RequestRead> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(RequestRead::Complete(buffer));
        }
        if let Some(end) = chunk[..read].iter().position(|&byte| byte == b'\n') {
            buffer.extend_from_slice(&chunk[..end]);
            return Ok(RequestRead::Complete(buffer));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_REQUEST_BYTES {
            return Ok(RequestRead::Oversized);
        }
    }
}

/// Consume up to `budget` further bytes, stopping at the request terminator,
/// so the peer can finish writing and read our reply without a reset.
fn drain_to_terminator(stream: &mut UnixStream, budget: usize) {
    let mut chunk = [0u8; 4096];
    let mut consumed = 0usize;
    while consumed < budget {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                consumed += read;
                if chunk[..read].contains(&b'\n') {
                    break;
                }
            }
        }
    }
}

fn respond(payload: &[u8], identity: &ServedIdentity, inbox: Option<&RequestInbox>) -> Response {
    match serde_json::from_slice::<Request>(payload) {
        Ok(request) if request.proto == PROTOCOL_VERSION => match request.body {
            RequestBody::Info => Response {
                proto: PROTOCOL_VERSION,
                body: ResponseBody::Info {
                    info: identity.info.clone(),
                },
            },
            RequestBody::Open { paths, focus } => forward_open(paths, focus, inbox),
        },
        Ok(_) => error_response(&format!(
            "unsupported protocol version; this instance speaks {PROTOCOL_VERSION}"
        )),
        Err(error) => error_response(&format!("malformed request: {error}")),
    }
}

fn error_response(message: &str) -> Response {
    Response {
        proto: PROTOCOL_VERSION,
        body: ResponseBody::Error {
            message: message.to_string(),
        },
    }
}

fn write_response(stream: &mut UnixStream, response: &Response) -> io::Result<()> {
    let mut line = serde_json::to_vec(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// One live instance proven by handshake.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub socket_path: PathBuf,
    pub info: InstanceInfo,
}

/// Enumerate live instances under `dir`. Missing directories yield an empty
/// list. At most [`MAX_DISCOVERED_INSTANCES`] sockets are probed, in sorted
/// file-name order, each with a [`DISCOVERY_TIMEOUT`] budget; results are
/// ordered deterministically by `(started_at_ms, pid)`.
pub fn discover(dir: &Path) -> Vec<Instance> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".sock"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    names.truncate(MAX_DISCOVERED_INSTANCES);
    let mut found = Vec::new();
    for name in names {
        let path = dir.join(&name);
        if let Ok(info) = exchange(&path, &RequestBody::Info, DISCOVERY_TIMEOUT)
            && let ResponseBody::Info { info } = info
        {
            found.push(Instance {
                socket_path: path,
                info,
            });
        }
    }
    found.sort_by(|left, right| {
        (left.info.started_at_ms, left.info.pid).cmp(&(right.info.started_at_ms, right.info.pid))
    });
    found
}

/// Send one request to the instance at `socket_path` and return its reply
/// body, checking the protocol version on the way.
pub fn send(socket_path: &Path, body: &RequestBody) -> Result<ResponseBody, IpcError> {
    // An `open` request may legitimately occupy the instance's whole
    // [`OPEN_REPLY_TIMEOUT`] budget, so the client's deadlines must exceed
    // it; otherwise a slow (not dead) instance would be reported as a bare
    // I/O timeout before its own expiry error could arrive.
    let timeout = match body {
        RequestBody::Open { .. } => OPEN_REPLY_TIMEOUT + Duration::from_secs(1),
        _ => IO_TIMEOUT,
    };
    exchange(socket_path, body, timeout)
}

fn exchange(
    socket_path: &Path,
    body: &RequestBody,
    timeout: Duration,
) -> Result<ResponseBody, IpcError> {
    let io_error = |source| IpcError::Io {
        path: socket_path.to_path_buf(),
        source,
    };
    let mut stream = connect_bounded(socket_path, timeout).map_err(io_error)?;
    stream.set_read_timeout(Some(timeout)).map_err(io_error)?;
    stream.set_write_timeout(Some(timeout)).map_err(io_error)?;

    let request = Request {
        proto: PROTOCOL_VERSION,
        body: body.clone(),
    };
    let mut line = serde_json::to_vec(&request)
        .map_err(|error| IpcError::Protocol(format!("request serialization failed: {error}")))?;
    if line.len() > MAX_REQUEST_BYTES {
        // Refuse locally: sending an oversized request can get the connection
        // reset before the instance's error reply arrives.
        return Err(IpcError::Protocol(
            "request exceeds the protocol size bound".to_string(),
        ));
    }
    line.push(b'\n');
    stream.write_all(&line).map_err(io_error)?;
    stream.flush().map_err(io_error)?;

    let mut reply = Vec::new();
    stream
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut reply)
        .map_err(io_error)?;
    if reply.len() > MAX_REQUEST_BYTES {
        return Err(IpcError::Protocol(
            "reply exceeds the protocol size bound".to_string(),
        ));
    }
    let response: Response = serde_json::from_slice(&reply)
        .map_err(|error| IpcError::Protocol(format!("malformed reply: {error}")))?;
    if response.proto != PROTOCOL_VERSION {
        return Err(IpcError::Protocol(format!(
            "unsupported protocol version {}; this build speaks {PROTOCOL_VERSION}",
            response.proto
        )));
    }
    Ok(response.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        tempfile::tempdir().expect("sandbox").path().to_path_buf()
    }

    #[test]
    fn socket_names_pin_pid_and_deterministic_salt() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let salt = startup_salt(now);
        assert_eq!(socket_file_name(4242, salt), socket_file_name(4242, salt));
        assert!(socket_file_name(4242, salt).ends_with(".sock"));
        assert!(socket_file_name(4242, salt).starts_with("4242-"));
        assert_ne!(socket_file_name(4242, salt), socket_file_name(4243, salt));
    }

    #[test]
    fn protocol_documents_round_trip_with_defaults() {
        let open = r#"{"proto":1,"cmd":"open","paths":["/tmp/a.md"]}"#;
        let request: Request = serde_json::from_str(open).expect("parse open");
        assert_eq!(
            request.body,
            RequestBody::Open {
                paths: vec!["/tmp/a.md".to_string()],
                focus: Focus::Active,
            }
        );
        let encoded = serde_json::to_string(&request).expect("encode");
        assert_eq!(
            serde_json::from_str::<Request>(&encoded).expect("reparse"),
            request
        );

        let info = Response {
            proto: PROTOCOL_VERSION,
            body: ResponseBody::Info {
                info: InstanceInfo {
                    proto: PROTOCOL_VERSION,
                    pid: 7,
                    root: "/work".to_string(),
                    version: "0.0.0".to_string(),
                    started_at_ms: 1,
                },
            },
        };
        let encoded = serde_json::to_string(&info).expect("encode reply");
        assert_eq!(
            serde_json::from_str::<Response>(&encoded).expect("reparse"),
            info
        );
    }

    #[test]
    fn a_server_serves_info_and_cleans_up_its_socket() {
        let dir = temp_dir();
        let root = temp_dir();
        let server = IpcServer::start_in(&dir, &root).expect("start server");
        assert!(server.socket_path().exists());

        let found = discover(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].info.pid, std::process::id());
        assert_eq!(found[0].info.proto, PROTOCOL_VERSION);
        assert_eq!(found[0].info.version, env!("CARGO_PKG_VERSION").to_string());
        assert_eq!(found[0].socket_path, server.socket_path());

        match send(server.socket_path(), &RequestBody::Info).expect("info") {
            ResponseBody::Info { info } => assert_eq!(info, found[0].info),
            other => panic!("expected info reply, got {other:?}"),
        }

        drop(server);
        assert!(!any_socket_file_present(&dir));
    }

    /// Whether any `*.sock` entry remains under `dir`.
    fn any_socket_file_present(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .expect("dir")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".sock"))
    }

    #[test]
    fn open_requests_are_declined_without_a_request_inbox() {
        let dir = temp_dir();
        let root = temp_dir();
        let server = IpcServer::start_in(&dir, &root).expect("start server");
        let reply = send(
            server.socket_path(),
            &RequestBody::Open {
                paths: vec!["/tmp/a.md".to_string()],
                focus: Focus::NewTab,
            },
        )
        .expect("open exchange");
        match reply {
            ResponseBody::Error { message } => {
                assert!(message.contains("does not accept 'open' requests"))
            }
            other => panic!("expected error reply, got {other:?}"),
        }
    }

    #[test]
    fn open_requests_are_forwarded_through_the_inbox_and_answered() {
        let dir = temp_dir();
        let root = temp_dir();
        let inbox = new_request_inbox();
        let server = IpcServer::start_in_serving(&dir, &root, &inbox).expect("start server");

        // Stand in for the instance main loop: drain one forwarded request,
        // then answer over its one-shot reply channel.
        let drainer = std::thread::spawn(move || {
            loop {
                let request = inbox
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .pop_front();
                match request {
                    Some(request) => {
                        assert_eq!(request.paths, vec!["/tmp/a.md".to_string()]);
                        assert_eq!(request.focus, Focus::NewTab);
                        request
                            .reply
                            .send(ResponseBody::Opened {
                                opened: request.paths.clone(),
                            })
                            .expect("reply");
                        break;
                    }
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });

        let reply = send(
            server.socket_path(),
            &RequestBody::Open {
                paths: vec!["/tmp/a.md".to_string()],
                focus: Focus::NewTab,
            },
        )
        .expect("open exchange");
        match reply {
            ResponseBody::Opened { opened } => assert_eq!(opened, vec!["/tmp/a.md".to_string()]),
            other => panic!("expected opened reply, got {other:?}"),
        }
        drainer.join().expect("drainer thread");
    }

    #[test]
    fn open_requests_time_out_when_the_instance_never_answers() {
        let dir = temp_dir();
        let root = temp_dir();
        let inbox = new_request_inbox();
        let server = IpcServer::start_in_serving(&dir, &root, &inbox).expect("start server");
        // Nobody drains the inbox, so the connection thread must give up on
        // its own instead of parking forever.
        let reply = send(
            server.socket_path(),
            &RequestBody::Open {
                paths: vec!["/tmp/a.md".to_string()],
                focus: Focus::Active,
            },
        )
        .expect("open exchange");
        match reply {
            ResponseBody::Error { message } => {
                assert!(message.contains("did not answer"))
            }
            other => panic!("expected error reply, got {other:?}"),
        }
    }

    #[test]
    fn protocol_version_mismatches_fail_with_a_message() {
        let dir = temp_dir();
        let root = temp_dir();
        let server = IpcServer::start_in(&dir, &root).expect("start server");
        let mut stream = UnixStream::connect(server.socket_path()).expect("connect");
        stream
            .write_all(b"{\"proto\":99,\"cmd\":\"info\"}\n")
            .expect("write");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).expect("read");
        let response: Response = serde_json::from_slice(&reply).expect("parse reply");
        match response.body {
            ResponseBody::Error { message } => {
                assert!(message.contains("protocol version"));
            }
            other => panic!("expected error reply, got {other:?}"),
        }
    }

    #[test]
    fn malformed_and_oversized_requests_get_error_replies() {
        let dir = temp_dir();
        let root = temp_dir();
        let server = IpcServer::start_in(&dir, &root).expect("start server");

        let mut stream = UnixStream::connect(server.socket_path()).expect("connect");
        stream.write_all(b"definitely not json\n").expect("write");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).expect("read");
        let response: Response = serde_json::from_slice(&reply).expect("parse reply");
        assert!(matches!(response.body, ResponseBody::Error { .. }));

        let oversized = format!("\"{}\"\n", "x".repeat(MAX_REQUEST_BYTES + 1024));
        let mut stream = UnixStream::connect(server.socket_path()).expect("connect");
        stream.write_all(oversized.as_bytes()).expect("write");
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).expect("read");
        let response: Response = serde_json::from_slice(&reply).expect("parse reply");
        match response.body {
            ResponseBody::Error { message } => assert!(message.contains("bound")),
            other => panic!("expected error reply, got {other:?}"),
        }
    }

    #[test]
    fn discovery_skips_stale_files_and_missing_directories() {
        let dir = temp_dir();
        assert!(discover(&dir).is_empty());

        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("424242-deadbeef.sock"), b"stale").expect("stale file");
        assert!(discover(&dir).is_empty());
    }

    #[test]
    fn two_instances_in_one_directory_are_both_discoverable() {
        let dir = temp_dir();
        let root = temp_dir();
        let first = IpcServer::start_in(&dir, &root).expect("first");
        let second = IpcServer::start_in(&dir, &root).expect("second");
        let found = discover(&dir);
        assert_eq!(found.len(), 2);
        assert_ne!(found[0].socket_path, found[1].socket_path);
        assert!(found[0].info.started_at_ms <= found[1].info.started_at_ms);
        drop(first);
        drop(second);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_directories_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        let nested = dir.join("instances");
        ensure_private_dir(&nested).expect("ensure");
        let mode = std::fs::metadata(&nested)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn the_runtime_dir_override_places_sockets_under_instances() {
        let _guard = crate::test_support::lock_env();
        let sandbox = temp_dir();
        let _env = crate::test_support::EnvironmentGuard::apply(&[(
            "LATTE_LENS_RUNTIME_DIR",
            Some(sandbox.clone().into_os_string()),
        )]);
        assert_eq!(
            runtime_dir().expect("runtime dir"),
            sandbox.join("instances")
        );
    }
}
