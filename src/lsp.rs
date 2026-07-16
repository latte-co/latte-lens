//! Bounded LSP framing, resource admission, protocol identifiers, and the
//! isolated disk-snapshot lane.
#![allow(dead_code)]

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{
        DeserializeOwned, DeserializeSeed, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor,
    },
};

use crate::{
    content_safety::{OpenRegular, open_regular},
    lsp_process::{IoThreadDone, IoThreadKind, OwnedProcessTree, ProcessIo, spawn_language_server},
    navigation::{
        DocumentVersion, LanguageFamily, MAX_NAVIGATION_LINES, MAX_NAVIGATION_TEXT_BYTES,
        NavigationDocument, NavigationOperation, NavigationSettings, NavigationSource,
        SourcePosition,
    },
};

pub(crate) const MAX_HEADER_BYTES: usize = 8 * 1024;
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const SESSION_PAYLOAD_BUDGET: usize = 32 * 1024 * 1024;
pub(crate) const GLOBAL_PAYLOAD_BUDGET: usize = 192 * 1024 * 1024;
pub(crate) const MAX_JSON_DEPTH: usize = 128;
pub(crate) const MAX_JSON_NUMBER_BYTES: usize = 128;
pub(crate) const MAX_RETIRED_IDS: usize = 64;
const MAX_RPC_STRING_ID_BYTES: usize = 4 * 1024;
const MAX_LOCATION_RESULTS: usize = 2_000;
const MAX_SYMBOL_RESULTS: usize = 4_096;
const MAX_NORMALIZED_RESULT_BYTES: usize = 1024 * 1024;
const MAX_RESULT_URI_BYTES: usize = 64 * 1024;
const MAX_SYMBOL_STRING_BYTES: usize = 4 * 1024;
const MAX_SYMBOL_TEXT_BYTES: usize = 512 * 1024;
const MAX_SYMBOL_DEPTH: usize = 64;
const SESSION_CONTROL_CAPACITY: usize = 3;
const SESSION_LIFECYCLE_CAPACITY: usize = 3;
const IO_THREAD_COUNT: usize = 3;
const MAX_CONFIGURATION_ITEMS: usize = 64;
const MAX_SERVER_CALL_PARAMS_BYTES: usize = 64 * 1024;
const MAX_CONFIGURATION_STRING_BYTES: usize = 4 * 1024;
const CLEANUP_SHUTDOWN_RESPONSE_WINDOW: Duration = Duration::from_millis(750);
const CLEANUP_TREE_EXIT_WINDOW: Duration = Duration::from_millis(750);
const CLEANUP_TERM_WINDOW: Duration = Duration::from_millis(250);
const CLEANUP_KILL_REAP_WINDOW: Duration = Duration::from_secs(1);
const CLEANUP_IO_WINDOW: Duration = Duration::from_millis(750);
const CLEANUP_JOIN_SETTLE_WINDOW: Duration = Duration::from_millis(250);
const CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug)]
struct PermitPool {
    limit: usize,
    used: Mutex<usize>,
}

impl PermitPool {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            used: Mutex::new(0),
        }
    }

    fn try_acquire(&self, bytes: usize) -> Result<()> {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = used
            .checked_add(bytes)
            .ok_or_else(|| anyhow!("navigation payload budget overflow"))?;
        if next > self.limit {
            bail!("navigation payload budget exhausted");
        }
        *used = next;
        Ok(())
    }

    fn release(&self, bytes: usize) {
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(*used >= bytes);
        *used = used.saturating_sub(bytes);
    }

    #[cfg(test)]
    fn used(&self) -> usize {
        *self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Shared logical payload admission. These permits intentionally do not claim
/// to be an allocator/RSS hard limit.
#[derive(Clone, Debug)]
pub(crate) struct PayloadBudget {
    session: Arc<PermitPool>,
    global: Arc<PermitPool>,
}

impl PayloadBudget {
    pub(crate) fn session(global: &GlobalPayloadBudget) -> Self {
        Self {
            session: Arc::new(PermitPool::new(SESSION_PAYLOAD_BUDGET)),
            global: Arc::clone(&global.pool),
        }
    }

    pub(crate) fn reserve(&self, bytes: usize) -> Result<PayloadPermit> {
        self.global.try_acquire(bytes)?;
        if let Err(error) = self.session.try_acquire(bytes) {
            self.global.release(bytes);
            return Err(error);
        }
        Ok(PayloadPermit {
            bytes,
            session: Arc::clone(&self.session),
            global: Arc::clone(&self.global),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GlobalPayloadBudget {
    pool: Arc<PermitPool>,
}

impl Default for GlobalPayloadBudget {
    fn default() -> Self {
        Self {
            pool: Arc::new(PermitPool::new(GLOBAL_PAYLOAD_BUDGET)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PayloadPermit {
    bytes: usize,
    session: Arc<PermitPool>,
    global: Arc<PermitPool>,
}

impl PayloadPermit {
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for PayloadPermit {
    fn drop(&mut self) {
        self.session.release(self.bytes);
        self.global.release(self.bytes);
    }
}

#[derive(Debug)]
pub(crate) struct ChargedPayload {
    bytes: Vec<u8>,
    _permit: PayloadPermit,
}

impl ChargedPayload {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
}

#[derive(Debug, Default)]
pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
    expected_body: Option<usize>,
    body_permit: Option<PayloadPermit>,
}

impl FrameDecoder {
    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
        budget: &PayloadBudget,
    ) -> Result<Vec<ChargedPayload>> {
        self.buffer
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| anyhow!("LSP read buffer length overflow"))?;
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.expected_body.is_none() {
                let Some(header_end) = find_header_end(&self.buffer) else {
                    if self.buffer.len() > MAX_HEADER_BYTES {
                        bail!("LSP header exceeds 8 KiB");
                    }
                    break;
                };
                if header_end + 4 > MAX_HEADER_BYTES {
                    bail!("LSP header exceeds 8 KiB");
                }
                let body_len = parse_headers(&self.buffer[..header_end])?;
                self.buffer.drain(..header_end + 4);
                self.body_permit = Some(budget.reserve(body_len)?);
                self.expected_body = Some(body_len);
            }
            let body_len = self.expected_body.expect("set above");
            if self.buffer.len() < body_len {
                break;
            }
            let remainder = self.buffer.split_off(body_len);
            let bytes = std::mem::replace(&mut self.buffer, remainder);
            let permit = self
                .body_permit
                .take()
                .expect("body permit is acquired with Content-Length");
            frames.push(ChargedPayload {
                bytes,
                _permit: permit,
            });
            self.expected_body = None;
        }
        Ok(frames)
    }

    pub(crate) fn finish(self) -> Result<()> {
        if self.expected_body.is_some() || !self.buffer.is_empty() {
            bail!("LSP stdout ended in the middle of a frame");
        }
        Ok(())
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_headers(bytes: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(bytes).context("LSP header is not ASCII")?;
    if !text.is_ascii() {
        bail!("LSP header is not ASCII");
    }
    let mut content_length = None;
    let mut content_type = None;
    for line in text.split("\r\n") {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed LSP header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                bail!("duplicate Content-Length");
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                bail!("invalid Content-Length");
            }
            let length: usize = value.parse().context("Content-Length overflow")?;
            if !(1..=MAX_BODY_BYTES).contains(&length) {
                bail!("Content-Length is outside 1..=4 MiB");
            }
            content_length = Some(length);
        } else if name.eq_ignore_ascii_case("content-type")
            && content_type.replace(value.trim()).is_some()
        {
            bail!("duplicate Content-Type");
        }
    }
    if let Some(value) = content_type {
        validate_content_type(value)?;
    }
    content_length.ok_or_else(|| anyhow!("missing Content-Length"))
}

fn validate_content_type(value: &str) -> Result<()> {
    let mut parts = value.split(';');
    let media = parts.next().unwrap_or_default().trim();
    if !media.eq_ignore_ascii_case("application/vscode-jsonrpc") {
        bail!("unsupported LSP Content-Type");
    }
    let mut charset_seen = false;
    for parameter in parts {
        let parameter = parameter.trim();
        let (name, value) = parameter
            .split_once('=')
            .ok_or_else(|| anyhow!("malformed Content-Type parameter"))?;
        if !name.trim().eq_ignore_ascii_case("charset") || charset_seen {
            bail!("unsupported or duplicate Content-Type parameter");
        }
        let value = value.trim();
        if !(value.eq_ignore_ascii_case("utf-8") || value.eq_ignore_ascii_case("utf8")) {
            bail!("LSP charset must be UTF-8");
        }
        charset_seen = true;
    }
    Ok(())
}

pub(crate) fn encode_frame(body: &[u8], budget: &PayloadBudget) -> Result<ChargedPayload> {
    if body.is_empty() || body.len() > MAX_BODY_BYTES {
        bail!("outbound LSP body is outside 1..=4 MiB");
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let total = header
        .len()
        .checked_add(body.len())
        .ok_or_else(|| anyhow!("outbound frame length overflow"))?;
    let permit = budget.reserve(total)?;
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(body);
    Ok(ChargedPayload {
        bytes,
        _permit: permit,
    })
}

/// Resource preflight that is string/escape aware. Grammar validation remains
/// serde_json's responsibility.
pub(crate) fn json_preflight(bytes: &[u8]) -> Result<()> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("JSON nesting overflow"))?;
                if depth > MAX_JSON_DEPTH {
                    bail!("JSON nesting exceeds {MAX_JSON_DEPTH}");
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'-' | b'0'..=b'9' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && matches!(bytes[index], b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E')
                {
                    index += 1;
                }
                if index - start > MAX_JSON_NUMBER_BYTES {
                    bail!("JSON number token exceeds {MAX_JSON_NUMBER_BYTES} bytes");
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    if in_string {
        // serde reports exact grammar; this keeps the preflight non-allocating.
        return Ok(());
    }
    Ok(())
}

/// Deserialize only after reserving a body-sized serde scratch charge. The
/// reservation lives through both Deserialize and `Deserializer::end()`.
pub(crate) fn parse_bounded<T: DeserializeOwned>(
    body: &ChargedPayload,
    budget: &PayloadBudget,
) -> Result<T> {
    json_preflight(body.as_slice())?;
    let _scratch = budget.reserve(body.len())?;
    let mut deserializer = serde_json::Deserializer::from_slice(body.as_slice());
    let value = T::deserialize(&mut deserializer).context("invalid JSON payload")?;
    deserializer.end().context("trailing JSON payload")?;
    Ok(value)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RpcId {
    Signed(i32),
    String(String),
}

impl Serialize for RpcId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Signed(value) => serializer.serialize_i32(*value),
            Self::String(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for RpcId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = RpcId;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a signed 32-bit integer or string JSON-RPC id")
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                i32::try_from(value)
                    .map(RpcId::Signed)
                    .map_err(|_| E::custom("JSON-RPC id is outside signed i32"))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                i32::try_from(value)
                    .map(RpcId::Signed)
                    .map_err(|_| E::custom("JSON-RPC id is outside signed i32"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() > MAX_RPC_STRING_ID_BYTES {
                    return Err(E::custom("JSON-RPC string id exceeds 4 KiB"));
                }
                Ok(RpcId::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.len() > MAX_RPC_STRING_ID_BYTES {
                    return Err(E::custom("JSON-RPC string id exceeds 4 KiB"));
                }
                Ok(RpcId::String(value))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SessionEpoch(pub u64);

#[derive(Debug)]
pub(crate) struct RequestIdAllocator {
    next: Option<i32>,
}

impl Default for RequestIdAllocator {
    fn default() -> Self {
        Self { next: Some(0) }
    }
}

impl RequestIdAllocator {
    pub(crate) fn allocate(&mut self) -> Result<RpcId> {
        let current = self.next.ok_or_else(|| anyhow!("RequestIdExhausted"))?;
        self.next = current.checked_add(1);
        Ok(RpcId::Signed(current))
    }
}

#[derive(Debug, Default)]
pub(crate) struct RetiredIds {
    order: VecDeque<RpcId>,
    set: HashSet<RpcId>,
}

impl RetiredIds {
    pub(crate) fn insert(&mut self, id: RpcId) -> Result<()> {
        if self.set.contains(&id) {
            return Ok(());
        }
        if self.order.len() == MAX_RETIRED_IDS {
            bail!("retired JSON-RPC id capacity requires session restart");
        }
        self.set.insert(id.clone());
        self.order.push_back(id);
        Ok(())
    }

    pub(crate) fn take(&mut self, id: &RpcId) -> bool {
        if !self.set.remove(id) {
            return false;
        }
        self.order.retain(|candidate| candidate != id);
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextChangeCapability {
    None,
    Full,
    Incremental,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextSyncCapability {
    pub open_close: bool,
    pub change: TextChangeCapability,
}

impl TextSyncCapability {
    pub(crate) const fn requires_disk_revalidation(self) -> bool {
        !self.open_close || matches!(self.change, TextChangeCapability::None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NavigationCapabilities {
    pub definition: bool,
    pub references: bool,
    pub implementations: bool,
    pub document_symbols: bool,
    pub text_document_sync: TextSyncCapability,
}

#[derive(Clone, Debug)]
pub(crate) enum ServerState {
    Disabled {
        reason: String,
    },
    Unavailable {
        reason: String,
    },
    Starting {
        since: Instant,
    },
    Ready {
        capabilities: NavigationCapabilities,
    },
    Backoff {
        attempt: u8,
        retry_at: Instant,
        error: String,
    },
    Failed {
        error: String,
    },
    StoppingShutdown,
    StoppingForced,
}

#[derive(Clone, Debug)]
pub(crate) struct DiskSnapshotJob {
    pub generation: u64,
    pub workspace_root: PathBuf,
    pub absolute_path: PathBuf,
    pub disk_raw_len: u64,
    pub expected_text: Arc<str>,
    pub deadline: Instant,
}

impl DiskSnapshotJob {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bounded(
        generation: u64,
        workspace_root: PathBuf,
        absolute_path: PathBuf,
        disk_raw_len: u64,
        expected_text: Arc<str>,
        now: Instant,
        request_deadline: Instant,
    ) -> Self {
        Self {
            generation,
            workspace_root,
            absolute_path,
            disk_raw_len,
            expected_text,
            deadline: request_deadline.min(now + Duration::from_millis(500)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DiskSnapshotResult {
    Current { generation: u64 },
    Stale { generation: u64 },
    Failed { generation: u64, message: String },
}

type DiskReadFn = dyn Fn(&DiskSnapshotJob, &AtomicBool) -> Result<bool> + Send + Sync + 'static;

/// One worker, at most one active plus one queued job. A blocked OS read never
/// causes a replacement thread to be spawned.
pub(crate) struct DiskRevalidationLane {
    sender: Option<SyncSender<DiskSnapshotJob>>,
    results: Receiver<DiskSnapshotResult>,
    cancel: Arc<AtomicBool>,
    wedged: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,
    active_deadline: Arc<Mutex<Option<Instant>>>,
    done: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl DiskRevalidationLane {
    pub(crate) fn start() -> Result<Self> {
        Self::start_with_reader(Arc::new(read_and_compare_snapshot))
    }

    fn start_with_reader(reader: Arc<DiskReadFn>) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<DiskSnapshotJob>(1);
        let (result_sender, results) = mpsc::sync_channel(2);
        let (done_sender, done) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let wedged = Arc::new(AtomicBool::new(false));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let active_deadline = Arc::new(Mutex::new(None));
        let worker_cancel = Arc::clone(&cancel);
        let worker_wedged = Arc::clone(&wedged);
        let worker_count = Arc::clone(&in_flight);
        let worker_deadline = Arc::clone(&active_deadline);
        let worker = thread::Builder::new()
            .name("latte-lens-lsp-disk".to_owned())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    if worker_cancel.load(Ordering::Acquire) {
                        worker_count.fetch_sub(1, Ordering::AcqRel);
                        break;
                    }
                    *worker_deadline
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(job.deadline);
                    let generation = job.generation;
                    let result = reader(&job, &worker_cancel);
                    *worker_deadline
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                    worker_count.fetch_sub(1, Ordering::AcqRel);
                    if worker_cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let completion = match result {
                        Ok(true) => DiskSnapshotResult::Current { generation },
                        Ok(false) => DiskSnapshotResult::Stale { generation },
                        Err(error) => DiskSnapshotResult::Failed {
                            generation,
                            message: format!("{error:#}"),
                        },
                    };
                    if result_sender.send(completion).is_err() {
                        break;
                    }
                    if worker_wedged.load(Ordering::Acquire) {
                        break;
                    }
                }
                let _ = done_sender.send(());
            })?;
        Ok(Self {
            sender: Some(sender),
            results,
            cancel,
            wedged,
            in_flight,
            active_deadline,
            done,
            worker: Some(worker),
        })
    }

    pub(crate) fn try_submit(&self, job: DiskSnapshotJob) -> Result<()> {
        if self.wedged.load(Ordering::Acquire) {
            bail!("File revalidation worker is unavailable.");
        }
        let previous = self.in_flight.fetch_add(1, Ordering::AcqRel);
        if previous >= 2 {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            bail!("File revalidation queue is full.");
        }
        match self.sender.as_ref().expect("lane is live").try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                bail!("File revalidation queue is full.")
            }
            Err(TrySendError::Disconnected(_)) => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                bail!("File revalidation worker is unavailable.")
            }
        }
    }

    pub(crate) fn try_result(&self) -> Option<DiskSnapshotResult> {
        self.results.try_recv().ok()
    }

    pub(crate) fn mark_wedged_if_overdue(&self, now: Instant) -> bool {
        let overdue = self
            .active_deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|deadline| now >= deadline);
        if overdue {
            self.wedged.store(true, Ordering::Release);
            self.cancel.store(true, Ordering::Release);
        }
        overdue
    }
}

impl Drop for DiskRevalidationLane {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.sender.take();
        match self.done.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(worker) = self.worker.take() {
                    let _ = worker.join();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Detach: std has no safe cross-platform cancellation for an
                // arbitrary blocking file read. The worker owns no App/session.
                self.worker.take();
            }
        }
    }
}

fn read_and_compare_snapshot(job: &DiskSnapshotJob, cancel: &AtomicBool) -> Result<bool> {
    if cancel.load(Ordering::Acquire) || Instant::now() >= job.deadline {
        return Ok(false);
    }
    let mut file = match open_regular(Some(&job.workspace_root), &job.absolute_path)? {
        OpenRegular::Opened(file) => file,
        OpenRegular::Declined(_) => return Ok(false),
    };
    if file.len() != job.disk_raw_len {
        return Ok(false);
    }
    let cap = MAX_NAVIGATION_TEXT_BYTES.saturating_add(1);
    let expected_cap = usize::try_from(job.disk_raw_len)
        .unwrap_or(usize::MAX)
        .saturating_add(1)
        .min(cap);
    let mut bytes = Vec::with_capacity(expected_cap);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= job.deadline {
            return Ok(false);
        }
        let remaining = expected_cap.saturating_sub(bytes.len());
        if remaining == 0 {
            return Ok(false);
        }
        let chunk_len = chunk.len();
        let read = file
            .read(&mut chunk[..remaining.min(chunk_len)])
            .context("cannot revalidate navigation source")?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    if bytes.len() as u64 != job.disk_raw_len || bytes.len() > MAX_NAVIGATION_TEXT_BYTES {
        return Ok(false);
    }
    if bytes.contains(&0) {
        return Ok(false);
    }
    let text = std::str::from_utf8(&bytes).context("navigation source is not strict UTF-8")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().take(MAX_NAVIGATION_LINES + 1).collect();
    if lines.len() > MAX_NAVIGATION_LINES {
        return Ok(false);
    }
    Ok(lines.join("\n").as_bytes() == job.expected_text.as_bytes())
}

const NAVIGATION_COMMAND_CAPACITY: usize = 32;
const NAVIGATION_COMPLETION_CAPACITY: usize = 16;
const SESSION_WRITER_CAPACITY: usize = 16;
const SESSION_FRAME_CAPACITY: usize = 2;
const NAVIGATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const STABLE_READY_RESET: Duration = Duration::from_secs(60);

fn enqueue_inbound_frame(
    frames: &SyncSender<ChargedPayload>,
    controls: &SyncSender<SessionControlEvent>,
    frame: ChargedPayload,
) -> bool {
    match frames.try_send(frame) {
        Ok(()) => true,
        Err(error) => {
            let message = match error {
                TrySendError::Full(_) => "language server frame channel is full",
                TrySendError::Disconnected(_) => "language server frame channel disconnected",
            };
            let _ = controls.send(SessionControlEvent::ReaderFailed(message.to_owned()));
            false
        }
    }
}

fn record_io_done(
    epoch: SessionEpoch,
    completed: &mut HashSet<IoThreadKind>,
    done: IoThreadDone,
) -> Result<()> {
    if done.session_epoch != epoch.0 {
        bail!("language server I/O epoch mismatch");
    }
    if !completed.insert(done.kind) {
        bail!("language server I/O thread completed twice");
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct NavigationRuntimeRequest {
    pub generation: u64,
    pub operation: NavigationOperation,
    pub origin: SourcePosition,
    pub source: Arc<NavigationSource>,
    pub version: DocumentVersion,
}

#[derive(Debug)]
pub(crate) struct ProtocolLocation {
    pub uri: lsp_types::Uri,
    pub range: lsp_types::Range,
    _permit: Option<Arc<PayloadPermit>>,
}

impl PartialEq for ProtocolLocation {
    fn eq(&self, other: &Self) -> bool {
        self.uri == other.uri && self.range == other.range
    }
}

impl Eq for ProtocolLocation {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolSymbolKind {
    Function,
    Method,
    Type,
    Module,
    Other,
}

#[derive(Debug)]
pub(crate) struct ProtocolDocumentSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub container: Option<String>,
    pub kind: ProtocolSymbolKind,
    pub range: crate::navigation::SourceRange,
    pub selection_range: crate::navigation::SourceRange,
    pub parent: Option<usize>,
    _permit: Arc<PayloadPermit>,
    _uri_permit: Option<Arc<PayloadPermit>>,
}

impl PartialEq for ProtocolDocumentSymbol {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.detail == other.detail
            && self.container == other.container
            && self.kind == other.kind
            && self.range == other.range
            && self.selection_range == other.selection_range
            && self.parent == other.parent
    }
}

impl Eq for ProtocolDocumentSymbol {}

#[cfg(test)]
impl ProtocolDocumentSymbol {
    pub(crate) fn for_app_test(
        name: impl Into<String>,
        range: crate::navigation::SourceRange,
        selection_range: crate::navigation::SourceRange,
        parent: Option<usize>,
        detail: Option<String>,
        container: Option<String>,
    ) -> Self {
        let name = name.into();
        let budget = PayloadBudget::session(&GlobalPayloadBudget::default());
        let permit = budget
            .reserve(name.len().saturating_add(1))
            .expect("small App test symbol fits the payload budget");
        Self {
            name,
            detail,
            container,
            kind: ProtocolSymbolKind::Other,
            range,
            selection_range,
            parent,
            _permit: Arc::new(permit),
            _uri_permit: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum NavigationProtocolResult {
    Locations(Vec<ProtocolLocation>),
    Unavailable(String),
    Failed(String),
    Cancelled,
}

#[derive(Debug)]
pub(crate) struct NavigationRuntimeCompletion {
    pub generation: u64,
    pub operation: NavigationOperation,
    pub source_identity: crate::runtime::ContentIdentity,
    pub source_version: DocumentVersion,
    pub result: NavigationProtocolResult,
}

#[derive(Debug)]
pub(crate) struct NavigationDocumentSymbolCompletion {
    pub generation: u64,
    pub source_identity: crate::runtime::ContentIdentity,
    pub source_version: DocumentVersion,
    pub symbols: Vec<ProtocolDocumentSymbol>,
}

enum NavigationCommand {
    Request(NavigationRuntimeRequest),
    Cancel(u64),
    Shutdown,
}

pub(crate) struct NavigationRuntime {
    commands: SyncSender<NavigationCommand>,
    completions: Arc<Mutex<VecDeque<NavigationRuntimeCompletion>>>,
    completion_permits: Arc<Mutex<CompletionPermitState>>,
    symbol_completions: Arc<Mutex<VecDeque<NavigationDocumentSymbolCompletion>>>,
    shutdown: Arc<AtomicBool>,
    cleanup_stats: Arc<Mutex<NavigationCleanupStats>>,
    manager: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct NavigationCleanupSnapshot {
    pub sessions_cleaned: usize,
    pub clean_exits: usize,
    pub forced_tree_cleanups: usize,
    pub direct_children_reaped: usize,
    pub io_threads_joined: usize,
    pub process_owners_dropped: usize,
    pub quarantined_process_owners: usize,
}

#[derive(Debug, Default)]
pub(crate) struct NavigationCleanupStats {
    pub(crate) snapshot: NavigationCleanupSnapshot,
}

#[derive(Debug, Default)]
struct CompletionPermitState {
    queued: VecDeque<Option<Arc<PayloadPermit>>>,
    leased: Vec<Arc<PayloadPermit>>,
}

impl NavigationRuntime {
    pub(crate) fn start(workspace_root: PathBuf, settings: NavigationSettings) -> Result<Self> {
        let (commands, receiver) = mpsc::sync_channel(NAVIGATION_COMMAND_CAPACITY);
        let completions = Arc::new(Mutex::new(VecDeque::new()));
        let manager_completions = Arc::clone(&completions);
        let completion_permits = Arc::new(Mutex::new(CompletionPermitState::default()));
        let manager_completion_permits = Arc::clone(&completion_permits);
        let symbol_completions = Arc::new(Mutex::new(VecDeque::new()));
        let manager_symbol_completions = Arc::clone(&symbol_completions);
        let shutdown = Arc::new(AtomicBool::new(false));
        let cleanup_stats = Arc::new(Mutex::new(NavigationCleanupStats::default()));
        let manager_cleanup_stats = Arc::clone(&cleanup_stats);
        let manager_shutdown = Arc::clone(&shutdown);
        let manager = thread::Builder::new()
            .name("latte-lens-navigation".to_owned())
            .spawn(move || {
                NavigationManager::new(
                    workspace_root,
                    settings,
                    manager_completions,
                    manager_completion_permits,
                    manager_symbol_completions,
                    manager_cleanup_stats,
                )
                .run(receiver, &manager_shutdown);
            })?;
        Ok(Self {
            commands,
            completions,
            completion_permits,
            symbol_completions,
            shutdown,
            cleanup_stats,
            manager: Some(manager),
        })
    }

    pub(crate) fn request(&self, request: NavigationRuntimeRequest) -> Result<()> {
        self.commands
            .try_send(NavigationCommand::Request(request))
            .map_err(|error| anyhow!("navigation command queue is full: {error}"))
    }

    pub(crate) fn cancel(&self, generation: u64) {
        let _ = self
            .commands
            .try_send(NavigationCommand::Cancel(generation));
    }

    pub(crate) fn take_completions(&self) -> Vec<NavigationRuntimeCompletion> {
        let mut completions = self
            .completions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut permits = self
            .completion_permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        permits.leased.clear();
        let output: Vec<_> = completions.drain(..).collect();
        for _ in 0..output.len() {
            if let Some(permit) = permits.queued.pop_front().flatten() {
                permits.leased.push(permit);
            }
        }
        output
    }

    pub(crate) fn take_symbol_completions(&self) -> Vec<NavigationDocumentSymbolCompletion> {
        self.symbol_completions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }

    pub(crate) fn cleanup_probe(&self) -> Arc<Mutex<NavigationCleanupStats>> {
        Arc::clone(&self.cleanup_stats)
    }
}

impl Drop for NavigationRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.commands.try_send(NavigationCommand::Shutdown);
        if let Some(manager) = self.manager.take() {
            let _ = manager.join();
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionKey {
    server_root: PathBuf,
    family: LanguageFamily,
}

struct PendingUserRequest {
    request: NavigationRuntimeRequest,
    document: Arc<NavigationDocument>,
}

enum PendingRequest {
    Initialize {
        deadline: Instant,
    },
    Navigation {
        request: NavigationRuntimeRequest,
        document: Arc<NavigationDocument>,
        deadline: Instant,
    },
}

enum SessionControlEvent {
    ReaderFailed(String),
    ReaderEof,
    WriterFailed(String),
    StderrFailed(String),
}

struct LspSession {
    key: SessionKey,
    epoch: SessionEpoch,
    state: ServerState,
    budget: PayloadBudget,
    allocator: RequestIdAllocator,
    retired: RetiredIds,
    pending: HashMap<RpcId, PendingRequest>,
    deferred: Option<PendingUserRequest>,
    opened: Option<OpenedDocument>,
    writer: Option<SyncSender<ChargedPayload>>,
    frames: Receiver<ChargedPayload>,
    controls: Receiver<SessionControlEvent>,
    lifecycle: Receiver<IoThreadDone>,
    io_done: HashSet<IoThreadKind>,
    io_joined: HashSet<IoThreadKind>,
    tree: Option<OwnedProcessTree>,
    io_threads: Vec<(IoThreadKind, JoinHandle<()>)>,
    cleanup_stats: Arc<Mutex<NavigationCleanupStats>>,
    ready_since: Option<Instant>,
}

struct SessionBackoff {
    attempt: u8,
    retry_at: Instant,
    error: String,
}

struct FailedSession {
    session: LspSession,
    error: String,
}

fn backoff_delay(attempt: u8) -> Duration {
    Duration::from_secs(match attempt {
        1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        _ => 30,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupOutcome {
    Complete,
    Quarantined,
}

#[derive(Debug, Default)]
struct CleanupProgress {
    shutdown_id: Option<RpcId>,
    shutdown_response_finished: bool,
    synthetic_session: bool,
    orderly: bool,
    clean_exit: bool,
    forced_cleanup: bool,
    tree_finished: bool,
    direct_child_reaped: bool,
    cleanup_error: Option<String>,
}

struct OpenedDocument {
    uri: lsp_types::Uri,
    version: DocumentVersion,
}

struct NavigationManager {
    workspace_root: PathBuf,
    settings: NavigationSettings,
    global_budget: GlobalPayloadBudget,
    sessions: HashMap<SessionKey, LspSession>,
    failed_sessions: HashMap<SessionKey, FailedSession>,
    backoffs: HashMap<SessionKey, SessionBackoff>,
    permanent_failures: HashMap<SessionKey, String>,
    quarantined: HashMap<SessionKey, LspSession>,
    quarantined_spawns: HashMap<SessionKey, OwnedProcessTree>,
    completions: Arc<Mutex<VecDeque<NavigationRuntimeCompletion>>>,
    completion_permits: Arc<Mutex<CompletionPermitState>>,
    symbol_completions: Arc<Mutex<VecDeque<NavigationDocumentSymbolCompletion>>>,
    next_epoch: u64,
    disk_lane: Option<DiskRevalidationLane>,
    disk_waiting: HashMap<u64, (SessionKey, PendingUserRequest)>,
    cleanup_stats: Arc<Mutex<NavigationCleanupStats>>,
}

impl NavigationManager {
    fn new(
        workspace_root: PathBuf,
        settings: NavigationSettings,
        completions: Arc<Mutex<VecDeque<NavigationRuntimeCompletion>>>,
        completion_permits: Arc<Mutex<CompletionPermitState>>,
        symbol_completions: Arc<Mutex<VecDeque<NavigationDocumentSymbolCompletion>>>,
        cleanup_stats: Arc<Mutex<NavigationCleanupStats>>,
    ) -> Self {
        Self {
            workspace_root,
            settings,
            global_budget: GlobalPayloadBudget::default(),
            sessions: HashMap::new(),
            failed_sessions: HashMap::new(),
            backoffs: HashMap::new(),
            permanent_failures: HashMap::new(),
            quarantined: HashMap::new(),
            quarantined_spawns: HashMap::new(),
            completions,
            completion_permits,
            symbol_completions,
            next_epoch: 1,
            disk_lane: DiskRevalidationLane::start().ok(),
            disk_waiting: HashMap::new(),
            cleanup_stats,
        }
    }

    fn run(&mut self, commands: Receiver<NavigationCommand>, runtime_shutdown: &AtomicBool) {
        let mut shutdown = false;
        while !shutdown && !runtime_shutdown.load(Ordering::Acquire) {
            match commands.recv_timeout(Duration::from_millis(10)) {
                Ok(NavigationCommand::Request(request)) => self.handle_request(request),
                Ok(NavigationCommand::Cancel(generation)) => self.cancel_generation(generation),
                Ok(NavigationCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    shutdown = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            while let Ok(command) = commands.try_recv() {
                match command {
                    NavigationCommand::Request(request) => self.handle_request(request),
                    NavigationCommand::Cancel(generation) => self.cancel_generation(generation),
                    NavigationCommand::Shutdown => shutdown = true,
                }
            }
            if runtime_shutdown.load(Ordering::Acquire) {
                shutdown = true;
            }
            self.poll_disk();
            self.poll_sessions();
            self.expire_requests();
            self.reset_stable_backoffs_at(Instant::now());
            if !shutdown && !runtime_shutdown.load(Ordering::Acquire) {
                self.cleanup_failed_sessions();
            }
        }
        self.cleanup_shutdown_sessions();
        if self.has_quarantined_owners() {
            // A thread stuck in an uninterruptible platform read still owns a
            // pipe. Dropping its process-tree owner would make cleanup appear
            // complete. Preserve every unfinished owner/handle bundle.
            let _ = Box::leak(Box::new(std::mem::take(&mut self.quarantined)));
            let _ = Box::leak(Box::new(std::mem::take(&mut self.quarantined_spawns)));
        }
    }

    fn handle_request(&mut self, request: NavigationRuntimeRequest) {
        let Some(family) = request.source.language.family else {
            self.complete_request(
                &request,
                NavigationProtocolResult::Unavailable(
                    "No semantic language server for this file type.".to_owned(),
                ),
            );
            return;
        };
        let Some(server) = self.settings.server(family).cloned() else {
            self.complete_request(
                &request,
                NavigationProtocolResult::Unavailable(format!(
                    "No configured {} language server.",
                    family.display_name()
                )),
            );
            return;
        };
        let key = SessionKey {
            server_root: request.source.server_root.clone(),
            family,
        };
        if let Some(error) = self.permanent_failures.get(&key) {
            self.complete_request(
                &request,
                NavigationProtocolResult::Failed(format!(
                    "Language server is disabled after repeated failures: {error}"
                )),
            );
            return;
        }
        if self.has_quarantined_owners() {
            self.complete_request(
                &request,
                NavigationProtocolResult::Failed(
                    "Language server navigation is unavailable because a process owner is quarantined."
                        .to_owned(),
                ),
            );
            return;
        }
        if let Some(failure) = self.failed_sessions.get(&key) {
            self.complete_request(
                &request,
                NavigationProtocolResult::Failed(format!(
                    "Language server is restarting after failure: {}",
                    clean_protocol_message(&failure.error)
                )),
            );
            return;
        }
        if let Some(backoff) = self.backoffs.get(&key)
            && Instant::now() < backoff.retry_at
        {
            self.complete_request(
                &request,
                NavigationProtocolResult::Failed(format!(
                    "Language server is restarting after failure: {}",
                    backoff.error
                )),
            );
            return;
        }
        if !self.sessions.contains_key(&key) {
            match self.spawn_session(key.clone(), &server) {
                Ok(session) => {
                    self.sessions.insert(key.clone(), session);
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    #[cfg(windows)]
                    let retained_tree = {
                        let mut error = error;
                        error
                            .downcast_mut::<crate::lsp_process::RetainedSpawnFailure>()
                            .and_then(crate::lsp_process::RetainedSpawnFailure::take_tree)
                    };
                    #[cfg(not(windows))]
                    let retained_tree = None;
                    if let Some(tree) = retained_tree {
                        self.permanent_failures.insert(key.clone(), message.clone());
                        self.retain_quarantined_spawn(key.clone(), tree);
                    } else {
                        self.record_failure_at(&key, &message, Instant::now());
                    }
                    self.complete_request(&request, NavigationProtocolResult::Failed(message));
                    return;
                }
            }
        }
        let budget = self
            .sessions
            .get(&key)
            .expect("session inserted")
            .budget
            .clone();
        let document =
            match NavigationDocument::from_source(&request.source, request.version, &budget) {
                Ok(document) => Arc::new(document),
                Err(error) => {
                    self.complete_request(
                        &request,
                        NavigationProtocolResult::Failed(format!("{error:#}")),
                    );
                    return;
                }
            };
        let pending = PendingUserRequest { request, document };
        let ready = matches!(
            self.sessions.get(&key).map(|session| &session.state),
            Some(ServerState::Ready { .. })
        );
        if ready {
            self.dispatch_or_revalidate(key, pending);
        } else {
            let replaced = self
                .sessions
                .get_mut(&key)
                .and_then(|session| session.deferred.replace(pending));
            if let Some(replaced) = replaced {
                self.complete_request(&replaced.request, NavigationProtocolResult::Cancelled);
            }
        }
    }

    fn spawn_session(
        &mut self,
        key: SessionKey,
        server: &crate::navigation::TrustedServer,
    ) -> Result<LspSession> {
        let spawned = spawn_language_server(server, &self.workspace_root, &key.server_root)?;
        let budget = PayloadBudget::session(&self.global_budget);
        let epoch = SessionEpoch(self.next_epoch);
        self.next_epoch = self.next_epoch.saturating_add(1);
        let (writer_sender, writer_receiver) =
            mpsc::sync_channel::<ChargedPayload>(SESSION_WRITER_CAPACITY);
        let (frame_sender, frames) = mpsc::sync_channel(SESSION_FRAME_CAPACITY);
        let (control_sender, controls) = mpsc::sync_channel(SESSION_CONTROL_CAPACITY);
        let (lifecycle_sender, lifecycle) = mpsc::sync_channel(SESSION_LIFECYCLE_CAPACITY);
        let ProcessIo {
            mut stdin,
            mut stdout,
            mut stderr,
        } = spawned.io;
        let writer_controls = control_sender.clone();
        let writer_lifecycle = lifecycle_sender.clone();
        let writer_epoch = epoch.0;
        let writer = thread::Builder::new()
            .name("latte-lens-lsp-writer".to_owned())
            .spawn(move || {
                while let Ok(frame) = writer_receiver.recv() {
                    if let Err(error) = stdin
                        .write_all(frame.as_slice())
                        .and_then(|_| stdin.flush())
                    {
                        let _ = writer_controls
                            .send(SessionControlEvent::WriterFailed(error.to_string()));
                        break;
                    }
                }
                drop(stdin);
                let _ = writer_lifecycle.send(IoThreadDone {
                    kind: IoThreadKind::Stdin,
                    session_epoch: writer_epoch,
                });
            })?;
        let reader_budget = budget.clone();
        let reader_controls = control_sender.clone();
        let reader_lifecycle = lifecycle_sender.clone();
        let reader_epoch = epoch.0;
        let reader = thread::Builder::new()
            .name("latte-lens-lsp-reader".to_owned())
            .spawn(move || {
                let mut decoder = FrameDecoder::default();
                let mut chunk = [0u8; 64 * 1024];
                'read: loop {
                    match stdout.read(&mut chunk) {
                        Ok(0) => {
                            let event = decoder.finish().map_or_else(
                                |error| SessionControlEvent::ReaderFailed(format!("{error:#}")),
                                |_| SessionControlEvent::ReaderEof,
                            );
                            let _ = reader_controls.send(event);
                            break;
                        }
                        Ok(read) => match decoder.push(&chunk[..read], &reader_budget) {
                            Ok(frames) => {
                                for frame in frames {
                                    if !enqueue_inbound_frame(
                                        &frame_sender,
                                        &reader_controls,
                                        frame,
                                    ) {
                                        break 'read;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = reader_controls
                                    .send(SessionControlEvent::ReaderFailed(format!("{error:#}")));
                                break;
                            }
                        },
                        Err(error) => {
                            let _ = reader_controls
                                .send(SessionControlEvent::ReaderFailed(error.to_string()));
                            break;
                        }
                    }
                }
                drop(stdout);
                let _ = reader_lifecycle.send(IoThreadDone {
                    kind: IoThreadKind::Stdout,
                    session_epoch: reader_epoch,
                });
            })?;
        let stderr_controls = control_sender;
        let stderr_lifecycle = lifecycle_sender;
        let stderr_epoch = epoch.0;
        let stderr_thread = thread::Builder::new()
            .name("latte-lens-lsp-stderr".to_owned())
            .spawn(move || {
                let mut sink = [0u8; 8 * 1024];
                loop {
                    match stderr.read(&mut sink) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error) => {
                            let _ = stderr_controls
                                .send(SessionControlEvent::StderrFailed(error.to_string()));
                            break;
                        }
                    }
                }
                drop(stderr);
                let _ = stderr_lifecycle.send(IoThreadDone {
                    kind: IoThreadKind::Stderr,
                    session_epoch: stderr_epoch,
                });
            })?;
        let mut session = LspSession {
            key,
            epoch,
            state: ServerState::Starting {
                since: Instant::now(),
            },
            budget,
            allocator: RequestIdAllocator::default(),
            retired: RetiredIds::default(),
            pending: HashMap::new(),
            deferred: None,
            opened: None,
            writer: Some(writer_sender),
            frames,
            controls,
            lifecycle,
            io_done: HashSet::new(),
            io_joined: HashSet::new(),
            tree: Some(spawned.tree),
            io_threads: vec![
                (IoThreadKind::Stdin, writer),
                (IoThreadKind::Stdout, reader),
                (IoThreadKind::Stderr, stderr_thread),
            ],
            cleanup_stats: Arc::clone(&self.cleanup_stats),
            ready_since: None,
        };
        let id = session.allocator.allocate()?;
        let root_uri = crate::navigation::path_to_lsp_uri(&session.key.server_root)?;
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": { "name": "latte-lens" },
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": session.key.server_root.file_name().and_then(|name| name.to_str()).unwrap_or("workspace") }],
                "capabilities": { "general": { "positionEncodings": ["utf-16"] } }
            }
        }))?;
        session.send_body(&body)?;
        session.pending.insert(
            id,
            PendingRequest::Initialize {
                deadline: Instant::now() + INITIALIZE_TIMEOUT,
            },
        );
        Ok(session)
    }

    fn dispatch_or_revalidate(&mut self, key: SessionKey, pending: PendingUserRequest) {
        let sync = match self.sessions.get(&key).map(|session| &session.state) {
            Some(ServerState::Ready { capabilities }) => capabilities.text_document_sync,
            _ => return,
        };
        if sync.requires_disk_revalidation() {
            let Some(lane) = self.disk_lane.as_ref() else {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(
                        "File revalidation worker is unavailable.".to_owned(),
                    ),
                );
                return;
            };
            let now = Instant::now();
            let job = DiskSnapshotJob::bounded(
                pending.request.generation,
                self.workspace_root.clone(),
                pending.document.absolute_path.clone(),
                pending.document.disk_raw_len,
                Arc::clone(&pending.document.text),
                now,
                now + NAVIGATION_REQUEST_TIMEOUT,
            );
            if let Err(error) = lane.try_submit(job) {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(format!("{error:#}")),
                );
                return;
            }
            self.disk_waiting
                .insert(pending.request.generation, (key, pending));
        } else {
            self.dispatch_navigation(&key, pending);
        }
    }

    fn dispatch_navigation(&mut self, key: &SessionKey, pending: PendingUserRequest) {
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        if !session.supports(pending.request.operation) {
            self.complete_request(
                &pending.request,
                NavigationProtocolResult::Unavailable(format!(
                    "Configured language server does not provide {:?}.",
                    pending.request.operation
                )),
            );
            return;
        }
        if let Err(error) = session.sync_document(&pending.document) {
            self.complete_request(
                &pending.request,
                NavigationProtocolResult::Failed(format!("{error:#}")),
            );
            self.fail_session(
                key,
                format!("language server synchronization failed: {error:#}"),
            );
            return;
        }
        let position = match pending.document.line_index.to_utf16(pending.request.origin) {
            Ok(position) => position,
            Err(error) => {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(format!("{error:#}")),
                );
                return;
            }
        };
        let uri = match crate::navigation::path_to_lsp_uri(&pending.document.absolute_path) {
            Ok(uri) => uri,
            Err(error) => {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(format!("{error:#}")),
                );
                return;
            }
        };
        let method = match pending.request.operation {
            NavigationOperation::Definition => "textDocument/definition",
            NavigationOperation::References => "textDocument/references",
            NavigationOperation::Implementations => "textDocument/implementation",
            NavigationOperation::DocumentSymbols => "textDocument/documentSymbol",
        };
        let id = match session.allocator.allocate() {
            Ok(id) => id,
            Err(error) => {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(format!("{error:#}")),
                );
                self.fail_session(key, format!("{error:#}"));
                return;
            }
        };
        if session.pending.len() >= MAX_RETIRED_IDS {
            self.complete_request(
                &pending.request,
                NavigationProtocolResult::Failed(
                    "language server pending request capacity requires restart".to_owned(),
                ),
            );
            self.fail_session(
                key,
                "language server pending request capacity requires restart".to_owned(),
            );
            return;
        }
        let mut params = if pending.request.operation == NavigationOperation::DocumentSymbols {
            serde_json::json!({"textDocument": { "uri": uri }})
        } else {
            serde_json::json!({
                "textDocument": { "uri": uri },
                "position": position,
            })
        };
        if pending.request.operation == NavigationOperation::References {
            params["context"] = serde_json::json!({"includeDeclaration": true});
        }
        let body = match serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        })) {
            Ok(body) => body,
            Err(error) => {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(error.to_string()),
                );
                return;
            }
        };
        if let Err(error) = session.send_body(&body) {
            self.complete_request(
                &pending.request,
                NavigationProtocolResult::Failed(format!("{error:#}")),
            );
            self.fail_session(key, format!("language server writer failed: {error:#}"));
            return;
        }
        session.pending.insert(
            id,
            PendingRequest::Navigation {
                request: pending.request,
                document: pending.document,
                deadline: Instant::now() + NAVIGATION_REQUEST_TIMEOUT,
            },
        );
    }

    fn poll_disk(&mut self) {
        let mut results = Vec::new();
        let mut wedged = false;
        if let Some(lane) = self.disk_lane.as_ref() {
            while let Some(result) = lane.try_result() {
                results.push(result);
            }
            wedged = lane.mark_wedged_if_overdue(Instant::now());
        }
        if wedged {
            for (_, (_, pending)) in std::mem::take(&mut self.disk_waiting) {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(
                        "File revalidation worker is unavailable.".to_owned(),
                    ),
                );
            }
        }
        for result in results {
            let generation = match result {
                DiskSnapshotResult::Current { generation }
                | DiskSnapshotResult::Stale { generation }
                | DiskSnapshotResult::Failed { generation, .. } => generation,
            };
            let Some((key, pending)) = self.disk_waiting.remove(&generation) else {
                continue;
            };
            match result {
                DiskSnapshotResult::Current { .. } => self.dispatch_navigation(&key, pending),
                DiskSnapshotResult::Stale { .. } => self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(
                        "File changed on disk; refresh Preview.".to_owned(),
                    ),
                ),
                DiskSnapshotResult::Failed { message, .. } => self
                    .complete_request(&pending.request, NavigationProtocolResult::Failed(message)),
            }
        }
    }

    fn poll_sessions(&mut self) {
        let keys: Vec<_> = self.sessions.keys().cloned().collect();
        let mut failed = Vec::new();
        for key in keys {
            let lifecycle_error = self.sessions.get_mut(&key).and_then(|session| {
                while let Ok(done) = session.lifecycle.try_recv() {
                    if let Err(error) = record_io_done(session.epoch, &mut session.io_done, done) {
                        return Some(format!("{error:#}"));
                    }
                }
                None
            });
            if let Some(error) = lifecycle_error {
                failed.push((key.clone(), error));
                continue;
            }

            let control_error =
                self.sessions
                    .get(&key)
                    .and_then(|session| match session.controls.try_recv() {
                        Ok(SessionControlEvent::ReaderFailed(error))
                        | Ok(SessionControlEvent::WriterFailed(error))
                        | Ok(SessionControlEvent::StderrFailed(error)) => Some(error),
                        Ok(SessionControlEvent::ReaderEof) => {
                            Some("language server stopped unexpectedly".to_owned())
                        }
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => {
                            Some("language server I/O control channel stopped".to_owned())
                        }
                    });
            if let Some(error) = control_error {
                failed.push((key.clone(), error));
                continue;
            }

            loop {
                let frame = self
                    .sessions
                    .get(&key)
                    .map(|session| session.frames.try_recv());
                match frame {
                    Some(Ok(frame)) => self.handle_frame(&key, frame),
                    Some(Err(TryRecvError::Empty)) => break,
                    Some(Err(TryRecvError::Disconnected)) => {
                        failed.push((
                            key.clone(),
                            "language server frame channel stopped".to_owned(),
                        ));
                        break;
                    }
                    None => break,
                }
            }
        }
        for (key, error) in failed {
            self.fail_session(&key, error);
        }
    }

    fn handle_frame(&mut self, key: &SessionKey, frame: ChargedPayload) {
        let budget = match self.sessions.get(key) {
            Some(session) => session.budget.clone(),
            None => return,
        };
        if let Err(error) = json_preflight(frame.as_slice()) {
            self.fail_session(key, format!("malformed language server JSON: {error:#}"));
            return;
        }
        let Ok(_scratch) = budget.reserve(frame.len()) else {
            self.fail_session(
                key,
                "language server response exceeded the navigation payload budget".to_owned(),
            );
            return;
        };
        let mut deserializer = serde_json::Deserializer::from_slice(frame.as_slice());
        let envelope = match BorrowedEnvelope::deserialize(&mut deserializer) {
            Ok(envelope) if deserializer.end().is_ok() && envelope.jsonrpc == "2.0" => envelope,
            _ => {
                self.fail_session(key, "invalid JSON-RPC envelope".to_owned());
                return;
            }
        };
        if let Some(method) = envelope.method {
            if envelope.result.is_some() || envelope.error.is_some() {
                self.fail_session(
                    key,
                    "invalid JSON-RPC message contains method and response fields".to_owned(),
                );
                return;
            }
            self.handle_server_call(key, envelope.id, method, envelope.params);
            return;
        }
        let Some(id) = envelope.id else {
            self.fail_session(key, "JSON-RPC response is missing its id".to_owned());
            return;
        };
        if envelope.result.is_some() == envelope.error.is_some() {
            self.fail_session(
                key,
                "JSON-RPC response must contain exactly one of result or error".to_owned(),
            );
            return;
        }
        if self
            .sessions
            .get_mut(key)
            .is_some_and(|session| session.retired.take(&id))
        {
            return;
        }
        if !self
            .sessions
            .get(key)
            .is_some_and(|session| session.pending.contains_key(&id))
        {
            self.fail_session(key, "unmatched JSON-RPC response id".to_owned());
            return;
        }
        let pending = self
            .sessions
            .get_mut(key)
            .and_then(|session| session.pending.remove(&id));
        let Some(pending) = pending else { return };
        match pending {
            PendingRequest::Initialize { .. } => {
                if let Some(error) = envelope.error {
                    let message = parse_protocol_error(error, &budget)
                        .map(|(message, _)| message)
                        .unwrap_or_else(|_| "invalid bounded error message".to_owned());
                    self.fail_session(
                        key,
                        format!("language server initialization failed: {message}"),
                    );
                    return;
                }
                let Some(result) = envelope.result else {
                    self.fail_session(
                        key,
                        "language server initialize response is missing result".to_owned(),
                    );
                    return;
                };
                let capabilities = match parse_initialize_capabilities(result.get()) {
                    Ok(capabilities) => capabilities,
                    Err(error) => {
                        self.fail_session(key, format!("{error:#}"));
                        return;
                    }
                };
                let initialized = self.sessions.get_mut(key).map_or_else(
                    || Err(anyhow!("language server session disappeared")),
                    |session| {
                        session.state = ServerState::Ready { capabilities };
                        session.ready_since = Some(Instant::now());
                        session.send_json(&serde_json::json!({
                            "jsonrpc":"2.0", "method":"initialized", "params":{}
                        }))?;
                        Ok(session.deferred.take())
                    },
                );
                match initialized {
                    Ok(Some(deferred)) => self.dispatch_or_revalidate(key.clone(), deferred),
                    Ok(None) => {}
                    Err(error) => {
                        self.fail_session(
                            key,
                            format!("cannot finish language server initialization: {error:#}"),
                        );
                    }
                }
            }
            PendingRequest::Navigation {
                request, document, ..
            } => {
                let result = if let Some(error) = envelope.error {
                    let (message, permit) = match parse_protocol_error(error, &budget) {
                        Ok(error) => error,
                        Err(error) => {
                            self.complete_request(
                                &request,
                                NavigationProtocolResult::Failed(format!("{error:#}")),
                            );
                            self.fail_session(
                                key,
                                "invalid language server error result".to_owned(),
                            );
                            return;
                        }
                    };
                    self.mark_healthy(key);
                    self.complete_request_with_permit(
                        &request,
                        NavigationProtocolResult::Failed(message),
                        Some(permit),
                    );
                    return;
                } else if let Some(raw) = envelope.result {
                    if request.operation == NavigationOperation::DocumentSymbols {
                        match parse_document_symbol_result(
                            raw.get(),
                            &budget,
                            &self.workspace_root,
                            &document,
                        ) {
                            Ok(symbols) => {
                                self.mark_healthy(key);
                                self.complete_document_symbols(&request, symbols);
                                return;
                            }
                            Err(error) => {
                                self.complete_request(
                                    &request,
                                    NavigationProtocolResult::Failed(format!("{error:#}")),
                                );
                                self.fail_session(
                                    key,
                                    "invalid language server document symbol result".to_owned(),
                                );
                                return;
                            }
                        }
                    }
                    match parse_navigation_result(request.operation, raw.get(), &budget) {
                        Ok(result) => {
                            self.mark_healthy(key);
                            result
                        }
                        Err(error) => {
                            self.complete_request(
                                &request,
                                NavigationProtocolResult::Failed(format!("{error:#}")),
                            );
                            self.fail_session(
                                key,
                                "invalid language server navigation result".to_owned(),
                            );
                            return;
                        }
                    }
                } else {
                    unreachable!("response shape validated before pending removal")
                };
                self.complete_request(&request, result);
            }
        }
    }

    fn fail_session(&mut self, key: &SessionKey, error: String) {
        let Some(mut session) = self.sessions.remove(key) else {
            return;
        };
        session.state = ServerState::StoppingForced;
        for (_, pending) in session.pending.drain() {
            if let PendingRequest::Navigation { request, .. } = pending {
                self.complete_request(&request, NavigationProtocolResult::Failed(error.clone()));
            }
        }
        if let Some(pending) = session.deferred.take() {
            self.complete_request(
                &pending.request,
                NavigationProtocolResult::Failed(error.clone()),
            );
        }

        let disk_generations: Vec<_> = self
            .disk_waiting
            .iter()
            .filter_map(|(generation, (waiting_key, _))| {
                (waiting_key == key).then_some(*generation)
            })
            .collect();
        for generation in disk_generations {
            if let Some((_, pending)) = self.disk_waiting.remove(&generation) {
                self.complete_request(
                    &pending.request,
                    NavigationProtocolResult::Failed(error.clone()),
                );
            }
        }

        let previous = self
            .failed_sessions
            .insert(key.clone(), FailedSession { session, error });
        assert!(
            previous.is_none(),
            "a SessionKey cannot fail twice before cleanup"
        );
    }

    fn cleanup_failed_sessions(&mut self) {
        let failures = std::mem::take(&mut self.failed_sessions);
        if failures.is_empty() {
            return;
        }
        let mut metadata = Vec::with_capacity(failures.len());
        let mut sessions = Vec::with_capacity(failures.len());
        for (key, failure) in failures {
            metadata.push((key, failure.error));
            sessions.push(failure.session);
        }
        let outcomes = cleanup_sessions(&mut sessions);
        let failed_at = Instant::now();
        for (((key, error), session), outcome) in metadata.into_iter().zip(sessions).zip(outcomes) {
            self.finish_failed_session_cleanup(key, error, session, outcome, failed_at);
        }
    }

    fn cleanup_shutdown_sessions(&mut self) {
        let failures = std::mem::take(&mut self.failed_sessions);
        let active = std::mem::take(&mut self.sessions);
        let mut failure_metadata = Vec::with_capacity(failures.len() + active.len());
        let mut sessions = Vec::with_capacity(failures.len() + active.len());
        for (key, failure) in failures {
            failure_metadata.push(Some((key, failure.error)));
            sessions.push(failure.session);
        }
        for session in active.into_values() {
            failure_metadata.push(None);
            sessions.push(session);
        }
        let outcomes = cleanup_sessions(&mut sessions);
        let failed_at = Instant::now();
        for ((metadata, session), outcome) in
            failure_metadata.into_iter().zip(sessions).zip(outcomes)
        {
            if let Some((key, error)) = metadata {
                self.finish_failed_session_cleanup(key, error, session, outcome, failed_at);
            } else if outcome == CleanupOutcome::Quarantined {
                self.retain_quarantined(session);
            }
        }
    }

    fn finish_failed_session_cleanup(
        &mut self,
        key: SessionKey,
        error: String,
        session: LspSession,
        outcome: CleanupOutcome,
        failed_at: Instant,
    ) {
        match outcome {
            CleanupOutcome::Complete => self.record_failure_at(&key, &error, failed_at),
            CleanupOutcome::Quarantined => {
                let message =
                    "language server cleanup quarantined an unfinished process owner".to_owned();
                self.permanent_failures.insert(key, message);
                self.retain_quarantined(session);
            }
        }
    }

    fn retain_quarantined(&mut self, session: LspSession) {
        let key = session.key.clone();
        assert!(
            !self.quarantined_spawns.contains_key(&key),
            "a SessionKey cannot have two quarantined process owners"
        );
        let previous = self.quarantined.insert(key, session);
        assert!(
            previous.is_none(),
            "a SessionKey cannot be quarantined twice"
        );
        self.cleanup_stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot
            .quarantined_process_owners = self.quarantined_owner_count();
    }

    fn retain_quarantined_spawn(&mut self, key: SessionKey, tree: OwnedProcessTree) {
        assert!(
            !self.quarantined.contains_key(&key),
            "a SessionKey cannot have two quarantined process owners"
        );
        let previous = self.quarantined_spawns.insert(key, tree);
        assert!(
            previous.is_none(),
            "a SessionKey cannot be quarantined twice"
        );
        self.cleanup_stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot
            .quarantined_process_owners = self.quarantined_owner_count();
    }

    fn has_quarantined_owners(&self) -> bool {
        self.quarantined_owner_count() != 0
    }

    fn quarantined_owner_count(&self) -> usize {
        self.quarantined.len() + self.quarantined_spawns.len()
    }

    fn record_failure_at(&mut self, key: &SessionKey, error: &str, now: Instant) {
        let attempt = self
            .backoffs
            .get(key)
            .map_or(1, |backoff| backoff.attempt.saturating_add(1))
            .min(5);
        let delay = backoff_delay(attempt);
        let error = clean_protocol_message(error);
        self.backoffs.insert(
            key.clone(),
            SessionBackoff {
                attempt,
                retry_at: now + delay,
                error: error.clone(),
            },
        );
        if attempt == 5 {
            self.permanent_failures.insert(key.clone(), error);
        }
    }

    fn mark_healthy(&mut self, key: &SessionKey) {
        let ready_since = self
            .sessions
            .get(key)
            .and_then(|session| session.ready_since);
        self.reset_backoff_if_stable_at(key, ready_since, Instant::now());
    }

    fn reset_backoff_if_stable_at(
        &mut self,
        key: &SessionKey,
        ready_since: Option<Instant>,
        now: Instant,
    ) {
        if ready_since
            .is_some_and(|ready| now.saturating_duration_since(ready) >= STABLE_READY_RESET)
        {
            self.backoffs.remove(key);
        }
    }

    fn reset_stable_backoffs_at(&mut self, now: Instant) {
        let stable: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(key, session)| {
                matches!(session.state, ServerState::Ready { .. })
                    .then_some(session.ready_since)
                    .flatten()
                    .filter(|ready| now.saturating_duration_since(*ready) >= STABLE_READY_RESET)
                    .map(|_| key.clone())
            })
            .collect();
        for key in stable {
            self.backoffs.remove(&key);
        }
    }

    fn handle_server_call(
        &mut self,
        key: &SessionKey,
        id: Option<RpcId>,
        method: &str,
        params: Option<&serde_json::value::RawValue>,
    ) {
        let Some(id) = id else { return };
        let response = match server_call_result(method, params, key) {
            Ok(result) => serde_json::json!({
                "jsonrpc":"2.0", "id":id, "result":result
            }),
            Err(ServerCallError::InvalidParams) => serde_json::json!({
                "jsonrpc":"2.0", "id":id,
                "error":{"code":-32602,"message":"Invalid params"}
            }),
            Err(ServerCallError::MethodNotFound) => serde_json::json!({
                "jsonrpc":"2.0", "id":id,
                "error":{"code":-32601,"message":"Method not found"}
            }),
        };
        let send_error = self
            .sessions
            .get_mut(key)
            .and_then(|session| session.send_json(&response).err());
        if let Some(error) = send_error {
            self.fail_session(
                key,
                format!("cannot answer language server request: {error:#}"),
            );
        }
    }

    fn expire_requests(&mut self) {
        self.expire_requests_at(Instant::now());
    }

    fn expire_requests_at(&mut self, now: Instant) {
        let keys: Vec<_> = self.sessions.keys().cloned().collect();
        for key in keys {
            let expired: Vec<_> = self.sessions.get(&key).map_or_else(Vec::new, |session| {
                session
                    .pending
                    .iter()
                    .filter_map(|(id, pending)| {
                        let deadline = match pending {
                            PendingRequest::Initialize { deadline }
                            | PendingRequest::Navigation { deadline, .. } => *deadline,
                        };
                        (now >= deadline).then_some(id.clone())
                    })
                    .collect()
            });
            if expired.iter().any(|id| {
                self.sessions.get(&key).is_some_and(|session| {
                    matches!(
                        session.pending.get(id),
                        Some(PendingRequest::Initialize { .. })
                    )
                })
            }) {
                self.fail_session(&key, "Language server initialization timed out.".to_owned());
                continue;
            }
            for id in expired {
                let pending = self
                    .sessions
                    .get_mut(&key)
                    .and_then(|session| session.pending.remove(&id));
                if let Some(PendingRequest::Navigation { request, .. }) = pending {
                    let retirement = self.sessions.get_mut(&key).map_or_else(
                        || Err(anyhow!("language server session disappeared")),
                        |session| {
                            session.send_json(&serde_json::json!({
                            "jsonrpc":"2.0", "method":"$/cancelRequest", "params":{"id":id}
                            }))?;
                            session.retired.insert(id.clone())
                        },
                    );
                    self.complete_request(
                        &request,
                        NavigationProtocolResult::Failed(
                            "Language server request timed out.".to_owned(),
                        ),
                    );
                    if let Err(error) = retirement {
                        self.fail_session(
                            &key,
                            format!("cannot retire timed-out language server request: {error:#}"),
                        );
                        break;
                    }
                }
            }
        }
    }

    fn cancel_generation(&mut self, generation: u64) {
        if let Some((_, pending)) = self.disk_waiting.remove(&generation) {
            self.complete_request(&pending.request, NavigationProtocolResult::Cancelled);
        }
        let keys: Vec<_> = self.sessions.keys().cloned().collect();
        for key in keys {
            let deferred = self.sessions.get_mut(&key).and_then(|session| {
                session
                    .deferred
                    .as_ref()
                    .is_some_and(|pending| pending.request.generation == generation)
                    .then(|| session.deferred.take())
                    .flatten()
            });
            if let Some(deferred) = deferred {
                self.complete_request(&deferred.request, NavigationProtocolResult::Cancelled);
            }
            let ids: Vec<_> = self.sessions.get(&key).map_or_else(Vec::new, |session| {
                session
                    .pending
                    .iter()
                    .filter_map(|(id, pending)| match pending {
                        PendingRequest::Navigation { request, .. }
                            if request.generation == generation =>
                        {
                            Some(id.clone())
                        }
                        _ => None,
                    })
                    .collect()
            });
            for id in ids {
                let pending = self
                    .sessions
                    .get_mut(&key)
                    .and_then(|session| session.pending.remove(&id));
                if let Some(PendingRequest::Navigation { request, .. }) = pending {
                    let retirement = self.sessions.get_mut(&key).map_or_else(
                        || Err(anyhow!("language server session disappeared")),
                        |session| {
                            session.send_json(&serde_json::json!({
                            "jsonrpc":"2.0", "method":"$/cancelRequest", "params":{"id":id}
                            }))?;
                            session.retired.insert(id.clone())
                        },
                    );
                    self.complete_request(&request, NavigationProtocolResult::Cancelled);
                    if let Err(error) = retirement {
                        self.fail_session(
                            &key,
                            format!("cannot retire cancelled language server request: {error:#}"),
                        );
                        break;
                    }
                }
            }
        }
    }

    fn complete_request(
        &self,
        request: &NavigationRuntimeRequest,
        result: NavigationProtocolResult,
    ) {
        self.complete_request_with_permit(request, result, None);
    }

    fn complete_request_with_permit(
        &self,
        request: &NavigationRuntimeRequest,
        result: NavigationProtocolResult,
        permit: Option<Arc<PayloadPermit>>,
    ) {
        let completion = NavigationRuntimeCompletion {
            generation: request.generation,
            operation: request.operation,
            source_identity: request.source.identity.clone(),
            source_version: request.version,
            result,
        };
        let mut queue = self
            .completions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut permits = self
            .completion_permits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.len() == NAVIGATION_COMPLETION_CAPACITY {
            queue.pop_front();
            permits.queued.pop_front();
        }
        queue.push_back(completion);
        permits.queued.push_back(permit);
    }

    fn complete_document_symbols(
        &self,
        request: &NavigationRuntimeRequest,
        symbols: Vec<ProtocolDocumentSymbol>,
    ) {
        let completion = NavigationDocumentSymbolCompletion {
            generation: request.generation,
            source_identity: request.source.identity.clone(),
            source_version: request.version,
            symbols,
        };
        let mut queue = self
            .symbol_completions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queue.len() == NAVIGATION_COMPLETION_CAPACITY {
            queue.pop_front();
        }
        queue.push_back(completion);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerCallError {
    InvalidParams,
    MethodNotFound,
}

fn server_call_result(
    method: &str,
    params: Option<&serde_json::value::RawValue>,
    key: &SessionKey,
) -> std::result::Result<serde_json::Value, ServerCallError> {
    let parse = || -> Result<serde_json::Value> {
        if let Some(params) = params
            && params.get().len() > MAX_SERVER_CALL_PARAMS_BYTES
        {
            bail!("server request params exceed 64 KiB");
        }
        match method {
            "workspace/configuration" => {
                let params = params.ok_or_else(|| anyhow!("missing configuration params"))?;
                let request: ConfigurationParams<'_> = serde_json::from_str(params.get())?;
                if request.items.len() > MAX_CONFIGURATION_ITEMS {
                    bail!("workspace/configuration has more than 64 items");
                }
                let item_count = request.items.len();
                for item in request.items {
                    let item: ConfigurationItem<'_> = serde_json::from_str(item.get())?;
                    if let Some(scope_uri) = item.scope_uri {
                        validate_param_uri(scope_uri)?;
                    }
                    if let Some(section) = item.section {
                        validate_param_string(section, MAX_CONFIGURATION_STRING_BYTES)?;
                    }
                }
                Ok(serde_json::Value::Array(
                    std::iter::repeat_n(serde_json::Value::Null, item_count).collect(),
                ))
            }
            "workspace/workspaceFolders" => {
                if params.is_some_and(|raw| raw.get() != "null") {
                    bail!("workspaceFolders does not accept params");
                }
                let uri = crate::navigation::path_to_lsp_uri(&key.server_root)?;
                Ok(serde_json::json!([{"uri":uri,"name":"workspace"}]))
            }
            "workspace/applyEdit" => {
                let params = params.ok_or_else(|| anyhow!("missing applyEdit params"))?;
                let request: ApplyEditParams<'_> = serde_json::from_str(params.get())?;
                if !raw_starts_with(request.edit, b'{') {
                    bail!("workspace edit must be an object");
                }
                if let Some(label) = request.label {
                    validate_param_string(label, MAX_CONFIGURATION_STRING_BYTES)?;
                }
                Ok(serde_json::json!({
                    "applied": false, "failureReason": "Latte Lens is read-only"
                }))
            }
            "client/registerCapability" => {
                validate_registrations(params, true)?;
                Ok(serde_json::Value::Null)
            }
            "client/unregisterCapability" => {
                validate_registrations(params, false)?;
                Ok(serde_json::Value::Null)
            }
            "window/workDoneProgress/create" => {
                let params = params.ok_or_else(|| anyhow!("missing progress params"))?;
                let request: WorkDoneProgressParams<'_> = serde_json::from_str(params.get())?;
                let _: RpcId = serde_json::from_str(request.token.get())?;
                Ok(serde_json::Value::Null)
            }
            "window/showMessageRequest" => {
                let params = params.ok_or_else(|| anyhow!("missing message params"))?;
                let request: ShowMessageParams<'_> = serde_json::from_str(params.get())?;
                if !(1..=4).contains(&request.kind) {
                    bail!("invalid showMessageRequest type");
                }
                validate_param_string(request.message, MAX_CONFIGURATION_STRING_BYTES)?;
                if request
                    .actions
                    .as_ref()
                    .is_some_and(|actions| actions.len() > 64)
                {
                    bail!("too many showMessageRequest actions");
                }
                for action in request.actions.into_iter().flatten() {
                    let action: MessageAction<'_> = serde_json::from_str(action.get())?;
                    validate_param_string(action.title, MAX_CONFIGURATION_STRING_BYTES)?;
                }
                Ok(serde_json::Value::Null)
            }
            _ => Err(anyhow!("method not found")),
        }
    };
    match parse() {
        Ok(value) => Ok(value),
        Err(_)
            if !matches!(
                method,
                "workspace/configuration"
                    | "workspace/workspaceFolders"
                    | "workspace/applyEdit"
                    | "client/registerCapability"
                    | "client/unregisterCapability"
                    | "window/workDoneProgress/create"
                    | "window/showMessageRequest"
            ) =>
        {
            Err(ServerCallError::MethodNotFound)
        }
        Err(_) => Err(ServerCallError::InvalidParams),
    }
}

fn raw_starts_with(raw: &serde_json::value::RawValue, expected: u8) -> bool {
    raw.get()
        .as_bytes()
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(expected)
}

fn validate_param_string(raw: &serde_json::value::RawValue, max_decoded: usize) -> Result<()> {
    let max_encoded = max_decoded
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("bounded string length overflow"))?;
    if raw.get().len() > max_encoded {
        bail!("bounded server request string is too large");
    }
    let value: Cow<'_, str> = serde_json::from_str(raw.get())?;
    if value.len() > max_decoded {
        bail!("bounded server request string is too large");
    }
    Ok(())
}

fn validate_param_uri(raw: &serde_json::value::RawValue) -> Result<()> {
    validate_param_string(raw, MAX_RESULT_URI_BYTES)?;
    let _: lsp_types::Uri = serde_json::from_str(raw.get()).context("invalid scopeUri")?;
    Ok(())
}

fn validate_registrations(
    params: Option<&serde_json::value::RawValue>,
    registering: bool,
) -> Result<()> {
    let params = params.ok_or_else(|| anyhow!("missing registration params"))?;
    let items = if registering {
        serde_json::from_str::<RegistrationParams<'_>>(params.get())?.registrations
    } else {
        serde_json::from_str::<UnregistrationParams<'_>>(params.get())?.unregisterations
    };
    if items.len() > 64 {
        bail!("too many capability registrations");
    }
    for item in items {
        let item: RegistrationItem<'_> = serde_json::from_str(item.get())?;
        validate_param_string(item.id, MAX_CONFIGURATION_STRING_BYTES)?;
        validate_param_string(item.method, MAX_CONFIGURATION_STRING_BYTES)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigurationParams<'a> {
    #[serde(borrow)]
    items: Vec<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigurationItem<'a> {
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    scope_uri: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    section: Option<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyEditParams<'a> {
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    label: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow)]
    edit: &'a serde_json::value::RawValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkDoneProgressParams<'a> {
    #[serde(borrow)]
    token: &'a serde_json::value::RawValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShowMessageParams<'a> {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(borrow)]
    message: &'a serde_json::value::RawValue,
    #[serde(borrow, default)]
    actions: Option<Vec<&'a serde_json::value::RawValue>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageAction<'a> {
    #[serde(borrow)]
    title: &'a serde_json::value::RawValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationParams<'a> {
    #[serde(borrow)]
    registrations: Vec<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnregistrationParams<'a> {
    #[serde(borrow)]
    unregisterations: Vec<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationItem<'a> {
    #[serde(borrow)]
    id: &'a serde_json::value::RawValue,
    #[serde(borrow)]
    method: &'a serde_json::value::RawValue,
}

impl LspSession {
    fn send_body(&mut self, body: &[u8]) -> Result<()> {
        let frame = encode_frame(body, &self.budget)?;
        self.writer
            .as_ref()
            .ok_or_else(|| anyhow!("language server writer stopped"))?
            .try_send(frame)
            .map_err(|error| anyhow!("language server writer queue is full: {error}"))
    }

    fn send_json(&mut self, value: &serde_json::Value) -> Result<()> {
        self.send_body(&serde_json::to_vec(value)?)
    }

    fn supports(&self, operation: NavigationOperation) -> bool {
        let ServerState::Ready { capabilities } = &self.state else {
            return false;
        };
        match operation {
            NavigationOperation::Definition => capabilities.definition,
            NavigationOperation::References => capabilities.references,
            NavigationOperation::Implementations => capabilities.implementations,
            NavigationOperation::DocumentSymbols => capabilities.document_symbols,
        }
    }

    fn sync_document(&mut self, document: &NavigationDocument) -> Result<()> {
        let ServerState::Ready { capabilities } = &self.state else {
            bail!("language server is not ready");
        };
        let open_close = capabilities.text_document_sync.open_close;
        if !open_close {
            return Ok(());
        }
        let uri = crate::navigation::path_to_lsp_uri(&document.absolute_path)?;
        if self
            .opened
            .as_ref()
            .is_some_and(|opened| opened.uri == uri && opened.version == document.version)
        {
            return Ok(());
        }
        let version = i32::try_from(document.version.0).context("document version exhausted")?;
        if let Some(opened) = self.opened.take() {
            // Latte Lens is a read-only viewer and deliberately never sends
            // didChange. A refreshed complete Preview is synchronized as a
            // close/open pair so the server cannot observe an editor-style
            // mutable document lifecycle.
            self.send_json(&serde_json::json!({
                "jsonrpc":"2.0", "method":"textDocument/didClose",
                "params":{"textDocument":{"uri":opened.uri}}
            }))?;
        }
        self.send_json(&serde_json::json!({
            "jsonrpc":"2.0", "method":"textDocument/didOpen",
            "params":{"textDocument":{
                "uri":uri, "languageId":document.language.language_id,
                "version":version, "text":document.text.as_ref()
            }}
        }))?;
        self.opened = Some(OpenedDocument {
            uri,
            version: document.version,
        });
        Ok(())
    }

    fn cleanup(&mut self) -> CleanupOutcome {
        cleanup_sessions(std::slice::from_mut(self))[0]
    }

    fn begin_cleanup(&mut self, progress: &mut CleanupProgress) {
        progress.synthetic_session = self.tree.is_none() && self.io_threads.is_empty();
        progress.orderly = matches!(self.state, ServerState::Ready { .. });
        if !progress.orderly {
            progress.shutdown_response_finished = true;
            return;
        }
        let Ok(id) = self.allocator.allocate() else {
            progress.shutdown_response_finished = true;
            return;
        };
        if self
            .send_json(&serde_json::json!({
                "jsonrpc":"2.0", "id":id, "method":"shutdown", "params":null
            }))
            .is_ok()
        {
            progress.shutdown_id = Some(id);
        } else {
            progress.shutdown_response_finished = true;
        }
    }

    fn poll_shutdown_response(&mut self, progress: &mut CleanupProgress) {
        if progress.shutdown_response_finished {
            return;
        }
        if !self.collect_lifecycle() {
            progress.cleanup_error.get_or_insert_with(|| {
                "invalid language server I/O lifecycle event during cleanup".to_owned()
            });
            progress.shutdown_response_finished = true;
            return;
        }
        loop {
            match self.frames.try_recv() {
                Ok(frame) => {
                    if progress
                        .shutdown_id
                        .as_ref()
                        .is_some_and(|id| is_shutdown_response(&frame, &self.budget, id))
                    {
                        let _ = self.send_json(&serde_json::json!({
                            "jsonrpc":"2.0", "method":"exit", "params":null
                        }));
                        progress.shutdown_response_finished = true;
                        return;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    progress.shutdown_response_finished = true;
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if !matches!(self.controls.try_recv(), Err(TryRecvError::Empty)) {
            progress.shutdown_response_finished = true;
        }
    }

    fn poll_tree_exit(&mut self, progress: &mut CleanupProgress, clean_phase: bool) {
        if progress.tree_finished {
            return;
        }
        let Some(tree) = self.tree.as_mut() else {
            progress.tree_finished = true;
            return;
        };
        match tree.poll_exit() {
            Ok(true) => {
                progress.tree_finished = true;
                progress.direct_child_reaped = true;
                progress.clean_exit = clean_phase && progress.orderly;
            }
            Ok(false) => {}
            Err(error) => {
                progress
                    .cleanup_error
                    .get_or_insert_with(|| format!("{error:#}"));
            }
        }
    }

    fn begin_force_cleanup(&mut self, progress: &mut CleanupProgress) {
        if progress.tree_finished || self.tree.is_none() {
            return;
        }
        progress.forced_cleanup = true;
        if let Err(error) = self
            .tree
            .as_mut()
            .expect("unfinished process tree is present")
            .begin_force_cleanup()
        {
            progress
                .cleanup_error
                .get_or_insert_with(|| format!("{error:#}"));
        }
    }

    fn escalate_force_cleanup(&mut self, progress: &mut CleanupProgress) {
        if progress.tree_finished || self.tree.is_none() {
            return;
        }
        if let Err(error) = self
            .tree
            .as_mut()
            .expect("unfinished process tree is present")
            .escalate_force_cleanup()
        {
            progress
                .cleanup_error
                .get_or_insert_with(|| format!("{error:#}"));
        }
    }

    fn poll_forced_tree(&mut self, progress: &mut CleanupProgress) {
        if progress.tree_finished || self.tree.is_none() {
            return;
        }
        match self
            .tree
            .as_mut()
            .expect("unfinished process tree is present")
            .poll_force_cleanup()
        {
            Ok(true) => {
                progress.tree_finished = true;
                progress.direct_child_reaped = true;
            }
            Ok(false) => {}
            Err(error) => {
                progress
                    .cleanup_error
                    .get_or_insert_with(|| format!("{error:#}"));
            }
        }
    }

    fn poll_io_completion(&mut self, progress: &mut CleanupProgress) {
        if progress.synthetic_session {
            return;
        }
        if !self.collect_lifecycle() {
            progress.cleanup_error.get_or_insert_with(|| {
                "invalid language server I/O lifecycle event during cleanup".to_owned()
            });
        }
        let mut remaining = Vec::new();
        for (kind, handle) in self.io_threads.drain(..) {
            if self.io_done.contains(&kind) && handle.is_finished() {
                if handle.join().is_ok() {
                    self.io_joined.insert(kind);
                } else {
                    progress
                        .cleanup_error
                        .get_or_insert_with(|| "language server I/O thread panicked".to_owned());
                }
            } else {
                remaining.push((kind, handle));
            }
        }
        self.io_threads = remaining;
    }

    fn finalize_cleanup(&mut self, progress: CleanupProgress) -> CleanupOutcome {
        let expected_joins = if progress.synthetic_session {
            0
        } else {
            IO_THREAD_COUNT
        };
        if !progress.tree_finished
            || progress.cleanup_error.is_some()
            || !self.io_threads.is_empty()
            || self.io_joined.len() != expected_joins
        {
            self.state = ServerState::Failed {
                error: progress.cleanup_error.unwrap_or_else(|| {
                    "language server cleanup quarantined an unfinished process owner".to_owned()
                }),
            };
            return CleanupOutcome::Quarantined;
        }

        // Publish completion only after the tree is empty, the direct child is
        // reaped, all matching I/O owners joined, and the platform owner drops.
        drop(self.tree.take());
        let mut stats = self
            .cleanup_stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stats.snapshot.sessions_cleaned += 1;
        stats.snapshot.clean_exits += usize::from(progress.clean_exit);
        stats.snapshot.forced_tree_cleanups += usize::from(progress.forced_cleanup);
        stats.snapshot.direct_children_reaped += usize::from(progress.direct_child_reaped);
        stats.snapshot.io_threads_joined += self.io_joined.len();
        stats.snapshot.process_owners_dropped += 1;
        CleanupOutcome::Complete
    }

    fn collect_lifecycle(&mut self) -> bool {
        loop {
            match self.lifecycle.try_recv() {
                Ok(done) if record_io_done(self.epoch, &mut self.io_done, done).is_ok() => {}
                Ok(_) => return false,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => {
                    return self.io_done.len() == IO_THREAD_COUNT;
                }
            }
        }
    }
}

fn cleanup_sessions(sessions: &mut [LspSession]) -> Vec<CleanupOutcome> {
    let mut progress: Vec<_> = (0..sessions.len())
        .map(|_| CleanupProgress::default())
        .collect();
    for (session, progress) in sessions.iter_mut().zip(&mut progress) {
        session.begin_cleanup(progress);
    }

    poll_cleanup_phase(CLEANUP_SHUTDOWN_RESPONSE_WINDOW, || {
        for (session, progress) in sessions.iter_mut().zip(&mut progress) {
            session.poll_shutdown_response(progress);
        }
        progress
            .iter()
            .all(|progress| progress.shutdown_response_finished)
    });
    for session in sessions.iter_mut() {
        // Let the writer drain an already queued exit; tree-first termination
        // releases it if the server stopped consuming stdin.
        session.writer.take();
    }

    poll_cleanup_phase(CLEANUP_TREE_EXIT_WINDOW, || {
        for (session, progress) in sessions.iter_mut().zip(&mut progress) {
            session.poll_tree_exit(progress, true);
        }
        progress.iter().all(|progress| progress.tree_finished)
    });
    for (session, progress) in sessions.iter_mut().zip(&mut progress) {
        session.begin_force_cleanup(progress);
    }
    poll_cleanup_phase(CLEANUP_TERM_WINDOW, || {
        for (session, progress) in sessions.iter_mut().zip(&mut progress) {
            session.poll_forced_tree(progress);
        }
        progress.iter().all(|progress| progress.tree_finished)
    });
    for (session, progress) in sessions.iter_mut().zip(&mut progress) {
        session.escalate_force_cleanup(progress);
    }
    poll_cleanup_phase(CLEANUP_KILL_REAP_WINDOW, || {
        for (session, progress) in sessions.iter_mut().zip(&mut progress) {
            session.poll_forced_tree(progress);
        }
        progress.iter().all(|progress| progress.tree_finished)
    });

    poll_cleanup_phase(CLEANUP_IO_WINDOW, || {
        for (session, progress) in sessions.iter_mut().zip(&mut progress) {
            session.poll_io_completion(progress);
        }
        sessions.iter().zip(&progress).all(|(session, progress)| {
            progress.synthetic_session
                || (session.io_done.len() == IO_THREAD_COUNT && session.io_threads.is_empty())
        })
    });
    poll_cleanup_phase(CLEANUP_JOIN_SETTLE_WINDOW, || {
        for (session, progress) in sessions.iter_mut().zip(&mut progress) {
            session.poll_io_completion(progress);
        }
        sessions.iter().all(|session| session.io_threads.is_empty())
    });

    sessions
        .iter_mut()
        .zip(progress)
        .map(|(session, progress)| session.finalize_cleanup(progress))
        .collect()
}

fn poll_cleanup_phase(window: Duration, mut poll: impl FnMut() -> bool) {
    let deadline = Instant::now() + window;
    loop {
        if poll() || Instant::now() >= deadline {
            return;
        }
        thread::sleep(CLEANUP_POLL_INTERVAL);
    }
}

#[derive(Deserialize)]
struct BorrowedEnvelope<'a> {
    jsonrpc: &'a str,
    id: Option<RpcId>,
    method: Option<&'a str>,
    #[serde(borrow)]
    params: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    result: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    error: Option<&'a serde_json::value::RawValue>,
}

fn deserialize_present_raw_value<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<&'de serde_json::value::RawValue>, D::Error>
where
    D: Deserializer<'de>,
{
    <&serde_json::value::RawValue>::deserialize(deserializer).map(Some)
}

fn is_shutdown_response(
    frame: &ChargedPayload,
    budget: &PayloadBudget,
    shutdown_id: &RpcId,
) -> bool {
    if json_preflight(frame.as_slice()).is_err() {
        return false;
    }
    let Ok(_scratch) = budget.reserve(frame.len()) else {
        return false;
    };
    let mut deserializer = serde_json::Deserializer::from_slice(frame.as_slice());
    let Ok(envelope) = BorrowedEnvelope::deserialize(&mut deserializer) else {
        return false;
    };
    deserializer.end().is_ok()
        && envelope.jsonrpc == "2.0"
        && envelope.id.as_ref() == Some(shutdown_id)
        && envelope.method.is_none()
        && envelope.result.is_some()
        && envelope.error.is_none()
}

#[derive(Deserialize)]
struct BorrowedProtocolError<'a> {
    #[serde(borrow)]
    message: &'a serde_json::value::RawValue,
}

fn parse_protocol_error(
    raw: &serde_json::value::RawValue,
    budget: &PayloadBudget,
) -> Result<(String, Arc<PayloadPermit>)> {
    let error: BorrowedProtocolError<'_> =
        serde_json::from_str(raw.get()).context("invalid JSON-RPC error object")?;
    let charge = bounded_raw_string_charge(error.message, MAX_CONFIGURATION_STRING_BYTES)?;
    let permit = Arc::new(budget.reserve(charge)?);
    let message = decode_bounded_owned_string(error.message, MAX_CONFIGURATION_STRING_BYTES)?;
    Ok((clean_protocol_message(&message), permit))
}

fn parse_initialize_capabilities(raw: &str) -> Result<NavigationCapabilities> {
    json_preflight(raw.as_bytes())?;
    if raw
        .as_bytes()
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        != Some(b'{')
    {
        bail!("language server initialize result must be an object");
    }
    // The caller holds the body-sized scratch permit acquired before the
    // envelope deserializer was created. This second streaming deserializer
    // borrows the same frame and constructs only the fixed-size fields below.
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let initialize = MinimalInitializeResult::deserialize(&mut deserializer)
        .context("malformed language server initialize result")?;
    deserializer
        .end()
        .context("trailing language server initialize result")?;
    if !raw_starts_with(initialize.capabilities, b'{') {
        bail!("language server capabilities must be an object");
    }
    let mut capabilities_deserializer =
        serde_json::Deserializer::from_str(initialize.capabilities.get());
    let capabilities = MinimalServerCapabilities::deserialize(&mut capabilities_deserializer)
        .context("malformed language server capabilities")?;
    capabilities_deserializer
        .end()
        .context("trailing language server capabilities")?;
    if let Some(encoding) = capabilities.position_encoding {
        if bounded_raw_string_charge(encoding, 32).is_err() {
            bail!("language server positionEncoding is too large");
        }
        let encoding: Cow<'_, str> =
            serde_json::from_str(encoding.get()).context("invalid positionEncoding")?;
        if encoding != "utf-16" {
            bail!(
                "language server selected unsupported positionEncoding {encoding}; Latte Lens requires UTF-16"
            );
        }
    }
    Ok(NavigationCapabilities {
        definition: CapabilitySupport::enabled(capabilities.definition_provider),
        references: CapabilitySupport::enabled(capabilities.references_provider),
        implementations: CapabilitySupport::enabled(capabilities.implementation_provider),
        document_symbols: CapabilitySupport::enabled(capabilities.document_symbol_provider),
        text_document_sync: MinimalTextDocumentSync::normalized(capabilities.text_document_sync),
    })
}

#[derive(Deserialize)]
struct MinimalInitializeResult<'a> {
    #[serde(borrow)]
    capabilities: &'a serde_json::value::RawValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MinimalServerCapabilities<'a> {
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    position_encoding: Option<&'a serde_json::value::RawValue>,
    #[serde(default)]
    definition_provider: Option<CapabilitySupport>,
    #[serde(default)]
    references_provider: Option<CapabilitySupport>,
    #[serde(default)]
    implementation_provider: Option<CapabilitySupport>,
    #[serde(default)]
    document_symbol_provider: Option<CapabilitySupport>,
    #[serde(default)]
    text_document_sync: Option<MinimalTextDocumentSync>,
}

#[derive(Clone, Copy)]
struct CapabilitySupport(bool);

impl CapabilitySupport {
    fn enabled(capability: Option<Self>) -> bool {
        capability.is_some_and(|support| support.0)
    }
}

impl<'de> Deserialize<'de> for CapabilitySupport {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CapabilityVisitor;

        impl<'de> Visitor<'de> for CapabilityVisitor {
            type Value = CapabilitySupport;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a boolean or bounded capability options object")
            }

            fn visit_bool<E>(self, enabled: bool) -> std::result::Result<Self::Value, E> {
                Ok(CapabilitySupport(enabled))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(CapabilitySupport(true))
            }
        }

        deserializer.deserialize_any(CapabilityVisitor)
    }
}

#[derive(Clone, Copy)]
struct MinimalTextDocumentSync(TextSyncCapability);

impl MinimalTextDocumentSync {
    fn normalized(capability: Option<Self>) -> TextSyncCapability {
        capability.map_or(
            TextSyncCapability {
                open_close: false,
                change: TextChangeCapability::None,
            },
            |sync| sync.0,
        )
    }
}

impl<'de> Deserialize<'de> for MinimalTextDocumentSync {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TextSyncVisitor;

        impl<'de> Visitor<'de> for TextSyncVisitor {
            type Value = MinimalTextDocumentSync;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an LSP textDocumentSync kind or options object")
            }

            fn visit_i64<E>(self, kind: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                normalize_text_sync_kind(kind)
                    .map(MinimalTextDocumentSync)
                    .map_err(E::custom)
            }

            fn visit_u64<E>(self, kind: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let kind = i64::try_from(kind).map_err(E::custom)?;
                self.visit_i64(kind)
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut open_close = None;
                let mut change = None;
                while let Some(field) = map.next_key::<Cow<'de, str>>()? {
                    match field.as_ref() {
                        "openClose" => {
                            if open_close.replace(map.next_value::<bool>()?).is_some() {
                                return Err(A::Error::duplicate_field("openClose"));
                            }
                        }
                        "change" => {
                            let value = map.next_value::<i64>()?;
                            if change.replace(value).is_some() {
                                return Err(A::Error::duplicate_field("change"));
                            }
                        }
                        _ => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                let change = normalize_text_sync_change(change.unwrap_or(0));
                Ok(MinimalTextDocumentSync(TextSyncCapability {
                    open_close: open_close.unwrap_or(false),
                    change,
                }))
            }
        }

        deserializer.deserialize_any(TextSyncVisitor)
    }
}

fn normalize_text_sync_kind(kind: i64) -> Result<TextSyncCapability> {
    Ok(match kind {
        0 => TextSyncCapability {
            open_close: false,
            change: TextChangeCapability::None,
        },
        1 => TextSyncCapability {
            open_close: true,
            change: TextChangeCapability::Full,
        },
        2 => TextSyncCapability {
            open_close: true,
            change: TextChangeCapability::Incremental,
        },
        _ => bail!("unsupported textDocumentSync kind"),
    })
}

fn normalize_text_sync_change(kind: i64) -> TextChangeCapability {
    match kind {
        1 => TextChangeCapability::Full,
        2 => TextChangeCapability::Incremental,
        _ => TextChangeCapability::None,
    }
}

fn parse_navigation_result(
    operation: NavigationOperation,
    raw: &str,
    budget: &PayloadBudget,
) -> Result<NavigationProtocolResult> {
    if raw == "null" {
        return Ok(NavigationProtocolResult::Locations(Vec::new()));
    }
    let allow_links = matches!(
        operation,
        NavigationOperation::Definition | NavigationOperation::Implementations
    );
    let mut locations = match operation {
        NavigationOperation::Definition
        | NavigationOperation::References
        | NavigationOperation::Implementations => {
            parse_bounded_locations(raw, budget, allow_links)?
        }
        NavigationOperation::DocumentSymbols => {
            bail!("document symbols use the bounded symbol result parser")
        }
    };
    locations.sort_by(|left, right| {
        (
            left.uri.as_str(),
            left.range.start.line,
            left.range.start.character,
            left.range.end.line,
            left.range.end.character,
        )
            .cmp(&(
                right.uri.as_str(),
                right.range.start.line,
                right.range.start.character,
                right.range.end.line,
                right.range.end.character,
            ))
    });
    locations.dedup();
    Ok(NavigationProtocolResult::Locations(locations))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BorrowedLocationItem<'a> {
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    uri: Option<&'a serde_json::value::RawValue>,
    range: Option<lsp_types::Range>,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    target_uri: Option<&'a serde_json::value::RawValue>,
    target_range: Option<lsp_types::Range>,
    target_selection_range: Option<lsp_types::Range>,
    origin_selection_range: Option<lsp_types::Range>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LocationItemKind {
    Location,
    Link,
}

struct LocationArraySeed<'a> {
    budget: &'a PayloadBudget,
    allow_links: bool,
}

impl<'de> DeserializeSeed<'de> for LocationArraySeed<'_> {
    type Value = Vec<ProtocolLocation>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(LocationArrayVisitor {
            budget: self.budget,
            allow_links: self.allow_links,
        })
    }
}

struct LocationArrayVisitor<'a> {
    budget: &'a PayloadBudget,
    allow_links: bool,
}

impl<'de> Visitor<'de> for LocationArrayVisitor<'_> {
    type Value = Vec<ProtocolLocation>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded homogeneous array of LSP locations")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut output = Vec::new();
        let mut kind = None;
        let mut charged = 0usize;
        while let Some(item) = sequence.next_element::<BorrowedLocationItem<'de>>()? {
            if output.len() == MAX_LOCATION_RESULTS {
                return Err(A::Error::custom(
                    "language server returned more than 2,000 locations",
                ));
            }
            let location = normalize_location_item(
                item,
                self.budget,
                self.allow_links,
                &mut kind,
                &mut charged,
            )
            .map_err(A::Error::custom)?;
            output.try_reserve(1).map_err(A::Error::custom)?;
            output.push(location);
        }
        Ok(output)
    }
}

fn parse_bounded_locations(
    raw: &str,
    budget: &PayloadBudget,
    allow_links: bool,
) -> Result<Vec<ProtocolLocation>> {
    match raw
        .as_bytes()
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
    {
        Some(b'{') if allow_links => {
            let item: BorrowedLocationItem<'_> =
                serde_json::from_str(raw).context("invalid LSP location")?;
            let mut kind = None;
            let mut charged = 0usize;
            Ok(vec![normalize_location_item(
                item,
                budget,
                allow_links,
                &mut kind,
                &mut charged,
            )?])
        }
        Some(b'[') => {
            let mut deserializer = serde_json::Deserializer::from_str(raw);
            let output = LocationArraySeed {
                budget,
                allow_links,
            }
            .deserialize(&mut deserializer)
            .context("invalid bounded LSP location array")?;
            deserializer.end().context("trailing LSP location result")?;
            Ok(output)
        }
        _ => bail!("language server returned an invalid location result shape"),
    }
}

fn normalize_location_item(
    item: BorrowedLocationItem<'_>,
    budget: &PayloadBudget,
    allow_links: bool,
    seen_kind: &mut Option<LocationItemKind>,
    charged: &mut usize,
) -> Result<ProtocolLocation> {
    let BorrowedLocationItem {
        uri,
        range,
        target_uri,
        target_range,
        target_selection_range,
        origin_selection_range,
    } = item;
    let (kind, uri, range) = match (uri, range, target_uri, target_range, target_selection_range) {
        (Some(uri), Some(range), None, None, None) if origin_selection_range.is_none() => {
            (LocationItemKind::Location, uri, range)
        }
        (None, None, Some(uri), Some(_), Some(selection)) if allow_links => {
            (LocationItemKind::Link, uri, selection)
        }
        _ => bail!("invalid or mixed LSP Location/LocationLink item"),
    };
    if seen_kind.is_some_and(|seen| seen != kind) {
        bail!("LSP location array mixes Location and LocationLink items");
    }
    *seen_kind = Some(kind);
    if !raw_starts_with(uri, b'"') || uri.get().len() > MAX_RESULT_URI_BYTES {
        bail!("LSP result URI exceeds 64 KiB");
    }
    let charge = uri
        .get()
        .len()
        .checked_add(std::mem::size_of::<ProtocolLocation>())
        .ok_or_else(|| anyhow!("normalized location charge overflow"))?;
    *charged = charged
        .checked_add(charge)
        .ok_or_else(|| anyhow!("normalized location result charge overflow"))?;
    if *charged > MAX_NORMALIZED_RESULT_BYTES {
        bail!("normalized location result exceeds 1 MiB");
    }
    // The encoded token is capped and charged before serde decodes any escape
    // into the Uri-owned string.
    let permit = Arc::new(budget.reserve(charge)?);
    let uri: lsp_types::Uri = serde_json::from_str(uri.get()).context("invalid LSP result URI")?;
    Ok(ProtocolLocation {
        uri,
        range,
        _permit: Some(permit),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SymbolDiscriminator<'a> {
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    selection_range: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    location: Option<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BorrowedDocumentSymbol<'a> {
    #[serde(borrow)]
    name: &'a serde_json::value::RawValue,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    detail: Option<&'a serde_json::value::RawValue>,
    kind: lsp_types::SymbolKind,
    range: lsp_types::Range,
    selection_range: lsp_types::Range,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    children: Option<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BorrowedSymbolInformation<'a> {
    #[serde(borrow)]
    name: &'a serde_json::value::RawValue,
    kind: lsp_types::SymbolKind,
    #[serde(borrow)]
    location: BorrowedSymbolLocation<'a>,
    #[serde(borrow, default, deserialize_with = "deserialize_present_raw_value")]
    container_name: Option<&'a serde_json::value::RawValue>,
}

#[derive(Deserialize)]
struct BorrowedSymbolLocation<'a> {
    #[serde(borrow)]
    uri: &'a serde_json::value::RawValue,
    range: lsp_types::Range,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SymbolResultVariant {
    Nested,
    Flat,
}

struct SymbolBuild<'a> {
    budget: &'a PayloadBudget,
    workspace_root: &'a std::path::Path,
    document: &'a NavigationDocument,
    output: Vec<ProtocolDocumentSymbol>,
    variant: Option<SymbolResultVariant>,
    string_bytes: usize,
    total_bytes: usize,
}

impl SymbolBuild<'_> {
    fn push_top(&mut self, raw: &serde_json::value::RawValue) -> Result<()> {
        let discriminator: SymbolDiscriminator<'_> =
            serde_json::from_str(raw.get()).context("invalid document symbol result item")?;
        let variant = match (
            discriminator.selection_range.is_some(),
            discriminator.location.is_some(),
        ) {
            (true, false) => SymbolResultVariant::Nested,
            (false, true) => SymbolResultVariant::Flat,
            _ => bail!("document symbol item has an invalid or mixed variant shape"),
        };
        if self.variant.is_some_and(|current| current != variant) {
            bail!("document symbol result mixes nested and flat variants");
        }
        self.variant = Some(variant);
        match variant {
            SymbolResultVariant::Nested => self.push_nested(raw, None, 1),
            SymbolResultVariant::Flat => self.push_flat(raw),
        }
    }

    fn push_nested(
        &mut self,
        raw: &serde_json::value::RawValue,
        parent: Option<usize>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_SYMBOL_DEPTH {
            bail!("nested document symbols exceed depth 64");
        }
        let item: BorrowedDocumentSymbol<'_> =
            serde_json::from_str(raw.get()).context("invalid nested DocumentSymbol")?;
        let range = self.document.line_index.range_from_utf16(item.range)?;
        let selection_range = self
            .document
            .line_index
            .range_from_utf16(item.selection_range)?;
        if !(range.start <= selection_range.start && selection_range.end <= range.end) {
            bail!("DocumentSymbol selectionRange is outside range");
        }
        let index = self.push_symbol(
            item.name,
            item.detail,
            None,
            item.kind,
            range,
            selection_range,
            parent,
            None,
        )?;
        if let Some(children) = item.children {
            if children.get() == "null" {
                return Ok(());
            }
            let mut deserializer = serde_json::Deserializer::from_str(children.get());
            NestedSymbolArraySeed {
                build: self,
                parent: index,
                depth: depth + 1,
            }
            .deserialize(&mut deserializer)
            .context("invalid nested DocumentSymbol children")?;
            deserializer
                .end()
                .context("trailing nested DocumentSymbol children")?;
        }
        Ok(())
    }

    fn push_flat(&mut self, raw: &serde_json::value::RawValue) -> Result<()> {
        let item: BorrowedSymbolInformation<'_> =
            serde_json::from_str(raw.get()).context("invalid flat SymbolInformation")?;
        if !raw_starts_with(item.location.uri, b'"')
            || item.location.uri.get().len() > MAX_RESULT_URI_BYTES
        {
            bail!("SymbolInformation URI exceeds 64 KiB");
        }
        let uri_charge = item.location.uri.get().len();
        let uri_permit = Arc::new(self.reserve_charge(uri_charge)?);
        let uri: lsp_types::Uri = serde_json::from_str(item.location.uri.get())
            .context("invalid SymbolInformation URI")?;
        let target = crate::navigation::lsp_uri_to_safe_path(&uri, self.workspace_root)?;
        if target != self.document.absolute_path {
            bail!("SymbolInformation URI does not match the requested document");
        }
        let range = self
            .document
            .line_index
            .range_from_utf16(item.location.range)?;
        let index = self.push_symbol(
            item.name,
            None,
            item.container_name,
            item.kind,
            range,
            range,
            None,
            Some(uri_permit),
        );
        index.map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_symbol(
        &mut self,
        name: &serde_json::value::RawValue,
        detail: Option<&serde_json::value::RawValue>,
        container: Option<&serde_json::value::RawValue>,
        kind: lsp_types::SymbolKind,
        range: crate::navigation::SourceRange,
        selection_range: crate::navigation::SourceRange,
        parent: Option<usize>,
        uri_permit: Option<Arc<PayloadPermit>>,
    ) -> Result<usize> {
        if self.output.len() == MAX_SYMBOL_RESULTS {
            bail!("language server returned more than 4,096 document symbols");
        }
        let encoded_bytes = [Some(name), detail, container]
            .into_iter()
            .flatten()
            .try_fold(0usize, |bytes, value| {
                let charge = bounded_raw_string_charge(value, MAX_SYMBOL_STRING_BYTES)?;
                bytes
                    .checked_add(charge)
                    .ok_or_else(|| anyhow!("document symbol string charge overflow"))
            })?;
        let charge = encoded_bytes
            .checked_add(std::mem::size_of::<ProtocolDocumentSymbol>())
            .ok_or_else(|| anyhow!("normalized document symbol charge overflow"))?;
        let permit = Arc::new(self.reserve_charge(charge)?);
        let mut name = decode_bounded_owned_string(name, MAX_SYMBOL_STRING_BYTES)?;
        trim_string_in_place(&mut name);
        if name.is_empty() {
            bail!("document symbol name is empty");
        }
        let detail = detail
            .map(|value| decode_bounded_owned_string(value, MAX_SYMBOL_STRING_BYTES))
            .transpose()?;
        let container = container
            .map(|value| decode_bounded_owned_string(value, MAX_SYMBOL_STRING_BYTES))
            .transpose()?;
        let text_bytes = name
            .len()
            .checked_add(detail.as_deref().map_or(0, str::len))
            .and_then(|bytes| bytes.checked_add(container.as_deref().map_or(0, str::len)))
            .ok_or_else(|| anyhow!("document symbol string charge overflow"))?;
        self.string_bytes = self
            .string_bytes
            .checked_add(text_bytes)
            .ok_or_else(|| anyhow!("document symbol string charge overflow"))?;
        if self.string_bytes > MAX_SYMBOL_TEXT_BYTES {
            bail!("document symbol strings exceed 512 KiB");
        }
        self.output.try_reserve(1)?;
        let index = self.output.len();
        self.output.push(ProtocolDocumentSymbol {
            name,
            detail,
            container,
            kind: normalize_symbol_kind(kind),
            range,
            selection_range,
            parent,
            _permit: permit,
            _uri_permit: uri_permit,
        });
        Ok(index)
    }

    fn reserve_charge(&mut self, charge: usize) -> Result<PayloadPermit> {
        self.total_bytes = self
            .total_bytes
            .checked_add(charge)
            .ok_or_else(|| anyhow!("normalized document symbol result charge overflow"))?;
        if self.total_bytes > MAX_NORMALIZED_RESULT_BYTES {
            bail!("normalized document symbol result exceeds 1 MiB");
        }
        self.budget.reserve(charge)
    }
}

struct TopSymbolArraySeed<'a, 'b> {
    build: &'a mut SymbolBuild<'b>,
}

impl<'de> DeserializeSeed<'de> for TopSymbolArraySeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(TopSymbolArrayVisitor { build: self.build })
    }
}

struct TopSymbolArrayVisitor<'a, 'b> {
    build: &'a mut SymbolBuild<'b>,
}

impl<'de> Visitor<'de> for TopSymbolArrayVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded DocumentSymbol or SymbolInformation array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(item) = sequence.next_element::<&'de serde_json::value::RawValue>()? {
            self.build.push_top(item).map_err(A::Error::custom)?;
        }
        Ok(())
    }
}

struct NestedSymbolArraySeed<'a, 'b> {
    build: &'a mut SymbolBuild<'b>,
    parent: usize,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for NestedSymbolArraySeed<'_, '_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(NestedSymbolArrayVisitor {
            build: self.build,
            parent: self.parent,
            depth: self.depth,
        })
    }
}

struct NestedSymbolArrayVisitor<'a, 'b> {
    build: &'a mut SymbolBuild<'b>,
    parent: usize,
    depth: usize,
}

impl<'de> Visitor<'de> for NestedSymbolArrayVisitor<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded nested DocumentSymbol children")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(item) = sequence.next_element::<&'de serde_json::value::RawValue>()? {
            self.build
                .push_nested(item, Some(self.parent), self.depth)
                .map_err(A::Error::custom)?;
        }
        Ok(())
    }
}

fn parse_document_symbol_result(
    raw: &str,
    budget: &PayloadBudget,
    workspace_root: &std::path::Path,
    document: &NavigationDocument,
) -> Result<Vec<ProtocolDocumentSymbol>> {
    if raw == "null" {
        return Ok(Vec::new());
    }
    let mut build = SymbolBuild {
        budget,
        workspace_root,
        document,
        output: Vec::new(),
        variant: None,
        string_bytes: 0,
        total_bytes: 0,
    };
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    TopSymbolArraySeed { build: &mut build }
        .deserialize(&mut deserializer)
        .context("invalid bounded document symbol result")?;
    deserializer
        .end()
        .context("trailing document symbol result")?;
    Ok(build.output)
}

fn normalize_symbol_kind(kind: lsp_types::SymbolKind) -> ProtocolSymbolKind {
    if kind == lsp_types::SymbolKind::FUNCTION {
        ProtocolSymbolKind::Function
    } else if matches!(
        kind,
        lsp_types::SymbolKind::METHOD | lsp_types::SymbolKind::CONSTRUCTOR
    ) {
        ProtocolSymbolKind::Method
    } else if matches!(
        kind,
        lsp_types::SymbolKind::CLASS
            | lsp_types::SymbolKind::ENUM
            | lsp_types::SymbolKind::INTERFACE
            | lsp_types::SymbolKind::STRUCT
            | lsp_types::SymbolKind::TYPE_PARAMETER
    ) {
        ProtocolSymbolKind::Type
    } else if matches!(
        kind,
        lsp_types::SymbolKind::MODULE
            | lsp_types::SymbolKind::NAMESPACE
            | lsp_types::SymbolKind::PACKAGE
    ) {
        ProtocolSymbolKind::Module
    } else {
        ProtocolSymbolKind::Other
    }
}

fn clean_protocol_message(message: &str) -> String {
    message
        .chars()
        .filter(|character| !character.is_control() && *character != '\u{1b}')
        .take(240)
        .collect()
}

fn bounded_raw_string_charge(
    raw: &serde_json::value::RawValue,
    max_decoded: usize,
) -> Result<usize> {
    let max_encoded = max_decoded
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("bounded JSON string length overflow"))?;
    if !raw_starts_with(raw, b'"') || raw.get().len() > max_encoded {
        bail!("bounded JSON string token is too large");
    }
    Ok(raw.get().len())
}

fn decode_bounded_owned_string(
    raw: &serde_json::value::RawValue,
    max_decoded: usize,
) -> Result<String> {
    bounded_raw_string_charge(raw, max_decoded)?;
    let value: String = serde_json::from_str(raw.get()).context("invalid JSON string")?;
    if value.len() > max_decoded {
        bail!("decoded JSON string is too large");
    }
    Ok(value)
}

fn trim_string_in_place(value: &mut String) {
    let start = value.len().saturating_sub(value.trim_start().len());
    if start > 0 {
        value.drain(..start);
    }
    value.truncate(value.trim_end().len());
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::{Arc, Condvar, Mutex},
        time::Instant,
    };

    use serde::Deserialize;

    use super::*;

    fn budget() -> PayloadBudget {
        PayloadBudget::session(&GlobalPayloadBudget::default())
    }

    fn source(root: &Path, name: &str, text: &str) -> Arc<NavigationSource> {
        let path = root.join(name);
        fs::write(&path, text).unwrap();
        let path = path.canonicalize().unwrap();
        let root = root.canonicalize().unwrap();
        let text: Arc<str> = Arc::from(text);
        Arc::new(NavigationSource {
            identity: crate::runtime::ContentIdentity::from_absolute(&root, &path).unwrap(),
            absolute_path: path,
            disk_raw_len: text.len() as u64,
            server_root: root,
            language: crate::navigation::language_for_path(Path::new(name)).unwrap(),
            line_index: Arc::new(crate::navigation::LineIndex::new(Arc::clone(&text)).unwrap()),
            structure: Arc::new(crate::folding::StructureSnapshot::unavailable()),
            text,
        })
    }

    fn request(
        generation: u64,
        operation: NavigationOperation,
        source: Arc<NavigationSource>,
    ) -> NavigationRuntimeRequest {
        NavigationRuntimeRequest {
            generation,
            operation,
            origin: SourcePosition { line: 0, byte: 0 },
            source,
            version: DocumentVersion(1),
        }
    }

    fn pending_user_request(
        generation: u64,
        operation: NavigationOperation,
        source: Arc<NavigationSource>,
        budget: &PayloadBudget,
    ) -> PendingUserRequest {
        let request = request(generation, operation, Arc::clone(&source));
        let document =
            Arc::new(NavigationDocument::from_source(&source, request.version, budget).unwrap());
        PendingUserRequest { request, document }
    }

    fn manager(root: &Path) -> NavigationManager {
        NavigationManager::new(
            root.canonicalize().unwrap(),
            NavigationSettings::disabled(),
            Arc::new(Mutex::new(VecDeque::new())),
            Arc::new(Mutex::new(CompletionPermitState::default())),
            Arc::new(Mutex::new(VecDeque::new())),
            Arc::new(Mutex::new(NavigationCleanupStats::default())),
        )
    }

    fn empty_session_channels() -> (
        Receiver<ChargedPayload>,
        Receiver<SessionControlEvent>,
        Receiver<IoThreadDone>,
    ) {
        let (frame_sender, frames) = mpsc::sync_channel(1);
        let (control_sender, controls) = mpsc::sync_channel(1);
        let (lifecycle_sender, lifecycle) = mpsc::sync_channel(1);
        drop((frame_sender, control_sender, lifecycle_sender));
        (frames, controls, lifecycle)
    }

    fn raw_value(json: &str) -> Box<serde_json::value::RawValue> {
        serde_json::from_str(json).unwrap()
    }

    fn starting_initialize_session(
        manager: &mut NavigationManager,
        root: &Path,
        budget: PayloadBudget,
    ) -> (SessionKey, Receiver<ChargedPayload>) {
        let key = SessionKey {
            server_root: root.to_path_buf(),
            family: LanguageFamily::Rust,
        };
        let (writer, queued_writes) = mpsc::sync_channel(SESSION_WRITER_CAPACITY);
        let (frames, controls, lifecycle) = empty_session_channels();
        let mut allocator = RequestIdAllocator::default();
        let initialize_id = allocator.allocate().unwrap();
        let mut pending = HashMap::new();
        pending.insert(
            initialize_id,
            PendingRequest::Initialize {
                deadline: Instant::now() + INITIALIZE_TIMEOUT,
            },
        );
        manager.sessions.insert(
            key.clone(),
            LspSession {
                key: key.clone(),
                epoch: SessionEpoch(1),
                state: ServerState::Starting {
                    since: Instant::now(),
                },
                budget,
                allocator,
                retired: RetiredIds::default(),
                pending,
                deferred: None,
                opened: None,
                writer: Some(writer),
                frames,
                controls,
                lifecycle,
                io_done: HashSet::new(),
                io_joined: HashSet::new(),
                tree: None,
                io_threads: Vec::new(),
                cleanup_stats: Arc::clone(&manager.cleanup_stats),
                ready_since: None,
            },
        );
        (key, queued_writes)
    }

    fn install_ready_session(
        manager: &mut NavigationManager,
        root: &Path,
        capabilities: NavigationCapabilities,
    ) -> (SessionKey, Receiver<ChargedPayload>, PayloadBudget) {
        let budget = PayloadBudget::session(&manager.global_budget);
        let (key, writes) = starting_initialize_session(manager, root, budget.clone());
        let session = manager.sessions.get_mut(&key).unwrap();
        session.pending.clear();
        session.state = ServerState::Ready { capabilities };
        session.ready_since = Some(Instant::now());
        (key, writes, budget)
    }

    fn navigation_capabilities(text_document_sync: TextSyncCapability) -> NavigationCapabilities {
        NavigationCapabilities {
            definition: true,
            references: true,
            implementations: true,
            document_symbols: true,
            text_document_sync,
        }
    }

    fn charged_frame(body: &str, budget: &PayloadBudget) -> ChargedPayload {
        ChargedPayload {
            bytes: body.as_bytes().to_vec(),
            _permit: budget.reserve(body.len()).unwrap(),
        }
    }

    fn decode_framed_response(body: &[u8], budget: &PayloadBudget) -> ChargedPayload {
        let mut decoder = FrameDecoder::default();
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        assert!(decoder.push(header.as_bytes(), budget).unwrap().is_empty());
        let mut frames = Vec::new();
        for chunk in body.chunks(64 * 1024) {
            frames.extend(decoder.push(chunk, budget).unwrap());
        }
        decoder.finish().unwrap();
        assert_eq!(frames.len(), 1);
        frames.pop().unwrap()
    }

    fn near_limit_initialize_response() -> String {
        let escaped = r"\u0061".repeat(650_000);
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":0,"result":{{"capabilities":{{"positionEncoding":"utf-16","definitionProvider":{{}},"referencesProvider":true,"implementationProvider":true,"documentSymbolProvider":true,"textDocumentSync":{{"openClose":true,"change":2}},"experimental":{{"escaped":"{escaped}"}}}}}}}}"#,
        );
        assert!(body.len() <= MAX_BODY_BYTES);
        assert!(body.len() > MAX_BODY_BYTES - 512 * 1024);
        body
    }

    #[test]
    fn frame_backpressure_reports_terminal_reader_failure_on_independent_control_channel() {
        let budget = budget();
        let (frame_sender, frames) = mpsc::sync_channel(SESSION_FRAME_CAPACITY);
        let (control_sender, controls) = mpsc::sync_channel(SESSION_CONTROL_CAPACITY);
        for index in 0..SESSION_FRAME_CAPACITY {
            let bytes = vec![b'0' + u8::try_from(index).unwrap()];
            let permit = budget.reserve(bytes.len()).unwrap();
            assert!(enqueue_inbound_frame(
                &frame_sender,
                &control_sender,
                ChargedPayload {
                    bytes,
                    _permit: permit,
                },
            ));
        }
        let permit = budget.reserve(1).unwrap();
        assert!(!enqueue_inbound_frame(
            &frame_sender,
            &control_sender,
            ChargedPayload {
                bytes: vec![b'x'],
                _permit: permit,
            },
        ));
        assert!(matches!(
            controls.try_recv(),
            Ok(SessionControlEvent::ReaderFailed(message)) if message.contains("frame channel is full")
        ));
        assert_eq!(frames.try_iter().count(), SESSION_FRAME_CAPACITY);
    }

    #[test]
    fn lifecycle_accepts_each_kind_once_and_rejects_duplicates_and_old_epochs() {
        let epoch = SessionEpoch(7);
        let mut completed = HashSet::new();
        for kind in [
            IoThreadKind::Stdin,
            IoThreadKind::Stdout,
            IoThreadKind::Stderr,
        ] {
            record_io_done(
                epoch,
                &mut completed,
                IoThreadDone {
                    kind,
                    session_epoch: 7,
                },
            )
            .unwrap();
        }
        assert_eq!(completed.len(), IO_THREAD_COUNT);
        assert!(
            record_io_done(
                epoch,
                &mut completed,
                IoThreadDone {
                    kind: IoThreadKind::Stdout,
                    session_epoch: 7,
                },
            )
            .is_err()
        );
        assert!(
            record_io_done(
                epoch,
                &mut HashSet::new(),
                IoThreadDone {
                    kind: IoThreadKind::Stdout,
                    session_epoch: 6,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn cleanup_quarantines_a_stuck_io_owner_without_fake_stats_or_blind_join() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let epoch = SessionEpoch(9);
        let budget = budget();
        let (frame_sender, frames) = mpsc::sync_channel(1);
        let (control_sender, controls) = mpsc::sync_channel(1);
        let (lifecycle_sender, lifecycle) = mpsc::sync_channel(3);
        drop((frame_sender, control_sender));
        let (release_sender, release) = mpsc::sync_channel::<()>(1);
        let mut io_threads = Vec::new();
        for kind in [IoThreadKind::Stdin, IoThreadKind::Stderr] {
            let done = lifecycle_sender.clone();
            io_threads.push((
                kind,
                thread::spawn(move || {
                    done.send(IoThreadDone {
                        kind,
                        session_epoch: 9,
                    })
                    .unwrap();
                }),
            ));
        }
        let done = lifecycle_sender;
        io_threads.push((
            IoThreadKind::Stdout,
            thread::spawn(move || {
                release.recv().unwrap();
                done.send(IoThreadDone {
                    kind: IoThreadKind::Stdout,
                    session_epoch: 9,
                })
                .unwrap();
            }),
        ));
        let stats = Arc::new(Mutex::new(NavigationCleanupStats::default()));
        let mut session = LspSession {
            key: SessionKey {
                server_root: root,
                family: LanguageFamily::Rust,
            },
            epoch,
            state: ServerState::StoppingForced,
            budget,
            allocator: RequestIdAllocator::default(),
            retired: RetiredIds::default(),
            pending: HashMap::new(),
            deferred: None,
            opened: None,
            writer: None,
            frames,
            controls,
            lifecycle,
            io_done: HashSet::new(),
            io_joined: HashSet::new(),
            tree: None,
            io_threads,
            cleanup_stats: Arc::clone(&stats),
            ready_since: None,
        };
        let started = Instant::now();
        assert_eq!(session.cleanup(), CleanupOutcome::Quarantined);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(session.io_threads.len(), 1);
        assert!(matches!(session.state, ServerState::Failed { .. }));
        assert_eq!(
            stats.lock().unwrap().snapshot,
            NavigationCleanupSnapshot::default()
        );

        release_sender.send(()).unwrap();
        assert_eq!(session.cleanup(), CleanupOutcome::Complete);
        let snapshot = stats.lock().unwrap().snapshot;
        assert_eq!(snapshot.sessions_cleaned, 1);
        assert_eq!(snapshot.io_threads_joined, IO_THREAD_COUNT);
        assert_eq!(snapshot.process_owners_dropped, 1);
    }

    #[test]
    fn batch_cleanup_drives_ready_sessions_through_shared_nonblocking_phases() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let stats = Arc::new(Mutex::new(NavigationCleanupStats::default()));
        let writes_seen = Arc::new(AtomicUsize::new(0));
        let mut sessions = Vec::new();
        for index in 0..2u64 {
            let epoch = SessionEpoch(index + 1);
            let budget = budget();
            let (writer, queued_writes) = mpsc::sync_channel(SESSION_WRITER_CAPACITY);
            let (frame_sender, frames) = mpsc::sync_channel(SESSION_FRAME_CAPACITY);
            let (control_sender, controls) = mpsc::sync_channel(SESSION_CONTROL_CAPACITY);
            let (lifecycle_sender, lifecycle) = mpsc::sync_channel(SESSION_LIFECYCLE_CAPACITY);
            drop(control_sender);
            frame_sender
                .send(charged_frame(
                    r#"{"jsonrpc":"2.0","id":0,"result":null}"#,
                    &budget,
                ))
                .unwrap();
            drop(frame_sender);
            let mut io_threads = Vec::new();
            let writer_done = lifecycle_sender.clone();
            let writer_count = Arc::clone(&writes_seen);
            io_threads.push((
                IoThreadKind::Stdin,
                thread::spawn(move || {
                    for _ in queued_writes {
                        writer_count.fetch_add(1, Ordering::SeqCst);
                    }
                    writer_done
                        .send(IoThreadDone {
                            kind: IoThreadKind::Stdin,
                            session_epoch: epoch.0,
                        })
                        .unwrap();
                }),
            ));
            for kind in [IoThreadKind::Stdout, IoThreadKind::Stderr] {
                let done = lifecycle_sender.clone();
                io_threads.push((
                    kind,
                    thread::spawn(move || {
                        done.send(IoThreadDone {
                            kind,
                            session_epoch: epoch.0,
                        })
                        .unwrap();
                    }),
                ));
            }
            drop(lifecycle_sender);
            sessions.push(LspSession {
                key: SessionKey {
                    server_root: root.join(format!("root-{index}")),
                    family: LanguageFamily::Rust,
                },
                epoch,
                state: ServerState::Ready {
                    capabilities: navigation_capabilities(TextSyncCapability {
                        open_close: true,
                        change: TextChangeCapability::None,
                    }),
                },
                budget,
                allocator: RequestIdAllocator::default(),
                retired: RetiredIds::default(),
                pending: HashMap::new(),
                deferred: None,
                opened: None,
                writer: Some(writer),
                frames,
                controls,
                lifecycle,
                io_done: HashSet::new(),
                io_joined: HashSet::new(),
                tree: None,
                io_threads,
                cleanup_stats: Arc::clone(&stats),
                ready_since: Some(Instant::now()),
            });
        }

        let outcomes = cleanup_sessions(&mut sessions);
        assert_eq!(outcomes, vec![CleanupOutcome::Complete; 2]);
        assert_eq!(writes_seen.load(Ordering::SeqCst), 4);
        let snapshot = stats.lock().unwrap().snapshot;
        assert_eq!(snapshot.sessions_cleaned, 2);
        assert_eq!(snapshot.io_threads_joined, 6);
        assert_eq!(snapshot.process_owners_dropped, 2);
        assert_eq!(snapshot.quarantined_process_owners, 0);
    }

    #[cfg(unix)]
    #[test]
    fn manager_batches_twelve_simultaneous_io_failures_with_concurrent_shutdown() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};

        let _environment = crate::navigation::lock_navigation_environment();
        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        let tools = container.path().join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let trace = container.path().join("failure-batch.trace");
        assert!(
            !trace.to_string_lossy().contains('\''),
            "test shell fixture requires a simple trace path"
        );

        let script = tools.join("stalled-failure-server");
        let body = r#"#!/bin/sh
trace='__TRACE__'
trap '' TERM
sh -c 'trap "" TERM; printf "descendant=%s\n" "$$" >> "__TRACE__"; exec sleep 30' &
printf 'direct=%s\n' "$$" >> "$trace"

read_message() {
    length=
    while IFS= read -r line; do
        line=$(printf '%s' "$line" | tr -d '\r')
        if [ -z "$line" ]; then
            break
        fi
        case "$line" in
            Content-Length:*) length=$(printf '%s' "$line" | tr -cd '0-9') ;;
        esac
    done
    [ -n "$length" ] || return 1
    dd bs=1 count="$length" 2>/dev/null
}

request_id() {
    printf '%s' "$1" | sed -n 's/.*"id":\([-0-9][0-9]*\).*/\1/p'
}

while message=$(read_message); do
    case "$message" in
        *'"method":"initialize"'*)
            id=$(request_id "$message")
            response='{"jsonrpc":"2.0","id":'"$id"',"result":{"capabilities":{"positionEncoding":"utf-16","definitionProvider":true,"textDocumentSync":{"openClose":true,"change":0}}}}'
            printf 'Content-Length: %s\r\n\r\n%s' "${#response}" "$response"
            ;;
        *) ;;
    esac
done
exec sleep 30
"#
        .replace("__TRACE__", trace.to_str().unwrap());
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let script = script.canonicalize().unwrap();
        let validated = crate::navigation::validate_executable(&script, &workspace).unwrap();
        let server = crate::navigation::TrustedServer {
            program: validated.path,
            args: Arc::from([]),
            identity: validated.identity,
        };

        let mut actor = manager(&workspace);
        let mut keys = Vec::new();
        let mut sources = Vec::new();
        for index in 0..12u64 {
            let root = workspace.join(format!("repo-{index:02}"));
            fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let source = source(&root, "caller.rs", "caller!();\n");
            let key = SessionKey {
                server_root: root,
                family: LanguageFamily::Rust,
            };
            let session = actor.spawn_session(key.clone(), &server).unwrap();
            actor.sessions.insert(key.clone(), session);
            keys.push(key);
            sources.push(source);
        }

        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while actor
            .sessions
            .values()
            .any(|session| !matches!(session.state, ServerState::Ready { .. }))
        {
            actor.poll_sessions();
            assert!(actor.failed_sessions.is_empty());
            assert!(
                Instant::now() < ready_deadline,
                "twelve real sessions did not initialize before the deadline"
            );
            thread::yield_now();
        }

        let read_trace_pids = |prefix: &str| -> Vec<u32> {
            fs::read_to_string(&trace)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| line.strip_prefix(prefix)?.parse().ok())
                .collect()
        };
        let pid_deadline = Instant::now() + Duration::from_secs(2);
        let (direct_pids, descendant_pids) = loop {
            let direct = read_trace_pids("direct=");
            let descendants = read_trace_pids("descendant=");
            if direct.len() == 12 && descendants.len() == 12 {
                break (direct, descendants);
            }
            assert!(
                Instant::now() < pid_deadline,
                "missing process-tree trace markers: direct={} descendant={}",
                direct.len(),
                descendants.len()
            );
            thread::yield_now();
        };
        let process_is_alive = |pid: u32| {
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        };
        assert!(
            direct_pids
                .iter()
                .chain(&descendant_pids)
                .all(|pid| process_is_alive(*pid))
        );

        for (index, (key, source)) in keys.iter().zip(&sources).enumerate() {
            let budget = actor.sessions[key].budget.clone();
            let pending = pending_user_request(
                10_000 + index as u64,
                NavigationOperation::Definition,
                Arc::clone(source),
                &budget,
            );
            match index % 3 {
                0 => {
                    actor.sessions.get_mut(key).unwrap().pending.insert(
                        RpcId::Signed(100 + index as i32),
                        PendingRequest::Navigation {
                            request: pending.request,
                            document: pending.document,
                            deadline: Instant::now() + Duration::from_secs(30),
                        },
                    );
                }
                1 => actor.sessions.get_mut(key).unwrap().deferred = Some(pending),
                _ => {
                    actor
                        .disk_waiting
                        .insert(pending.request.generation, (key.clone(), pending));
                }
            }
            let (failure_sender, failure_receiver) = mpsc::sync_channel(1);
            failure_sender
                .send(SessionControlEvent::ReaderFailed(format!(
                    "simultaneous reader failure {index}"
                )))
                .unwrap();
            actor.sessions.get_mut(key).unwrap().controls = failure_receiver;
        }

        let (command_sender, commands) = mpsc::sync_channel(1);
        command_sender.send(NavigationCommand::Shutdown).unwrap();
        drop(command_sender);
        let runtime_shutdown = AtomicBool::new(false);
        let started = Instant::now();
        actor.run(commands, &runtime_shutdown);
        let elapsed = started.elapsed();
        eprintln!("failure-batch-manager-elapsed={elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(6),
            "12 simultaneous manager failures took {elapsed:?}; serial cleanup exceeds 40s"
        );

        let pid_exit_deadline = Instant::now() + Duration::from_secs(2);
        while direct_pids
            .iter()
            .chain(&descendant_pids)
            .any(|pid| process_is_alive(*pid))
        {
            assert!(
                Instant::now() < pid_exit_deadline,
                "a process-tree member survived the batched failure cleanup"
            );
            thread::yield_now();
        }

        assert!(actor.sessions.is_empty());
        assert!(actor.failed_sessions.is_empty());
        assert!(actor.disk_waiting.is_empty());
        assert!(actor.quarantined.is_empty());
        assert!(actor.quarantined_spawns.is_empty());
        assert!(
            actor.permanent_failures.is_empty(),
            "unexpected permanent failures: {:?}",
            actor.permanent_failures
        );
        assert_eq!(actor.backoffs.len(), 12);
        assert!(actor.backoffs.values().all(|backoff| backoff.attempt == 1));

        let completions = actor.completions.lock().unwrap();
        assert_eq!(completions.len(), 12);
        let generations: HashSet<_> = completions
            .iter()
            .map(|completion| completion.generation)
            .collect();
        assert_eq!(generations.len(), 12);
        assert!(completions.iter().all(|completion| matches!(
            &completion.result,
            NavigationProtocolResult::Failed(message)
                if message.contains("simultaneous reader failure")
        )));
        drop(completions);

        assert_eq!(
            actor.cleanup_stats.lock().unwrap().snapshot,
            NavigationCleanupSnapshot {
                sessions_cleaned: 12,
                clean_exits: 0,
                forced_tree_cleanups: 12,
                direct_children_reaped: 12,
                io_threads_joined: 36,
                process_owners_dropped: 12,
                quarantined_process_owners: 0,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn retained_pre_session_owner_is_keyed_counted_and_disables_navigation() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let tools = directory.path().join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let script = tools.join("server");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let script = script.canonicalize().unwrap();
        let validated = crate::navigation::validate_executable(&script, &workspace).unwrap();
        let server = crate::navigation::TrustedServer {
            program: validated.path,
            args: Arc::from([]),
            identity: validated.identity,
        };
        let spawned = spawn_language_server(&server, &workspace, &workspace).unwrap();
        drop(spawned.io);
        let key = SessionKey {
            server_root: workspace.clone(),
            family: LanguageFamily::Rust,
        };
        let mut actor = manager(&workspace);
        actor.retain_quarantined_spawn(key.clone(), spawned.tree);
        assert!(actor.has_quarantined_owners());
        assert_eq!(actor.quarantined_owner_count(), 1);
        assert!(actor.quarantined_spawns.contains_key(&key));
        assert_eq!(
            actor
                .cleanup_stats
                .lock()
                .unwrap()
                .snapshot
                .quarantined_process_owners,
            1
        );

        let mut tree = actor.quarantined_spawns.remove(&key).unwrap();
        tree.force_cleanup().unwrap();
    }

    #[test]
    fn consecutive_failure_backoff_is_persistent_and_fifth_failure_is_permanent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut actor = manager(&root);
        let key = SessionKey {
            server_root: root,
            family: LanguageFamily::Rust,
        };
        let base = Instant::now();
        for (attempt, seconds) in [1, 2, 4, 8, 30].into_iter().enumerate() {
            actor.record_failure_at(&key, "crash", base);
            let backoff = actor.backoffs.get(&key).unwrap();
            assert_eq!(usize::from(backoff.attempt), attempt + 1);
            assert_eq!(
                backoff.retry_at.duration_since(base),
                Duration::from_secs(seconds)
            );
        }
        assert!(actor.permanent_failures.contains_key(&key));

        let mut actor = manager(&key.server_root);
        actor.record_failure_at(&key, "crash", base);
        actor.record_failure_at(&key, "crash", base);
        actor.reset_backoff_if_stable_at(
            &key,
            Some(base),
            base + STABLE_READY_RESET - Duration::from_millis(1),
        );
        assert_eq!(actor.backoffs[&key].attempt, 2);
        actor.reset_backoff_if_stable_at(&key, Some(base), base + STABLE_READY_RESET);
        assert!(!actor.backoffs.contains_key(&key));
        actor.record_failure_at(&key, "new crash", base);
        assert_eq!(actor.backoffs[&key].attempt, 1);
    }

    #[test]
    fn server_configuration_preserves_item_count_and_invalid_params_are_nonfatal() {
        let directory = tempfile::tempdir().unwrap();
        let key = SessionKey {
            server_root: directory.path().canonicalize().unwrap(),
            family: LanguageFamily::Rust,
        };
        for (json, expected) in [
            (r#"{"items":[]}"#, 0usize),
            (r#"{"items":[{"section":"rust"}]}"#, 1),
            (
                r#"{"items":[{"scopeUri":"file:///tmp/a","section":"one"},{"section":"two"},{"section":"three"}]}"#,
                3,
            ),
        ] {
            let params = raw_value(json);
            let result = server_call_result("workspace/configuration", Some(&params), &key)
                .expect("valid bounded configuration request");
            assert_eq!(result.as_array().unwrap().len(), expected);
            assert!(
                result
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(serde_json::Value::is_null)
            );
        }
        let too_many = format!(
            r#"{{"items":[{}]}}"#,
            std::iter::repeat_n(r#"{"section":"x"}"#, MAX_CONFIGURATION_ITEMS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        let too_many = raw_value(&too_many);
        assert_eq!(
            server_call_result("workspace/configuration", Some(&too_many), &key),
            Err(ServerCallError::InvalidParams)
        );
        let invalid = raw_value(r#"{"items":"not-an-array"}"#);
        assert_eq!(
            server_call_result("workspace/configuration", Some(&invalid), &key),
            Err(ServerCallError::InvalidParams)
        );
        let folders_invalid = raw_value(r#"{}"#);
        assert_eq!(
            server_call_result("workspace/workspaceFolders", Some(&folders_invalid), &key),
            Err(ServerCallError::InvalidParams)
        );

        let folders = server_call_result("workspace/workspaceFolders", None, &key).unwrap();
        assert_eq!(folders[0]["name"], "workspace");
        for (method, json) in [
            (
                "workspace/applyEdit",
                r#"{"label":"preview edit","edit":{"changes":{}}}"#,
            ),
            (
                "client/registerCapability",
                r#"{"registrations":[{"id":"one","method":"workspace/didChangeConfiguration"}]}"#,
            ),
            (
                "client/unregisterCapability",
                r#"{"unregisterations":[{"id":"one","method":"workspace/didChangeConfiguration"}]}"#,
            ),
            ("window/workDoneProgress/create", r#"{"token":"progress"}"#),
            (
                "window/showMessageRequest",
                r#"{"type":2,"message":"choose","actions":[{"title":"dismiss"}]}"#,
            ),
        ] {
            let params = raw_value(json);
            let result = server_call_result(method, Some(&params), &key).unwrap();
            if method == "workspace/applyEdit" {
                assert_eq!(result["applied"], false);
            } else {
                assert!(result.is_null());
            }
        }
        for (method, json) in [
            ("workspace/applyEdit", r#"{"edit":[]}"#),
            (
                "client/registerCapability",
                r#"{"registrations":[{"id":{},"method":"x"}]}"#,
            ),
            (
                "client/unregisterCapability",
                r#"{"unregisterations":"bad"}"#,
            ),
            ("window/workDoneProgress/create", r#"{"token":{}}"#),
            ("window/showMessageRequest", r#"{"type":9,"message":"bad"}"#),
        ] {
            let params = raw_value(json);
            assert_eq!(
                server_call_result(method, Some(&params), &key),
                Err(ServerCallError::InvalidParams),
                "{method}"
            );
        }
        let oversized = raw_value(&format!(
            r#"{{"items":[],"padding":"{}"}}"#,
            "x".repeat(65_536)
        ));
        assert_eq!(
            server_call_result("workspace/configuration", Some(&oversized), &key),
            Err(ServerCallError::InvalidParams)
        );
        assert_eq!(
            server_call_result("latte/unknown", None, &key),
            Err(ServerCallError::MethodNotFound)
        );
    }

    #[test]
    fn escaped_uri_symbol_and_error_strings_are_capped_before_owned_decode() {
        let budget = budget();
        let escaped_uri = format!("file:///tmp/{}", r"\u0061".repeat(9_000));
        let location = format!(
            r#"[{{"uri":"{escaped_uri}","range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}}}}]"#
        );
        let locations = parse_bounded_locations(&location, &budget, false).unwrap();
        assert!(budget.session.used() > 0);
        drop(locations);
        assert_eq!(budget.session.used(), 0);

        let huge_escape = r"\u0061".repeat(700_000);
        let huge_location = format!(
            r#"[{{"uri":"file:///tmp/{huge_escape}","range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}}}}]"#
        );
        assert!(parse_bounded_locations(&huge_location, &budget, false).is_err());
        assert_eq!(budget.session.used(), 0);

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let document =
            NavigationDocument::from_source(&source, DocumentVersion(1), &budget).unwrap();
        let baseline = budget.session.used();
        let huge_symbol = format!(
            r#"[{{"name":"{huge_escape}","kind":12,"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":12}}}},"selectionRange":{{"start":{{"line":0,"character":3}},"end":{{"line":0,"character":7}}}}}}]"#
        );
        assert!(parse_document_symbol_result(&huge_symbol, &budget, &root, &document).is_err());
        assert_eq!(budget.session.used(), baseline);

        let small_error = raw_value(r#"{"message":"f\u0061iled"}"#);
        let (message, permit) = parse_protocol_error(&small_error, &budget).unwrap();
        assert_eq!(message, "failed");
        assert!(budget.session.used() > baseline);
        drop(permit);
        assert_eq!(budget.session.used(), baseline);
        let huge_error = raw_value(&format!(r#"{{"message":"{huge_escape}"}}"#));
        assert!(parse_protocol_error(&huge_error, &budget).is_err());
        assert_eq!(budget.session.used(), baseline);
    }

    #[test]
    fn protocol_error_completion_permit_is_leased_until_the_next_app_drain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let request = request(41, NavigationOperation::Definition, source);
        let actor = manager(&root);
        let budget = PayloadBudget::session(&actor.global_budget);
        let error = raw_value(r#"{"message":"escaped \u0065rror"}"#);
        let (message, permit) = parse_protocol_error(&error, &budget).unwrap();
        actor.complete_request_with_permit(
            &request,
            NavigationProtocolResult::Failed(message),
            Some(permit),
        );
        assert!(budget.session.used() > 0);

        let (commands, _receiver) = mpsc::sync_channel(1);
        let runtime = NavigationRuntime {
            commands,
            completions: Arc::clone(&actor.completions),
            completion_permits: Arc::clone(&actor.completion_permits),
            symbol_completions: Arc::clone(&actor.symbol_completions),
            shutdown: Arc::new(AtomicBool::new(false)),
            cleanup_stats: Arc::clone(&actor.cleanup_stats),
            manager: None,
        };
        let completions = runtime.take_completions();
        assert_eq!(completions.len(), 1);
        assert!(budget.session.used() > 0);
        drop(completions);
        assert!(runtime.take_completions().is_empty());
        assert_eq!(budget.session.used(), 0);
    }

    #[test]
    fn synthetic_sessions_reject_unsupported_bad_positions_and_writer_backpressure() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let capabilities = |definition| NavigationCapabilities {
            definition,
            references: false,
            implementations: false,
            document_symbols: false,
            text_document_sync: TextSyncCapability {
                open_close: false,
                change: TextChangeCapability::Full,
            },
        };

        let mut actor = manager(&root);
        let (key, _writes, budget) = install_ready_session(&mut actor, &root, capabilities(false));
        let document = pending_user_request(
            1,
            NavigationOperation::Definition,
            Arc::clone(&source),
            &budget,
        );
        actor.dispatch_navigation(&key, document);
        assert!(matches!(
            actor.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Unavailable(_)
        ));
        assert!(actor.sessions.contains_key(&key));

        let mut actor = manager(&root);
        let (key, _writes, budget) = install_ready_session(&mut actor, &root, capabilities(true));
        let mut bad_position = pending_user_request(
            2,
            NavigationOperation::Definition,
            Arc::clone(&source),
            &budget,
        );
        bad_position.request.origin.byte = usize::MAX;
        actor.dispatch_navigation(&key, bad_position);
        assert!(matches!(
            actor.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("outside the line")
        ));
        assert!(actor.sessions.contains_key(&key));

        let mut actor = manager(&root);
        let (key, _writes, budget) = install_ready_session(&mut actor, &root, capabilities(true));
        let session = actor.sessions.get_mut(&key).unwrap();
        for value in 0..SESSION_WRITER_CAPACITY {
            session
                .send_json(&serde_json::json!({"fill": value}))
                .unwrap();
        }
        assert!(
            session
                .send_json(&serde_json::json!({"full": true}))
                .is_err()
        );
        let pending = pending_user_request(3, NavigationOperation::Definition, source, &budget);
        actor.dispatch_navigation(&key, pending);
        actor.cleanup_failed_sessions();
        assert!(!actor.sessions.contains_key(&key));
        assert!(actor.backoffs.contains_key(&key));
        assert!(matches!(
            actor.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("queue is full")
        ));
    }

    #[test]
    fn bounded_completion_queues_evict_the_oldest_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "README.md", "# title\n");
        let actor = manager(&root);
        for generation in 0..=NAVIGATION_COMPLETION_CAPACITY as u64 {
            let request = request(
                generation,
                NavigationOperation::DocumentSymbols,
                Arc::clone(&source),
            );
            actor.complete_request(&request, NavigationProtocolResult::Cancelled);
            actor.complete_document_symbols(&request, Vec::new());
        }
        let completions = actor.completions.lock().unwrap();
        let symbols = actor.symbol_completions.lock().unwrap();
        assert_eq!(completions.len(), NAVIGATION_COMPLETION_CAPACITY);
        assert_eq!(symbols.len(), NAVIGATION_COMPLETION_CAPACITY);
        assert_eq!(completions.front().unwrap().generation, 1);
        assert_eq!(symbols.front().unwrap().generation, 1);
        assert_eq!(
            actor.completion_permits.lock().unwrap().queued.len(),
            completions.len()
        );
    }

    #[test]
    fn framing_accepts_split_and_multiple_frames() {
        let budget = budget();
        let mut decoder = FrameDecoder::default();
        assert!(decoder.push(b"Content-Len", &budget).unwrap().is_empty());
        let frames = decoder
            .push(
                b"gth: 2\r\nContent-Type: application/vscode-jsonrpc; charset=UTF-8\r\n\r\n{}Content-Length: 2\r\n\r\n[]",
                &budget,
            )
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].as_slice(), b"{}");
        assert_eq!(frames[1].as_slice(), b"[]");
        decoder.finish().unwrap();
    }

    #[test]
    fn framing_rejects_duplicate_length_bad_charset_and_limits() {
        let budget = budget();
        for header in [
            "X: 1\r\n\r\n{}",
            "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            "Content-Length: -1\r\n\r\n{}",
            "Content-Length: 0\r\n\r\n",
            "Content-Length: 4194305\r\n\r\n",
            "Content-Length: 2\r\nContent-Type: text/plain\r\n\r\n{}",
            "Content-Length: 2\r\nContent-Type: application/vscode-jsonrpc; boundary=x\r\n\r\n{}",
            "Content-Length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=latin1\r\n\r\n{}",
            "Content-Length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf8; charset=utf8\r\n\r\n{}",
        ] {
            assert!(
                FrameDecoder::default()
                    .push(header.as_bytes(), &budget)
                    .is_err()
            );
        }
        let huge = format!("X: {}", "x".repeat(MAX_HEADER_BYTES));
        assert!(
            FrameDecoder::default()
                .push(huge.as_bytes(), &budget)
                .is_err()
        );
        assert!(
            FrameDecoder::default()
                .push(&[0xff, b':', b'1', b'\r', b'\n', b'\r', b'\n'], &budget)
                .is_err()
        );
        let mut partial = FrameDecoder::default();
        partial
            .push(b"Content-Length: 2\r\n\r\n{", &budget)
            .unwrap();
        assert!(partial.finish().is_err());
        assert!(encode_frame(b"", &budget).is_err());
        assert!(encode_frame(&vec![b'x'; MAX_BODY_BYTES + 1], &budget).is_err());
    }

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    struct Message {
        value: String,
    }

    #[test]
    fn preflight_and_parse_hold_a_body_sized_scratch_permit() {
        let budget = budget();
        let body = br#"{"value":"escaped\nvalue"}"#;
        let permit = budget.reserve(body.len()).unwrap();
        let payload = ChargedPayload {
            bytes: body.to_vec(),
            _permit: permit,
        };
        let before = budget.session.used();
        let parsed: Message = parse_bounded(&payload, &budget).unwrap();
        assert_eq!(parsed.value, "escaped\nvalue");
        assert_eq!(budget.session.used(), before);
        drop(payload);
        assert_eq!(budget.session.used(), 0);
    }

    #[test]
    fn preflight_rejects_long_numbers_and_depth() {
        let number = format!("{{\"n\":{}}}", "1".repeat(MAX_JSON_NUMBER_BYTES + 1));
        assert!(json_preflight(number.as_bytes()).is_err());
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        );
        assert!(json_preflight(nested.as_bytes()).is_err());
        assert!(json_preflight(br#"{"text":"123456789012345678901234567890"}"#).is_ok());
    }

    #[test]
    fn payload_admission_is_atomic_and_releases() {
        let global = GlobalPayloadBudget::default();
        let budget = PayloadBudget::session(&global);
        let permit = budget.reserve(SESSION_PAYLOAD_BUDGET).unwrap();
        assert!(budget.reserve(1).is_err());
        assert_eq!(budget.session.used(), SESSION_PAYLOAD_BUDGET);
        drop(permit);
        assert_eq!(budget.session.used(), 0);
        budget.reserve(1).unwrap();
    }

    #[test]
    fn global_admission_rejects_the_seventh_full_session_and_recovers() {
        let global = GlobalPayloadBudget::default();
        let budgets: Vec<_> = (0..7).map(|_| PayloadBudget::session(&global)).collect();
        let permits: Vec<_> = budgets[..6]
            .iter()
            .map(|budget| budget.reserve(SESSION_PAYLOAD_BUDGET).unwrap())
            .collect();
        assert!(budgets[6].reserve(1).is_err());
        drop(permits);
        assert!(budgets[6].reserve(1).is_ok());
    }

    #[test]
    fn framing_holds_a_permit_while_building_a_near_four_mib_body() {
        let budget = budget();
        let body_len = MAX_BODY_BYTES - 1;
        let header = format!("Content-Length: {body_len}\r\n\r\n");
        let mut decoder = FrameDecoder::default();
        assert!(decoder.push(header.as_bytes(), &budget).unwrap().is_empty());
        assert_eq!(budget.session.used(), body_len);
        let chunk = vec![b' '; 64 * 1024];
        let mut remaining = body_len;
        let mut completed = Vec::new();
        while remaining > 0 {
            let take = remaining.min(chunk.len());
            completed.extend(decoder.push(&chunk[..take], &budget).unwrap());
            remaining -= take;
        }
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].len(), body_len);
        assert_eq!(budget.session.used(), body_len);
        drop(completed);
        assert_eq!(budget.session.used(), 0);
    }

    #[test]
    fn rpc_ids_preserve_signed_and_string_and_allocator_never_wraps() {
        assert_eq!(
            serde_json::from_str::<RpcId>("-2").unwrap(),
            RpcId::Signed(-2)
        );
        assert_eq!(
            serde_json::from_str::<RpcId>(r#""server""#).unwrap(),
            RpcId::String("server".into())
        );
        assert!(serde_json::from_str::<RpcId>("1.5").is_err());
        assert!(serde_json::from_str::<RpcId>("2147483648").is_err());

        let mut allocator = RequestIdAllocator {
            next: Some(i32::MAX),
        };
        assert_eq!(allocator.allocate().unwrap(), RpcId::Signed(i32::MAX));
        assert!(allocator.allocate().is_err());
        assert_eq!(serde_json::to_string(&RpcId::Signed(-7)).unwrap(), "-7");
        assert_eq!(
            serde_json::to_string(&RpcId::String("server".into())).unwrap(),
            r#""server""#
        );
    }

    #[test]
    fn shutdown_response_distinguishes_present_null_result_from_missing_result() {
        let budget = budget();
        let shutdown_id = RpcId::Signed(7);
        for (body, expected) in [
            (
                br#"{"jsonrpc":"2.0","id":7,"result":null}"#.as_slice(),
                true,
            ),
            (br#"{"jsonrpc":"2.0","id":7}"#.as_slice(), false),
            (
                br#"{"jsonrpc":"2.0","id":7,"error":{"message":"failed"}}"#.as_slice(),
                false,
            ),
        ] {
            let permit = budget.reserve(body.len()).unwrap();
            let frame = ChargedPayload {
                bytes: body.to_vec(),
                _permit: permit,
            };
            assert_eq!(
                is_shutdown_response(&frame, &budget, &shutdown_id),
                expected
            );
        }
    }

    #[test]
    fn initialize_requires_utf16_and_accepts_the_lsp_default() {
        for raw in [
            r#"{"capabilities":{}}"#,
            r#"{"capabilities":{"positionEncoding":"utf-16"}}"#,
        ] {
            assert!(parse_initialize_capabilities(raw).is_ok());
        }
        for encoding in ["utf-8", "utf-32", "future-encoding"] {
            let raw = format!(r#"{{"capabilities":{{"positionEncoding":"{encoding}"}}}}"#);
            let error = parse_initialize_capabilities(&raw).unwrap_err().to_string();
            assert!(error.contains("requires UTF-16"), "{error}");
        }
        assert!(parse_initialize_capabilities(r#"{"capabilities":[]}"#).is_err());
        assert!(
            parse_initialize_capabilities(
                r#"{"capabilities":{"definitionProvider":[],"referencesProvider":true}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn minimal_initialize_parser_accepts_known_boolean_object_and_text_sync_shapes() {
        let capabilities = parse_initialize_capabilities(
            r#"{
                "capabilities": {
                    "definitionProvider": true,
                    "referencesProvider": false,
                    "implementationProvider": {"workDoneProgress": true},
                    "documentSymbolProvider": {"label": "outline"},
                    "textDocumentSync": {"openClose": true, "change": 2, "save": {"includeText": true}}
                },
                "serverInfo": {"name": "ignored", "version": "1"}
            }"#,
        )
        .unwrap();
        assert!(capabilities.definition);
        assert!(!capabilities.references);
        assert!(capabilities.implementations);
        assert!(capabilities.document_symbols);
        assert_eq!(
            capabilities.text_document_sync,
            TextSyncCapability {
                open_close: true,
                change: TextChangeCapability::Incremental,
            }
        );

        for (kind, expected) in [
            (
                0,
                TextSyncCapability {
                    open_close: false,
                    change: TextChangeCapability::None,
                },
            ),
            (
                1,
                TextSyncCapability {
                    open_close: true,
                    change: TextChangeCapability::Full,
                },
            ),
            (
                2,
                TextSyncCapability {
                    open_close: true,
                    change: TextChangeCapability::Incremental,
                },
            ),
        ] {
            let raw = format!(r#"{{"capabilities":{{"textDocumentSync":{kind}}}}}"#);
            assert_eq!(
                parse_initialize_capabilities(&raw)
                    .unwrap()
                    .text_document_sync,
                expected
            );
        }
        for invalid in [
            r#"{"capabilities":{"textDocumentSync":3}}"#,
            r#"{"capabilities":{"textDocumentSync":[]}}"#,
            r#"{"capabilities":{"textDocumentSync":{"openClose":true,"openClose":false}}}"#,
            r#"{"capabilities":{"textDocumentSync":{"change":1,"change":2}}}"#,
        ] {
            assert!(parse_initialize_capabilities(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            parse_initialize_capabilities(
                r#"{"capabilities":{"textDocumentSync":{"openClose":true}}}"#
            )
            .unwrap()
            .text_document_sync
            .change,
            TextChangeCapability::None
        );
    }

    #[test]
    fn near_four_mib_initialize_response_uses_production_framing_budget_and_actor_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut manager = manager(&root);
        let budget = PayloadBudget::session(&manager.global_budget);
        let (key, queued_writes) = starting_initialize_session(&mut manager, &root, budget.clone());
        let body = near_limit_initialize_response();
        let frame = decode_framed_response(body.as_bytes(), &budget);
        assert_eq!(budget.session.used(), body.len());
        assert_eq!(budget.global.used(), body.len());

        manager.handle_frame(&key, frame);

        let session = manager.sessions.get(&key).unwrap();
        let ServerState::Ready { capabilities } = &session.state else {
            panic!("initialize response did not make the production session ready");
        };
        assert!(capabilities.definition);
        assert!(capabilities.references);
        assert!(capabilities.implementations);
        assert!(capabilities.document_symbols);
        assert_eq!(
            capabilities.text_document_sync,
            TextSyncCapability {
                open_close: true,
                change: TextChangeCapability::Incremental,
            }
        );

        let initialized = queued_writes.try_recv().unwrap();
        assert!(
            std::str::from_utf8(initialized.as_slice())
                .unwrap()
                .contains(r#""method":"initialized""#)
        );
        assert_eq!(budget.session.used(), initialized.len());
        assert_eq!(budget.global.used(), initialized.len());
        drop(initialized);
        assert_eq!(budget.session.used(), 0);
        assert_eq!(budget.global.used(), 0);
    }

    #[test]
    fn initialize_actor_rejects_when_body_sized_scratch_permit_cannot_be_acquired() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut manager = manager(&root);
        let budget = PayloadBudget::session(&manager.global_budget);
        let (key, queued_writes) = starting_initialize_session(&mut manager, &root, budget.clone());
        let body = near_limit_initialize_response();
        let frame = decode_framed_response(body.as_bytes(), &budget);
        let scratch_headroom = body.len() - 1;
        let blocker = budget
            .reserve(SESSION_PAYLOAD_BUDGET - body.len() - scratch_headroom)
            .unwrap();
        assert_eq!(
            SESSION_PAYLOAD_BUDGET - budget.session.used(),
            scratch_headroom
        );

        manager.handle_frame(&key, frame);
        manager.cleanup_failed_sessions();

        assert!(!manager.sessions.contains_key(&key));
        let backoff = manager.backoffs.get(&key).unwrap();
        assert_eq!(
            backoff.error,
            "language server response exceeded the navigation payload budget"
        );
        assert!(matches!(
            queued_writes.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
        assert_eq!(budget.session.used(), blocker.bytes());
        drop(blocker);
        assert_eq!(budget.session.used(), 0);
    }

    #[test]
    fn production_initialize_parser_has_no_open_lsp_capability_dom() {
        let source = include_str!("lsp.rs");
        let start = source.find("fn parse_initialize_capabilities").unwrap();
        let end = source[start..]
            .find("fn parse_navigation_result")
            .map(|offset| start + offset)
            .unwrap();
        let parser = &source[start..end];
        assert!(!parser.contains("lsp_types::InitializeResult"));
        assert!(!parser.contains("lsp_types::ServerCapabilities"));
        assert!(!parser.contains("serde_json::Value"));
    }

    #[test]
    fn bounded_location_parser_rejects_count_mixed_shape_and_long_uri() {
        let budget = budget();
        let location = r#"{"uri":"file:///tmp/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}"#;
        let link = r#"{"targetUri":"file:///tmp/a.rs","targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"targetSelectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}"#;
        let too_many = format!(
            "[{}]",
            std::iter::repeat_n(location, 2_001)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse_bounded_locations(&too_many, &budget, true).is_err());
        assert!(parse_bounded_locations(&format!("[{location},{link}]"), &budget, true).is_err());
        assert!(parse_bounded_locations(location, &budget, false).is_err());
        let long_uri = format!(
            r#"[{{"uri":"file:///{}","range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}}}}]"#,
            "a".repeat(MAX_RESULT_URI_BYTES + 1)
        );
        assert!(parse_bounded_locations(&long_uri, &budget, false).is_err());
        assert!(matches!(
            parse_navigation_result(NavigationOperation::Definition, "null", &budget).unwrap(),
            NavigationProtocolResult::Locations(locations) if locations.is_empty()
        ));
        assert!(
            parse_navigation_result(NavigationOperation::DocumentSymbols, "[]", &budget).is_err()
        );
        assert_eq!(
            parse_bounded_locations(link, &budget, true).unwrap().len(),
            1
        );
        assert!(parse_bounded_locations(link, &budget, false).is_err());
    }

    #[test]
    fn near_four_mib_location_frame_is_streamed_and_permit_follows_result() {
        let budget = budget();
        let location = r#"{"uri":"file:///tmp/target.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}"#;
        let whitespace = " ".repeat(MAX_BODY_BYTES - location.len() - 3);
        let raw = format!("[{whitespace}{location}]");
        assert!(raw.len() < MAX_BODY_BYTES);
        let locations = parse_bounded_locations(&raw, &budget, false).unwrap();
        assert_eq!(locations.len(), 1);
        assert!(budget.session.used() > 0);
        drop(locations);
        assert_eq!(budget.session.used(), 0);
    }

    #[test]
    fn initialize_timeout_forces_terminal_cleanup_and_completes_deferred_request() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let mut manager = manager(&root);
        let key = SessionKey {
            server_root: root.clone(),
            family: LanguageFamily::Rust,
        };
        let budget = PayloadBudget::session(&manager.global_budget);
        let deferred = pending_user_request(7, NavigationOperation::Definition, source, &budget);
        let (frames, controls, lifecycle) = empty_session_channels();
        let deadline = Instant::now();
        let mut pending = HashMap::new();
        pending.insert(RpcId::Signed(0), PendingRequest::Initialize { deadline });
        manager.sessions.insert(
            key.clone(),
            LspSession {
                key: key.clone(),
                epoch: SessionEpoch(1),
                state: ServerState::Starting { since: deadline },
                budget,
                allocator: RequestIdAllocator::default(),
                retired: RetiredIds::default(),
                pending,
                deferred: Some(deferred),
                opened: None,
                writer: None,
                frames,
                controls,
                lifecycle,
                io_done: HashSet::new(),
                io_joined: HashSet::new(),
                tree: None,
                io_threads: Vec::new(),
                cleanup_stats: Arc::clone(&manager.cleanup_stats),
                ready_since: None,
            },
        );
        manager.expire_requests_at(deadline + Duration::from_millis(1));
        manager.cleanup_failed_sessions();
        assert!(!manager.sessions.contains_key(&key));
        assert!(manager.backoffs.contains_key(&key));
        let completions = manager.completions.lock().unwrap();
        assert_eq!(completions.len(), 1);
        assert!(matches!(
            &completions[0].result,
            NavigationProtocolResult::Failed(message)
                if message.contains("initialization timed out")
        ));
    }

    #[test]
    fn document_symbols_normalize_nested_utf16_and_flat_same_file_results() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "mod 😀 {}\n");
        let budget = budget();
        let document =
            NavigationDocument::from_source(&source, DocumentVersion(1), &budget).unwrap();
        let before = budget.session.used();
        let nested = r#"[{"name":"Module","kind":2,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":9}},"selectionRange":{"start":{"line":0,"character":4},"end":{"line":0,"character":6}},"children":[{"name":"child","detail":"fn","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":9}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}}]}]"#;
        let symbols = parse_document_symbol_result(nested, &budget, &root, &document).unwrap();
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].selection_range.start.byte, 4);
        assert_eq!(symbols[0].selection_range.end.byte, 8);
        assert_eq!(symbols[1].parent, Some(0));
        assert_eq!(symbols[1].kind, ProtocolSymbolKind::Function);
        assert!(budget.session.used() > before);
        drop(symbols);
        assert_eq!(budget.session.used(), before);

        let uri = crate::navigation::path_to_lsp_uri(&document.absolute_path).unwrap();
        let flat = serde_json::json!([{
            "name":"flat",
            "kind":6,
            "location":{"uri":uri,"range":{
                "start":{"line":0,"character":0},
                "end":{"line":0,"character":9}
            }},
            "containerName":"container"
        }]);
        let symbols =
            parse_document_symbol_result(&flat.to_string(), &budget, &root, &document).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].parent, None);
        assert_eq!(symbols[0].container.as_deref(), Some("container"));
        assert_eq!(symbols[0].kind, ProtocolSymbolKind::Method);
    }

    #[test]
    fn document_symbols_reject_mixed_cross_file_invalid_ranges_and_long_strings() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let budget = budget();
        let document =
            NavigationDocument::from_source(&source, DocumentVersion(1), &budget).unwrap();
        let nested = r#"{"name":"nested","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}}}"#;
        let other = root.join("other.rs");
        fs::write(&other, "fn other() {}\n").unwrap();
        let other_uri = crate::navigation::path_to_lsp_uri(&other.canonicalize().unwrap()).unwrap();
        let flat = serde_json::json!({
            "name":"flat","kind":12,
            "location":{"uri":other_uri,"range":{
                "start":{"line":0,"character":0},"end":{"line":0,"character":2}
            }}
        });
        let mixed = format!("[{nested},{flat}]");
        assert!(parse_document_symbol_result(&mixed, &budget, &root, &document).is_err());
        assert!(
            parse_document_symbol_result(&format!("[{flat}]"), &budget, &root, &document).is_err()
        );
        let invalid_selection = r#"[{"name":"bad","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}}]"#;
        assert!(
            parse_document_symbol_result(invalid_selection, &budget, &root, &document).is_err()
        );
        let long = serde_json::json!([{
            "name":"x".repeat(MAX_SYMBOL_STRING_BYTES + 1),
            "kind":12,
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},
            "selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}}
        }]);
        assert!(
            parse_document_symbol_result(&long.to_string(), &budget, &root, &document).is_err()
        );
    }

    #[test]
    fn wedged_disk_lane_atomically_completes_active_and_queued_requests() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let reader_gate = Arc::clone(&gate);
        let reader: Arc<DiskReadFn> = Arc::new(move |_, _| {
            let (lock, changed) = &*reader_gate;
            let ready = lock.lock().unwrap();
            let _guard = changed.wait_while(ready, |ready| !*ready).unwrap();
            Ok(true)
        });
        let lane = DiskRevalidationLane::start_with_reader(reader).unwrap();
        let now = Instant::now();
        let make_job = |generation| DiskSnapshotJob {
            generation,
            workspace_root: root.clone(),
            absolute_path: source.absolute_path.clone(),
            disk_raw_len: source.disk_raw_len,
            expected_text: Arc::clone(&source.text),
            deadline: now,
        };
        lane.try_submit(make_job(1)).unwrap();
        while lane.active_deadline.lock().unwrap().is_none() {
            thread::yield_now();
        }
        lane.try_submit(make_job(2)).unwrap();
        assert!(lane.try_submit(make_job(3)).is_err());

        let mut manager = manager(&root);
        manager.disk_lane = Some(lane);
        let document_budget = budget();
        let key = SessionKey {
            server_root: root.clone(),
            family: LanguageFamily::Rust,
        };
        for generation in [1, 2] {
            manager.disk_waiting.insert(
                generation,
                (
                    key.clone(),
                    pending_user_request(
                        generation,
                        NavigationOperation::Definition,
                        Arc::clone(&source),
                        &document_budget,
                    ),
                ),
            );
        }
        manager.poll_disk();
        assert!(manager.disk_waiting.is_empty());
        let completions = manager.completions.lock().unwrap();
        assert_eq!(completions.len(), 2);
        assert!(completions.iter().all(|completion| matches!(
            &completion.result,
            NavigationProtocolResult::Failed(message)
                if message == "File revalidation worker is unavailable."
        )));
        drop(completions);

        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(1);
        while manager
            .disk_lane
            .as_ref()
            .is_some_and(|lane| lane.done.try_recv().is_err())
        {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        manager.poll_disk();
        assert_eq!(manager.completions.lock().unwrap().len(), 2);
    }

    #[test]
    fn disk_dispatch_distinguishes_missing_full_current_stale_and_failed_workers() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let disk_sync = TextSyncCapability {
            open_close: false,
            change: TextChangeCapability::None,
        };

        let mut missing = manager(&root);
        let (missing_key, _writes, missing_budget) =
            install_ready_session(&mut missing, &root, navigation_capabilities(disk_sync));
        missing.disk_lane = None;
        missing.dispatch_or_revalidate(
            missing_key,
            pending_user_request(
                1,
                NavigationOperation::Definition,
                Arc::clone(&source),
                &missing_budget,
            ),
        );
        assert!(matches!(
            missing.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("unavailable")
        ));

        let reader: Arc<DiskReadFn> = Arc::new(|job, _| match job.generation {
            2 => Ok(true),
            3 => Ok(false),
            4 => bail!("synthetic disk read failure"),
            generation => bail!("unexpected generation {generation}"),
        });
        let lane = DiskRevalidationLane::start_with_reader(reader).unwrap();
        let mut actor = manager(&root);
        let (key, writes, document_budget) =
            install_ready_session(&mut actor, &root, navigation_capabilities(disk_sync));
        actor.disk_lane = Some(lane);
        for generation in 2..=4 {
            actor.dispatch_or_revalidate(
                key.clone(),
                pending_user_request(
                    generation,
                    NavigationOperation::Definition,
                    Arc::clone(&source),
                    &document_budget,
                ),
            );
            let deadline = Instant::now() + Duration::from_secs(1);
            while actor.disk_waiting.contains_key(&generation) {
                actor.poll_disk();
                assert!(
                    Instant::now() < deadline,
                    "disk result {generation} did not arrive"
                );
                thread::yield_now();
            }
        }
        assert_eq!(actor.sessions[&key].pending.len(), 1);
        let request_frame = writes.try_recv().unwrap();
        assert!(
            std::str::from_utf8(request_frame.as_slice())
                .unwrap()
                .contains(r#""method":"textDocument/definition""#)
        );
        let completions = actor.completions.lock().unwrap();
        assert_eq!(completions.len(), 2);
        assert!(matches!(
            completions[0].result,
            NavigationProtocolResult::Failed(ref message) if message.contains("changed on disk")
        ));
        assert!(matches!(
            completions[1].result,
            NavigationProtocolResult::Failed(ref message) if message.contains("synthetic disk read failure")
        ));
        drop(completions);

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let reader_gate = Arc::clone(&gate);
        let reader: Arc<DiskReadFn> = Arc::new(move |_, _| {
            let (lock, changed) = &*reader_gate;
            let ready = lock.lock().unwrap();
            let _guard = changed.wait_while(ready, |ready| !*ready).unwrap();
            Ok(true)
        });
        let lane = DiskRevalidationLane::start_with_reader(reader).unwrap();
        let now = Instant::now();
        let job = |generation| {
            DiskSnapshotJob::bounded(
                generation,
                root.clone(),
                source.absolute_path.clone(),
                source.disk_raw_len,
                Arc::clone(&source.text),
                now,
                now + Duration::from_secs(5),
            )
        };
        lane.try_submit(job(90)).unwrap();
        while lane.active_deadline.lock().unwrap().is_none() {
            thread::yield_now();
        }
        lane.try_submit(job(91)).unwrap();
        let mut full = manager(&root);
        let (full_key, _writes, full_budget) =
            install_ready_session(&mut full, &root, navigation_capabilities(disk_sync));
        full.disk_lane = Some(lane);
        full.dispatch_or_revalidate(
            full_key,
            pending_user_request(5, NavigationOperation::Definition, source, &full_budget),
        );
        assert!(matches!(
            full.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("queue is full")
        ));
        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }

    #[test]
    fn timeout_and_cancellation_retire_every_request_location_and_absorb_late_results() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let mut actor = manager(&root);
        let (key, writes, document_budget) = install_ready_session(
            &mut actor,
            &root,
            navigation_capabilities(TextSyncCapability {
                open_close: true,
                change: TextChangeCapability::Full,
            }),
        );
        let expired_id = RpcId::Signed(7);
        let expired = pending_user_request(
            10,
            NavigationOperation::Definition,
            Arc::clone(&source),
            &document_budget,
        );
        actor.sessions.get_mut(&key).unwrap().pending.insert(
            expired_id.clone(),
            PendingRequest::Navigation {
                request: expired.request,
                document: expired.document,
                deadline: Instant::now(),
            },
        );
        actor.expire_requests_at(Instant::now() + Duration::from_millis(1));
        assert!(actor.sessions[&key].retired.set.contains(&expired_id));
        assert!(matches!(
            actor.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("timed out")
        ));
        let cancel_frame = writes.try_recv().unwrap();
        assert!(
            std::str::from_utf8(cancel_frame.as_slice())
                .unwrap()
                .contains(r#""method":"$/cancelRequest""#)
        );
        actor.handle_frame(
            &key,
            charged_frame(
                r#"{"jsonrpc":"2.0","id":7,"result":null}"#,
                &document_budget,
            ),
        );
        assert!(actor.sessions.contains_key(&key));
        assert!(!actor.sessions[&key].retired.set.contains(&expired_id));

        actor.sessions.get_mut(&key).unwrap().deferred = Some(pending_user_request(
            11,
            NavigationOperation::References,
            Arc::clone(&source),
            &document_budget,
        ));
        actor.disk_waiting.insert(
            12,
            (
                key.clone(),
                pending_user_request(
                    12,
                    NavigationOperation::Implementations,
                    Arc::clone(&source),
                    &document_budget,
                ),
            ),
        );
        actor.cancel_generation(11);
        actor.cancel_generation(12);
        assert!(actor.sessions[&key].deferred.is_none());
        assert!(!actor.disk_waiting.contains_key(&12));
        assert!(
            actor
                .completions
                .lock()
                .unwrap()
                .iter()
                .filter(|completion| matches!(
                    completion.result,
                    NavigationProtocolResult::Cancelled
                ))
                .count()
                >= 2
        );

        let failed_id = RpcId::Signed(8);
        let failed = pending_user_request(
            13,
            NavigationOperation::Definition,
            source,
            &document_budget,
        );
        let session = actor.sessions.get_mut(&key).unwrap();
        session.pending.insert(
            failed_id,
            PendingRequest::Navigation {
                request: failed.request,
                document: failed.document,
                deadline: Instant::now(),
            },
        );
        session.writer = None;
        actor.expire_requests_at(Instant::now() + Duration::from_millis(1));
        actor.cleanup_failed_sessions();
        assert!(!actor.sessions.contains_key(&key));
        assert!(
            actor.backoffs[&key]
                .error
                .contains("cannot retire timed-out")
        );
    }

    #[test]
    fn polling_classifies_lifecycle_control_and_frame_channel_failures_terminally() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let capabilities = navigation_capabilities(TextSyncCapability {
            open_close: true,
            change: TextChangeCapability::Full,
        });
        for (event, expected) in [
            (
                SessionControlEvent::ReaderFailed("reader".to_owned()),
                "reader",
            ),
            (
                SessionControlEvent::WriterFailed("writer".to_owned()),
                "writer",
            ),
            (
                SessionControlEvent::StderrFailed("stderr".to_owned()),
                "stderr",
            ),
            (SessionControlEvent::ReaderEof, "stopped unexpectedly"),
        ] {
            let mut actor = manager(&root);
            let (key, _writes, _) = install_ready_session(&mut actor, &root, capabilities);
            let (sender, controls) = mpsc::sync_channel(1);
            sender.send(event).unwrap();
            actor.sessions.get_mut(&key).unwrap().controls = controls;
            actor.poll_sessions();
            actor.cleanup_failed_sessions();
            assert!(!actor.sessions.contains_key(&key));
            assert!(actor.backoffs[&key].error.contains(expected));
        }

        let mut lifecycle = manager(&root);
        let (key, _writes, _) = install_ready_session(&mut lifecycle, &root, capabilities);
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(IoThreadDone {
                kind: IoThreadKind::Stdout,
                session_epoch: 99,
            })
            .unwrap();
        lifecycle.sessions.get_mut(&key).unwrap().lifecycle = receiver;
        lifecycle.poll_sessions();
        lifecycle.cleanup_failed_sessions();
        assert!(!lifecycle.sessions.contains_key(&key));
        assert!(lifecycle.backoffs[&key].error.contains("epoch mismatch"));

        let mut disconnected = manager(&root);
        let (key, _writes, _) = install_ready_session(&mut disconnected, &root, capabilities);
        let (_lifecycle_sender, lifecycle_receiver) = mpsc::sync_channel(1);
        let (_control_sender, control_receiver) = mpsc::sync_channel(1);
        let (frame_sender, frame_receiver) = mpsc::sync_channel(1);
        drop(frame_sender);
        let session = disconnected.sessions.get_mut(&key).unwrap();
        session.lifecycle = lifecycle_receiver;
        session.controls = control_receiver;
        session.frames = frame_receiver;
        disconnected.poll_sessions();
        disconnected.cleanup_failed_sessions();
        assert!(!disconnected.sessions.contains_key(&key));
        assert!(
            disconnected.backoffs[&key]
                .error
                .contains("frame channel stopped")
        );

        let mut control_disconnected = manager(&root);
        let (key, _writes, _) = install_ready_session(
            &mut control_disconnected,
            &root,
            navigation_capabilities(TextSyncCapability {
                open_close: true,
                change: TextChangeCapability::Full,
            }),
        );
        control_disconnected.poll_sessions();
        control_disconnected.cleanup_failed_sessions();
        assert!(!control_disconnected.sessions.contains_key(&key));
        assert!(
            control_disconnected.backoffs[&key]
                .error
                .contains("control channel stopped")
        );
    }

    #[test]
    fn invalid_initialize_responses_and_unmatched_ids_fail_the_session_terminally() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        for body in [
            r#"{"jsonrpc":"2.0","result":{"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","id":0,"method":"window/logMessage","result":null}"#,
            r#"{"jsonrpc":"1.0","id":0,"result":{"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{}},"error":{"message":"both"}}"#,
            r#"{"jsonrpc":"2.0","id":9,"result":{"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","id":0}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"message":"rejected"}}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":[]}}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","id":0,"result": "#,
        ] {
            let mut manager = manager(&root);
            let key = SessionKey {
                server_root: root.clone(),
                family: LanguageFamily::Rust,
            };
            let budget = PayloadBudget::session(&manager.global_budget);
            let deferred = pending_user_request(
                17,
                NavigationOperation::Definition,
                Arc::clone(&source),
                &budget,
            );
            let (frames, controls, lifecycle) = empty_session_channels();
            let mut pending = HashMap::new();
            pending.insert(
                RpcId::Signed(0),
                PendingRequest::Initialize {
                    deadline: Instant::now() + Duration::from_secs(5),
                },
            );
            manager.sessions.insert(
                key.clone(),
                LspSession {
                    key: key.clone(),
                    epoch: SessionEpoch(1),
                    state: ServerState::Starting {
                        since: Instant::now(),
                    },
                    budget: budget.clone(),
                    allocator: RequestIdAllocator::default(),
                    retired: RetiredIds::default(),
                    pending,
                    deferred: Some(deferred),
                    opened: None,
                    writer: None,
                    frames,
                    controls,
                    lifecycle,
                    io_done: HashSet::new(),
                    io_joined: HashSet::new(),
                    tree: None,
                    io_threads: Vec::new(),
                    cleanup_stats: Arc::clone(&manager.cleanup_stats),
                    ready_since: None,
                },
            );
            let permit = budget.reserve(body.len()).unwrap();
            manager.handle_frame(
                &key,
                ChargedPayload {
                    bytes: body.as_bytes().to_vec(),
                    _permit: permit,
                },
            );
            manager.cleanup_failed_sessions();
            assert!(!manager.sessions.contains_key(&key), "body={body}");
            assert!(manager.backoffs.contains_key(&key), "body={body}");
            let completions = manager.completions.lock().unwrap();
            assert_eq!(completions.len(), 1, "body={body}");
            assert!(matches!(
                completions[0].result,
                NavigationProtocolResult::Failed(_)
            ));
        }
    }

    #[test]
    fn invalid_navigation_error_location_and_symbol_results_fail_and_clean_up() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        for (operation, body) in [
            (
                NavigationOperation::Definition,
                r#"{"jsonrpc":"2.0","id":4,"error":[]}"#,
            ),
            (
                NavigationOperation::Definition,
                r#"{"jsonrpc":"2.0","id":4,"result":42}"#,
            ),
            (
                NavigationOperation::DocumentSymbols,
                r#"{"jsonrpc":"2.0","id":4,"result":42}"#,
            ),
        ] {
            let mut actor = manager(&root);
            let (key, _writes, document_budget) = install_ready_session(
                &mut actor,
                &root,
                navigation_capabilities(TextSyncCapability {
                    open_close: true,
                    change: TextChangeCapability::Full,
                }),
            );
            let pending =
                pending_user_request(40, operation, Arc::clone(&source), &document_budget);
            actor.sessions.get_mut(&key).unwrap().pending.insert(
                RpcId::Signed(4),
                PendingRequest::Navigation {
                    request: pending.request,
                    document: pending.document,
                    deadline: Instant::now() + Duration::from_secs(1),
                },
            );
            actor.handle_frame(&key, charged_frame(body, &document_budget));
            actor.cleanup_failed_sessions();
            assert!(!actor.sessions.contains_key(&key), "body={body}");
            assert!(actor.backoffs.contains_key(&key), "body={body}");
            assert!(matches!(
                actor.completions.lock().unwrap().back().unwrap().result,
                NavigationProtocolResult::Failed(_)
            ));
        }
    }

    #[test]
    fn document_symbol_actor_emits_the_bounded_symbol_completion() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let mut manager = manager(&root);
        let key = SessionKey {
            server_root: root.clone(),
            family: LanguageFamily::Rust,
        };
        let budget = PayloadBudget::session(&manager.global_budget);
        let user_request = request(
            23,
            NavigationOperation::DocumentSymbols,
            Arc::clone(&source),
        );
        let document = Arc::new(
            NavigationDocument::from_source(&source, user_request.version, &budget).unwrap(),
        );
        let mut pending = HashMap::new();
        pending.insert(
            RpcId::Signed(4),
            PendingRequest::Navigation {
                request: user_request,
                document,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        );
        let (frames, controls, lifecycle) = empty_session_channels();
        manager.sessions.insert(
            key.clone(),
            LspSession {
                key: key.clone(),
                epoch: SessionEpoch(1),
                state: ServerState::Ready {
                    capabilities: NavigationCapabilities {
                        definition: true,
                        references: true,
                        implementations: true,
                        document_symbols: true,
                        text_document_sync: TextSyncCapability {
                            open_close: true,
                            change: TextChangeCapability::Full,
                        },
                    },
                },
                budget: budget.clone(),
                allocator: RequestIdAllocator::default(),
                retired: RetiredIds::default(),
                pending,
                deferred: None,
                opened: None,
                writer: None,
                frames,
                controls,
                lifecycle,
                io_done: HashSet::new(),
                io_joined: HashSet::new(),
                tree: None,
                io_threads: Vec::new(),
                cleanup_stats: Arc::clone(&manager.cleanup_stats),
                ready_since: Some(Instant::now()),
            },
        );
        let body = r#"{"jsonrpc":"2.0","id":4,"result":[{"name":"main","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}},"selectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":7}}}]}"#;
        let permit = budget.reserve(body.len()).unwrap();
        manager.handle_frame(
            &key,
            ChargedPayload {
                bytes: body.as_bytes().to_vec(),
                _permit: permit,
            },
        );
        assert!(manager.sessions.contains_key(&key));
        assert!(manager.completions.lock().unwrap().is_empty());
        let symbols = manager.symbol_completions.lock().unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].generation, 23);
        assert_eq!(symbols[0].symbols.len(), 1);
        assert_eq!(symbols[0].symbols[0].name, "main");
    }

    #[test]
    fn cancelled_request_retirement_absorbs_one_late_response_without_session_failure() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = source(&root, "main.rs", "fn main() {}\n");
        let mut manager = manager(&root);
        let key = SessionKey {
            server_root: root.clone(),
            family: LanguageFamily::Rust,
        };
        let budget = PayloadBudget::session(&manager.global_budget);
        let user_request = request(29, NavigationOperation::Definition, Arc::clone(&source));
        let document = Arc::new(
            NavigationDocument::from_source(&source, user_request.version, &budget).unwrap(),
        );
        let mut pending = HashMap::new();
        pending.insert(
            RpcId::Signed(5),
            PendingRequest::Navigation {
                request: user_request,
                document,
                deadline: Instant::now() + Duration::from_secs(3),
            },
        );
        let (writer, _writer_receiver) = mpsc::sync_channel(2);
        let (frames, controls, lifecycle) = empty_session_channels();
        manager.sessions.insert(
            key.clone(),
            LspSession {
                key: key.clone(),
                epoch: SessionEpoch(1),
                state: ServerState::Ready {
                    capabilities: NavigationCapabilities {
                        definition: true,
                        references: true,
                        implementations: true,
                        document_symbols: true,
                        text_document_sync: TextSyncCapability {
                            open_close: true,
                            change: TextChangeCapability::Full,
                        },
                    },
                },
                budget: budget.clone(),
                allocator: RequestIdAllocator::default(),
                retired: RetiredIds::default(),
                pending,
                deferred: None,
                opened: None,
                writer: Some(writer),
                frames,
                controls,
                lifecycle,
                io_done: HashSet::new(),
                io_joined: HashSet::new(),
                tree: None,
                io_threads: Vec::new(),
                cleanup_stats: Arc::clone(&manager.cleanup_stats),
                ready_since: Some(Instant::now()),
            },
        );
        manager.cancel_generation(29);
        let body = r#"{"jsonrpc":"2.0","id":5,"result":null}"#;
        let permit = budget.reserve(body.len()).unwrap();
        manager.handle_frame(
            &key,
            ChargedPayload {
                bytes: body.as_bytes().to_vec(),
                _permit: permit,
            },
        );
        assert!(manager.sessions.contains_key(&key));
        let completions = manager.completions.lock().unwrap();
        assert_eq!(completions.len(), 1);
        assert!(matches!(
            completions[0].result,
            NavigationProtocolResult::Cancelled
        ));
    }

    #[test]
    fn retired_ids_force_restart_at_the_sixty_fifth_unique_id() {
        let mut retired = RetiredIds::default();
        for value in 0..MAX_RETIRED_IDS {
            retired.insert(RpcId::Signed(value as i32)).unwrap();
        }
        assert!(retired.insert(RpcId::Signed(99)).is_err());
        assert!(retired.take(&RpcId::Signed(0)));
        retired.insert(RpcId::Signed(99)).unwrap();
    }

    #[test]
    fn disk_lane_matches_normalized_preview_and_rejects_stale() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        fs::write(&path, b"\xef\xbb\xbffn main() {}\r\n").unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = path.canonicalize().unwrap();
        let lane = DiskRevalidationLane::start().unwrap();
        lane.try_submit(DiskSnapshotJob {
            generation: 1,
            workspace_root: root,
            absolute_path: path,
            disk_raw_len: 17,
            expected_text: Arc::from("fn main() {}"),
            deadline: Instant::now() + Duration::from_secs(1),
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(result) = lane.try_result() {
                assert_eq!(result, DiskSnapshotResult::Current { generation: 1 });
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }

    #[test]
    fn disk_revalidation_rejects_expiry_type_size_encoding_nul_and_line_drift() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let cancel = AtomicBool::new(false);
        let now = Instant::now();
        let path = root.join("source.rs");
        fs::write(&path, "current\n").unwrap();
        let bounded = DiskSnapshotJob::bounded(
            1,
            root.clone(),
            path.clone(),
            8,
            Arc::from("current"),
            now,
            now + Duration::from_secs(5),
        );
        assert_eq!(bounded.deadline, now + Duration::from_millis(500));
        let job = |path: &Path, raw_len, expected: &'static str| DiskSnapshotJob {
            generation: 1,
            workspace_root: root.clone(),
            absolute_path: path.to_path_buf(),
            disk_raw_len: raw_len,
            expected_text: Arc::from(expected),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(!read_and_compare_snapshot(&job(&path, 7, "current"), &cancel).unwrap());
        assert!(!read_and_compare_snapshot(&job(&root, 0, ""), &cancel).unwrap());
        let expired = DiskSnapshotJob {
            deadline: Instant::now(),
            ..job(&path, 8, "current")
        };
        assert!(!read_and_compare_snapshot(&expired, &cancel).unwrap());
        cancel.store(true, Ordering::Release);
        assert!(!read_and_compare_snapshot(&job(&path, 8, "current"), &cancel).unwrap());
        cancel.store(false, Ordering::Release);

        for (bytes, expected_error) in [
            (b"nul\0byte".to_vec(), false),
            (vec![0xff, 0xfe], true),
            (
                "line\n".repeat(MAX_NAVIGATION_LINES + 1).into_bytes(),
                false,
            ),
            (vec![b'x'; MAX_NAVIGATION_TEXT_BYTES + 1], false),
        ] {
            fs::write(&path, &bytes).unwrap();
            let result =
                read_and_compare_snapshot(&job(&path, bytes.len() as u64, "other"), &cancel);
            assert_eq!(result.is_err(), expected_error);
            if !expected_error {
                assert!(!result.unwrap());
            }
        }

        let reader: Arc<DiskReadFn> = Arc::new(|_, _| bail!("synthetic read failure"));
        let lane = DiskRevalidationLane::start_with_reader(reader).unwrap();
        lane.try_submit(job(&path, 0, "")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(result) = lane.try_result() {
                assert!(matches!(
                    result,
                    DiskSnapshotResult::Failed { message, .. }
                        if message.contains("synthetic")
                ));
                break;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }

    #[test]
    fn disk_lane_drop_detaches_one_blocked_reader_within_bound() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let reader_gate = Arc::clone(&gate);
        let reader: Arc<DiskReadFn> = Arc::new(move |_, _| {
            let (lock, changed) = &*reader_gate;
            let ready = lock.lock().unwrap();
            let _guard = changed.wait_while(ready, |ready| !*ready).unwrap();
            Ok(false)
        });
        let lane = DiskRevalidationLane::start_with_reader(reader).unwrap();
        let directory = tempfile::tempdir().unwrap();
        lane.try_submit(DiskSnapshotJob {
            generation: 1,
            workspace_root: directory.path().to_path_buf(),
            absolute_path: directory.path().join("blocked"),
            disk_raw_len: 0,
            expected_text: Arc::from(""),
            deadline: Instant::now() + Duration::from_millis(10),
        })
        .unwrap();
        while lane.in_flight.load(Ordering::Acquire) == 0 {
            thread::yield_now();
        }
        let started = Instant::now();
        drop(lane);
        assert!(started.elapsed() < Duration::from_millis(500));
        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }

    #[cfg(unix)]
    #[test]
    fn production_runtime_drives_framed_session_reuse_server_calls_and_orderly_cleanup() {
        use std::os::unix::fs::PermissionsExt;

        let _environment = crate::navigation::lock_navigation_environment();

        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        let tools = container.path().join("tools");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let caller = source(&workspace, "caller.rs", "fn caller() {}\n");
        let second = source(&workspace, "second.rs", "fn second() {}\n");
        let target_path = workspace.join("target.rs");
        fs::write(&target_path, "fn target() {}\n").unwrap();
        let target_path = target_path.canonicalize().unwrap();
        let caller_uri = crate::navigation::path_to_lsp_uri(&caller.absolute_path).unwrap();
        let target_uri = crate::navigation::path_to_lsp_uri(&target_path).unwrap();
        let trace = container.path().join("server.trace");
        for value in [
            caller_uri.as_str(),
            target_uri.as_str(),
            trace.to_str().unwrap(),
        ] {
            assert!(
                !value.contains('\''),
                "test shell fixture requires simple temp paths"
            );
        }

        let script = tools.join("framed-server");
        let body = r#"#!/bin/sh
trace='__TRACE__'
caller_uri='__CALLER_URI__'
target_uri='__TARGET_URI__'
references=0

read_message() {
    length=
    while IFS= read -r line; do
        line=$(printf '%s' "$line" | tr -d '\r')
        if [ -z "$line" ]; then
            break
        fi
        case "$line" in
            Content-Length:*) length=$(printf '%s' "$line" | tr -cd '0-9') ;;
        esac
    done
    [ -n "$length" ] || return 1
    dd bs=1 count="$length" 2>/dev/null
}

write_message() {
    printf 'Content-Length: %s\r\n\r\n%s' "${#1}" "$1"
}

request_id() {
    printf '%s' "$1" | sed -n 's/.*"id":\([-0-9][0-9]*\).*/\1/p'
}

while message=$(read_message); do
    case "$message" in
        *'"method":"initialize"'*)
            id=$(request_id "$message")
            sleep 0.1
            write_message '{"jsonrpc":"2.0","id":'"$id"',"result":{"capabilities":{"positionEncoding":"utf-16","definitionProvider":true,"referencesProvider":true,"implementationProvider":true,"documentSymbolProvider":true,"textDocumentSync":{"openClose":true,"change":1}}}}'
            ;;
        *'"method":"initialized"'*)
            printf '%s\n' initialized >> "$trace"
            write_message '{"jsonrpc":"2.0","id":"config-valid","method":"workspace/configuration","params":{"items":[{"scopeUri":"__CALLER_URI__","section":"rust"},{}]}}'
            ;;
        *'"id":"config-valid"'*'"result"'*)
            printf '%s\n' config-valid >> "$trace"
            write_message '{"jsonrpc":"2.0","id":"config-invalid","method":"workspace/configuration","params":{"items":"bad"}}'
            ;;
        *'"error"'*'"id":"config-invalid"'*)
            printf '%s\n' config-invalid >> "$trace"
            write_message '{"jsonrpc":"2.0","id":"unknown","method":"latte/unknown","params":null}'
            ;;
        *'"error"'*'"id":"unknown"'*)
            printf '%s\n' method-not-found >> "$trace"
            ;;
        *'"method":"textDocument/didOpen"'*)
            printf '%s\n' did-open >> "$trace"
            ;;
        *'"method":"textDocument/didClose"'*)
            printf '%s\n' did-close >> "$trace"
            ;;
        *'"method":"textDocument/definition"'*)
            id=$(request_id "$message")
            write_message '{"jsonrpc":"2.0","id":'"$id"',"result":{"uri":"__TARGET_URI__","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}}}}'
            ;;
        *'"method":"textDocument/references"'*)
            id=$(request_id "$message")
            references=$((references + 1))
            if [ "$references" -eq 2 ]; then
                sleep 0.2
            fi
            write_message '{"jsonrpc":"2.0","id":'"$id"',"result":[{"uri":"__TARGET_URI__","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}}},{"uri":"__CALLER_URI__","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}}}]}'
            ;;
        *'"method":"textDocument/implementation"'*)
            id=$(request_id "$message")
            write_message '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32001,"message":"not impl\u0065mented"}}'
            ;;
        *'"method":"textDocument/documentSymbol"'*)
            id=$(request_id "$message")
            write_message '{"jsonrpc":"2.0","id":'"$id"',"result":[{"name":"module","kind":2,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}},"selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"children":[{"name":"child","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}},"selectionRange":{"start":{"line":0,"character":3},"end":{"line":0,"character":9}}}]}]}'
            ;;
        *'"method":"$/cancelRequest"'*)
            printf '%s\n' cancelled >> "$trace"
            ;;
        *'"method":"shutdown"'*)
            id=$(request_id "$message")
            write_message '{"jsonrpc":"2.0","id":'"$id"',"result":null}'
            ;;
        *'"method":"exit"'*)
            printf '%s\n' orderly-exit >> "$trace"
            exit 0
            ;;
        *)
            printf 'unexpected: %s\n' "$message" >> "$trace"
            ;;
    esac
done
"#
        .replace("__TRACE__", trace.to_str().unwrap())
        .replace("__CALLER_URI__", caller_uri.as_str())
        .replace("__TARGET_URI__", target_uri.as_str());
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let script = script.canonicalize().unwrap();

        let config = container.path().join("lsp.json");
        fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "enabled": true,
                "servers": {
                    "rust": {"enabled": true, "program": script.clone(), "args": []}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let config = config.canonicalize().unwrap();
        // SAFETY: this test serializes the process-wide configuration mutation.
        unsafe { std::env::set_var("LATTELENS_LSP_CONFIG", &config) };
        let loaded = NavigationSettings::load_user_config(&workspace);
        // SAFETY: paired with the serialized set above before any assertion can panic.
        unsafe { std::env::remove_var("LATTELENS_LSP_CONFIG") };
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert!(loaded.settings.is_enabled());

        let settings = loaded.settings;
        let mut branches = manager(&workspace);
        branches.settings = settings.clone();
        let markdown = source(&workspace, "README.md", "# title\n");
        branches.handle_request(request(90, NavigationOperation::Definition, markdown));
        assert!(matches!(
            branches.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Unavailable(_)
        ));
        branches.completions.lock().unwrap().clear();
        branches.completion_permits.lock().unwrap().queued.clear();

        let key = SessionKey {
            server_root: workspace.clone(),
            family: LanguageFamily::Rust,
        };
        branches
            .permanent_failures
            .insert(key.clone(), "permanent".to_owned());
        branches.handle_request(request(
            91,
            NavigationOperation::Definition,
            Arc::clone(&caller),
        ));
        assert!(matches!(
            branches.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("disabled")
        ));
        branches.completions.lock().unwrap().clear();
        branches.completion_permits.lock().unwrap().queued.clear();
        branches.permanent_failures.clear();
        branches.backoffs.insert(
            key.clone(),
            SessionBackoff {
                attempt: 1,
                retry_at: Instant::now() + Duration::from_secs(1),
                error: "restart".to_owned(),
            },
        );
        branches.handle_request(request(
            92,
            NavigationOperation::Definition,
            Arc::clone(&caller),
        ));
        assert!(matches!(
            branches.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("restarting")
        ));
        branches.completions.lock().unwrap().clear();
        branches.completion_permits.lock().unwrap().queued.clear();
        branches.backoffs.clear();
        let quarantine_budget = PayloadBudget::session(&branches.global_budget);
        let (quarantine_key, _) =
            starting_initialize_session(&mut branches, &workspace, quarantine_budget);
        let quarantined = branches.sessions.remove(&quarantine_key).unwrap();
        branches.retain_quarantined(quarantined);
        branches.handle_request(request(
            93,
            NavigationOperation::Definition,
            Arc::clone(&caller),
        ));
        assert!(matches!(
            branches.completions.lock().unwrap().back().unwrap().result,
            NavigationProtocolResult::Failed(ref message) if message.contains("quarantined")
        ));

        let runtime = NavigationRuntime::start(workspace.clone(), settings.clone()).unwrap();
        let probe = runtime.cleanup_probe();
        let wait_completion = |generation| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(completion) = runtime
                    .take_completions()
                    .into_iter()
                    .find(|completion| completion.generation == generation)
                {
                    break completion;
                }
                assert!(
                    Instant::now() < deadline,
                    "completion {generation} timed out"
                );
                thread::sleep(Duration::from_millis(5));
            }
        };

        runtime
            .request(request(
                0,
                NavigationOperation::References,
                Arc::clone(&caller),
            ))
            .unwrap();
        runtime
            .request(request(
                1,
                NavigationOperation::Definition,
                Arc::clone(&caller),
            ))
            .unwrap();
        assert!(matches!(
            wait_completion(0).result,
            NavigationProtocolResult::Cancelled
        ));
        let definition = wait_completion(1);
        assert!(
            matches!(
                definition.result,
                NavigationProtocolResult::Locations(ref locations)
                    if locations.len() == 1 && locations[0].uri == target_uri
            ),
            "{definition:?}"
        );

        runtime
            .request(request(
                2,
                NavigationOperation::References,
                Arc::clone(&caller),
            ))
            .unwrap();
        let references = wait_completion(2);
        assert!(matches!(
            references.result,
            NavigationProtocolResult::Locations(ref locations)
                if locations.len() == 2 && locations[0].uri == caller_uri
                    && locations[1].uri == target_uri
        ));

        runtime
            .request(request(
                3,
                NavigationOperation::Implementations,
                Arc::clone(&caller),
            ))
            .unwrap();
        assert!(matches!(
            wait_completion(3).result,
            NavigationProtocolResult::Failed(ref message) if message == "not implemented"
        ));

        runtime
            .request(request(
                4,
                NavigationOperation::DocumentSymbols,
                Arc::clone(&caller),
            ))
            .unwrap();
        let symbol_deadline = Instant::now() + Duration::from_secs(5);
        let symbols = loop {
            if let Some(completion) = runtime
                .take_symbol_completions()
                .into_iter()
                .find(|completion| completion.generation == 4)
            {
                break completion.symbols;
            }
            assert!(Instant::now() < symbol_deadline, "symbols timed out");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[1].parent, Some(0));

        runtime
            .request(request(5, NavigationOperation::Definition, second))
            .unwrap();
        assert!(matches!(
            wait_completion(5).result,
            NavigationProtocolResult::Locations(ref locations) if locations.len() == 1
        ));

        runtime
            .request(request(
                6,
                NavigationOperation::References,
                Arc::clone(&caller),
            ))
            .unwrap();
        runtime.cancel(6);
        assert!(matches!(
            wait_completion(6).result,
            NavigationProtocolResult::Cancelled
        ));

        let trace_deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(&trace).is_ok_and(|trace| {
            [
                "config-valid",
                "config-invalid",
                "method-not-found",
                "did-close",
                "cancelled",
            ]
            .into_iter()
            .all(|marker| trace.lines().any(|line| line == marker))
        }) {
            assert!(
                Instant::now() < trace_deadline,
                "server calls did not converge: {}",
                fs::read_to_string(&trace).unwrap_or_else(|error| error.to_string())
            );
            thread::sleep(Duration::from_millis(5));
        }

        drop(runtime);
        let snapshot = probe
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot;
        assert_eq!(snapshot.sessions_cleaned, 1);
        assert_eq!(snapshot.clean_exits, 1);
        assert_eq!(snapshot.forced_tree_cleanups, 0);
        assert_eq!(snapshot.io_threads_joined, IO_THREAD_COUNT);
        assert_eq!(snapshot.process_owners_dropped, 1);
        assert!(
            fs::read_to_string(&trace)
                .unwrap()
                .lines()
                .any(|line| line == "orderly-exit")
        );

        fs::remove_file(script).unwrap();
        let mut failed_spawn = manager(&workspace);
        failed_spawn.settings = settings;
        failed_spawn.handle_request(request(
            94,
            NavigationOperation::Definition,
            Arc::clone(&caller),
        ));
        assert!(matches!(
            failed_spawn
                .completions
                .lock()
                .unwrap()
                .back()
                .unwrap()
                .result,
            NavigationProtocolResult::Failed(_)
        ));
        assert!(failed_spawn.backoffs.contains_key(&key));
    }
}
