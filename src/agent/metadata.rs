use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use super::{
    AgentKey, AgentKind, AgentObservation, AgentRef, BoundedVec, HmacIdentityKeyer, InstallId,
    LifecycleOp, ObservationKind, ObserverId, ReportedActivityState, ReportedSessionLifecycle,
    SensitiveId, SessionKey, SessionRef, StableDigest, SubjectNamespace, Timestamp,
    ValidatedEnvelope, WorkspaceHint, WorkspaceSelector, stable_hash,
};

pub const MAX_METADATA_FILE_BYTES: usize = 4 * 1024;
pub const MAX_METADATA_WORKSPACES: usize = 256;
pub const MAX_METADATA_SESSIONS: usize = 4_096;
pub const MAX_METADATA_SESSIONS_PER_WORKSPACE: usize = 256;
pub const MAX_METADATA_AGENTS: usize = 32;
pub const MAX_METADATA_OBSERVERS: usize = 4;
pub const DEFAULT_METADATA_WRITE_INTERVAL: Duration = Duration::from_secs(2);
pub const DEFAULT_ENDED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEFAULT_NON_TERMINAL_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const DEFAULT_MAINTENANCE_RECORD_BUDGET: usize = 128;

const SESSION_MAGIC: &[u8; 5] = b"LLSM\x01";
const WORKSPACE_MAGIC: &[u8; 5] = b"LLWM\x02";
const CHECKSUM_BYTES: usize = 32;
const INSTALL_SECRET_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataLoadLimits {
    pub max_workspaces: usize,
    pub max_sessions: usize,
    pub max_total_bytes: usize,
}

impl Default for MetadataLoadLimits {
    fn default() -> Self {
        Self {
            max_workspaces: MAX_METADATA_WORKSPACES,
            max_sessions: MAX_METADATA_SESSIONS,
            max_total_bytes: MAX_METADATA_SESSIONS * MAX_METADATA_FILE_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMetadata {
    pub workspace: WorkspaceHint,
    pub last_observed_at: Timestamp,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SessionDiscovery {
    DiscoveredMidSession,
    StartConfirmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycleHint {
    Unknown,
    Open,
    Ended,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityStateHint {
    Unknown,
    Working,
    WaitingPermission,
    Idle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationKindTag {
    Presence,
    Session,
    Lifecycle,
    Activity,
    Turn,
    Permission,
    Tool,
    Agent,
    Change,
    Artifact,
    Presentation,
    Diagnostic,
}

impl ObservationKindTag {
    pub const fn from_observation(observation: &AgentObservation) -> Self {
        match observation.kind {
            ObservationKind::Presence(_) => Self::Presence,
            ObservationKind::Session(_) => Self::Session,
            ObservationKind::Lifecycle(_) => Self::Lifecycle,
            ObservationKind::Activity(_) => Self::Activity,
            ObservationKind::Turn(_) => Self::Turn,
            ObservationKind::Permission(_) => Self::Permission,
            ObservationKind::Tool(_) => Self::Tool,
            ObservationKind::Agent(_) => Self::Agent,
            ObservationKind::Change(_) => Self::Change,
            ObservationKind::Artifact(_) => Self::Artifact,
            ObservationKind::Presentation(_) => Self::Presentation,
            ObservationKind::Diagnostic(_) => Self::Diagnostic,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataWriteClass {
    Structural,
    Activity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSummary {
    pub key: AgentKey,
    pub parent: Option<AgentKey>,
    pub kind: Option<AgentKind>,
    pub first_observed_at: Timestamp,
    pub last_observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSummaryDelta {
    pub agent: AgentRef,
    pub observed_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSummary {
    pub lifecycle: SessionLifecycleHint,
    pub observed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadata {
    pub session: SessionRef,
    pub observers: BoundedVec<ObserverId, MAX_METADATA_OBSERVERS>,
    pub observers_truncated: bool,
    pub discovery: SessionDiscovery,
    pub first_observed_at: Timestamp,
    pub last_observed_at: Timestamp,
    pub lifecycle_hint: SessionLifecycleHint,
    pub last_activity_hint: ActivityStateHint,
    pub last_event_kind: ObservationKindTag,
    pub known_agents: BoundedVec<AgentSummary, MAX_METADATA_AGENTS>,
    pub agents_truncated: bool,
    pub start_observed: bool,
    pub terminal: Option<TerminalSummary>,
    pub generation: u64,
    pub partial: bool,
    pub revived: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadataDelta {
    pub session: SessionRef,
    pub observer: ObserverId,
    pub observed_at: Timestamp,
    pub discovery: Option<SessionDiscovery>,
    pub lifecycle_hint: Option<SessionLifecycleHint>,
    pub activity_hint: Option<ActivityStateHint>,
    pub event_kind: ObservationKindTag,
    pub agents: BoundedVec<AgentSummaryDelta, 8>,
    pub terminal: Option<TerminalSummary>,
    pub write_class: MetadataWriteClass,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataSnapshot {
    pub workspaces: BoundedVec<WorkspaceMetadata, MAX_METADATA_WORKSPACES>,
    pub sessions: BoundedVec<SessionMetadata, MAX_METADATA_SESSIONS>,
    pub truncated: bool,
    pub corrupt_records_ignored: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataWriteOutcome {
    Updated,
    SkippedFresh,
    Contended,
    CapacityReached,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub now: Timestamp,
    pub ended_retention_ms: u64,
    pub non_terminal_retention_ms: u64,
    pub max_sessions: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaintenanceBudget {
    pub max_records: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PruneSummary {
    pub inspected: usize,
    pub removed: usize,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataError {
    Unavailable,
    UnsafeStateRoot,
    Corrupt,
    PermissionDenied,
    BoundsExceeded,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "session metadata operation failed: {self:?}")
    }
}

impl Error for MetadataError {}

/// Bounded, metadata-only persistence boundary.
pub trait SessionMetadataStore: Send + Sync {
    fn load_workspace(
        &self,
        selector: &WorkspaceSelector,
        limits: MetadataLoadLimits,
    ) -> Result<MetadataSnapshot, MetadataError>;

    fn merge(&self, delta: &SessionMetadataDelta, deadline: Instant) -> MetadataWriteOutcome;

    fn prune(
        &self,
        policy: &RetentionPolicy,
        budget: MaintenanceBudget,
    ) -> Result<PruneSummary, MetadataError>;
}

/// Convert a validated, normalized envelope into bounded metadata-only deltas.
pub fn project_metadata(envelope: &ValidatedEnvelope) -> BoundedVec<SessionMetadataDelta, 64> {
    let observer = envelope.envelope().stream().observer.clone();
    let mut grouped: BTreeMap<SessionRef, Vec<&AgentObservation>> = BTreeMap::new();
    for observation in envelope.envelope().observations() {
        if let Some(session) = &observation.session {
            grouped
                .entry(session.clone())
                .or_default()
                .push(observation);
        }
    }

    let mut output = BoundedVec::new();
    for (session, observations) in grouped {
        let mut observed_at = Timestamp::from_unix_millis(0);
        let mut discovery = None;
        let mut lifecycle_hint = None;
        let mut activity_hint = None;
        let mut terminal = None;
        let mut event_kind = ObservationKindTag::Session;
        let mut agents = BoundedVec::new();
        let mut structural = false;
        for observation in observations {
            observed_at = observed_at.max(observation.observed_at);
            event_kind = ObservationKindTag::from_observation(observation);
            discovery.get_or_insert(SessionDiscovery::DiscoveredMidSession);
            match observation.kind {
                ObservationKind::Lifecycle(LifecycleOp::Set(lifecycle)) => {
                    structural = true;
                    lifecycle_hint = Some(match lifecycle {
                        ReportedSessionLifecycle::Open => {
                            discovery = Some(SessionDiscovery::StartConfirmed);
                            SessionLifecycleHint::Open
                        }
                        ReportedSessionLifecycle::Ended => SessionLifecycleHint::Ended,
                        ReportedSessionLifecycle::Failed => SessionLifecycleHint::Failed,
                    });
                    if !matches!(lifecycle, ReportedSessionLifecycle::Open) {
                        terminal = Some(TerminalSummary {
                            lifecycle: lifecycle_hint.expect("lifecycle mapped"),
                            observed_at: observation.observed_at,
                        });
                    }
                }
                ObservationKind::Activity(super::ActivityOp::Set(activity)) => {
                    activity_hint = Some(match activity {
                        ReportedActivityState::Working => ActivityStateHint::Working,
                        ReportedActivityState::WaitingPermission => {
                            ActivityStateHint::WaitingPermission
                        }
                        ReportedActivityState::Idle => ActivityStateHint::Idle,
                    });
                }
                ObservationKind::Activity(super::ActivityOp::Clear) => {
                    activity_hint = Some(ActivityStateHint::Unknown);
                }
                ObservationKind::Agent(_) => structural = true,
                _ => {}
            }
            if let Some(agent) = &observation.agent {
                let _ = agents.try_push(AgentSummaryDelta {
                    agent: agent.clone(),
                    observed_at: observation.observed_at,
                });
            }
        }
        let _ = output.try_push(SessionMetadataDelta {
            session,
            observer: observer.clone(),
            observed_at,
            discovery,
            lifecycle_hint,
            activity_hint,
            event_kind,
            agents,
            terminal,
            write_class: if structural {
                MetadataWriteClass::Structural
            } else {
                MetadataWriteClass::Activity
            },
            generation: 0,
        });
    }
    output
}

/// Resolve the durable Lens state root without retaining raw workspace paths.
pub fn resolve_state_root_from_environment() -> Result<PathBuf, MetadataError> {
    if let Some(path) = env::var_os("LATTE_LENS_STATE_DIR") {
        return validate_absolute_state_root(PathBuf::from(path));
    }
    if let Some(home) = env::var_os("LATTE_HOME") {
        return validate_absolute_state_root(PathBuf::from(home).join("lens").join("state"));
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or(MetadataError::Unavailable)?;
    validate_absolute_state_root(
        PathBuf::from(home)
            .join(".latte")
            .join("lens")
            .join("state"),
    )
}

/// Load or create the install-scoped HMAC identity secret under the validated
/// Lens state root. The final file is created atomically and is never exposed
/// through the returned API.
pub fn load_or_create_install_identity(
    state_root: PathBuf,
) -> Result<HmacIdentityKeyer, MetadataError> {
    let state_root = validate_absolute_state_root(state_root)?;
    let identity_root = state_root.join("session-index");
    create_private_directories(&state_root, &identity_root)?;
    let secret_path = identity_root.join("install.key");
    let mut secret = match read_bounded_no_follow(&secret_path, INSTALL_SECRET_BYTES) {
        Ok(bytes) => exact_install_secret(bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_install_secret(&secret_path)?
        }
        Err(error) => return Err(map_io_error(error)),
    };
    let keyer =
        HmacIdentityKeyer::new(SensitiveId::new(&secret)).map_err(|_| MetadataError::Corrupt)?;
    secret.fill(0);
    Ok(keyer)
}

fn exact_install_secret(bytes: Vec<u8>) -> Result<[u8; INSTALL_SECRET_BYTES], MetadataError> {
    bytes.try_into().map_err(|_| MetadataError::Corrupt)
}

fn create_install_secret(path: &Path) -> Result<[u8; INSTALL_SECRET_BYTES], MetadataError> {
    let mut secret = random_install_secret()?;
    let parent = path.parent().ok_or(MetadataError::UnsafeStateRoot)?;
    ensure_directory_not_link(parent)?;
    let suffix = StableDigest::from_bytes(secret).to_hex();
    let temp = parent.join(format!(
        ".install.{}.{}.tmp",
        std::process::id(),
        &suffix[..16]
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_options(&mut options);
    let mut file = options.open(&temp).map_err(map_io_error)?;
    set_private_file_permissions(&temp)?;
    let result = (|| {
        file.write_all(&secret).map_err(map_io_error)?;
        file.flush().map_err(map_io_error)?;
        drop(file);
        match fs::hard_link(&temp, path) {
            Ok(()) => Ok(secret),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                secret.fill(0);
                exact_install_secret(
                    read_bounded_no_follow(path, INSTALL_SECRET_BYTES).map_err(map_io_error)?,
                )
            }
            Err(error) => Err(map_io_error(error)),
        }
    })();
    let _ = fs::remove_file(temp);
    result
}

#[cfg(unix)]
fn random_install_secret() -> Result<[u8; INSTALL_SECRET_BYTES], MetadataError> {
    let mut secret = [0_u8; INSTALL_SECRET_BYTES];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut secret))
        .map_err(map_io_error)?;
    Ok(secret)
}

#[cfg(windows)]
fn random_install_secret() -> Result<[u8; INSTALL_SECRET_BYTES], MetadataError> {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: isize, buffer: *mut u8, length: u32, flags: u32) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let mut secret = [0_u8; INSTALL_SECRET_BYTES];
    // SAFETY: the system RNG writes exactly the provided fixed-size buffer.
    let status = unsafe {
        BCryptGenRandom(
            0,
            secret.as_mut_ptr(),
            INSTALL_SECRET_BYTES as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        Err(MetadataError::Unavailable)
    } else {
        Ok(secret)
    }
}

pub(crate) fn validate_absolute_state_root(path: PathBuf) -> Result<PathBuf, MetadataError> {
    if !path.is_absolute() || contains_parent_component(&path) || is_network_share(&path) {
        return Err(MetadataError::UnsafeStateRoot);
    }
    Ok(path)
}

fn contains_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

#[cfg(windows)]
fn is_network_share(path: &Path) -> bool {
    use std::path::Prefix;
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
    )
}

#[cfg(not(windows))]
const fn is_network_share(_path: &Path) -> bool {
    false
}

/// Filesystem implementation with bounded reads, no-follow files, short
/// per-session locks, checksum validation, and atomic replacement.
pub struct FilesystemMetadataStore {
    state_root: PathBuf,
    install: InstallId,
    write_interval: Duration,
    capacity: MetadataCapacity,
}

#[derive(Clone, Copy)]
struct MetadataCapacity {
    workspaces: usize,
    sessions: usize,
    sessions_per_workspace: usize,
}

impl Default for MetadataCapacity {
    fn default() -> Self {
        Self {
            workspaces: MAX_METADATA_WORKSPACES,
            sessions: MAX_METADATA_SESSIONS,
            sessions_per_workspace: MAX_METADATA_SESSIONS_PER_WORKSPACE,
        }
    }
}

impl FilesystemMetadataStore {
    pub fn new(state_root: PathBuf, install: InstallId) -> Result<Self, MetadataError> {
        Ok(Self {
            state_root: validate_absolute_state_root(state_root)?,
            install,
            write_interval: DEFAULT_METADATA_WRITE_INTERVAL,
            capacity: MetadataCapacity::default(),
        })
    }

    pub fn with_write_interval(mut self, interval: Duration) -> Self {
        self.write_interval = interval;
        self
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    #[cfg(test)]
    fn with_capacity(mut self, workspaces: usize, sessions: usize, per_workspace: usize) -> Self {
        self.capacity = MetadataCapacity {
            workspaces,
            sessions,
            sessions_per_workspace: per_workspace,
        };
        self
    }

    fn install_root(&self) -> PathBuf {
        self.state_root
            .join("session-index")
            .join("installs")
            .join(self.install.digest().to_hex())
    }

    fn workspace_root(&self, workspace: &WorkspaceHint) -> PathBuf {
        self.install_root()
            .join("workspaces")
            .join(workspace.digest().to_hex())
    }

    fn session_path(&self, session: &SessionRef) -> PathBuf {
        self.workspace_root(session.workspace())
            .join("sessions")
            .join(format!("{}.meta", session.key().stable_id().to_hex()))
    }

    fn workspace_metadata_path(&self, workspace: &WorkspaceHint) -> PathBuf {
        self.workspace_root(workspace).join("workspace.meta")
    }

    fn workspace_lock_path(&self, workspace: &WorkspaceHint) -> PathBuf {
        self.install_root()
            .join("locks")
            .join("workspaces")
            .join(format!("{}.lock", workspace.digest().to_hex()))
    }

    fn session_lock_path(&self, session: &SessionRef) -> PathBuf {
        self.install_root()
            .join("locks")
            .join("sessions")
            .join(format!("{}.lock", session.key().stable_id().to_hex()))
    }

    fn ensure_session_directory(&self, session: &SessionRef) -> Result<PathBuf, MetadataError> {
        let directory = self.workspace_root(session.workspace()).join("sessions");
        create_private_directories(&self.state_root, &directory)?;
        create_private_directories(
            &self.state_root,
            &self.install_root().join("locks").join("workspaces"),
        )?;
        create_private_directories(
            &self.state_root,
            &self.install_root().join("locks").join("sessions"),
        )?;
        Ok(directory)
    }

    fn merge_locked(
        &self,
        delta: &SessionMetadataDelta,
        path: &Path,
    ) -> Result<MetadataWriteOutcome, MetadataError> {
        let previous = match read_bounded_no_follow(path, MAX_METADATA_FILE_BYTES) {
            Ok(bytes) => Some(decode_session_metadata(&bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err(MetadataError::Unavailable),
        };

        if previous.is_none() {
            let session_dir = path.parent().ok_or(MetadataError::UnsafeStateRoot)?;
            if count_regular_files_bounded(
                session_dir,
                self.capacity.sessions_per_workspace.saturating_add(1),
            )? >= self.capacity.sessions_per_workspace
            {
                return Ok(MetadataWriteOutcome::CapacityReached);
            }
        }

        let merged = merge_record(previous, delta)?;
        let encoded = encode_session_metadata(&merged)?;
        atomic_replace_private(path, &encoded)?;
        Ok(MetadataWriteOutcome::Updated)
    }

    fn merge_workspace_locked(&self, delta: &SessionMetadataDelta) -> Result<(), MetadataError> {
        let path = self.workspace_metadata_path(delta.session.workspace());
        let previous = match read_bounded_no_follow(&path, MAX_METADATA_FILE_BYTES) {
            Ok(bytes) => Some(decode_workspace_metadata(&bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Err(MetadataError::Unavailable),
        };
        let record = WorkspaceMetadata {
            workspace: delta.session.workspace().clone(),
            last_observed_at: previous.as_ref().map_or(delta.observed_at, |metadata| {
                metadata.last_observed_at.max(delta.observed_at)
            }),
            truncated: previous.as_ref().is_some_and(|metadata| metadata.truncated),
        };
        atomic_replace_private(&path, &encode_workspace_metadata(&record)?)
    }

    fn has_capacity_for_new_session(
        &self,
        workspace: &WorkspaceHint,
    ) -> Result<bool, MetadataError> {
        if self.capacity.workspaces == 0
            || self.capacity.sessions == 0
            || self.capacity.sessions_per_workspace == 0
        {
            return Ok(false);
        }
        let workspaces_root = self.install_root().join("workspaces");
        let (workspace_dirs, truncated) =
            directory_paths_bounded(&workspaces_root, self.capacity.workspaces.saturating_add(1))
                .map_err(map_io_error)?;
        if truncated {
            return Ok(false);
        }

        let workspace_record_exists =
            metadata_file_exists(&self.workspace_metadata_path(workspace))?;
        let mut workspace_count = 0_usize;
        let mut session_count = 0_usize;
        for directory in workspace_dirs {
            if metadata_file_exists(&directory.join("workspace.meta"))? {
                workspace_count = workspace_count.saturating_add(1);
            }
            let sessions = directory.join("sessions");
            let remaining = self
                .capacity
                .sessions
                .saturating_add(1)
                .saturating_sub(session_count);
            session_count = session_count
                .saturating_add(count_regular_files_bounded_optional(&sessions, remaining)?);
            if session_count >= self.capacity.sessions {
                return Ok(false);
            }
        }
        Ok(session_count < self.capacity.sessions
            && (workspace_record_exists || workspace_count < self.capacity.workspaces))
    }
}

impl SessionMetadataStore for FilesystemMetadataStore {
    fn load_workspace(
        &self,
        selector: &WorkspaceSelector,
        limits: MetadataLoadLimits,
    ) -> Result<MetadataSnapshot, MetadataError> {
        let workspace_limit = limits.max_workspaces.min(MAX_METADATA_WORKSPACES);
        let session_limit = limits.max_sessions.min(MAX_METADATA_SESSIONS);
        let selected = selector
            .workspaces()
            .iter()
            .map(|workspace| workspace.digest().clone())
            .collect::<BTreeSet<_>>();
        let workspaces_root = self.install_root().join("workspaces");
        let (workspace_dirs, workspace_scan_truncated) =
            directory_paths_bounded(&workspaces_root, MAX_METADATA_WORKSPACES)
                .or_else(|error| {
                    (error.kind() == io::ErrorKind::NotFound)
                        .then_some((Vec::new(), false))
                        .ok_or(error)
                })
                .map_err(|_| MetadataError::Unavailable)?;
        let mut discovered = Vec::<WorkspaceMetadata>::new();
        let mut corrupt_records_ignored = 0_u32;
        for directory in workspace_dirs {
            let metadata_path = directory.join("workspace.meta");
            let metadata = match read_bounded_no_follow(&metadata_path, MAX_METADATA_FILE_BYTES) {
                Ok(bytes) => match decode_workspace_metadata(&bytes) {
                    Ok(metadata) => metadata,
                    Err(_) => {
                        corrupt_records_ignored = corrupt_records_ignored.saturating_add(1);
                        continue;
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
                        continue;
                    };
                    let Ok(digest) = StableDigest::parse_hex(name) else {
                        continue;
                    };
                    let workspace = WorkspaceHint::from_digest(digest.clone());
                    if !selector.workspaces().contains(&workspace) {
                        continue;
                    }
                    WorkspaceMetadata {
                        workspace,
                        last_observed_at: Timestamp::from_unix_millis(0),
                        truncated: true,
                    }
                }
                Err(_) => {
                    corrupt_records_ignored = corrupt_records_ignored.saturating_add(1);
                    continue;
                }
            };
            let matches = selected.contains(metadata.workspace.digest());
            if matches {
                discovered.push(metadata);
            }
        }
        for workspace in selector.workspaces() {
            if !discovered
                .iter()
                .any(|metadata| &metadata.workspace == workspace)
            {
                discovered.push(WorkspaceMetadata {
                    workspace: workspace.clone(),
                    last_observed_at: Timestamp::from_unix_millis(0),
                    truncated: false,
                });
            }
        }
        discovered.sort_by(|left, right| left.workspace.cmp(&right.workspace));
        discovered.dedup_by(|left, right| left.workspace == right.workspace);
        let mut truncated = workspace_scan_truncated || discovered.len() > workspace_limit;
        discovered.truncate(workspace_limit);

        let mut sessions = Vec::new();
        let mut total_bytes = 0_usize;
        for workspace_metadata in &mut discovered {
            let directory = self
                .workspace_root(&workspace_metadata.workspace)
                .join("sessions");
            let remaining = session_limit.saturating_sub(sessions.len());
            let (mut paths, paths_truncated) =
                match regular_file_paths_bounded(&directory, remaining) {
                    Ok(paths) => paths,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (Vec::new(), false),
                    Err(_) => return Err(MetadataError::Unavailable),
                };
            truncated |= paths_truncated;
            paths.sort();
            let mut last_observed_at = Timestamp::from_unix_millis(0);
            for path in paths {
                if sessions.len() == session_limit || total_bytes >= limits.max_total_bytes {
                    truncated = true;
                    break;
                }
                match read_bounded_no_follow(&path, MAX_METADATA_FILE_BYTES) {
                    Ok(bytes) => {
                        if total_bytes.saturating_add(bytes.len()) > limits.max_total_bytes {
                            truncated = true;
                            break;
                        }
                        total_bytes += bytes.len();
                        match decode_session_metadata(&bytes) {
                            Ok(record)
                                if record.session.workspace() == &workspace_metadata.workspace =>
                            {
                                last_observed_at = last_observed_at.max(record.last_observed_at);
                                sessions.push(record);
                            }
                            Ok(_) | Err(_) => {
                                corrupt_records_ignored = corrupt_records_ignored.saturating_add(1);
                            }
                        }
                    }
                    Err(_) => {
                        corrupt_records_ignored = corrupt_records_ignored.saturating_add(1);
                    }
                }
            }
            workspace_metadata.last_observed_at =
                workspace_metadata.last_observed_at.max(last_observed_at);
            workspace_metadata.truncated |= truncated;
        }

        sessions.sort_by(|left, right| {
            right
                .last_observed_at
                .cmp(&left.last_observed_at)
                .then_with(|| left.session.key().cmp(right.session.key()))
        });
        Ok(MetadataSnapshot {
            workspaces: BoundedVec::try_from_vec(discovered)
                .map_err(|_| MetadataError::BoundsExceeded)?,
            sessions: BoundedVec::try_from_vec(sessions)
                .map_err(|_| MetadataError::BoundsExceeded)?,
            truncated,
            corrupt_records_ignored,
        })
    }

    fn merge(&self, delta: &SessionMetadataDelta, deadline: Instant) -> MetadataWriteOutcome {
        if self.ensure_session_directory(&delta.session).is_err() {
            return MetadataWriteOutcome::Failed;
        }
        let path = self.session_path(&delta.session);
        if delta.write_class == MetadataWriteClass::Activity && is_fresh(&path, self.write_interval)
        {
            return MetadataWriteOutcome::SkippedFresh;
        }
        let workspace_lock_path = self.workspace_lock_path(delta.session.workspace());
        let Ok(Some(_workspace_lock)) = FileLock::acquire(&workspace_lock_path, deadline) else {
            return MetadataWriteOutcome::Contended;
        };
        let new_session = match metadata_file_exists(&path) {
            Ok(exists) => !exists,
            Err(_) => return MetadataWriteOutcome::Failed,
        };
        let _capacity_lock = if new_session {
            let capacity_lock_path = self.install_root().join("capacity.lock");
            let Ok(Some(lock)) = FileLock::acquire(&capacity_lock_path, deadline) else {
                return MetadataWriteOutcome::Contended;
            };
            match self.has_capacity_for_new_session(delta.session.workspace()) {
                Ok(true) => Some(lock),
                Ok(false) => return MetadataWriteOutcome::CapacityReached,
                Err(_) => return MetadataWriteOutcome::Failed,
            }
        } else {
            None
        };
        let lock_path = self.session_lock_path(&delta.session);
        let Ok(Some(_lock)) = FileLock::acquire(&lock_path, deadline) else {
            return MetadataWriteOutcome::Contended;
        };
        if self.merge_workspace_locked(delta).is_err() {
            return MetadataWriteOutcome::Failed;
        }
        self.merge_locked(delta, &path)
            .unwrap_or(MetadataWriteOutcome::Failed)
    }

    fn prune(
        &self,
        policy: &RetentionPolicy,
        budget: MaintenanceBudget,
    ) -> Result<PruneSummary, MetadataError> {
        let workspaces_root = self.install_root().join("workspaces");
        create_private_directories(
            &self.state_root,
            &self.install_root().join("locks").join("workspaces"),
        )?;
        let (workspace_dirs, mut truncated) =
            match directory_paths_bounded(&workspaces_root, MAX_METADATA_WORKSPACES) {
                Ok(paths) => paths,
                Err(error) if error.kind() == io::ErrorKind::NotFound => (Vec::new(), false),
                Err(_) => return Err(MetadataError::Unavailable),
            };
        let mut inspected = 0_usize;
        let mut removed = 0_usize;
        let mut retained = 0_usize;
        for workspace in workspace_dirs {
            let Some(workspace_digest) = workspace
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| StableDigest::parse_hex(name).ok())
            else {
                truncated = true;
                continue;
            };
            let workspace_hint = WorkspaceHint::from_digest(workspace_digest);
            let Some(_workspace_lock) = FileLock::acquire(
                &self.workspace_lock_path(&workspace_hint),
                Instant::now() + Duration::from_millis(2),
            )?
            else {
                truncated = true;
                continue;
            };
            let remaining = budget.max_records.saturating_sub(inspected);
            let sessions_directory = workspace.join("sessions");
            let (mut records, records_truncated) =
                match regular_file_paths_bounded(&sessions_directory, remaining) {
                    Ok(records) => records,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => (Vec::new(), false),
                    Err(_) => return Err(MetadataError::Unavailable),
                };
            truncated |= records_truncated;
            records.sort();
            for path in records {
                if inspected == budget.max_records {
                    return Ok(PruneSummary {
                        inspected,
                        removed,
                        truncated,
                    });
                }
                inspected += 1;
                let Ok(bytes) = read_bounded_no_follow(&path, MAX_METADATA_FILE_BYTES) else {
                    continue;
                };
                let Ok(record) = decode_session_metadata(&bytes) else {
                    continue;
                };
                let retention = if record.terminal.is_some() {
                    policy.ended_retention_ms
                } else {
                    policy.non_terminal_retention_ms
                };
                let expired = policy
                    .now
                    .as_unix_millis()
                    .saturating_sub(record.last_observed_at.as_unix_millis())
                    > retention;
                if expired || retained >= policy.max_sessions {
                    fs::remove_file(path).map_err(|_| MetadataError::Unavailable)?;
                    let _ = fs::remove_file(self.session_lock_path(&record.session));
                    removed += 1;
                } else {
                    retained += 1;
                }
            }
            if count_regular_files_bounded_optional(&sessions_directory, 1)? == 0 {
                let workspace_metadata = workspace.join("workspace.meta");
                if metadata_file_exists(&workspace_metadata).unwrap_or(false) {
                    let _ = fs::remove_file(&workspace_metadata);
                }
                let _ = fs::remove_dir(&sessions_directory);
                let _ = fs::remove_dir(&workspace);
            }
        }
        Ok(PruneSummary {
            inspected,
            removed,
            truncated,
        })
    }
}

fn merge_record(
    previous: Option<SessionMetadata>,
    delta: &SessionMetadataDelta,
) -> Result<SessionMetadata, MetadataError> {
    let mut record = previous.unwrap_or_else(|| SessionMetadata {
        session: delta.session.clone(),
        observers: BoundedVec::new(),
        observers_truncated: false,
        discovery: delta
            .discovery
            .unwrap_or(SessionDiscovery::DiscoveredMidSession),
        first_observed_at: delta.observed_at,
        last_observed_at: delta.observed_at,
        lifecycle_hint: SessionLifecycleHint::Unknown,
        last_activity_hint: ActivityStateHint::Unknown,
        last_event_kind: delta.event_kind,
        known_agents: BoundedVec::new(),
        agents_truncated: false,
        start_observed: false,
        terminal: None,
        generation: delta.generation,
        partial: false,
        revived: false,
    });
    if record.session != delta.session {
        return Err(MetadataError::Corrupt);
    }

    if delta.generation < record.generation {
        return Ok(record);
    }
    let is_newer = delta.observed_at >= record.last_observed_at;
    record.first_observed_at = record.first_observed_at.min(delta.observed_at);
    if is_newer {
        if record.terminal.is_some()
            && delta.observed_at > record.last_observed_at
            && delta.terminal.is_none()
        {
            record.revived = true;
        }
        record.last_observed_at = delta.observed_at;
        record.last_event_kind = delta.event_kind;
        if let Some(discovery) = delta.discovery {
            record.discovery = record.discovery.max(discovery);
            record.start_observed |= discovery == SessionDiscovery::StartConfirmed;
        }
        if let Some(lifecycle) = delta.lifecycle_hint
            && lifecycle != SessionLifecycleHint::Unknown
        {
            record.lifecycle_hint = lifecycle;
        }
        if let Some(activity) = delta.activity_hint
            && activity != ActivityStateHint::Unknown
        {
            record.last_activity_hint = activity;
        }
        if let Some(terminal) = delta.terminal
            && record
                .terminal
                .is_none_or(|current| terminal.observed_at >= current.observed_at)
        {
            record.terminal = Some(terminal);
            record.revived = false;
        }
        record.generation = delta.generation;
    }

    let mut observers = record.observers.clone().into_vec();
    if !observers.contains(&delta.observer) {
        observers.push(delta.observer.clone());
        observers.sort();
        if observers.len() > MAX_METADATA_OBSERVERS {
            observers.truncate(MAX_METADATA_OBSERVERS);
            record.observers_truncated = true;
            record.partial = true;
        }
        record.observers =
            BoundedVec::try_from_vec(observers).map_err(|_| MetadataError::BoundsExceeded)?;
    }

    let mut agents = record.known_agents.clone().into_vec();
    for update in &delta.agents {
        if let Some(existing) = agents
            .iter_mut()
            .find(|agent| agent.key == *update.agent.key())
        {
            existing.first_observed_at = existing.first_observed_at.min(update.observed_at);
            if update.observed_at >= existing.last_observed_at {
                existing.last_observed_at = update.observed_at;
                existing.parent = update.agent.parent().cloned();
                existing.kind = update.agent.kind();
            }
        } else {
            agents.push(AgentSummary {
                key: update.agent.key().clone(),
                parent: update.agent.parent().cloned(),
                kind: update.agent.kind(),
                first_observed_at: update.observed_at,
                last_observed_at: update.observed_at,
            });
        }
    }
    agents.sort_by(|left, right| left.key.cmp(&right.key));
    if agents.len() > MAX_METADATA_AGENTS {
        agents.truncate(MAX_METADATA_AGENTS);
        record.agents_truncated = true;
        record.partial = true;
    }
    record.known_agents =
        BoundedVec::try_from_vec(agents).map_err(|_| MetadataError::BoundsExceeded)?;
    Ok(record)
}

fn encode_session_metadata(record: &SessionMetadata) -> Result<Vec<u8>, MetadataError> {
    let mut writer = BinaryWriter::default();
    writer.bytes(SESSION_MAGIC);
    writer.text_u8(record.session.key().subject().as_str())?;
    writer.digest(record.session.key().install_id().digest());
    writer.digest(record.session.key().authority_id().digest());
    writer.digest(record.session.key().stable_id());
    writer.digest(record.session.workspace().digest());
    writer.u8(record.observers.len() as u8);
    for observer in &record.observers {
        writer.text_u8(observer.as_str())?;
    }
    writer.u8(u8::from(record.observers_truncated));
    writer.u8(discovery_tag(record.discovery));
    writer.u64(record.first_observed_at.as_unix_millis());
    writer.u64(record.last_observed_at.as_unix_millis());
    writer.u8(lifecycle_tag(record.lifecycle_hint));
    writer.u8(activity_tag(record.last_activity_hint));
    writer.u8(event_tag(record.last_event_kind));
    writer.u8(record.known_agents.len() as u8);
    for agent in &record.known_agents {
        writer.digest(agent.key.stable_id());
        writer.u8(u8::from(agent.parent.is_some()));
        if let Some(parent) = &agent.parent {
            writer.digest(parent.stable_id());
        }
        writer.u8(match agent.kind {
            None => 0,
            Some(AgentKind::Primary) => 1,
            Some(AgentKind::Subagent) => 2,
        });
        writer.u64(agent.first_observed_at.as_unix_millis());
        writer.u64(agent.last_observed_at.as_unix_millis());
    }
    writer.u8(u8::from(record.agents_truncated));
    writer.u8(u8::from(record.start_observed));
    writer.u8(u8::from(record.terminal.is_some()));
    if let Some(terminal) = record.terminal {
        writer.u8(lifecycle_tag(terminal.lifecycle));
        writer.u64(terminal.observed_at.as_unix_millis());
    }
    writer.u64(record.generation);
    writer.u8(u8::from(record.partial));
    writer.u8(u8::from(record.revived));
    let checksum = stable_hash(b"metadata-checksum", &[&writer.0]);
    writer.digest(&checksum);
    if writer.0.len() > MAX_METADATA_FILE_BYTES {
        return Err(MetadataError::BoundsExceeded);
    }
    Ok(writer.0)
}

fn decode_session_metadata(bytes: &[u8]) -> Result<SessionMetadata, MetadataError> {
    if bytes.len() > MAX_METADATA_FILE_BYTES || bytes.len() < SESSION_MAGIC.len() + CHECKSUM_BYTES {
        return Err(MetadataError::BoundsExceeded);
    }
    let (body, checksum) = bytes.split_at(bytes.len() - CHECKSUM_BYTES);
    let expected = stable_hash(b"metadata-checksum", &[body]);
    if !constant_time_equal(expected.as_bytes(), checksum) {
        return Err(MetadataError::Corrupt);
    }
    let mut reader = BinaryReader::new(body);
    if reader.take(SESSION_MAGIC.len())? != SESSION_MAGIC {
        return Err(MetadataError::Corrupt);
    }
    let subject = SubjectNamespace::parse(reader.text_u8()?).map_err(|_| MetadataError::Corrupt)?;
    let install = InstallId::from_digest(reader.digest()?);
    let authority = super::AuthorityId::from_digest(reader.digest()?);
    let stable_id = reader.digest()?;
    let workspace = WorkspaceHint::from_digest(reader.digest()?);
    let session_key = SessionKey::new(subject, install, authority, stable_id);
    let session = SessionRef::new(session_key.clone(), workspace);
    let observer_count = usize::from(reader.u8()?);
    if observer_count > MAX_METADATA_OBSERVERS {
        return Err(MetadataError::Corrupt);
    }
    let mut observers = Vec::with_capacity(observer_count);
    for _ in 0..observer_count {
        observers.push(ObserverId::parse(reader.text_u8()?).map_err(|_| MetadataError::Corrupt)?);
    }
    let observers_truncated = reader.bool()?;
    let discovery = decode_discovery(reader.u8()?)?;
    let first_observed_at = Timestamp::from_unix_millis(reader.u64()?);
    let last_observed_at = Timestamp::from_unix_millis(reader.u64()?);
    let lifecycle_hint = decode_lifecycle(reader.u8()?)?;
    let last_activity_hint = decode_activity(reader.u8()?)?;
    let last_event_kind = decode_event(reader.u8()?)?;
    let agent_count = usize::from(reader.u8()?);
    if agent_count > MAX_METADATA_AGENTS {
        return Err(MetadataError::Corrupt);
    }
    let mut agents = Vec::with_capacity(agent_count);
    for _ in 0..agent_count {
        let key = AgentKey::new(session_key.clone(), reader.digest()?);
        let parent = if reader.bool()? {
            Some(AgentKey::new(session_key.clone(), reader.digest()?))
        } else {
            None
        };
        let kind = match reader.u8()? {
            0 => None,
            1 => Some(AgentKind::Primary),
            2 => Some(AgentKind::Subagent),
            _ => return Err(MetadataError::Corrupt),
        };
        agents.push(AgentSummary {
            key,
            parent,
            kind,
            first_observed_at: Timestamp::from_unix_millis(reader.u64()?),
            last_observed_at: Timestamp::from_unix_millis(reader.u64()?),
        });
    }
    let agents_truncated = reader.bool()?;
    let start_observed = reader.bool()?;
    let terminal = if reader.bool()? {
        Some(TerminalSummary {
            lifecycle: decode_lifecycle(reader.u8()?)?,
            observed_at: Timestamp::from_unix_millis(reader.u64()?),
        })
    } else {
        None
    };
    let generation = reader.u64()?;
    let partial = reader.bool()?;
    let revived = reader.bool()?;
    if !reader.is_empty() {
        return Err(MetadataError::Corrupt);
    }
    Ok(SessionMetadata {
        session,
        observers: BoundedVec::try_from_vec(observers).map_err(|_| MetadataError::Corrupt)?,
        observers_truncated,
        discovery,
        first_observed_at,
        last_observed_at,
        lifecycle_hint,
        last_activity_hint,
        last_event_kind,
        known_agents: BoundedVec::try_from_vec(agents).map_err(|_| MetadataError::Corrupt)?,
        agents_truncated,
        start_observed,
        terminal,
        generation,
        partial,
        revived,
    })
}

fn encode_workspace_metadata(record: &WorkspaceMetadata) -> Result<Vec<u8>, MetadataError> {
    let mut writer = BinaryWriter::default();
    writer.bytes(WORKSPACE_MAGIC);
    writer.digest(record.workspace.digest());
    writer.u64(record.last_observed_at.as_unix_millis());
    writer.u8(u8::from(record.truncated));
    let checksum = stable_hash(b"workspace-metadata-checksum", &[&writer.0]);
    writer.digest(&checksum);
    if writer.0.len() > MAX_METADATA_FILE_BYTES {
        return Err(MetadataError::BoundsExceeded);
    }
    Ok(writer.0)
}

fn decode_workspace_metadata(bytes: &[u8]) -> Result<WorkspaceMetadata, MetadataError> {
    if bytes.len() > MAX_METADATA_FILE_BYTES || bytes.len() < WORKSPACE_MAGIC.len() + CHECKSUM_BYTES
    {
        return Err(MetadataError::BoundsExceeded);
    }
    let (body, checksum) = bytes.split_at(bytes.len() - CHECKSUM_BYTES);
    let expected = stable_hash(b"workspace-metadata-checksum", &[body]);
    if !constant_time_equal(expected.as_bytes(), checksum) {
        return Err(MetadataError::Corrupt);
    }
    let mut reader = BinaryReader::new(body);
    if reader.take(WORKSPACE_MAGIC.len())? != WORKSPACE_MAGIC {
        return Err(MetadataError::Corrupt);
    }
    let workspace = WorkspaceHint::from_digest(reader.digest()?);
    let last_observed_at = Timestamp::from_unix_millis(reader.u64()?);
    let truncated = reader.bool()?;
    if !reader.is_empty() {
        return Err(MetadataError::Corrupt);
    }
    Ok(WorkspaceMetadata {
        workspace,
        last_observed_at,
        truncated,
    })
}

#[derive(Default)]
struct BinaryWriter(Vec<u8>);

impl BinaryWriter {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }

    fn digest(&mut self, value: &StableDigest) {
        self.bytes(value.as_bytes());
    }

    fn text_u8(&mut self, value: &str) -> Result<(), MetadataError> {
        let length = u8::try_from(value.len()).map_err(|_| MetadataError::BoundsExceeded)?;
        self.u8(length);
        self.bytes(value.as_bytes());
        Ok(())
    }
}

struct BinaryReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BinaryReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], MetadataError> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(MetadataError::Corrupt)?;
        let output = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(output)
    }

    fn u8(&mut self) -> Result<u8, MetadataError> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, MetadataError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(MetadataError::Corrupt),
        }
    }

    fn u64(&mut self) -> Result<u64, MetadataError> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| MetadataError::Corrupt)?,
        ))
    }

    fn digest(&mut self) -> Result<StableDigest, MetadataError> {
        Ok(StableDigest::from_bytes(
            self.take(32)?
                .try_into()
                .map_err(|_| MetadataError::Corrupt)?,
        ))
    }

    fn text_u8(&mut self) -> Result<&'a str, MetadataError> {
        let length = usize::from(self.u8()?);
        std::str::from_utf8(self.take(length)?).map_err(|_| MetadataError::Corrupt)
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

fn discovery_tag(value: SessionDiscovery) -> u8 {
    match value {
        SessionDiscovery::DiscoveredMidSession => 0,
        SessionDiscovery::StartConfirmed => 1,
    }
}

fn decode_discovery(value: u8) -> Result<SessionDiscovery, MetadataError> {
    match value {
        0 => Ok(SessionDiscovery::DiscoveredMidSession),
        1 => Ok(SessionDiscovery::StartConfirmed),
        _ => Err(MetadataError::Corrupt),
    }
}

fn lifecycle_tag(value: SessionLifecycleHint) -> u8 {
    match value {
        SessionLifecycleHint::Unknown => 0,
        SessionLifecycleHint::Open => 1,
        SessionLifecycleHint::Ended => 2,
        SessionLifecycleHint::Failed => 3,
    }
}

fn decode_lifecycle(value: u8) -> Result<SessionLifecycleHint, MetadataError> {
    match value {
        0 => Ok(SessionLifecycleHint::Unknown),
        1 => Ok(SessionLifecycleHint::Open),
        2 => Ok(SessionLifecycleHint::Ended),
        3 => Ok(SessionLifecycleHint::Failed),
        _ => Err(MetadataError::Corrupt),
    }
}

fn activity_tag(value: ActivityStateHint) -> u8 {
    match value {
        ActivityStateHint::Unknown => 0,
        ActivityStateHint::Working => 1,
        ActivityStateHint::WaitingPermission => 2,
        ActivityStateHint::Idle => 3,
    }
}

fn decode_activity(value: u8) -> Result<ActivityStateHint, MetadataError> {
    match value {
        0 => Ok(ActivityStateHint::Unknown),
        1 => Ok(ActivityStateHint::Working),
        2 => Ok(ActivityStateHint::WaitingPermission),
        3 => Ok(ActivityStateHint::Idle),
        _ => Err(MetadataError::Corrupt),
    }
}

fn event_tag(value: ObservationKindTag) -> u8 {
    value as u8
}

fn decode_event(value: u8) -> Result<ObservationKindTag, MetadataError> {
    match value {
        0 => Ok(ObservationKindTag::Presence),
        1 => Ok(ObservationKindTag::Session),
        2 => Ok(ObservationKindTag::Lifecycle),
        3 => Ok(ObservationKindTag::Activity),
        4 => Ok(ObservationKindTag::Turn),
        5 => Ok(ObservationKindTag::Permission),
        6 => Ok(ObservationKindTag::Tool),
        7 => Ok(ObservationKindTag::Agent),
        8 => Ok(ObservationKindTag::Change),
        9 => Ok(ObservationKindTag::Artifact),
        10 => Ok(ObservationKindTag::Presentation),
        11 => Ok(ObservationKindTag::Diagnostic),
        _ => Err(MetadataError::Corrupt),
    }
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |different, (left, right)| different | (left ^ right))
        == 0
}

fn is_fresh(path: &Path, interval: Duration) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < interval)
}

pub(crate) fn create_private_directories(root: &Path, target: &Path) -> Result<(), MetadataError> {
    if !target.starts_with(root) {
        return Err(MetadataError::UnsafeStateRoot);
    }
    if let Some(existing) = nearest_existing_ancestor(root) {
        ensure_directory_not_link(&existing)?;
    }
    let mut current = root.to_path_buf();
    if !current.exists() {
        fs::create_dir_all(&current).map_err(map_io_error)?;
        set_private_directory_permissions(&current)?;
    }
    ensure_directory_not_link(&current)?;
    let relative = target
        .strip_prefix(root)
        .map_err(|_| MetadataError::UnsafeStateRoot)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(MetadataError::UnsafeStateRoot);
        };
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => set_private_directory_permissions(&current)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(map_io_error(error)),
        }
        ensure_directory_not_link(&current)?;
    }
    Ok(())
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.exists())
        .map(Path::to_path_buf)
}

fn ensure_directory_not_link(path: &Path) -> Result<(), MetadataError> {
    let metadata = fs::symlink_metadata(path).map_err(map_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(MetadataError::UnsafeStateRoot);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(MetadataError::UnsafeStateRoot);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), MetadataError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(map_io_error)
}

#[cfg(windows)]
fn set_private_directory_permissions(path: &Path) -> Result<(), MetadataError> {
    set_windows_current_user_acl(path)
}

#[cfg(windows)]
fn set_windows_current_user_acl(path: &Path) -> Result<(), MetadataError> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr::null_mut};

    const SDDL_REVISION_1: u32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor: *const u16,
            revision: u32,
            security_descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn SetFileSecurityW(
            file_name: *const u16,
            security_information: u32,
            security_descriptor: *mut c_void,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    let sddl = "D:P(A;;FA;;;OW)"
        .encode_utf16()
        .chain([0])
        .collect::<Vec<_>>();
    let path = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let mut descriptor = null_mut();
    // SAFETY: SDDL and output storage are valid for the conversion call.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(map_io_error(io::Error::last_os_error()));
    }
    // SAFETY: path is NUL-terminated and descriptor was produced by the
    // conversion API. Only the protected DACL is changed.
    let result = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    // SAFETY: descriptor is LocalAlloc-owned.
    unsafe { LocalFree(descriptor) };
    if result == 0 {
        Err(map_io_error(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn read_bounded_no_follow(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_no_follow(&mut options);
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > limit as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe metadata file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata exceeds cap",
        ));
    }
    Ok(bytes)
}

fn atomic_replace_private(path: &Path, bytes: &[u8]) -> Result<(), MetadataError> {
    if bytes.len() > MAX_METADATA_FILE_BYTES {
        return Err(MetadataError::BoundsExceeded);
    }
    let parent = path.parent().ok_or(MetadataError::UnsafeStateRoot)?;
    ensure_directory_not_link(parent)?;
    let temp_name = format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(MetadataError::UnsafeStateRoot)?,
        std::process::id()
    );
    let temp = parent.join(temp_name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_options(&mut options);
    let mut file = options.open(&temp).map_err(map_io_error)?;
    set_private_file_permissions(&temp)?;
    let result = (|| {
        file.write_all(bytes).map_err(map_io_error)?;
        file.flush().map_err(map_io_error)?;
        drop(file);
        let reparsed =
            read_bounded_no_follow(&temp, MAX_METADATA_FILE_BYTES).map_err(map_io_error)?;
        if reparsed != bytes {
            return Err(MetadataError::Corrupt);
        }
        replace_atomically(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(unix)]
pub(crate) fn set_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
pub(crate) fn set_no_follow(options: &mut OpenOptions) {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(unix)]
pub(crate) fn set_private_file_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
pub(crate) fn set_private_file_options(options: &mut OpenOptions) {
    set_no_follow(options);
}

#[cfg(unix)]
pub(crate) const fn set_private_file_permissions(_path: &Path) -> Result<(), MetadataError> {
    Ok(())
}

#[cfg(windows)]
pub(crate) fn set_private_file_permissions(path: &Path) -> Result<(), MetadataError> {
    set_windows_current_user_acl(path)
}

#[cfg(unix)]
fn replace_atomically(source: &Path, destination: &Path) -> Result<(), MetadataError> {
    fs::rename(source, destination).map_err(map_io_error)
}

#[cfg(windows)]
fn replace_atomically(source: &Path, destination: &Path) -> Result<(), MetadataError> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    let source = source
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    // SAFETY: both paths are NUL-terminated UTF-16 buffers and refer to files
    // in the same private metadata directory.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(map_io_error(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

fn regular_file_paths_bounded(
    directory: &Path,
    max_items: usize,
) -> io::Result<(Vec<PathBuf>, bool)> {
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe directory",
        ));
    }
    let mut output = Vec::new();
    let mut truncated = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".meta"))
        {
            if output.len() == max_items {
                truncated = true;
                break;
            }
            output.push(entry.path());
        }
    }
    output.sort();
    Ok((output, truncated))
}

fn directory_paths_bounded(directory: &Path, max_items: usize) -> io::Result<(Vec<PathBuf>, bool)> {
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe directory",
        ));
    }
    let mut output = Vec::new();
    let mut truncated = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if output.len() == max_items {
            truncated = true;
            break;
        }
        output.push(entry.path());
    }
    output.sort();
    Ok((output, truncated))
}

fn count_regular_files_bounded(directory: &Path, limit: usize) -> Result<usize, MetadataError> {
    if limit == 0 {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(directory).map_err(map_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(MetadataError::UnsafeStateRoot);
    }
    let mut count = 0_usize;
    for entry in fs::read_dir(directory).map_err(map_io_error)? {
        let entry = entry.map_err(map_io_error)?;
        if entry.file_type().map_err(map_io_error)?.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".meta"))
        {
            count += 1;
            if count == limit {
                break;
            }
        }
    }
    Ok(count)
}

fn count_regular_files_bounded_optional(
    directory: &Path,
    limit: usize,
) -> Result<usize, MetadataError> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(MetadataError::UnsafeStateRoot)
        }
        Ok(_) => count_regular_files_bounded(directory, limit),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(map_io_error(error)),
    }
}

fn metadata_file_exists(path: &Path) -> Result<bool, MetadataError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(MetadataError::UnsafeStateRoot)
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(map_io_error(error)),
    }
}

struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire(path: &Path, deadline: Instant) -> Result<Option<Self>, MetadataError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_file_options(&mut options);
        let file = options.open(path).map_err(map_io_error)?;
        set_private_file_permissions(path)?;
        loop {
            match try_lock_file(&file) {
                Ok(true) => return Ok(Some(Self { file })),
                Ok(false) if Instant::now() >= deadline => return Ok(None),
                Ok(false) => std::thread::yield_now(),
                Err(error) => return Err(map_io_error(error)),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unlock_file(&self.file);
    }
}

#[cfg(unix)]
fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock operates on this owned regular-file descriptor.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn unlock_file(file: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: releases only the advisory lock held by this descriptor.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
#[repr(C)]
struct WindowsOverlapped {
    internal: usize,
    internal_high: usize,
    offset_or_pointer: [u8; 8],
    event: *mut std::ffi::c_void,
}

#[cfg(windows)]
fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::{os::windows::io::AsRawHandle, ptr::null_mut};

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFileEx(
            file: *mut std::ffi::c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut WindowsOverlapped,
        ) -> i32;
    }
    let mut overlapped = WindowsOverlapped {
        internal: 0,
        internal_high: 0,
        offset_or_pointer: [0; 8],
        event: null_mut(),
    };
    // SAFETY: the handle is valid for the lifetime of this call and the
    // overlapped structure describes the fixed byte range starting at zero.
    if unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_FAIL_IMMEDIATELY | LOCKFILE_EXCLUSIVE_LOCK,
            0,
            1,
            0,
            &raw mut overlapped,
        )
    } != 0
    {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn unlock_file(file: &File) {
    use std::{os::windows::io::AsRawHandle, ptr::null_mut};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn UnlockFileEx(
            file: *mut std::ffi::c_void,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut WindowsOverlapped,
        ) -> i32;
    }
    let mut overlapped = WindowsOverlapped {
        internal: 0,
        internal_high: 0,
        offset_or_pointer: [0; 8],
        event: null_mut(),
    };
    // SAFETY: this handle owns the matching one-byte lock range.
    unsafe {
        UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &raw mut overlapped);
    }
}

fn map_io_error(error: io::Error) -> MetadataError {
    match error.kind() {
        io::ErrorKind::PermissionDenied => MetadataError::PermissionDenied,
        io::ErrorKind::InvalidData => MetadataError::Corrupt,
        _ => MetadataError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AuthorityId, SensitiveId};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, FilesystemMetadataStore, SessionMetadataDelta) {
        let temp = TempDir::new().expect("tempdir");
        let keyer =
            super::super::HmacIdentityKeyer::new(SensitiveId::new(&[9; 32])).expect("keyer");
        let subject = SubjectNamespace::parse("test/agent").expect("subject");
        let authority = AuthorityId::from_digest(StableDigest::from_bytes([2; 32]));
        let workspace = WorkspaceHint::from_digest(StableDigest::from_bytes([3; 32]));
        let session = SessionRef::new(
            SessionKey::new(
                subject,
                keyer.install_id().clone(),
                authority,
                StableDigest::from_bytes([4; 32]),
            ),
            workspace,
        );
        let store =
            FilesystemMetadataStore::new(temp.path().join("state"), keyer.install_id().clone())
                .expect("store")
                .with_write_interval(Duration::ZERO);
        let delta = SessionMetadataDelta {
            session,
            observer: ObserverId::parse("test/hook").expect("observer"),
            observed_at: Timestamp::from_unix_millis(10),
            discovery: Some(SessionDiscovery::DiscoveredMidSession),
            lifecycle_hint: None,
            activity_hint: Some(ActivityStateHint::Working),
            event_kind: ObservationKindTag::Activity,
            agents: BoundedVec::new(),
            terminal: None,
            write_class: MetadataWriteClass::Structural,
            generation: 1,
        };
        (temp, store, delta)
    }

    #[test]
    fn filesystem_store_round_trips_and_merges_monotonically() {
        let (_temp, store, mut delta) = fixture();
        assert_eq!(
            store.merge(&delta, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::Updated
        );
        delta.observed_at = Timestamp::from_unix_millis(5);
        delta.activity_hint = Some(ActivityStateHint::Idle);
        assert_eq!(
            store.merge(&delta, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::Updated
        );
        let selector = WorkspaceSelector::new(
            BoundedVec::try_from_vec(vec![delta.session.workspace().clone()]).expect("selector"),
        );
        let snapshot = store
            .load_workspace(&selector, MetadataLoadLimits::default())
            .expect("load");
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].first_observed_at.as_unix_millis(), 5);
        assert_eq!(snapshot.sessions[0].last_observed_at.as_unix_millis(), 10);
        assert_eq!(
            snapshot.sessions[0].last_activity_hint,
            ActivityStateHint::Working
        );
    }

    #[test]
    fn write_path_enforces_global_workspace_and_session_capacity() {
        let (_temp, store, delta) = fixture();
        let workspace_limited = store.with_capacity(1, 10, 10);
        assert_eq!(
            workspace_limited.merge(&delta, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::Updated
        );
        let mut other_workspace = delta.clone();
        other_workspace.session = SessionRef::new(
            SessionKey::new(
                delta.session.key().subject().clone(),
                delta.session.key().install_id().clone(),
                delta.session.key().authority_id().clone(),
                StableDigest::from_bytes([8; 32]),
            ),
            WorkspaceHint::from_digest(StableDigest::from_bytes([9; 32])),
        );
        assert_eq!(
            workspace_limited.merge(&other_workspace, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::CapacityReached
        );

        let (_temp, store, delta) = fixture();
        let session_limited = store.with_capacity(2, 1, 10);
        assert_eq!(
            session_limited.merge(&delta, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::Updated
        );
        let mut other_session = delta.clone();
        other_session.session = SessionRef::new(
            SessionKey::new(
                delta.session.key().subject().clone(),
                delta.session.key().install_id().clone(),
                delta.session.key().authority_id().clone(),
                StableDigest::from_bytes([10; 32]),
            ),
            delta.session.workspace().clone(),
        );
        assert_eq!(
            session_limited.merge(&other_session, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::CapacityReached
        );
    }

    #[test]
    fn corruption_is_ignored_and_reported_without_exposing_content() {
        let (_temp, store, delta) = fixture();
        assert_eq!(
            store.merge(&delta, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::Updated
        );
        let path = store.session_path(&delta.session);
        fs::write(&path, b"prompt-canary").expect("corrupt fixture");
        let selector = WorkspaceSelector::new(
            BoundedVec::try_from_vec(vec![delta.session.workspace().clone()]).expect("selector"),
        );
        let snapshot = store
            .load_workspace(&selector, MetadataLoadLimits::default())
            .expect("load");
        assert!(snapshot.sessions.is_empty());
        assert_eq!(snapshot.corrupt_records_ignored, 1);
    }

    #[test]
    fn advisory_lock_is_bounded_and_reusable_after_release() {
        let (_temp, store, delta) = fixture();
        assert_eq!(
            store.merge(&delta, Instant::now() + Duration::from_secs(1)),
            MetadataWriteOutcome::Updated
        );
        let path = store.session_lock_path(&delta.session);
        let held = FileLock::acquire(&path, Instant::now() + Duration::from_secs(1))
            .expect("first lock")
            .expect("acquired");
        assert!(
            FileLock::acquire(&path, Instant::now() + Duration::from_millis(2))
                .expect("contended lock")
                .is_none()
        );
        drop(held);
        assert!(
            FileLock::acquire(&path, Instant::now() + Duration::from_secs(1))
                .expect("reopened lock")
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_root_rejects_symlinked_session_directory() {
        use std::os::unix::fs::symlink;
        let (temp, store, delta) = fixture();
        let workspace_root = store.workspace_root(delta.session.workspace());
        fs::create_dir_all(&workspace_root).expect("workspace root");
        symlink(temp.path(), workspace_root.join("sessions")).expect("symlink");
        assert_eq!(
            store.merge(&delta, Instant::now() + Duration::from_millis(10)),
            MetadataWriteOutcome::Failed
        );
    }
}
