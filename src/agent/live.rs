use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    BoundedText, BoundedVec, EventEnvelope, InstallId, LIVE_PROTOCOL_MAJOR, LIVE_PROTOCOL_MINOR,
    LiveIngressPolicy, LiveObservationPublisher, LiveObservationReceiver, MetadataError,
    ObservationFrame, PublishOutcome, StableDigest, WorkspaceHint, create_private_directories,
    set_no_follow, set_private_file_options, set_private_file_permissions, stable_hash,
    validate_absolute_state_root,
};

#[cfg(unix)]
use super::{UnixLivePublisher, UnixLiveReceiver};
#[cfg(windows)]
use super::{WindowsNamedPipePublisher, WindowsNamedPipeReceiver};

pub const MAX_REGISTERED_LIVE_RECEIVERS: usize = 16;
const MAX_RECEIVER_MANIFEST_BYTES: usize = 4 * 1024;
const RECEIVER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const RECEIVER_LEASE_TTL: Duration = Duration::from_secs(15);
static RECEIVER_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LiveEndpoint {
    Unix(PathBuf),
    Windows(BoundedText<256>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveReceiverManifest {
    pub endpoint: LiveEndpoint,
    pub receiver_generation: u64,
    pub selected_workspaces: BoundedVec<WorkspaceHint, 32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveRegistryError {
    Unavailable,
    UnsafeRuntimeRoot,
    InvalidManifest,
    BoundsExceeded,
}

impl fmt::Display for LiveRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "live receiver registry failed: {self:?}")
    }
}

impl Error for LiveRegistryError {}

impl From<MetadataError> for LiveRegistryError {
    fn from(error: MetadataError) -> Self {
        match error {
            MetadataError::UnsafeStateRoot => Self::UnsafeRuntimeRoot,
            _ => Self::Unavailable,
        }
    }
}

/// Resolve the ephemeral, current-user runtime root shared by Lens and Hook.
pub fn resolve_runtime_root_from_environment() -> Result<PathBuf, LiveRegistryError> {
    if let Some(path) = env::var_os("LATTE_LENS_RUNTIME_DIR") {
        return validate_absolute_state_root(PathBuf::from(path)).map_err(Into::into);
    }
    #[cfg(unix)]
    {
        if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
            return validate_absolute_state_root(PathBuf::from(path).join("latte-lens"))
                .map_err(Into::into);
        }
        // SAFETY: geteuid has no preconditions and is used only to partition
        // a private temporary runtime directory.
        let uid = unsafe { libc::geteuid() };
        validate_absolute_state_root(env::temp_dir().join(format!("latte-lens-{uid}")))
            .map_err(Into::into)
    }
    #[cfg(windows)]
    {
        validate_absolute_state_root(env::temp_dir().join("latte-lens")).map_err(Into::into)
    }
}

/// Current-user registry containing one lease per running Lens instance.
#[derive(Clone)]
pub struct FilesystemLiveReceiverRegistry {
    runtime_root: PathBuf,
    install: InstallId,
}

impl FilesystemLiveReceiverRegistry {
    pub fn new(runtime_root: PathBuf, install: InstallId) -> Result<Self, LiveRegistryError> {
        Ok(Self {
            runtime_root: validate_absolute_state_root(runtime_root)?,
            install,
        })
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    fn install_root(&self) -> PathBuf {
        self.runtime_root
            .join("installs")
            .join(self.install.digest().to_hex())
    }

    fn receiver_root(&self) -> PathBuf {
        self.install_root().join("receivers")
    }

    #[cfg(unix)]
    fn endpoint_root(&self) -> PathBuf {
        self.runtime_root.clone()
    }

    fn ensure_layout(&self) -> Result<(), LiveRegistryError> {
        create_private_directories(&self.runtime_root, &self.receiver_root())?;
        #[cfg(unix)]
        create_private_directories(&self.runtime_root, &self.endpoint_root())?;
        Ok(())
    }

    fn register(
        &self,
        receiver_id: &str,
        manifest: &LiveReceiverManifest,
    ) -> Result<LiveReceiverLease, LiveRegistryError> {
        self.ensure_layout()?;
        let path = self.receiver_root().join(format!("{receiver_id}.receiver"));
        let body = encode_manifest(manifest)?;
        write_manifest(&path, &body, true)?;
        Ok(LiveReceiverLease {
            path,
            body,
            next_refresh: Instant::now() + RECEIVER_HEARTBEAT_INTERVAL,
            active: true,
        })
    }

    pub fn discover_matching(
        &self,
        workspace_hints: &[WorkspaceHint],
    ) -> Result<Vec<LiveReceiverManifest>, LiveRegistryError> {
        let directory = self.receiver_root();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(LiveRegistryError::Unavailable),
        };
        let requested = workspace_hints.iter().collect::<BTreeSet<_>>();
        let now = SystemTime::now();
        let mut candidates = Vec::new();
        for entry in entries.take(MAX_REGISTERED_LIVE_RECEIVERS.saturating_mul(4)) {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            if now.duration_since(modified).unwrap_or_default() > RECEIVER_LEASE_TTL {
                let _ = fs::remove_file(path);
                continue;
            }
            candidates.push((modified, entry.path()));
        }
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

        let mut manifests = Vec::new();
        for (_, path) in candidates {
            if manifests.len() == MAX_REGISTERED_LIVE_RECEIVERS {
                break;
            }
            let Ok(bytes) = read_manifest(&path) else {
                continue;
            };
            let Ok(manifest) = decode_manifest(&bytes) else {
                continue;
            };
            if manifest
                .selected_workspaces
                .iter()
                .any(|workspace| requested.contains(workspace))
            {
                manifests.push(manifest);
            }
        }
        Ok(manifests)
    }
}

struct LiveReceiverLease {
    path: PathBuf,
    body: Vec<u8>,
    next_refresh: Instant,
    active: bool,
}

impl LiveReceiverLease {
    fn refresh_if_due(&mut self) {
        if self.active
            && Instant::now() >= self.next_refresh
            && write_manifest(&self.path, &self.body, false).is_ok()
        {
            self.next_refresh = Instant::now() + RECEIVER_HEARTBEAT_INTERVAL;
        }
    }

    fn remove(&mut self) {
        if self.active {
            self.active = false;
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Drop for LiveReceiverLease {
    fn drop(&mut self) {
        self.remove();
    }
}

struct RegisteredLiveReceiver {
    receiver: Box<dyn LiveObservationReceiver>,
    lease: LiveReceiverLease,
}

impl LiveObservationReceiver for RegisteredLiveReceiver {
    fn receive(&mut self, deadline: Instant) -> super::ReceiveOutcome {
        self.lease.refresh_if_due();
        self.receiver.receive(deadline)
    }

    fn begin_draining(&mut self) {
        self.lease.remove();
        self.receiver.begin_draining();
    }
}

/// Bind a unique receiver and publish its lease. No install-wide owner exists;
/// every Lens process receives its own endpoint and can observe concurrently.
pub fn bind_registered_live_receiver(
    registry: &FilesystemLiveReceiverRegistry,
    policy: std::sync::Arc<LiveIngressPolicy>,
    selected_workspaces: &[WorkspaceHint],
) -> Result<Box<dyn LiveObservationReceiver>, LiveRegistryError> {
    registry.ensure_layout()?;
    let receiver_id = unique_receiver_id();
    #[cfg(unix)]
    let (endpoint, receiver): (LiveEndpoint, Box<dyn LiveObservationReceiver>) = {
        let endpoint = registry
            .endpoint_root()
            .join(format!("r-{}.sock", &receiver_id[..12]));
        let receiver = UnixLiveReceiver::bind(endpoint.clone(), std::sync::Arc::clone(&policy))
            .map_err(|_| LiveRegistryError::Unavailable)?;
        (LiveEndpoint::Unix(endpoint), Box::new(receiver))
    };
    #[cfg(windows)]
    let (endpoint, receiver): (LiveEndpoint, Box<dyn LiveObservationReceiver>) = {
        let install = &registry.install.digest().to_hex()[..16];
        let name = BoundedText::try_new(format!(
            r"\\.\pipe\latte-lens-{install}-{}",
            &receiver_id[..24]
        ))
        .map_err(|_| LiveRegistryError::BoundsExceeded)?;
        let receiver = WindowsNamedPipeReceiver::bind(name.clone(), std::sync::Arc::clone(&policy))
            .map_err(|_| LiveRegistryError::Unavailable)?;
        (LiveEndpoint::Windows(name), Box::new(receiver))
    };
    let selected_workspaces = BoundedVec::try_from_vec(
        selected_workspaces
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    )
    .map_err(|_| LiveRegistryError::BoundsExceeded)?;
    let manifest = LiveReceiverManifest {
        endpoint,
        receiver_generation: policy.receiver_generation(),
        selected_workspaces,
    };
    let lease = registry.register(&receiver_id, &manifest)?;
    Ok(Box::new(RegisteredLiveReceiver { receiver, lease }))
}

/// Publisher used by the Hook path to fan one event out to every matching Lens.
pub struct RegistryLivePublisher {
    registry: FilesystemLiveReceiverRegistry,
    workspace: WorkspaceHint,
}

impl RegistryLivePublisher {
    pub fn new(registry: FilesystemLiveReceiverRegistry, workspace: WorkspaceHint) -> Self {
        Self {
            registry,
            workspace,
        }
    }

    fn publish_to_manifest(
        manifest: &LiveReceiverManifest,
        event: &EventEnvelope,
        deadline: Instant,
    ) -> PublishOutcome {
        match &manifest.endpoint {
            #[cfg(unix)]
            LiveEndpoint::Unix(endpoint) => {
                UnixLivePublisher::new(endpoint.clone()).publish(event, deadline)
            }
            #[cfg(windows)]
            LiveEndpoint::Windows(endpoint) => {
                WindowsNamedPipePublisher::new(endpoint.clone()).publish(event, deadline)
            }
            #[allow(unreachable_patterns)]
            _ => PublishOutcome::Unavailable,
        }
    }
}

impl LiveObservationPublisher for RegistryLivePublisher {
    fn publish(&self, event: &EventEnvelope, deadline: Instant) -> PublishOutcome {
        let Ok(frame) = ObservationFrame::new(event.clone()) else {
            return PublishOutcome::Rejected;
        };
        if frame.workspace_hints.iter().next() != Some(&self.workspace) {
            return PublishOutcome::NotMember;
        }
        let routes = std::slice::from_ref(&self.workspace);
        let Ok(manifests) = self.registry.discover_matching(routes) else {
            return PublishOutcome::Unavailable;
        };
        if manifests.is_empty() {
            return PublishOutcome::Unavailable;
        }
        let mut accepted_generation: Option<u64> = None;
        let mut accepted = 0_u16;
        let mut strongest_failure = PublishOutcome::Unavailable;
        let outcomes = thread::scope(|scope| {
            manifests
                .iter()
                .map(|manifest| {
                    scope.spawn(move || Self::publish_to_manifest(manifest, event, deadline))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|worker| worker.join().unwrap_or(PublishOutcome::Unavailable))
                .collect::<Vec<_>>()
        });
        for outcome in outcomes {
            match outcome {
                PublishOutcome::Accepted {
                    receiver_generation,
                } => {
                    accepted = accepted.saturating_add(1);
                    accepted_generation = Some(
                        accepted_generation
                            .unwrap_or_default()
                            .max(receiver_generation),
                    );
                }
                failure => strongest_failure = stronger_failure(strongest_failure, failure),
            }
        }
        if usize::from(accepted) == manifests.len() {
            PublishOutcome::Accepted {
                receiver_generation: accepted_generation.unwrap_or_default(),
            }
        } else if accepted > 0 {
            PublishOutcome::Partial {
                accepted,
                attempted: manifests.len().min(usize::from(u16::MAX)) as u16,
            }
        } else {
            strongest_failure
        }
    }
}

fn stronger_failure(current: PublishOutcome, candidate: PublishOutcome) -> PublishOutcome {
    fn rank(outcome: PublishOutcome) -> u8 {
        match outcome {
            PublishOutcome::Busy => 5,
            PublishOutcome::Incompatible => 4,
            PublishOutcome::Rejected => 3,
            PublishOutcome::NotMember => 2,
            PublishOutcome::Unavailable
            | PublishOutcome::Accepted { .. }
            | PublishOutcome::Partial { .. } => 1,
        }
    }
    if rank(candidate) > rank(current) {
        candidate
    } else {
        current
    }
}

fn unique_receiver_id() -> String {
    let pid = std::process::id().to_be_bytes();
    let counter = RECEIVER_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .to_be_bytes();
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_be_bytes();
    stable_hash(b"live-receiver", &[&pid, &counter, &time]).to_hex()
}

fn encode_manifest(manifest: &LiveReceiverManifest) -> Result<Vec<u8>, LiveRegistryError> {
    let endpoint = match &manifest.endpoint {
        LiveEndpoint::Unix(path) => format!(
            "unix:{}",
            path.to_str().ok_or(LiveRegistryError::InvalidManifest)?
        ),
        LiveEndpoint::Windows(name) => format!("pipe:{}", name.as_str()),
    };
    if endpoint.contains(['\n', '\r']) || manifest.selected_workspaces.is_empty() {
        return Err(LiveRegistryError::InvalidManifest);
    }
    let mut body = format!(
        "LLAR 1\nendpoint={endpoint}\ngeneration={}\nprotocol={LIVE_PROTOCOL_MAJOR}.{LIVE_PROTOCOL_MINOR}\n",
        manifest.receiver_generation
    );
    for workspace in &manifest.selected_workspaces {
        body.push_str("workspace=");
        body.push_str(&workspace.digest().to_hex());
        body.push('\n');
    }
    if body.len() > MAX_RECEIVER_MANIFEST_BYTES {
        return Err(LiveRegistryError::BoundsExceeded);
    }
    Ok(body.into_bytes())
}

fn decode_manifest(bytes: &[u8]) -> Result<LiveReceiverManifest, LiveRegistryError> {
    if bytes.len() > MAX_RECEIVER_MANIFEST_BYTES {
        return Err(LiveRegistryError::BoundsExceeded);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| LiveRegistryError::InvalidManifest)?;
    let mut lines = text.lines();
    if lines.next() != Some("LLAR 1") {
        return Err(LiveRegistryError::InvalidManifest);
    }
    let endpoint = lines
        .next()
        .and_then(|line| line.strip_prefix("endpoint="))
        .ok_or(LiveRegistryError::InvalidManifest)?;
    let endpoint = if let Some(path) = endpoint.strip_prefix("unix:") {
        LiveEndpoint::Unix(PathBuf::from(path))
    } else if let Some(name) = endpoint.strip_prefix("pipe:") {
        LiveEndpoint::Windows(
            BoundedText::try_new(name).map_err(|_| LiveRegistryError::BoundsExceeded)?,
        )
    } else {
        return Err(LiveRegistryError::InvalidManifest);
    };
    let receiver_generation = lines
        .next()
        .and_then(|line| line.strip_prefix("generation="))
        .and_then(|value| value.parse().ok())
        .ok_or(LiveRegistryError::InvalidManifest)?;
    let protocol = format!("protocol={LIVE_PROTOCOL_MAJOR}.{LIVE_PROTOCOL_MINOR}");
    if lines.next() != Some(protocol.as_str()) {
        return Err(LiveRegistryError::InvalidManifest);
    }
    let mut workspaces = BTreeSet::new();
    for line in lines {
        let value = line
            .strip_prefix("workspace=")
            .ok_or(LiveRegistryError::InvalidManifest)?;
        workspaces.insert(WorkspaceHint::from_digest(
            StableDigest::parse_hex(value).map_err(|_| LiveRegistryError::InvalidManifest)?,
        ));
    }
    if workspaces.is_empty() || workspaces.len() > 32 {
        return Err(LiveRegistryError::BoundsExceeded);
    }
    Ok(LiveReceiverManifest {
        endpoint,
        receiver_generation,
        selected_workspaces: BoundedVec::try_from_vec(workspaces.into_iter().collect())
            .map_err(|_| LiveRegistryError::BoundsExceeded)?,
    })
}

fn write_manifest(path: &Path, bytes: &[u8], create_new: bool) -> Result<(), LiveRegistryError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .truncate(!create_new)
        .create_new(create_new);
    set_private_file_options(&mut options);
    let mut file = options
        .open(path)
        .map_err(|_| LiveRegistryError::Unavailable)?;
    set_private_file_permissions(path)?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|_| LiveRegistryError::Unavailable)
}

fn read_manifest(path: &Path) -> Result<Vec<u8>, LiveRegistryError> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_no_follow(&mut options);
    let mut file = options
        .open(path)
        .map_err(|_| LiveRegistryError::Unavailable)?;
    let mut bytes = Vec::with_capacity(512);
    Read::by_ref(&mut file)
        .take(MAX_RECEIVER_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| LiveRegistryError::Unavailable)?;
    if bytes.len() > MAX_RECEIVER_MANIFEST_BYTES {
        return Err(LiveRegistryError::BoundsExceeded);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(byte: u8) -> WorkspaceHint {
        WorkspaceHint::from_digest(StableDigest::from_bytes([byte; 32]))
    }

    #[test]
    fn manifest_round_trip_keeps_only_safe_endpoint_and_workspace_hints() {
        let manifest = LiveReceiverManifest {
            endpoint: LiveEndpoint::Unix(PathBuf::from("/tmp/lens.sock")),
            receiver_generation: 9,
            selected_workspaces: BoundedVec::try_from_vec(vec![workspace(1), workspace(2)])
                .expect("workspaces"),
        };
        let bytes = encode_manifest(&manifest).expect("encode");
        assert_eq!(decode_manifest(&bytes), Ok(manifest));
    }

    #[test]
    fn registry_filters_receivers_by_workspace_membership() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = FilesystemLiveReceiverRegistry::new(
            directory.path().to_path_buf(),
            InstallId::from_digest(StableDigest::from_bytes([9; 32])),
        )
        .expect("registry");
        let first = LiveReceiverManifest {
            endpoint: LiveEndpoint::Unix(PathBuf::from("/tmp/first.sock")),
            receiver_generation: 1,
            selected_workspaces: BoundedVec::try_from_vec(vec![workspace(1)]).expect("first"),
        };
        let second = LiveReceiverManifest {
            endpoint: LiveEndpoint::Unix(PathBuf::from("/tmp/second.sock")),
            receiver_generation: 2,
            selected_workspaces: BoundedVec::try_from_vec(vec![workspace(2)]).expect("second"),
        };
        let _first = registry.register("first", &first).expect("first lease");
        let _second = registry.register("second", &second).expect("second lease");

        assert_eq!(
            registry
                .discover_matching(&[workspace(2)])
                .expect("discover"),
            vec![second]
        );
    }

    #[test]
    fn registry_filters_before_applying_the_sixteen_receiver_cap() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = FilesystemLiveReceiverRegistry::new(
            directory.path().to_path_buf(),
            InstallId::from_digest(StableDigest::from_bytes([8; 32])),
        )
        .expect("registry");
        let matching = LiveReceiverManifest {
            endpoint: LiveEndpoint::Unix(PathBuf::from("/tmp/matching.sock")),
            receiver_generation: 1,
            selected_workspaces: BoundedVec::try_from_vec(vec![workspace(1)]).expect("matching"),
        };
        let mut leases = vec![
            registry
                .register("old-matching", &matching)
                .expect("matching lease"),
        ];
        for index in 0..MAX_REGISTERED_LIVE_RECEIVERS {
            let irrelevant = LiveReceiverManifest {
                endpoint: LiveEndpoint::Unix(PathBuf::from(format!(
                    "/tmp/irrelevant-{index}.sock"
                ))),
                receiver_generation: index as u64 + 2,
                selected_workspaces: BoundedVec::try_from_vec(vec![workspace(2)])
                    .expect("irrelevant"),
            };
            leases.push(
                registry
                    .register(&format!("new-irrelevant-{index}"), &irrelevant)
                    .expect("irrelevant lease"),
            );
        }
        assert_eq!(
            registry
                .discover_matching(&[workspace(1)])
                .expect("discover matching"),
            vec![matching]
        );
        drop(leases);
    }

    #[test]
    fn registry_returns_at_most_sixteen_matching_receivers() {
        let directory = tempfile::tempdir().expect("tempdir");
        let registry = FilesystemLiveReceiverRegistry::new(
            directory.path().to_path_buf(),
            InstallId::from_digest(StableDigest::from_bytes([7; 32])),
        )
        .expect("registry");
        let mut leases = Vec::new();
        for index in 0..=MAX_REGISTERED_LIVE_RECEIVERS {
            let manifest = LiveReceiverManifest {
                endpoint: LiveEndpoint::Unix(PathBuf::from(format!("/tmp/matching-{index}.sock"))),
                receiver_generation: index as u64,
                selected_workspaces: BoundedVec::try_from_vec(vec![workspace(1)])
                    .expect("matching"),
            };
            leases.push(
                registry
                    .register(&format!("matching-{index}"), &manifest)
                    .expect("lease"),
            );
        }
        assert_eq!(
            registry
                .discover_matching(&[workspace(1)])
                .expect("discover")
                .len(),
            MAX_REGISTERED_LIVE_RECEIVERS
        );
        drop(leases);
    }
}
