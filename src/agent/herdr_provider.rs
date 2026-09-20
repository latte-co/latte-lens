//! Herdr read-only snapshot provider (stage 1).
//!
//! Owns one background poll thread that runs `herdr agent list` (and
//! `herdr --version` at a slow cadence) under the same subprocess discipline
//! as `src/send_agent.rs`: separate argv arguments, no shell, bounded
//! timeouts, ETXTBSY retries, and SIGKILL on overrun. The argv allowlist is
//! exactly those two shapes — this provider has no send/focus/read channel.
//!
//! Trait methods only read a bounded shared cache, so they stay inside the
//! runtime's 10 ms provider poll budget (`docs/design/herdr-observation-
//! provider.md` §4).

use std::{
    io,
    process::{Child, Command, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::herdr::{HERDR_ENTRY_EVENT, HERDR_SNAPSHOT_OBSERVER_ID, HerdrEntry, herdr_instance_id};
use super::hook_json::HookJsonParser;
use super::{
    AdapterError, BoundedBytes, BoundedText, BoundedVec, CodeAgentAdapter, ContractRevision,
    HerdrSnapshotAdapter, InstanceContract, ObservationProvider, ObserverId, ObserverInstanceId,
    ProviderCursor, ProviderDiscoveryLimits, ProviderEndpointKind, ProviderError,
    ProviderEventOutcome, ProviderHealth, ProviderInstance, RawProviderItem, RawSnapshot,
    SnapshotLimits, Timestamp, WorkspaceSelector,
};

/// Poll cadence for `herdr agent list` (design §6.1: 5s default, tunable
/// 2–10s before profiling).
const HERDR_POLL_CADENCE: Duration = Duration::from_secs(5);
/// `herdr --version` re-check interval; a change is a restart signal.
const HERDR_VERSION_INTERVAL: Duration = Duration::from_secs(60);
/// Per-call subprocess timeout, matching the #26 discover timeout.
const HERDR_CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// stdout read cap for one `agent list` reply. A reply cut by this cap is
/// rejected fail-closed; it is never silently truncated (design §8).
const HERDR_STDOUT_CAP: usize = 128 * 1024;
/// Maximum agent entries served per snapshot; beyond that the snapshot is
/// served as `Truncated` with the stable pane-sorted prefix.
const HERDR_MAX_AGENT_ENTRIES: usize = 64;
/// A cached list older than this is not served: one poll interval plus one
/// tolerance round (design §3 Freshness). Older data must surface as
/// `Unavailable`, never as a current snapshot.
const HERDR_CACHE_FRESHNESS: Duration = Duration::from_secs(10);
/// Consecutive failures before health degrades (design §6.1).
const HERDR_DEGRADED_FAILURES: u32 = 2;

struct CachedList {
    captured_at: Timestamp,
    /// `false` when the source list exceeded the entry cap and only the
    /// stable prefix was kept.
    complete: bool,
    /// Raw per-entry JSON byte slices in stable pane order. They are handed
    /// to the Herdr adapter in-process and never persisted or logged.
    items: Vec<Vec<u8>>,
}

struct ProviderState {
    draining: bool,
    health: ProviderHealth,
    /// Internal epoch counter encoded into the probe contract revision
    /// (design §4.3 two-phase reset).
    epoch_counter: u64,
    version: Option<String>,
    cache: Option<CachedList>,
    failures: u32,
    /// Last successful outer-shape validation verdict.
    shape_valid: bool,
    /// Per-pane `state_change_seq` from the last accepted list; a
    /// regression means the server restarted. Never leaves this struct.
    last_seqs: std::collections::BTreeMap<String, u64>,
}

impl ProviderState {
    fn new() -> Self {
        Self {
            draining: false,
            health: ProviderHealth::Available,
            epoch_counter: 1,
            version: None,
            cache: None,
            failures: 0,
            shape_valid: false,
            last_seqs: std::collections::BTreeMap::new(),
        }
    }
}

/// Read-only Herdr snapshot provider for one local Herdr server.
///
/// The raw socket path, pane ids, and native agent identities exist only
/// inside this struct and the poll thread; everything served through the
/// trait is bounded and non-sensitive.
pub struct HerdrSnapshotProvider {
    binary: String,
    cadence: Duration,
    version_interval: Duration,
    instance: ObserverInstanceId,
    adapter: HerdrSnapshotAdapter,
    shared: Arc<(Mutex<ProviderState>, Condvar)>,
    worker: Option<thread::JoinHandle<()>>,
}

fn unix_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn lock_state(mutex: &Mutex<ProviderState>) -> std::sync::MutexGuard<'_, ProviderState> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl HerdrSnapshotProvider {
    /// Environment gate (design §2.2): `HERDR_ENV == "1"` AND a non-empty
    /// `HERDR_SOCKET_PATH`. Binary resolution: `HERDR_BIN_PATH`, falling
    /// back to `herdr` on PATH.
    pub fn from_environment() -> Option<Self> {
        let enabled = std::env::var("HERDR_ENV").is_ok_and(|value| value == "1");
        if !enabled {
            return None;
        }
        let socket_path = std::env::var("HERDR_SOCKET_PATH")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())?;
        let binary = std::env::var("HERDR_BIN_PATH")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "herdr".to_owned());
        Some(Self::new(binary, socket_path, HERDR_POLL_CADENCE))
    }

    fn new(binary: String, socket_path: String, cadence: Duration) -> Self {
        Self::with_timings(binary, socket_path, cadence, HERDR_VERSION_INTERVAL)
    }

    fn with_timings(
        binary: String,
        socket_path: String,
        cadence: Duration,
        version_interval: Duration,
    ) -> Self {
        let instance = herdr_instance_id(&socket_path);
        Self {
            binary,
            cadence,
            version_interval,
            instance,
            adapter: HerdrSnapshotAdapter::new(),
            shared: Arc::new((Mutex::new(ProviderState::new()), Condvar::new())),
            worker: None,
        }
    }

    fn observer() -> ObserverId {
        ObserverId::parse(HERDR_SNAPSHOT_OBSERVER_ID).expect("static Herdr observer id")
    }

    fn matches(&self, instance: &ProviderInstance) -> bool {
        instance.observer == Self::observer() && instance.instance == self.instance
    }

    /// Spawn the poll thread lazily: the first `discover` starts polling.
    /// Later trait calls reuse the running worker.
    fn ensure_worker(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let binary = self.binary.clone();
        let cadence = self.cadence;
        let version_interval = self.version_interval;
        let shared = Arc::clone(&self.shared);
        let worker = thread::Builder::new()
            .name("herdr-snapshot-poll".to_owned())
            .spawn(move || poll_loop(binary, shared, cadence, version_interval))
            .expect("herdr poll thread spawns");
        self.worker = Some(worker);
    }
}

/// One poll iteration result handed to the state writer.
struct PollOutcome {
    verdict: PollVerdict,
    /// The UTF-8-validated `agent list` stdout on success.
    list: Option<String>,
    version: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollVerdict {
    Succeeded,
    Failed,
    /// The Herdr binary is gone; no poll can ever succeed.
    Terminal,
}

fn poll_once(binary: &str, need_version: bool) -> PollOutcome {
    // A version probe failure (other than a missing binary) is tolerated:
    // restart detection simply keeps the previous version until the next
    // interval (design §4.3).
    let version = if need_version {
        match run_capture(binary, &["--version"], HERDR_CALL_TIMEOUT) {
            Ok(output) => Some(output.trim().chars().take(64).collect::<String>()),
            Err(CaptureError::NotFound) => {
                return PollOutcome {
                    verdict: PollVerdict::Terminal,
                    list: None,
                    version: None,
                };
            }
            Err(_) => None,
        }
    } else {
        None
    };
    match run_capture(binary, &["agent", "list"], HERDR_CALL_TIMEOUT) {
        Ok(stdout) => PollOutcome {
            verdict: PollVerdict::Succeeded,
            list: Some(stdout),
            version,
        },
        Err(CaptureError::NotFound) => PollOutcome {
            verdict: PollVerdict::Terminal,
            list: None,
            version,
        },
        Err(_) => PollOutcome {
            verdict: PollVerdict::Failed,
            list: None,
            version,
        },
    }
}

fn poll_loop(
    binary: String,
    shared: Arc<(Mutex<ProviderState>, Condvar)>,
    cadence: Duration,
    version_interval: Duration,
) {
    let (state_lock, tick) = &*shared;
    let mut next_version_check = Instant::now();
    loop {
        let need_version = Instant::now() >= next_version_check;
        if need_version {
            next_version_check = Instant::now() + version_interval;
        }
        let outcome = poll_once(&binary, need_version);
        {
            let mut state = lock_state(state_lock);
            if state.draining {
                return;
            }
            match outcome.verdict {
                PollVerdict::Terminal => {
                    state.health = ProviderHealth::Unavailable;
                    state.cache = None;
                    return;
                }
                PollVerdict::Failed => {
                    state.failures = state.failures.saturating_add(1);
                    if state.failures >= HERDR_DEGRADED_FAILURES {
                        state.health = ProviderHealth::Degraded;
                    }
                }
                PollVerdict::Succeeded => {
                    let stdout = outcome.list.expect("succeeded carries the list");
                    apply_successful_poll(&mut state, stdout.as_bytes(), outcome.version);
                }
            }
        }
        // The condvar makes draining responsive: begin_draining notifies and
        // the loop exits within one wakeup instead of one full cadence.
        let (state, _timeout) = tick
            .wait_timeout(lock_state(state_lock), cadence)
            .unwrap_or_else(|poison| poison.into_inner());
        if state.draining {
            return;
        }
    }
}

/// Fold one successful `agent list` capture into the shared state:
/// restart detection (seq regression or version change → epoch bump and
/// cache invalidation), stable pane ordering, and entry-level dropping of
/// entries without a stable pane identity (design §8 单条目丢弃).
fn apply_successful_poll(state: &mut ProviderState, stdout: &[u8], version: Option<String>) {
    let sliced = match slice_agent_list(stdout) {
        Ok(sliced) => sliced,
        // Shape drift is fail-closed: the previous cache is kept but the
        // instance is demoted until a later poll proves the expected
        // envelope again.
        Err(_) => {
            state.failures = state.failures.saturating_add(1);
            state.shape_valid = false;
            state.health = ProviderHealth::Degraded;
            return;
        }
    };

    let mut parsed: Vec<(String, u64, &[u8])> = Vec::with_capacity(sliced.spans.len());
    for (start, end) in &sliced.spans {
        let span = &stdout[*start..*end];
        if let Ok(Some(entry)) = HerdrEntry::parse(span) {
            parsed.push((entry.pane_id().to_owned(), entry.state_change_seq(), span));
        }
        // Entries without a stable pane identity (or a syntactically broken
        // single entry) are dropped; the rest of the list still flows
        // (design §8).
    }

    // Restart detection (design §4.3): a per-pane sequence regression or a
    // server version change invalidates the cached list and bumps the epoch
    // counter. The next probe's contract revision follows, giving the
    // runtime a clean two-phase reset instead of a silent merge across
    // server lifetimes.
    let seq_regression = parsed
        .iter()
        .any(|(pane, seq, _)| state.last_seqs.get(pane).is_some_and(|prev| *seq < *prev));
    let version_changed = version
        .as_ref()
        .zip(state.version.as_ref())
        .is_some_and(|(new, old)| new != old);
    if let Some(version) = version {
        state.version = Some(version);
    }
    if seq_regression || version_changed {
        state.epoch_counter = state.epoch_counter.saturating_add(1);
        state.cache = None;
        state.last_seqs.clear();
        state.failures = 0;
        state.shape_valid = true;
        state.health = ProviderHealth::Available;
        return;
    }

    // Stable ordering: the pane-id sort keeps the truncated prefix stable.
    parsed.sort_by(|a, b| a.0.cmp(&b.0));
    state.last_seqs = parsed
        .iter()
        .map(|(pane, seq, _)| (pane.clone(), *seq))
        .collect();
    let items = parsed
        .into_iter()
        .map(|(_, _, span)| span.to_vec())
        .collect();
    state.cache = Some(CachedList {
        captured_at: Timestamp::from_unix_millis(unix_now_millis()),
        complete: sliced.complete,
        items,
    });
    state.failures = 0;
    state.shape_valid = true;
    state.health = ProviderHealth::Available;
}

enum CaptureError {
    NotFound,
    Timeout,
    Failed,
    TooLarge,
    InvalidUtf8,
}

/// Same subprocess discipline as `src/send_agent.rs::HerdrProvider::run`:
/// separate argv arguments, no shell, null stdin, detached waiter with a
/// bounded channel, SIGKILL on timeout, ETXTBSY retries. Errors collapse
/// into the coarse [`CaptureError`] shapes the poll loop needs.
fn run_capture(binary: &str, args: &[&str], timeout: Duration) -> Result<String, CaptureError> {
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn_with_retry(&mut command)?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = thread::spawn(move || tx.send(child.wait_with_output()));
    let output = match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => {
            let _ = waiter.join();
            return Err(CaptureError::Failed);
        }
        Err(_) => {
            // Best-effort SIGKILL so a wedged backend cannot outlive the
            // timeout; the detached waiter then reaps the exit.
            kill_process(pid);
            let _ = waiter.join();
            return Err(CaptureError::Timeout);
        }
    };
    if !output.status.success() {
        return Err(CaptureError::Failed);
    }
    if output.stdout.len() > HERDR_STDOUT_CAP {
        return Err(CaptureError::TooLarge);
    }
    String::from_utf8(output.stdout).map_err(|_| CaptureError::InvalidUtf8)
}

fn spawn_with_retry(command: &mut Command) -> Result<Child, CaptureError> {
    let mut delay_ms = 10_u64;
    for _ in 0..3 {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if is_text_busy(&error) => {
                // The executable is briefly open for writing (fresh fake
                // binaries in tests); retry with linear backoff.
                thread::sleep(Duration::from_millis(delay_ms));
                delay_ms = delay_ms.saturating_mul(2);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CaptureError::NotFound);
            }
            Err(_) => return Err(CaptureError::Failed),
        }
    }
    Err(CaptureError::Failed)
}

/// Whether a spawn failed with ETXTBSY. POSIX reports raw error 26;
/// Windows has no equivalent state.
#[cfg(unix)]
fn is_text_busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

#[cfg(not(unix))]
fn is_text_busy(_error: &io::Error) -> bool {
    false
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    // SAFETY: a plain signal syscall with a pid we spawned.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    // SAFETY: a plain terminate call on a handle we opened for a pid we
    // spawned.
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// One accepted `agent list` outer envelope, sliced into entry spans.
struct SlicedList {
    /// Byte spans `[start, end)` into the captured stdout, in source order.
    spans: Vec<(usize, usize)>,
    /// `false` when the source array had more entries than the cap.
    complete: bool,
}

/// Validate the outer shape (`result.type == "agent_list"`, `result.agents`
/// is an array) and slice entry spans (design §4.2). Span collection stops
/// one past the entry cap so truncation is detected, never silent.
fn slice_agent_list(stdout: &[u8]) -> Result<SlicedList, AdapterError> {
    let mut list_type_seen = false;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut parser = HookJsonParser::new(stdout);
    parser.parse_object(|parser, key| match key {
        "result" => parser.parse_object(|parser, key| match key {
            "type" => {
                if parser.parse_bounded_string()? != "agent_list" {
                    return Err(AdapterError::MalformedInput);
                }
                list_type_seen = true;
                Ok(())
            }
            "agents" => parser.parse_array_spans(|start, end| {
                if spans.len() <= HERDR_MAX_AGENT_ENTRIES {
                    spans.push((start, end));
                }
                Ok(())
            }),
            _ => parser.skip_value(1),
        }),
        _ => parser.skip_value(1),
    })?;
    parser.finish()?;
    if !list_type_seen {
        return Err(AdapterError::MalformedInput);
    }
    let complete = spans.len() <= HERDR_MAX_AGENT_ENTRIES;
    spans.truncate(HERDR_MAX_AGENT_ENTRIES);
    Ok(SlicedList { spans, complete })
}

impl ObservationProvider for HerdrSnapshotProvider {
    fn observer_id(&self) -> ObserverId {
        Self::observer()
    }

    /// The workspace selector is intentionally ignored: one provider
    /// instance serves exactly one local Herdr server, and workspace
    /// scoping happens downstream via adapter workspace hints and reducer
    /// scope rules.
    fn discover(
        &mut self,
        _selector: &WorkspaceSelector,
        _limits: ProviderDiscoveryLimits,
        _deadline: Instant,
    ) -> Result<BoundedVec<ProviderInstance, 32>, ProviderError> {
        self.ensure_worker();
        let state = lock_state(&self.shared.0);
        if state.draining || state.health == ProviderHealth::Unavailable {
            return Ok(BoundedVec::new());
        }
        let instance = ProviderInstance {
            observer: Self::observer(),
            instance: self.instance.clone(),
            version: bounded_version(state.version.as_deref())?,
            endpoint_kind: ProviderEndpointKind::LocalSocket,
            health: state.health,
        };
        let mut instances = BoundedVec::new();
        instances
            .try_push(instance)
            .map_err(|_| ProviderError::BoundsExceeded)?;
        Ok(instances)
    }

    /// Materialize the contract from the static adapter template with the
    /// internal epoch counter as the revision (two-phase reset, §4.3). The
    /// outer-shape verdict comes from the last poll; no subprocess ever
    /// runs on this path.
    fn probe(
        &mut self,
        instance: &ProviderInstance,
        _deadline: Instant,
    ) -> Result<InstanceContract, ProviderError> {
        if !self.matches(instance) {
            return Err(ProviderError::Incompatible);
        }
        self.ensure_worker();
        let state = lock_state(&self.shared.0);
        if state.draining || state.health == ProviderHealth::Unavailable {
            return Err(ProviderError::Unavailable);
        }
        if !state.shape_valid {
            return Err(ProviderError::InvalidResponse);
        }
        if state.cache.is_none() {
            // No successful poll yet (or a reset just invalidated the
            // cache): the contract is knowable but the stream cannot be
            // trusted until a fresh list lands.
            return Err(ProviderError::Unavailable);
        }
        let template = self.adapter.contract_template(state.version.as_deref());
        Ok(template.hook_contract(
            self.instance.clone(),
            ContractRevision::new(state.epoch_counter),
            bounded_version(state.version.as_deref())?,
        ))
    }

    fn snapshot(
        &mut self,
        instance: &ProviderInstance,
        _cursor: Option<&ProviderCursor>,
        limits: SnapshotLimits,
        _deadline: Instant,
    ) -> Result<RawSnapshot, ProviderError> {
        if !self.matches(instance) {
            return Err(ProviderError::Incompatible);
        }
        let state = lock_state(&self.shared.0);
        if state.draining || state.health == ProviderHealth::Unavailable {
            return Err(ProviderError::Unavailable);
        }
        if !state.shape_valid {
            return Err(ProviderError::InvalidResponse);
        }
        let cache = state.cache.as_ref().ok_or(ProviderError::Unavailable)?;
        let age = unix_now_millis().saturating_sub(cache.captured_at.as_unix_millis());
        if age > u64::try_from(HERDR_CACHE_FRESHNESS.as_millis()).unwrap_or(u64::MAX) {
            return Err(ProviderError::Unavailable);
        }
        let mut items = BoundedVec::new();
        let mut complete = cache.complete;
        let mut total = 0_usize;
        for item in &cache.items {
            if items.len() >= limits.max_items.min(HERDR_MAX_AGENT_ENTRIES) {
                complete = false;
                break;
            }
            let bounded =
                BoundedBytes::try_new(item.clone()).map_err(|_| ProviderError::BoundsExceeded)?;
            total = total
                .checked_add(bounded.as_slice().len())
                .ok_or(ProviderError::BoundsExceeded)?;
            if total > limits.max_total_bytes {
                complete = false;
                break;
            }
            items
                .try_push(RawProviderItem {
                    event_name: BoundedText::try_new(HERDR_ENTRY_EVENT)
                        .map_err(|_| ProviderError::InvalidResponse)?,
                    observed_at: cache.captured_at,
                    payload: bounded,
                })
                .map_err(|_| ProviderError::BoundsExceeded)?;
        }
        RawSnapshot::try_new(None, None, complete, items).map_err(|_| ProviderError::BoundsExceeded)
    }

    /// Stage 1 is snapshot-only: there is no event stream yet (§4.2).
    fn next_event(
        &mut self,
        instance: &ProviderInstance,
        _deadline: Instant,
    ) -> ProviderEventOutcome {
        if !self.matches(instance) {
            return ProviderEventOutcome::Failed(ProviderError::Incompatible);
        }
        let state = lock_state(&self.shared.0);
        if state.draining {
            ProviderEventOutcome::Closed
        } else {
            ProviderEventOutcome::Idle
        }
    }

    /// Stop polling and drop the cache. No message is ever sent to the
    /// Herdr server — not even an exit or unsubscribe.
    fn begin_draining(&mut self) {
        {
            let mut state = lock_state(&self.shared.0);
            state.draining = true;
            state.cache = None;
        }
        self.shared.1.notify_all();
        // The worker is deliberately not joined: it may be inside a bounded
        // subprocess call, and begin_draining must return within budget.
        // Dropping the handle detaches the thread; it exits within one
        // wakeup.
        self.worker = None;
    }
}

impl Drop for HerdrSnapshotProvider {
    fn drop(&mut self) {
        // Same non-blocking shutdown as begin_draining.
        lock_state(&self.shared.0).draining = true;
        self.shared.1.notify_all();
    }
}

fn bounded_version(version: Option<&str>) -> Result<Option<BoundedText<64>>, ProviderError> {
    version
        .map(BoundedText::try_new)
        .transpose()
        .map_err(|_| ProviderError::InvalidResponse)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::MutexGuard,
    };

    // Env mutation is process-global in the lib test binary: reuse the
    // adapter tests' lock so provider and adapter env tests never overlap.
    use super::super::herdr::tests::ENV_LOCK;

    const TEST_CADENCE: Duration = Duration::from_millis(25);
    const TEST_VERSION_INTERVAL: Duration = Duration::from_millis(150);

    struct EnvGuard<'a> {
        _guard: MutexGuard<'a, ()>,
        saved_env: Option<std::ffi::OsString>,
        saved_socket: Option<std::ffi::OsString>,
        saved_bin: Option<std::ffi::OsString>,
    }

    impl EnvGuard<'_> {
        fn configure(socket: Option<&str>, bin: Option<&Path>, enabled: bool) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
            let saved_env = std::env::var_os("HERDR_ENV");
            let saved_socket = std::env::var_os("HERDR_SOCKET_PATH");
            let saved_bin = std::env::var_os("HERDR_BIN_PATH");
            // SAFETY: serialized on ENV_LOCK shared with the adapter tests.
            unsafe {
                if enabled {
                    std::env::set_var("HERDR_ENV", "1");
                } else {
                    std::env::remove_var("HERDR_ENV");
                }
                match socket {
                    Some(socket) => std::env::set_var("HERDR_SOCKET_PATH", socket),
                    None => std::env::remove_var("HERDR_SOCKET_PATH"),
                }
                if let Some(bin) = bin {
                    std::env::set_var("HERDR_BIN_PATH", bin);
                }
            }
            Self {
                _guard: guard,
                saved_env,
                saved_socket,
                saved_bin,
            }
        }

        fn restore(name: &str, value: &Option<std::ffi::OsString>) {
            // SAFETY: serialized on ENV_LOCK shared with the adapter tests.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            Self::restore("HERDR_ENV", &self.saved_env);
            Self::restore("HERDR_SOCKET_PATH", &self.saved_socket);
            Self::restore("HERDR_BIN_PATH", &self.saved_bin);
        }
    }

    fn shell() -> &'static str {
        for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
            if Path::new(candidate).exists() {
                return candidate;
            }
        }
        "/bin/sh"
    }

    /// Install a fake `herdr` binary plus switchable fixture files. The
    /// script answers `--version` and `agent list` from fixture files and
    /// appends every invocation's argv to calls.log, NUL-joined, so tests
    /// can lock the exact argv protocol.
    fn install_fake_herdr(dir: &Path) -> PathBuf {
        let script = dir.join("fake-herdr");
        let log = dir.join("calls.log");
        let body = format!(
            r#"#!{shell}
set -eu
log="{log}"
printf '%s\0' "$@" >> "$log"
if [ "$1" = "--version" ]; then
  cat "{version}"
  exit 0
fi
if [ "$1" = "agent" ] && [ "$2" = "list" ]; then
  cat "{agents}"
  exit 0
fi
echo "unexpected invocation: $*" >&2
exit 2
"#,
            shell = shell(),
            log = log.display(),
            version = dir.join("version.txt").display(),
            agents = dir.join("agents.json").display(),
        );
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        // No eager exec: invoking the freshly-written script races the
        // kernel's executable/write window (ETXTBSY) under load; the
        // provider retries.
        script
    }

    fn write_atomic(path: &Path, bytes: &[u8]) {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes).unwrap();
        fs::rename(&tmp, path).unwrap();
    }

    fn write_version(dir: &Path, text: &str) {
        write_atomic(&dir.join("version.txt"), text.as_bytes());
    }

    fn write_agents(dir: &Path, json: &str) {
        write_atomic(&dir.join("agents.json"), json.as_bytes());
    }

    fn entry(pane: &str, agent: &str, status: &str, seq: u64) -> String {
        format!(
            r#"{{"agent":"{agent}","agent_status":"{status}","cwd":"/workspace/repo","pane_id":"{pane}","state_change_seq":{seq}}}"#
        )
    }

    fn agents_json(entries: &[String]) -> String {
        let joined = entries.join(",\n");
        format!(r#"{{"id":"cli:agent:list","result":{{"agents":[{joined}],"type":"agent_list"}}}}"#)
    }

    fn provider(dir: &Path) -> HerdrSnapshotProvider {
        let bin = install_fake_herdr(dir);
        HerdrSnapshotProvider::with_timings(
            bin.to_str().unwrap().to_owned(),
            "/tmp/herdr-provider-test.sock".to_owned(),
            TEST_CADENCE,
            TEST_VERSION_INTERVAL,
        )
    }

    /// Poll the shared state until `check` holds, with a generous deadline
    /// for the loaded shared CI machine.
    fn wait_for(provider: &HerdrSnapshotProvider, check: impl Fn(&ProviderState) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if check(&lock_state(&provider.shared.0)) {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn cache_len(state: &ProviderState) -> usize {
        state.cache.as_ref().map_or(0, |cache| cache.items.len())
    }

    fn selector() -> WorkspaceSelector {
        WorkspaceSelector::default()
    }

    fn limits() -> SnapshotLimits {
        SnapshotLimits {
            max_items: 64,
            max_total_bytes: 256 * 1024,
        }
    }

    #[test]
    fn from_environment_requires_env_and_socket() {
        let dir = tempfile::tempdir().unwrap();
        let bin = install_fake_herdr(dir.path());
        {
            let _guard = EnvGuard::configure(Some("/tmp/herdr-x.sock"), Some(&bin), true);
            assert!(HerdrSnapshotProvider::from_environment().is_some());
        }
        {
            // HERDR_ENV=1 but no socket: the server is not identifiable.
            let _guard = EnvGuard::configure(None, Some(&bin), true);
            assert!(HerdrSnapshotProvider::from_environment().is_none());
        }
        {
            // Socket present but HERDR_ENV is not "1": Herdr is invisible.
            let _guard = EnvGuard::configure(Some("/tmp/herdr-x.sock"), Some(&bin), false);
            assert!(HerdrSnapshotProvider::from_environment().is_none());
        }
    }

    #[test]
    fn polls_agent_list_and_serves_cached_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(
            path,
            &agents_json(&[
                entry("w3:p9", "codex", "idle", 4),
                entry("w1:p2", "claude", "working", 7),
            ]),
        );
        let mut provider = provider(path);

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(instances.len(), 1);
        let instance = &instances[0];
        assert_eq!(instance.observer, HerdrSnapshotProvider::observer());
        assert_eq!(instance.endpoint_kind, ProviderEndpointKind::LocalSocket);
        assert_eq!(instance.health, ProviderHealth::Available);

        assert!(
            wait_for(&provider, |state| cache_len(state) == 2),
            "first poll never landed"
        );

        // Discover reflects the version after the first poll beat.
        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(
            instances[0].version.as_ref().map(|v| v.as_str()),
            Some("herdr 0.9.1")
        );

        let contract = provider.probe(&instances[0], Instant::now()).unwrap();
        assert_eq!(contract.revision.get(), 1);
        assert_eq!(contract.instance, provider.instance);
        assert_eq!(
            contract.observer_version.as_ref().map(|v| v.as_str()),
            Some("herdr 0.9.1")
        );

        let snapshot = provider
            .snapshot(&instances[0], None, limits(), Instant::now())
            .unwrap();
        assert!(snapshot.is_complete());
        assert_eq!(snapshot.items().len(), 2);
        // Stable pane ordering: w1:p2 sorts ahead of w3:p9.
        let first = String::from_utf8(snapshot.items()[0].payload.as_slice().to_vec()).unwrap();
        let second = String::from_utf8(snapshot.items()[1].payload.as_slice().to_vec()).unwrap();
        assert!(first.contains(r#""pane_id":"w1:p2""#), "got {first}");
        assert!(second.contains(r#""pane_id":"w3:p9""#), "got {second}");
        assert_eq!(snapshot.items()[0].event_name.as_str(), HERDR_ENTRY_EVENT);

        // Stage 1 has no event stream.
        assert_eq!(
            provider.next_event(&instances[0], Instant::now()),
            ProviderEventOutcome::Idle
        );

        // The argv protocol lock: only the two read-only shapes ever run.
        let log_bytes = fs::read(path.join("calls.log")).unwrap();
        let log = String::from_utf8_lossy(&log_bytes);
        for forbidden in [
            "send-text",
            "focus",
            "prompt",
            "read",
            "wait",
            "--machine",
            "start",
            "resume",
        ] {
            assert!(!log.contains(forbidden), "forbidden argv in log: {log}");
        }
        assert!(log.contains("--version"));
        assert!(log.contains("agent"));
        assert!(log.contains("list"));

        provider.begin_draining();
        assert_eq!(
            provider.next_event(&instances[0], Instant::now()),
            ProviderEventOutcome::Closed
        );
    }

    #[test]
    fn lists_beyond_the_entry_cap_are_served_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        let entries: Vec<String> = (0..65)
            .map(|index| entry(&format!("w0:p{index:03}"), "claude", "idle", index as u64))
            .collect();
        write_agents(path, &agents_json(&entries));
        let mut provider = provider(path);

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(wait_for(&provider, |state| {
            state
                .cache
                .as_ref()
                .is_some_and(|cache| cache.items.len() == 64 && !cache.complete)
        }));

        let snapshot = provider
            .snapshot(&instances[0], None, limits(), Instant::now())
            .unwrap();
        assert!(!snapshot.is_complete());
        assert_eq!(snapshot.items().len(), 64);
        let first = String::from_utf8(snapshot.items()[0].payload.as_slice().to_vec()).unwrap();
        let last = String::from_utf8(snapshot.items()[63].payload.as_slice().to_vec()).unwrap();
        assert!(first.contains(r#""pane_id":"w0:p000""#), "got {first}");
        // The stable pane-sorted prefix keeps w0:p063 and drops w0:p064.
        assert!(last.contains(r#""pane_id":"w0:p063""#), "got {last}");
    }

    #[test]
    fn server_version_change_triggers_two_phase_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 10)]));
        let mut provider = provider(path);

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(wait_for(&provider, |state| cache_len(state) == 1));
        let contract = provider.probe(&instances[0], Instant::now()).unwrap();
        assert_eq!(contract.revision.get(), 1);

        write_version(path, "herdr 0.9.2\n");

        // Reset beat: epoch bumps, cache invalidates, then the next poll
        // rebuilds the cache at the new epoch without further bumps.
        assert!(
            wait_for(&provider, |state| {
                state.epoch_counter == 2 && cache_len(state) == 1
            }),
            "reset never converged"
        );
        let contract = provider.probe(&instances[0], Instant::now()).unwrap();
        assert_eq!(contract.revision.get(), 2);
        assert_eq!(
            contract.observer_version.as_ref().map(|v| v.as_str()),
            Some("herdr 0.9.2")
        );

        // The epoch stays put once the server is stable again.
        thread::sleep(3 * TEST_VERSION_INTERVAL);
        let state = lock_state(&provider.shared.0);
        assert_eq!(state.epoch_counter, 2);
    }

    #[test]
    fn pane_sequence_regression_triggers_two_phase_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 10)]));
        let mut provider = provider(path);

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(wait_for(&provider, |state| cache_len(state) == 1));
        assert_eq!(
            provider
                .probe(&instances[0], Instant::now())
                .unwrap()
                .revision
                .get(),
            1
        );

        // Same pane, lower sequence: the server restarted and its counters
        // reset. The version is unchanged, so only the regression fires.
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 3)]));
        assert!(
            wait_for(&provider, |state| state.epoch_counter == 2
                && cache_len(state) == 1),
            "regression never bumped the epoch"
        );
        assert_eq!(
            provider
                .probe(&instances[0], Instant::now())
                .unwrap()
                .revision
                .get(),
            2
        );
    }

    #[test]
    fn stale_cache_is_never_served() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 1)]));
        // Long cadence so the first poll's cache stays untouched while the
        // test rewinds its capture time.
        let bin = install_fake_herdr(path);
        let mut provider = HerdrSnapshotProvider::with_timings(
            bin.to_str().unwrap().to_owned(),
            "/tmp/herdr-provider-test.sock".to_owned(),
            Duration::from_secs(10),
            Duration::from_secs(10),
        );

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(wait_for(&provider, |state| cache_len(state) == 1));

        // Age the cache past the freshness horizon: it must surface as
        // Unavailable, never as a current snapshot.
        lock_state(&provider.shared.0)
            .cache
            .as_mut()
            .unwrap()
            .captured_at = Timestamp::from_unix_millis(0);
        assert_eq!(
            provider.snapshot(&instances[0], None, limits(), Instant::now()),
            Err(ProviderError::Unavailable)
        );
    }

    #[test]
    fn shape_drift_is_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 1)]));
        let mut provider = provider(path);

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(wait_for(&provider, |state| cache_len(state) == 1));
        assert!(provider.probe(&instances[0], Instant::now()).is_ok());

        // The envelope no longer declares an agent list.
        write_agents(path, r#"{"id":"cli:agent:list","result":{"type":"other"}}"#);
        assert!(
            wait_for(&provider, |state| {
                !state.shape_valid && state.health == ProviderHealth::Degraded
            }),
            "shape drift never degraded the provider"
        );
        assert_eq!(
            provider.probe(&instances[0], Instant::now()),
            Err(ProviderError::InvalidResponse)
        );
    }

    #[test]
    fn failing_polls_degrade_health() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 1)]));
        let mut provider = provider(path);

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        drop(instances);
        assert!(wait_for(&provider, |state| cache_len(state) == 1));

        // Remove the fixture: `cat` fails, the script exits non-zero, and
        // consecutive failures degrade the instance.
        fs::remove_file(path.join("agents.json")).unwrap();
        assert!(
            wait_for(&provider, |state| state.health == ProviderHealth::Degraded),
            "failures never degraded the provider"
        );
        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(instances[0].health, ProviderHealth::Degraded);
    }

    #[test]
    fn missing_binary_marks_the_provider_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 1)]));
        let bin = install_fake_herdr(path);
        let mut provider = HerdrSnapshotProvider::with_timings(
            bin.to_str().unwrap().to_owned(),
            "/tmp/herdr-provider-test.sock".to_owned(),
            TEST_CADENCE,
            TEST_VERSION_INTERVAL,
        );

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        let instance = instances[0].clone();
        assert!(wait_for(&provider, |state| cache_len(state) == 1));

        // The binary disappears: the provider becomes terminally
        // unavailable and stops reporting an instance.
        fs::rename(&bin, path.join("fake-herdr.bak")).unwrap();
        assert!(
            wait_for(&provider, |state| state.health
                == ProviderHealth::Unavailable),
            "missing binary never marked the provider unavailable"
        );
        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(instances.is_empty());
        assert_eq!(
            provider.probe(&instance, Instant::now()),
            Err(ProviderError::Unavailable)
        );
    }

    #[test]
    fn draining_stops_polling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        write_version(path, "herdr 0.9.1\n");
        write_agents(path, &agents_json(&[entry("w1:p2", "claude", "idle", 1)]));
        let mut provider = provider(path);

        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(wait_for(&provider, |state| cache_len(state) == 1));

        provider.begin_draining();
        assert_eq!(
            provider.next_event(&instances[0], Instant::now()),
            ProviderEventOutcome::Closed
        );
        assert_eq!(
            provider.snapshot(&instances[0], None, limits(), Instant::now()),
            Err(ProviderError::Unavailable)
        );
        let instances = provider
            .discover(
                &selector(),
                ProviderDiscoveryLimits { max_instances: 32 },
                Instant::now(),
            )
            .unwrap();
        assert!(instances.is_empty());

        // Polling stops after draining. One in-flight iteration may still
        // write its log line; give the worker one wakeup to exit, then the
        // log must be frozen.
        thread::sleep(10 * TEST_CADENCE);
        let log_len = fs::read(path.join("calls.log")).unwrap().len();
        thread::sleep(10 * TEST_CADENCE);
        assert_eq!(fs::read(path.join("calls.log")).unwrap().len(), log_len);
    }
}
