use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    time::Instant,
};

use super::{
    ActivityOp, AdapterRegistry, AgentKey, AgentKind, AgentObservation, AgentOp, AgentRef,
    ArtifactKey, ArtifactKind, ArtifactObservation, AuthorityId, BoundedVec, CapabilitySupport,
    ChangeKind, ChangeObservation, ContractRevision, DiagnosticCode, EventEnvelope, EventId,
    EvidenceAuthority, EvidenceClaim, EvidenceProvenance, InstallId, InstanceRegistry, LifecycleOp,
    ObservationEnvelope, ObservationError, ObservationKind, ObserverId, ObserverInstanceId,
    PermissionOp, PresenceOp, PresenceRef, PresentationOp, ReportedActivityState,
    ReportedSessionLifecycle, SanitizedDiagnostic, SessionKey, SessionOp, SessionRef, StableDigest,
    StreamEpoch, StreamOp, StreamRef, StreamSequence, SubjectNamespace, Timestamp, ToolOp, TurnKey,
    TurnOp, WorkspaceHint, stable_hash,
};

pub const LIVE_PROTOCOL_MAJOR: u16 = 1;
pub const LIVE_PROTOCOL_MINOR: u16 = 0;
pub const MAX_LIVE_FRAME_BYTES: usize = 4 * 1024;
pub const MAX_LIVE_WORKSPACE_HINTS: usize = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationFrame {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub workspace_hints: BoundedVec<WorkspaceHint, MAX_LIVE_WORKSPACE_HINTS>,
    pub event: EventEnvelope,
}

impl ObservationFrame {
    pub fn new(event: EventEnvelope) -> Result<Self, FrameError> {
        if !matches!(event.op, StreamOp::Upsert(_)) {
            return Err(FrameError::Unsupported);
        }
        let workspace_hints = event_workspace_hints(&event)?;
        Ok(Self {
            protocol_major: LIVE_PROTOCOL_MAJOR,
            protocol_minor: LIVE_PROTOCOL_MINOR,
            workspace_hints,
            event,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        if self.protocol_major != LIVE_PROTOCOL_MAJOR
            || !matches!(self.event.op, StreamOp::Upsert(_))
        {
            return Err(FrameError::Unsupported);
        }
        ObservationEnvelope::Event(self.event.clone())
            .validate_shape()
            .map_err(|_| FrameError::Invalid)?;
        let mut writer = Writer::default();
        writer.bytes(b"LLAO");
        writer.u16(self.protocol_major);
        writer.u16(self.protocol_minor);
        writer.u8(self.workspace_hints.len() as u8);
        for workspace in &self.workspace_hints {
            writer.digest(workspace.digest());
        }
        encode_event(&mut writer, &self.event)?;
        if writer.0.len() > MAX_LIVE_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        Ok(writer.0)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.len() > MAX_LIVE_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let mut reader = Reader::new(bytes);
        if reader.take(4)? != b"LLAO" {
            return Err(FrameError::Invalid);
        }
        let protocol_major = reader.u16()?;
        let protocol_minor = reader.u16()?;
        if protocol_major != LIVE_PROTOCOL_MAJOR {
            return Err(FrameError::VersionMismatch);
        }
        let workspace_count = usize::from(reader.u8()?);
        if workspace_count > MAX_LIVE_WORKSPACE_HINTS {
            return Err(FrameError::TooLarge);
        }
        let mut workspace_hints = Vec::with_capacity(workspace_count);
        for _ in 0..workspace_count {
            workspace_hints.push(WorkspaceHint::from_digest(reader.digest()?));
        }
        let workspace_hints =
            BoundedVec::try_from_vec(workspace_hints).map_err(|_| FrameError::TooLarge)?;
        let event = decode_event(&mut reader)?;
        if !reader.finished() || !matches!(event.op, StreamOp::Upsert(_)) {
            return Err(FrameError::Invalid);
        }
        ObservationEnvelope::Event(event.clone())
            .validate_shape()
            .map_err(|_| FrameError::Invalid)?;
        if event_workspace_hints(&event)? != workspace_hints {
            return Err(FrameError::Invalid);
        }
        Ok(Self {
            protocol_major,
            protocol_minor,
            workspace_hints,
            event,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    TooLarge,
    Invalid,
    Unsupported,
    VersionMismatch,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "live observation frame rejected: {self:?}")
    }
}

impl Error for FrameError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveAck {
    Accepted { receiver_generation: u64 },
    NotMember,
    Busy,
    VersionMismatch,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    Accepted { receiver_generation: u64 },
    Partial { accepted: u16, attempted: u16 },
    Unavailable,
    NotMember,
    Busy,
    Incompatible,
    Rejected,
}

pub trait LiveObservationPublisher: Send + Sync {
    fn publish(&self, event: &EventEnvelope, deadline: Instant) -> PublishOutcome;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportRejectReason {
    InvalidPeer,
    InvalidEnvelope,
    IncompatibleProtocol,
    StateRootMismatch,
    NotMember,
    Busy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiveOutcome {
    Event {
        receiver_generation: u64,
        workspace_hints: BoundedVec<WorkspaceHint, MAX_LIVE_WORKSPACE_HINTS>,
        event: Box<EventEnvelope>,
    },
    Idle,
    Closed,
    Rejected(TransportRejectReason),
}

pub trait LiveObservationReceiver: Send {
    fn receive(&mut self, deadline: Instant) -> ReceiveOutcome;
    fn begin_draining(&mut self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IngressRejection {
    Invalid,
    NotMember,
    StateRootMismatch,
}

/// Shared trust-boundary policy used by in-memory and OS loopback receivers.
/// It revalidates the current adapter/instance contract before an Accepted ACK.
pub struct LiveIngressPolicy {
    generation: AtomicU64,
    install: InstallId,
    selected_workspaces: RwLock<BTreeSet<WorkspaceHint>>,
    adapters: Arc<AdapterRegistry>,
    instances: Arc<RwLock<InstanceRegistry>>,
}

impl LiveIngressPolicy {
    pub fn new(
        generation: u64,
        install: InstallId,
        selected_workspaces: impl IntoIterator<Item = WorkspaceHint>,
        adapters: Arc<AdapterRegistry>,
        instances: Arc<RwLock<InstanceRegistry>>,
    ) -> Self {
        Self {
            generation: AtomicU64::new(generation),
            install,
            selected_workspaces: RwLock::new(selected_workspaces.into_iter().collect()),
            adapters,
            instances,
        }
    }

    pub fn receiver_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn membership_digest(&self) -> StableDigest {
        let Ok(workspaces) = self.selected_workspaces.read() else {
            return stable_hash(b"live-membership-unavailable", &[]);
        };
        let parts = workspaces
            .iter()
            .map(|workspace| workspace.digest().as_bytes().as_slice())
            .collect::<Vec<_>>();
        stable_hash(b"live-membership", &parts)
    }

    pub fn select_workspaces(
        &self,
        generation: u64,
        selected_workspaces: impl IntoIterator<Item = WorkspaceHint>,
    ) {
        if let Ok(mut workspaces) = self.selected_workspaces.write() {
            *workspaces = selected_workspaces.into_iter().collect();
            self.generation.store(generation, Ordering::Release);
        }
    }

    fn validate(&self, frame: &ObservationFrame) -> Result<(), IngressRejection> {
        let envelope = ObservationEnvelope::Event(frame.event.clone());
        if envelope.observations().iter().any(|observation| {
            observation
                .session
                .as_ref()
                .is_some_and(|session| session.key().install_id() != &self.install)
        }) {
            return Err(IngressRejection::StateRootMismatch);
        }
        let validation = self
            .instances
            .read()
            .map_err(|_| IngressRejection::Invalid)
            .and_then(|instances| {
                self.adapters
                    .validate_registered_envelope(envelope.clone(), &instances)
                    .map_err(|error| match error {
                        ObservationError::InstanceMismatch | ObservationError::WrongEpoch => {
                            IngressRejection::NotMember
                        }
                        _ => IngressRejection::Invalid,
                    })
            });
        if validation == Err(IngressRejection::NotMember) {
            let adapter = self
                .adapters
                .resolve(&frame.event.stream.observer)
                .ok_or(IngressRejection::Invalid)?;
            let contract = adapter.contract_template(None).hook_contract(
                frame.event.stream.instance.clone(),
                ContractRevision::new(1),
                None,
            );
            let mut instances = self
                .instances
                .write()
                .map_err(|_| IngressRejection::Invalid)?;
            instances
                .upsert(contract, frame.event.stream.epoch.clone())
                .map_err(|_| IngressRejection::Invalid)?;
            self.adapters
                .validate_registered_envelope(envelope, &instances)
                .map_err(|_| IngressRejection::Invalid)?;
        } else {
            validation?;
        }
        let selected = self
            .selected_workspaces
            .read()
            .map_err(|_| IngressRejection::Invalid)?;
        let member = frame
            .workspace_hints
            .iter()
            .any(|workspace| selected.contains(workspace));
        if !member {
            return Err(IngressRejection::NotMember);
        }
        Ok(())
    }
}

fn event_workspace_hints(
    event: &EventEnvelope,
) -> Result<BoundedVec<WorkspaceHint, MAX_LIVE_WORKSPACE_HINTS>, FrameError> {
    let mut workspaces = BTreeSet::new();
    if let StreamOp::Upsert(observations) = &event.op {
        for observation in observations {
            if let Some(workspace) = observation
                .workspace
                .as_ref()
                .or_else(|| observation.session.as_ref().map(SessionRef::workspace))
                .or_else(|| {
                    observation
                        .presence
                        .as_ref()
                        .and_then(PresenceRef::workspace)
                })
            {
                workspaces.insert(workspace.clone());
            }
        }
    }
    let workspace_hints = BoundedVec::try_from_vec(workspaces.into_iter().collect())
        .map_err(|_| FrameError::Invalid)?;
    if workspace_hints.len() != 1 {
        return Err(FrameError::Invalid);
    }
    Ok(workspace_hints)
}

pub struct InMemoryLivePublisher {
    sender: SyncSender<ObservationFrame>,
    policy: Arc<LiveIngressPolicy>,
    draining: Arc<AtomicBool>,
}

pub struct InMemoryLiveReceiver {
    receiver: Receiver<ObservationFrame>,
    policy: Arc<LiveIngressPolicy>,
    draining: Arc<AtomicBool>,
}

pub fn in_memory_live_transport(
    capacity: usize,
    policy: Arc<LiveIngressPolicy>,
) -> (InMemoryLivePublisher, InMemoryLiveReceiver) {
    let (sender, receiver) = sync_channel(capacity);
    let draining = Arc::new(AtomicBool::new(false));
    (
        InMemoryLivePublisher {
            sender,
            policy: Arc::clone(&policy),
            draining: Arc::clone(&draining),
        },
        InMemoryLiveReceiver {
            receiver,
            policy,
            draining,
        },
    )
}

impl LiveObservationPublisher for InMemoryLivePublisher {
    fn publish(&self, event: &EventEnvelope, deadline: Instant) -> PublishOutcome {
        if self.draining.load(Ordering::Acquire) || Instant::now() >= deadline {
            return PublishOutcome::Unavailable;
        }
        let Ok(frame) = ObservationFrame::new(event.clone()) else {
            return PublishOutcome::Rejected;
        };
        match self.policy.validate(&frame) {
            Ok(()) => {}
            Err(IngressRejection::NotMember) => return PublishOutcome::NotMember,
            Err(IngressRejection::StateRootMismatch) => return PublishOutcome::Rejected,
            Err(IngressRejection::Invalid) => return PublishOutcome::Rejected,
        }
        match self.sender.try_send(frame) {
            Ok(()) => PublishOutcome::Accepted {
                receiver_generation: self.policy.receiver_generation(),
            },
            Err(TrySendError::Full(_)) => PublishOutcome::Busy,
            Err(TrySendError::Disconnected(_)) => PublishOutcome::Unavailable,
        }
    }
}

impl LiveObservationReceiver for InMemoryLiveReceiver {
    fn receive(&mut self, _deadline: Instant) -> ReceiveOutcome {
        match self.receiver.try_recv() {
            Ok(frame) => ReceiveOutcome::Event {
                receiver_generation: self.policy.receiver_generation(),
                workspace_hints: frame.workspace_hints,
                event: Box::new(frame.event),
            },
            Err(TryRecvError::Empty) if self.draining.load(Ordering::Acquire) => {
                ReceiveOutcome::Closed
            }
            Err(TryRecvError::Empty) => ReceiveOutcome::Idle,
            Err(TryRecvError::Disconnected) => ReceiveOutcome::Closed,
        }
    }

    fn begin_draining(&mut self) {
        self.draining.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        fs::{File, OpenOptions},
        io::{self, Read, Write},
        os::unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
            io::{AsRawFd, FromRawFd},
            net::{UnixListener, UnixStream},
        },
        path::{Path, PathBuf},
        sync::Arc,
        time::Instant,
    };

    use super::*;

    pub struct UnixLivePublisher {
        endpoint: PathBuf,
    }

    impl UnixLivePublisher {
        pub fn new(endpoint: PathBuf) -> Self {
            Self { endpoint }
        }
    }

    impl LiveObservationPublisher for UnixLivePublisher {
        fn publish(&self, event: &EventEnvelope, deadline: Instant) -> PublishOutcome {
            if Instant::now() >= deadline {
                return PublishOutcome::Unavailable;
            }
            let frame = match ObservationFrame::new(event.clone()).and_then(|frame| frame.encode())
            {
                Ok(frame) => frame,
                Err(FrameError::VersionMismatch | FrameError::Unsupported) => {
                    return PublishOutcome::Incompatible;
                }
                Err(FrameError::TooLarge | FrameError::Invalid) => {
                    return PublishOutcome::Rejected;
                }
            };
            let mut stream = match connect_with_deadline(&self.endpoint, deadline) {
                Ok(stream) => stream,
                Err(_) => return PublishOutcome::Unavailable,
            };
            if stream.set_nonblocking(true).is_err() {
                return PublishOutcome::Unavailable;
            }
            let mut message = Vec::with_capacity(frame.len() + 4);
            message.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            message.extend_from_slice(&frame);
            if write_until(&mut stream, &message, deadline).is_err() {
                return PublishOutcome::Unavailable;
            }
            let mut ack = [0_u8; 9];
            if read_until(&mut stream, &mut ack, deadline).is_err() {
                return PublishOutcome::Unavailable;
            }
            decode_ack(&ack)
        }
    }

    pub struct UnixLiveReceiver {
        endpoint: PathBuf,
        listener: UnixListener,
        policy: Arc<LiveIngressPolicy>,
        draining: bool,
        owner: UnixOwnerLease,
        ready_marker: PathBuf,
    }

    impl UnixLiveReceiver {
        pub fn bind(endpoint: PathBuf, policy: Arc<LiveIngressPolicy>) -> io::Result<Self> {
            prepare_endpoint_parent(&endpoint)?;
            let owner = UnixOwnerLease::acquire(endpoint.with_extension("owner"))?;
            match fs::symlink_metadata(&endpoint) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    fs::remove_file(&endpoint)?;
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unsafe live endpoint",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(&endpoint)?;
            fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let ready_marker = endpoint.with_extension("ready");
            write_ready_marker(&ready_marker, policy.as_ref())?;
            Ok(Self {
                endpoint,
                listener,
                policy,
                draining: false,
                owner,
                ready_marker,
            })
        }
    }

    impl LiveObservationReceiver for UnixLiveReceiver {
        fn receive(&mut self, deadline: Instant) -> ReceiveOutcome {
            if self.draining {
                return ReceiveOutcome::Closed;
            }
            let (mut stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return ReceiveOutcome::Idle;
                }
                Err(_) => return ReceiveOutcome::Closed,
            };
            if !peer_is_current_user(&stream) {
                let _ = write_ack(&mut stream, LiveAck::Invalid, deadline);
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidPeer);
            }
            if stream.set_nonblocking(true).is_err() {
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
            }
            let mut length = [0_u8; 4];
            if read_until(&mut stream, &mut length, deadline).is_err() {
                let _ = write_ack(&mut stream, LiveAck::Invalid, deadline);
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
            }
            let length = u32::from_be_bytes(length) as usize;
            if length > MAX_LIVE_FRAME_BYTES {
                let _ = write_ack(&mut stream, LiveAck::Invalid, deadline);
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
            }
            let mut frame = vec![0_u8; length];
            if read_until(&mut stream, &mut frame, deadline).is_err() {
                let _ = write_ack(&mut stream, LiveAck::Invalid, deadline);
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
            }
            let frame = match ObservationFrame::decode(&frame) {
                Ok(frame) => frame,
                Err(FrameError::VersionMismatch) => {
                    let _ = write_ack(&mut stream, LiveAck::VersionMismatch, deadline);
                    return ReceiveOutcome::Rejected(TransportRejectReason::IncompatibleProtocol);
                }
                Err(_) => {
                    let _ = write_ack(&mut stream, LiveAck::Invalid, deadline);
                    return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
                }
            };
            match self.policy.validate(&frame) {
                Ok(()) => {
                    let generation = self.policy.receiver_generation();
                    if write_ack(
                        &mut stream,
                        LiveAck::Accepted {
                            receiver_generation: generation,
                        },
                        deadline,
                    )
                    .is_err()
                    {
                        // Delivery remains accepted even when the ACK is lost;
                        // metadata fallback converges by stable EventId.
                    }
                    ReceiveOutcome::Event {
                        receiver_generation: generation,
                        workspace_hints: frame.workspace_hints,
                        event: Box::new(frame.event),
                    }
                }
                Err(IngressRejection::NotMember) => {
                    let _ = write_ack(&mut stream, LiveAck::NotMember, deadline);
                    ReceiveOutcome::Rejected(TransportRejectReason::NotMember)
                }
                Err(IngressRejection::StateRootMismatch) => {
                    let _ = write_ack(&mut stream, LiveAck::Invalid, deadline);
                    ReceiveOutcome::Rejected(TransportRejectReason::StateRootMismatch)
                }
                Err(IngressRejection::Invalid) => {
                    let _ = write_ack(&mut stream, LiveAck::Invalid, deadline);
                    ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope)
                }
            }
        }

        fn begin_draining(&mut self) {
            self.draining = true;
            let _ = fs::remove_file(&self.ready_marker);
        }
    }

    impl Drop for UnixLiveReceiver {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.ready_marker);
            let _ = fs::remove_file(&self.endpoint);
            let _ = &self.owner;
        }
    }

    struct UnixOwnerLease {
        _path: PathBuf,
        file: File,
    }

    impl UnixOwnerLease {
        fn acquire(path: PathBuf) -> io::Result<Self> {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW);
            let file = options.open(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            // SAFETY: flock operates on this owned regular-file descriptor.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "live receiver owner already exists",
                ));
            }
            Ok(Self { _path: path, file })
        }
    }

    impl Drop for UnixOwnerLease {
        fn drop(&mut self) {
            // SAFETY: releases only the advisory lock held by this descriptor.
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    fn write_ready_marker(path: &Path, policy: &LiveIngressPolicy) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(path)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsafe ready marker",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        let body = format!(
            "LLAO {LIVE_PROTOCOL_MAJOR}.{LIVE_PROTOCOL_MINOR} generation={} membership={}\n",
            policy.receiver_generation(),
            policy.membership_digest().to_hex()
        );
        file.write_all(body.as_bytes())?;
        file.flush()?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }

    fn prepare_endpoint_parent(endpoint: &Path) -> io::Result<()> {
        let parent = endpoint.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing endpoint parent")
        })?;
        fs::create_dir_all(parent)?;
        let metadata = fs::symlink_metadata(parent)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe endpoint parent",
            ));
        }
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
    }

    fn write_ack(stream: &mut UnixStream, ack: LiveAck, deadline: Instant) -> io::Result<()> {
        write_until(stream, &encode_ack(ack), deadline)
    }

    fn connect_with_deadline(endpoint: &Path, deadline: Instant) -> io::Result<UnixStream> {
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
        }
        // SAFETY: socket has no pointer arguments and returns a new descriptor.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            // SAFETY: fcntl operates on the newly created descriptor.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fcntl operates on the newly created descriptor.
            let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if descriptor_flags < 0
                || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) }
                    < 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: zeroed is a valid initial sockaddr_un representation.
            let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            address.sun_family = libc::AF_UNIX as libc::sa_family_t;
            let path = endpoint.as_os_str().as_bytes();
            if path.is_empty() || path.len() >= address.sun_path.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "endpoint path is too long",
                ));
            }
            for (target, byte) in address.sun_path.iter_mut().zip(path.iter().copied()) {
                *target = byte as libc::c_char;
            }
            // SAFETY: address is initialized and points to a bounded pathname.
            let connected = unsafe {
                libc::connect(
                    fd,
                    (&raw const address).cast(),
                    std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
                )
            };
            if connected != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINPROGRESS) {
                    return Err(error);
                }
                loop {
                    let remaining = deadline
                        .checked_duration_since(Instant::now())
                        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "deadline"))?;
                    let timeout = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
                    let mut descriptor = libc::pollfd {
                        fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // SAFETY: descriptor points to one valid pollfd.
                    let ready = unsafe { libc::poll(&raw mut descriptor, 1, timeout) };
                    if ready == 0 {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
                    }
                    if ready < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(error);
                    }
                    let mut socket_error = 0_i32;
                    let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
                    // SAFETY: getsockopt writes one i32 into the provided buffer.
                    if unsafe {
                        libc::getsockopt(
                            fd,
                            libc::SOL_SOCKET,
                            libc::SO_ERROR,
                            (&raw mut socket_error).cast(),
                            &raw mut length,
                        )
                    } != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if socket_error != 0 {
                        return Err(io::Error::from_raw_os_error(socket_error));
                    }
                    break;
                }
            }
            // SAFETY: ownership of the connected descriptor transfers to UnixStream.
            Ok(unsafe { UnixStream::from_raw_fd(fd) })
        })();
        if result.is_err() {
            // SAFETY: fd has not transferred to UnixStream on the error path.
            unsafe { libc::close(fd) };
        }
        result
    }

    fn write_until(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> io::Result<()> {
        let mut written = 0_usize;
        while written < bytes.len() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
            }
            match stream.write(&bytes[written..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "closed")),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn read_until(stream: &mut UnixStream, bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
        let mut read = 0_usize;
        while read < bytes.len() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
            }
            match stream.read(&mut bytes[read..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    fn peer_is_current_user(stream: &UnixStream) -> bool {
        use std::os::fd::AsRawFd;
        let mut uid = 0_u32;
        let mut gid = 0_u32;
        // SAFETY: getpeereid writes two scalar outputs for a valid socket fd.
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        // SAFETY: geteuid has no preconditions.
        result == 0 && uid == unsafe { libc::geteuid() }
    }

    #[cfg(target_os = "linux")]
    fn peer_is_current_user(stream: &UnixStream) -> bool {
        use std::{mem::size_of, os::fd::AsRawFd};
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: getsockopt writes a ucred into a correctly sized buffer.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &raw mut length,
            )
        };
        // SAFETY: geteuid has no preconditions.
        result == 0 && credentials.uid == unsafe { libc::geteuid() }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )))]
    fn peer_is_current_user(_stream: &UnixStream) -> bool {
        false
    }
}

#[cfg(unix)]
pub use unix::{UnixLivePublisher, UnixLiveReceiver};

#[cfg(windows)]
mod windows {
    use std::{
        ffi::{OsStr, c_void},
        io,
        mem::{size_of, zeroed},
        os::windows::ffi::OsStrExt,
        ptr::{null, null_mut},
        sync::Arc,
        time::Instant,
    };

    use super::*;

    type Handle = *mut c_void;
    type Bool = i32;

    const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_TYPE_BYTE: u32 = 0;
    const PIPE_READMODE_BYTE: u32 = 0;
    const PIPE_WAIT: u32 = 0;
    const ERROR_IO_PENDING: i32 = 997;
    const ERROR_PIPE_CONNECTED: i32 = 535;
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER_CLASS: u32 = 1;
    const SDDL_REVISION_1: u32 = 1;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: Bool,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct OverlappedOffsets {
        offset: u32,
        offset_high: u32,
    }

    #[repr(C)]
    union OverlappedUnion {
        offsets: OverlappedOffsets,
        pointer: *mut c_void,
    }

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        union: OverlappedUnion,
        event: Handle,
    }

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }

    #[repr(C)]
    struct TokenUser {
        user: SidAndAttributes,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share_mode: u32,
            security_attributes: *mut SecurityAttributes,
            creation_disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *mut SecurityAttributes,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut Overlapped) -> Bool;
        fn DisconnectNamedPipe(pipe: Handle) -> Bool;
        fn GetNamedPipeClientProcessId(pipe: Handle, client_process_id: *mut u32) -> Bool;
        fn ReadFile(
            file: Handle,
            buffer: *mut c_void,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut Overlapped,
        ) -> Bool;
        fn WriteFile(
            file: Handle,
            buffer: *const c_void,
            bytes_to_write: u32,
            bytes_written: *mut u32,
            overlapped: *mut Overlapped,
        ) -> Bool;
        fn CreateEventW(
            security_attributes: *mut SecurityAttributes,
            manual_reset: Bool,
            initial_state: Bool,
            name: *const u16,
        ) -> Handle;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetOverlappedResult(
            file: Handle,
            overlapped: *mut Overlapped,
            transferred: *mut u32,
            wait: Bool,
        ) -> Bool;
        fn CancelIoEx(file: Handle, overlapped: *mut Overlapped) -> Bool;
        fn CloseHandle(handle: Handle) -> Bool;
        fn GetLastError() -> u32;
        fn OpenProcess(access: u32, inherit: Bool, process_id: u32) -> Handle;
        fn GetCurrentProcess() -> Handle;
        fn LocalFree(memory: Handle) -> Handle;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor: *const u16,
            revision: u32,
            security_descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> Bool;
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> Bool;
        fn GetTokenInformation(
            token: Handle,
            information_class: u32,
            information: *mut c_void,
            information_length: u32,
            return_length: *mut u32,
        ) -> Bool;
        fn EqualSid(first: *const c_void, second: *const c_void) -> Bool;
    }

    pub struct WindowsNamedPipePublisher {
        endpoint: super::super::BoundedText<256>,
    }

    impl WindowsNamedPipePublisher {
        pub const fn new(endpoint: super::super::BoundedText<256>) -> Self {
            Self { endpoint }
        }
    }

    impl LiveObservationPublisher for WindowsNamedPipePublisher {
        fn publish(&self, event: &EventEnvelope, deadline: Instant) -> PublishOutcome {
            if Instant::now() >= deadline {
                return PublishOutcome::Unavailable;
            }
            let frame = match ObservationFrame::new(event.clone()).and_then(|frame| frame.encode())
            {
                Ok(frame) => frame,
                Err(FrameError::VersionMismatch | FrameError::Unsupported) => {
                    return PublishOutcome::Incompatible;
                }
                Err(FrameError::TooLarge | FrameError::Invalid) => {
                    return PublishOutcome::Rejected;
                }
            };
            let name = wide(self.endpoint.as_str());
            // SAFETY: the UTF-16 name is NUL terminated and all optional
            // pointers are null as required by CreateFileW.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    null_mut(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    null_mut(),
                )
            };
            let Some(handle) = OwnedHandle::new(handle) else {
                return PublishOutcome::Unavailable;
            };
            let mut message = Vec::with_capacity(frame.len() + 4);
            message.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            message.extend_from_slice(&frame);
            if write_all(handle.0, &message, deadline).is_err() {
                return PublishOutcome::Unavailable;
            }
            let mut ack = [0_u8; 9];
            if read_exact(handle.0, &mut ack, deadline).is_err() {
                return PublishOutcome::Unavailable;
            }
            decode_ack(&ack)
        }
    }

    pub struct WindowsNamedPipeReceiver {
        handle: OwnedHandle,
        policy: Arc<LiveIngressPolicy>,
        draining: bool,
        connected: bool,
    }

    // SAFETY: the pipe handle is exclusively owned and all operations are
    // serialized through &mut self by LiveObservationReceiver.
    unsafe impl Send for WindowsNamedPipeReceiver {}

    impl WindowsNamedPipeReceiver {
        pub fn bind(
            endpoint: super::super::BoundedText<256>,
            policy: Arc<LiveIngressPolicy>,
        ) -> io::Result<Self> {
            let name = wide(endpoint.as_str());
            let sddl = wide("D:P(A;;GA;;;OW)");
            let mut descriptor = null_mut();
            // SAFETY: the SDDL and output pointer are valid for the duration
            // of the conversion call.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &raw mut descriptor,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut attributes = SecurityAttributes {
                length: size_of::<SecurityAttributes>() as u32,
                security_descriptor: descriptor,
                inherit_handle: 0,
            };
            // SAFETY: the name and security attributes remain valid for the
            // duration of CreateNamedPipeW.
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    MAX_LIVE_FRAME_BYTES as u32 + 16,
                    MAX_LIVE_FRAME_BYTES as u32 + 16,
                    0,
                    &raw mut attributes,
                )
            };
            // SAFETY: descriptor was allocated by the SDDL conversion API.
            unsafe { LocalFree(descriptor) };
            let handle = OwnedHandle::new(handle).ok_or_else(io::Error::last_os_error)?;
            Ok(Self {
                handle,
                policy,
                draining: false,
                connected: false,
            })
        }

        fn disconnect(&mut self) {
            if self.connected {
                // SAFETY: handle owns a connected named-pipe instance.
                unsafe { DisconnectNamedPipe(self.handle.0) };
                self.connected = false;
            }
        }
    }

    impl LiveObservationReceiver for WindowsNamedPipeReceiver {
        fn receive(&mut self, deadline: Instant) -> ReceiveOutcome {
            if self.draining {
                self.disconnect();
                return ReceiveOutcome::Closed;
            }
            if !self.connected {
                match connect_pipe(self.handle.0, deadline) {
                    Ok(true) => self.connected = true,
                    Ok(false) => return ReceiveOutcome::Idle,
                    Err(_) => return ReceiveOutcome::Closed,
                }
            }
            if !peer_is_current_user(self.handle.0) {
                let _ = write_all(self.handle.0, &encode_ack(LiveAck::Invalid), deadline);
                self.disconnect();
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidPeer);
            }
            let mut length = [0_u8; 4];
            if read_exact(self.handle.0, &mut length, deadline).is_err() {
                self.disconnect();
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
            }
            let length = u32::from_be_bytes(length) as usize;
            if length > MAX_LIVE_FRAME_BYTES {
                let _ = write_all(self.handle.0, &encode_ack(LiveAck::Invalid), deadline);
                self.disconnect();
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
            }
            let mut bytes = vec![0_u8; length];
            if read_exact(self.handle.0, &mut bytes, deadline).is_err() {
                self.disconnect();
                return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
            }
            let frame = match ObservationFrame::decode(&bytes) {
                Ok(frame) => frame,
                Err(FrameError::VersionMismatch) => {
                    let _ = write_all(
                        self.handle.0,
                        &encode_ack(LiveAck::VersionMismatch),
                        deadline,
                    );
                    self.disconnect();
                    return ReceiveOutcome::Rejected(TransportRejectReason::IncompatibleProtocol);
                }
                Err(_) => {
                    let _ = write_all(self.handle.0, &encode_ack(LiveAck::Invalid), deadline);
                    self.disconnect();
                    return ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope);
                }
            };
            let outcome = match self.policy.validate(&frame) {
                Ok(()) => {
                    let generation = self.policy.receiver_generation();
                    let _ = write_all(
                        self.handle.0,
                        &encode_ack(LiveAck::Accepted {
                            receiver_generation: generation,
                        }),
                        deadline,
                    );
                    ReceiveOutcome::Event {
                        receiver_generation: generation,
                        workspace_hints: frame.workspace_hints,
                        event: Box::new(frame.event),
                    }
                }
                Err(IngressRejection::NotMember) => {
                    let _ = write_all(self.handle.0, &encode_ack(LiveAck::NotMember), deadline);
                    ReceiveOutcome::Rejected(TransportRejectReason::NotMember)
                }
                Err(IngressRejection::StateRootMismatch) => {
                    let _ = write_all(self.handle.0, &encode_ack(LiveAck::Invalid), deadline);
                    ReceiveOutcome::Rejected(TransportRejectReason::StateRootMismatch)
                }
                Err(IngressRejection::Invalid) => {
                    let _ = write_all(self.handle.0, &encode_ack(LiveAck::Invalid), deadline);
                    ReceiveOutcome::Rejected(TransportRejectReason::InvalidEnvelope)
                }
            };
            self.disconnect();
            outcome
        }

        fn begin_draining(&mut self) {
            self.draining = true;
            self.disconnect();
        }
    }

    struct OwnedHandle(Handle);

    impl OwnedHandle {
        fn new(handle: Handle) -> Option<Self> {
            (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self(handle))
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: OwnedHandle is the unique owner of this valid handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value.as_ref().encode_wide().chain([0]).collect()
    }

    fn remaining_millis(deadline: Instant) -> Option<u32> {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        Some(remaining.as_millis().max(1).min(u128::from(u32::MAX - 1)) as u32)
    }

    fn connect_pipe(handle: Handle, deadline: Instant) -> io::Result<bool> {
        let Some(timeout) = remaining_millis(deadline) else {
            return Ok(false);
        };
        let event = create_event()?;
        let mut overlapped = new_overlapped(event.0);
        // SAFETY: handle is a named pipe and overlapped remains alive through
        // completion/cancellation.
        let started = unsafe { ConnectNamedPipe(handle, &raw mut overlapped) };
        if started != 0 {
            return Ok(true);
        }
        let error = unsafe { GetLastError() } as i32;
        if error == ERROR_PIPE_CONNECTED {
            return Ok(true);
        }
        if error != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(error));
        }
        match wait_overlapped(handle, &mut overlapped, timeout) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn read_exact(handle: Handle, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = read_once(handle, bytes, deadline)?;
            if count == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed"));
            }
            bytes = &mut bytes[count..];
        }
        Ok(())
    }

    fn write_all(handle: Handle, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = write_once(handle, bytes, deadline)?;
            if count == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "pipe closed"));
            }
            bytes = &bytes[count..];
        }
        Ok(())
    }

    fn read_once(handle: Handle, bytes: &mut [u8], deadline: Instant) -> io::Result<usize> {
        let timeout = remaining_millis(deadline)
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "deadline"))?;
        let event = create_event()?;
        let mut overlapped = new_overlapped(event.0);
        let mut immediate = 0_u32;
        // SAFETY: the output slice and overlapped structure remain valid until
        // the operation completes or is cancelled.
        let started = unsafe {
            ReadFile(
                handle,
                bytes.as_mut_ptr().cast(),
                bytes.len().min(u32::MAX as usize) as u32,
                &raw mut immediate,
                &raw mut overlapped,
            )
        };
        if started != 0 {
            return Ok(immediate as usize);
        }
        let error = unsafe { GetLastError() } as i32;
        if error != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(error));
        }
        wait_overlapped(handle, &mut overlapped, timeout).map(|count| count as usize)
    }

    fn write_once(handle: Handle, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        let timeout = remaining_millis(deadline)
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "deadline"))?;
        let event = create_event()?;
        let mut overlapped = new_overlapped(event.0);
        let mut immediate = 0_u32;
        // SAFETY: the input slice and overlapped structure remain valid until
        // the operation completes or is cancelled.
        let started = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr().cast(),
                bytes.len().min(u32::MAX as usize) as u32,
                &raw mut immediate,
                &raw mut overlapped,
            )
        };
        if started != 0 {
            return Ok(immediate as usize);
        }
        let error = unsafe { GetLastError() } as i32;
        if error != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(error));
        }
        wait_overlapped(handle, &mut overlapped, timeout).map(|count| count as usize)
    }

    fn wait_overlapped(
        handle: Handle,
        overlapped: &mut Overlapped,
        timeout: u32,
    ) -> io::Result<u32> {
        // SAFETY: the event belongs to the pending OVERLAPPED operation.
        match unsafe { WaitForSingleObject(overlapped.event, timeout) } {
            WAIT_OBJECT_0 => {
                let mut transferred = 0_u32;
                // SAFETY: the event is signalled and the OVERLAPPED remains valid.
                if unsafe { GetOverlappedResult(handle, overlapped, &raw mut transferred, 0) } == 0
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(transferred)
                }
            }
            WAIT_TIMEOUT => {
                // SAFETY: the pending operation belongs to this handle/OVERLAPPED.
                unsafe { CancelIoEx(handle, overlapped) };
                Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn create_event() -> io::Result<OwnedHandle> {
        // SAFETY: unnamed auto-reset event with default security.
        OwnedHandle::new(unsafe { CreateEventW(null_mut(), 0, 0, null()) })
            .ok_or_else(io::Error::last_os_error)
    }

    fn new_overlapped(event: Handle) -> Overlapped {
        // SAFETY: zero is a valid initial state for OVERLAPPED before setting hEvent.
        let mut overlapped: Overlapped = unsafe { zeroed() };
        overlapped.event = event;
        overlapped
    }

    fn peer_is_current_user(pipe: Handle) -> bool {
        let mut process_id = 0_u32;
        // SAFETY: process_id is a valid scalar output.
        if unsafe { GetNamedPipeClientProcessId(pipe, &raw mut process_id) } == 0 {
            return false;
        }
        // SAFETY: opens a read-only query handle to the peer process.
        let Some(client) = OwnedHandle::new(unsafe {
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id)
        }) else {
            return false;
        };
        token_sid_equal(client.0, unsafe { GetCurrentProcess() })
    }

    fn token_sid_equal(left_process: Handle, right_process: Handle) -> bool {
        let Some((left_token, left_buffer)) = token_user(left_process) else {
            return false;
        };
        let Some((right_token, right_buffer)) = token_user(right_process) else {
            return false;
        };
        let left = unsafe { &*(left_buffer.as_ptr().cast::<TokenUser>()) };
        let right = unsafe { &*(right_buffer.as_ptr().cast::<TokenUser>()) };
        // Keep token handles and buffers alive through EqualSid.
        let equal = unsafe { EqualSid(left.user.sid, right.user.sid) } != 0;
        drop((left_token, right_token, left_buffer, right_buffer));
        equal
    }

    fn token_user(process: Handle) -> Option<(OwnedHandle, Vec<u8>)> {
        let mut token = null_mut();
        // SAFETY: token is a valid output pointer and access is query-only.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
            return None;
        }
        let token = OwnedHandle::new(token)?;
        let mut required = 0_u32;
        // SAFETY: the first call intentionally supplies no output buffer.
        let first = unsafe {
            GetTokenInformation(token.0, TOKEN_USER_CLASS, null_mut(), 0, &raw mut required)
        };
        if first != 0 || unsafe { GetLastError() } as i32 != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        let mut buffer = vec![0_u8; required as usize];
        // SAFETY: buffer has the size requested by GetTokenInformation.
        if unsafe {
            GetTokenInformation(
                token.0,
                TOKEN_USER_CLASS,
                buffer.as_mut_ptr().cast(),
                required,
                &raw mut required,
            )
        } == 0
        {
            return None;
        }
        Some((token, buffer))
    }
}

#[cfg(windows)]
pub use windows::{WindowsNamedPipePublisher, WindowsNamedPipeReceiver};

fn encode_ack(ack: LiveAck) -> [u8; 9] {
    let (tag, generation) = match ack {
        LiveAck::Accepted {
            receiver_generation,
        } => (0, receiver_generation),
        LiveAck::NotMember => (1, 0),
        LiveAck::Busy => (2, 0),
        LiveAck::VersionMismatch => (3, 0),
        LiveAck::Invalid => (4, 0),
    };
    let mut output = [0_u8; 9];
    output[0] = tag;
    output[1..].copy_from_slice(&generation.to_be_bytes());
    output
}

fn decode_ack(bytes: &[u8; 9]) -> PublishOutcome {
    match bytes[0] {
        0 => PublishOutcome::Accepted {
            receiver_generation: u64::from_be_bytes(bytes[1..].try_into().expect("ACK generation")),
        },
        1 => PublishOutcome::NotMember,
        2 => PublishOutcome::Busy,
        3 => PublishOutcome::Incompatible,
        _ => PublishOutcome::Rejected,
    }
}

fn encode_event(writer: &mut Writer, event: &EventEnvelope) -> Result<(), FrameError> {
    encode_stream(writer, &event.stream)?;
    writer.digest(event.event_id.digest());
    writer.optional_u64(event.sequence.map(StreamSequence::get));
    match &event.op {
        StreamOp::Upsert(observations) => {
            writer.u8(0);
            writer.u8(observations.len() as u8);
            for observation in observations {
                encode_observation(writer, observation)?;
            }
        }
        StreamOp::Reset => writer.u8(1),
        StreamOp::Delete { .. } | StreamOp::Gap { .. } => return Err(FrameError::Unsupported),
    }
    Ok(())
}

fn decode_event(reader: &mut Reader<'_>) -> Result<EventEnvelope, FrameError> {
    let stream = decode_stream(reader)?;
    let event_id = EventId::from_digest(reader.digest()?);
    let sequence = reader.optional_u64()?.map(StreamSequence::new);
    let op = match reader.u8()? {
        0 => {
            let count = usize::from(reader.u8()?);
            if count > 8 {
                return Err(FrameError::Invalid);
            }
            let mut observations = Vec::with_capacity(count);
            for _ in 0..count {
                observations.push(decode_observation(reader)?);
            }
            StreamOp::Upsert(
                BoundedVec::try_from_vec(observations).map_err(|_| FrameError::Invalid)?,
            )
        }
        1 => StreamOp::Reset,
        _ => return Err(FrameError::Invalid),
    };
    Ok(EventEnvelope {
        stream,
        event_id,
        sequence,
        op,
    })
}

fn encode_stream(writer: &mut Writer, stream: &StreamRef) -> Result<(), FrameError> {
    writer.text_u8(stream.observer.as_str())?;
    writer.digest(stream.instance.digest());
    writer.digest(stream.epoch.digest());
    Ok(())
}

fn decode_stream(reader: &mut Reader<'_>) -> Result<StreamRef, FrameError> {
    Ok(StreamRef {
        observer: ObserverId::parse(reader.text_u8()?).map_err(|_| FrameError::Invalid)?,
        instance: ObserverInstanceId::from_digest(reader.digest()?),
        epoch: StreamEpoch::from_digest(reader.digest()?),
    })
}

fn encode_observation(writer: &mut Writer, value: &AgentObservation) -> Result<(), FrameError> {
    writer.u64(value.observed_at.as_unix_millis());
    writer.optional_u64(value.valid_until.map(Timestamp::as_unix_millis));
    writer.bool(value.presence.is_some());
    if let Some(presence) = &value.presence {
        encode_presence(writer, presence)?;
    }
    writer.bool(value.session.is_some());
    if let Some(session) = &value.session {
        encode_session_ref(writer, session)?;
    }
    writer.bool(value.agent.is_some());
    if let Some(agent) = &value.agent {
        writer.digest(agent.key().stable_id());
        writer.bool(agent.parent().is_some());
        if let Some(parent) = agent.parent() {
            writer.digest(parent.stable_id());
        }
        writer.u8(match agent.kind() {
            None => 0,
            Some(AgentKind::Primary) => 1,
            Some(AgentKind::Subagent) => 2,
        });
    }
    writer.bool(value.turn.is_some());
    if let Some(turn) = &value.turn {
        writer.digest(turn.authority_id().digest());
        writer.digest(turn.stable_id());
    }
    writer.bool(value.workspace.is_some());
    if let Some(workspace) = &value.workspace {
        writer.digest(workspace.digest());
    }
    encode_kind(writer, &value.kind);
    writer.u8(support_tag(value.evidence.support));
    writer.u8(authority_tag(value.evidence.authority));
    writer.u8(provenance_tag(value.evidence.provenance));
    Ok(())
}

fn decode_observation(reader: &mut Reader<'_>) -> Result<AgentObservation, FrameError> {
    let observed_at = Timestamp::from_unix_millis(reader.u64()?);
    let valid_until = reader.optional_u64()?.map(Timestamp::from_unix_millis);
    let presence = if reader.bool()? {
        Some(decode_presence(reader)?)
    } else {
        None
    };
    let session = if reader.bool()? {
        Some(decode_session_ref(reader)?)
    } else {
        None
    };
    let agent = if reader.bool()? {
        let session_key = session.as_ref().ok_or(FrameError::Invalid)?.key().clone();
        let key = AgentKey::new(session_key.clone(), reader.digest()?);
        let parent = if reader.bool()? {
            Some(AgentKey::new(session_key, reader.digest()?))
        } else {
            None
        };
        let kind = match reader.u8()? {
            0 => None,
            1 => Some(AgentKind::Primary),
            2 => Some(AgentKind::Subagent),
            _ => return Err(FrameError::Invalid),
        };
        Some(AgentRef::new(key, parent, kind))
    } else {
        None
    };
    let turn = if reader.bool()? {
        Some(TurnKey::new(
            session.as_ref().ok_or(FrameError::Invalid)?.key().clone(),
            AuthorityId::from_digest(reader.digest()?),
            reader.digest()?,
        ))
    } else {
        None
    };
    let workspace = if reader.bool()? {
        Some(WorkspaceHint::from_digest(reader.digest()?))
    } else {
        None
    };
    let kind = decode_kind(reader)?;
    let evidence = EvidenceClaim {
        support: decode_support(reader.u8()?)?,
        authority: decode_authority(reader.u8()?)?,
        provenance: decode_provenance(reader.u8()?)?,
    };
    Ok(AgentObservation {
        observed_at,
        valid_until,
        presence,
        session,
        agent,
        turn,
        workspace,
        kind,
        evidence,
    })
}

fn encode_session_ref(writer: &mut Writer, session: &SessionRef) -> Result<(), FrameError> {
    encode_session_key(writer, session.key())?;
    writer.digest(session.workspace().digest());
    Ok(())
}

fn decode_session_ref(reader: &mut Reader<'_>) -> Result<SessionRef, FrameError> {
    Ok(SessionRef::new(
        decode_session_key(reader)?,
        WorkspaceHint::from_digest(reader.digest()?),
    ))
}

fn encode_session_key(writer: &mut Writer, key: &SessionKey) -> Result<(), FrameError> {
    writer.text_u8(key.subject().as_str())?;
    writer.digest(key.install_id().digest());
    writer.digest(key.authority_id().digest());
    writer.digest(key.stable_id());
    Ok(())
}

fn decode_session_key(reader: &mut Reader<'_>) -> Result<SessionKey, FrameError> {
    Ok(SessionKey::new(
        SubjectNamespace::parse(reader.text_u8()?).map_err(|_| FrameError::Invalid)?,
        InstallId::from_digest(reader.digest()?),
        AuthorityId::from_digest(reader.digest()?),
        reader.digest()?,
    ))
}

fn encode_presence(writer: &mut Writer, presence: &PresenceRef) -> Result<(), FrameError> {
    writer.digest(presence.stable_id());
    writer.bool(presence.subject_hint().is_some());
    if let Some(subject) = presence.subject_hint() {
        writer.text_u8(subject.as_str())?;
    }
    writer.bool(presence.workspace().is_some());
    if let Some(workspace) = presence.workspace() {
        writer.digest(workspace.digest());
    }
    Ok(())
}

fn decode_presence(reader: &mut Reader<'_>) -> Result<PresenceRef, FrameError> {
    let stable_id = reader.digest()?;
    let subject = if reader.bool()? {
        Some(SubjectNamespace::parse(reader.text_u8()?).map_err(|_| FrameError::Invalid)?)
    } else {
        None
    };
    let workspace = if reader.bool()? {
        Some(WorkspaceHint::from_digest(reader.digest()?))
    } else {
        None
    };
    Ok(PresenceRef::new(stable_id, subject, workspace))
}

fn encode_kind(writer: &mut Writer, kind: &ObservationKind) {
    match kind {
        ObservationKind::Presence(value) => {
            writer.u8(0);
            writer.u8(match value {
                PresenceOp::Seen => 0,
                PresenceOp::Released => 1,
            });
        }
        ObservationKind::Session(SessionOp::Observed) => writer.u8(1),
        ObservationKind::Lifecycle(value) => {
            writer.u8(2);
            writer.u8(match value {
                LifecycleOp::Clear => 0,
                LifecycleOp::Set(ReportedSessionLifecycle::Open) => 1,
                LifecycleOp::Set(ReportedSessionLifecycle::Ended) => 2,
                LifecycleOp::Set(ReportedSessionLifecycle::Failed) => 3,
            });
        }
        ObservationKind::Activity(value) => {
            writer.u8(3);
            writer.u8(match value {
                ActivityOp::Clear => 0,
                ActivityOp::Set(ReportedActivityState::Working) => 1,
                ActivityOp::Set(ReportedActivityState::WaitingPermission) => 2,
                ActivityOp::Set(ReportedActivityState::Idle) => 3,
            });
        }
        ObservationKind::Turn(value) => {
            writer.u8(4);
            writer.u8(match value {
                TurnOp::Started => 0,
                TurnOp::Updated => 1,
                TurnOp::Completed => 2,
                TurnOp::Failed => 3,
                TurnOp::UnattributedEvidence => 4,
            });
        }
        ObservationKind::Permission(value) => {
            writer.u8(5);
            writer.u8(match value {
                PermissionOp::Requested => 0,
                PermissionOp::Granted => 1,
                PermissionOp::Denied => 2,
            });
        }
        ObservationKind::Tool(value) => {
            writer.u8(6);
            writer.u8(match value {
                ToolOp::Started => 0,
                ToolOp::Completed => 1,
                ToolOp::Failed => 2,
            });
        }
        ObservationKind::Agent(value) => {
            writer.u8(7);
            writer.u8(match value {
                AgentOp::Observed => 0,
                AgentOp::Released => 1,
            });
        }
        ObservationKind::Change(value) => {
            writer.u8(8);
            writer.u8(match value.kind {
                ChangeKind::Added => 0,
                ChangeKind::Modified => 1,
                ChangeKind::Deleted => 2,
                ChangeKind::Renamed => 3,
            });
        }
        ObservationKind::Artifact(value) => {
            writer.u8(9);
            writer.digest(value.key.digest());
            writer.u8(match value.kind {
                ArtifactKind::Document => 0,
                ArtifactKind::Link => 1,
                ArtifactKind::Media => 2,
                ArtifactKind::Other => 3,
            });
        }
        ObservationKind::Presentation(value) => {
            writer.u8(10);
            writer.u8(match value {
                PresentationOp::Set => 0,
                PresentationOp::Clear => 1,
            });
        }
        ObservationKind::Diagnostic(value) => {
            writer.u8(11);
            writer.u8(match value.code {
                DiagnosticCode::UnsupportedEvent => 0,
                DiagnosticCode::MissingStableIdentity => 1,
                DiagnosticCode::PartialCoverage => 2,
                DiagnosticCode::ProviderUnavailable => 3,
            });
        }
    }
}

fn decode_kind(reader: &mut Reader<'_>) -> Result<ObservationKind, FrameError> {
    Ok(match reader.u8()? {
        0 => ObservationKind::Presence(match reader.u8()? {
            0 => PresenceOp::Seen,
            1 => PresenceOp::Released,
            _ => return Err(FrameError::Invalid),
        }),
        1 => ObservationKind::Session(SessionOp::Observed),
        2 => ObservationKind::Lifecycle(match reader.u8()? {
            0 => LifecycleOp::Clear,
            1 => LifecycleOp::Set(ReportedSessionLifecycle::Open),
            2 => LifecycleOp::Set(ReportedSessionLifecycle::Ended),
            3 => LifecycleOp::Set(ReportedSessionLifecycle::Failed),
            _ => return Err(FrameError::Invalid),
        }),
        3 => ObservationKind::Activity(match reader.u8()? {
            0 => ActivityOp::Clear,
            1 => ActivityOp::Set(ReportedActivityState::Working),
            2 => ActivityOp::Set(ReportedActivityState::WaitingPermission),
            3 => ActivityOp::Set(ReportedActivityState::Idle),
            _ => return Err(FrameError::Invalid),
        }),
        4 => ObservationKind::Turn(match reader.u8()? {
            0 => TurnOp::Started,
            1 => TurnOp::Updated,
            2 => TurnOp::Completed,
            3 => TurnOp::Failed,
            4 => TurnOp::UnattributedEvidence,
            _ => return Err(FrameError::Invalid),
        }),
        5 => ObservationKind::Permission(match reader.u8()? {
            0 => PermissionOp::Requested,
            1 => PermissionOp::Granted,
            2 => PermissionOp::Denied,
            _ => return Err(FrameError::Invalid),
        }),
        6 => ObservationKind::Tool(match reader.u8()? {
            0 => ToolOp::Started,
            1 => ToolOp::Completed,
            2 => ToolOp::Failed,
            _ => return Err(FrameError::Invalid),
        }),
        7 => ObservationKind::Agent(match reader.u8()? {
            0 => AgentOp::Observed,
            1 => AgentOp::Released,
            _ => return Err(FrameError::Invalid),
        }),
        8 => ObservationKind::Change(ChangeObservation {
            kind: match reader.u8()? {
                0 => ChangeKind::Added,
                1 => ChangeKind::Modified,
                2 => ChangeKind::Deleted,
                3 => ChangeKind::Renamed,
                _ => return Err(FrameError::Invalid),
            },
        }),
        9 => ObservationKind::Artifact(ArtifactObservation {
            key: ArtifactKey::from_digest(reader.digest()?),
            kind: match reader.u8()? {
                0 => ArtifactKind::Document,
                1 => ArtifactKind::Link,
                2 => ArtifactKind::Media,
                3 => ArtifactKind::Other,
                _ => return Err(FrameError::Invalid),
            },
        }),
        10 => ObservationKind::Presentation(match reader.u8()? {
            0 => PresentationOp::Set,
            1 => PresentationOp::Clear,
            _ => return Err(FrameError::Invalid),
        }),
        11 => ObservationKind::Diagnostic(SanitizedDiagnostic {
            code: match reader.u8()? {
                0 => DiagnosticCode::UnsupportedEvent,
                1 => DiagnosticCode::MissingStableIdentity,
                2 => DiagnosticCode::PartialCoverage,
                3 => DiagnosticCode::ProviderUnavailable,
                _ => return Err(FrameError::Invalid),
            },
        }),
        _ => return Err(FrameError::Invalid),
    })
}

fn support_tag(value: CapabilitySupport) -> u8 {
    match value {
        CapabilitySupport::Confirmed => 0,
        CapabilitySupport::Partial => 1,
        CapabilitySupport::Unsupported => 2,
        CapabilitySupport::Unknown => 3,
    }
}

fn decode_support(value: u8) -> Result<CapabilitySupport, FrameError> {
    match value {
        0 => Ok(CapabilitySupport::Confirmed),
        1 => Ok(CapabilitySupport::Partial),
        2 => Ok(CapabilitySupport::Unsupported),
        3 => Ok(CapabilitySupport::Unknown),
        _ => Err(FrameError::Invalid),
    }
}

fn authority_tag(value: EvidenceAuthority) -> u8 {
    match value {
        EvidenceAuthority::None => 0,
        EvidenceAuthority::Observational => 1,
        EvidenceAuthority::Authoritative => 2,
    }
}

fn decode_authority(value: u8) -> Result<EvidenceAuthority, FrameError> {
    match value {
        0 => Ok(EvidenceAuthority::None),
        1 => Ok(EvidenceAuthority::Observational),
        2 => Ok(EvidenceAuthority::Authoritative),
        _ => Err(FrameError::Invalid),
    }
}

fn provenance_tag(value: EvidenceProvenance) -> u8 {
    match value {
        EvidenceProvenance::NativeControlPlane => 0,
        EvidenceProvenance::InstrumentedHook => 1,
        EvidenceProvenance::AggregatedHookAuthority => 2,
        EvidenceProvenance::AggregatedScreenInference => 3,
        EvidenceProvenance::ProcessPresence => 4,
        EvidenceProvenance::VcsInference => 5,
    }
}

fn decode_provenance(value: u8) -> Result<EvidenceProvenance, FrameError> {
    match value {
        0 => Ok(EvidenceProvenance::NativeControlPlane),
        1 => Ok(EvidenceProvenance::InstrumentedHook),
        2 => Ok(EvidenceProvenance::AggregatedHookAuthority),
        3 => Ok(EvidenceProvenance::AggregatedScreenInference),
        4 => Ok(EvidenceProvenance::ProcessPresence),
        5 => Ok(EvidenceProvenance::VcsInference),
        _ => Err(FrameError::Invalid),
    }
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.u64(value);
        }
    }

    fn bytes(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }

    fn digest(&mut self, value: &StableDigest) {
        self.bytes(value.as_bytes());
    }

    fn text_u8(&mut self, value: &str) -> Result<(), FrameError> {
        let length = u8::try_from(value.len()).map_err(|_| FrameError::TooLarge)?;
        self.u8(length);
        self.bytes(value.as_bytes());
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], FrameError> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(FrameError::Invalid)?;
        let output = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(output)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, FrameError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| FrameError::Invalid)?,
        ))
    }

    fn u64(&mut self) -> Result<u64, FrameError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| FrameError::Invalid)?,
        ))
    }

    fn bool(&mut self) -> Result<bool, FrameError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(FrameError::Invalid),
        }
    }

    fn optional_u64(&mut self) -> Result<Option<u64>, FrameError> {
        if self.bool()? {
            Ok(Some(self.u64()?))
        } else {
            Ok(None)
        }
    }

    fn digest(&mut self) -> Result<StableDigest, FrameError> {
        Ok(StableDigest::from_bytes(
            self.take(32)?.try_into().map_err(|_| FrameError::Invalid)?,
        ))
    }

    fn text_u8(&mut self) -> Result<&'a str, FrameError> {
        let count = usize::from(self.u8()?);
        std::str::from_utf8(self.take(count)?).map_err(|_| FrameError::Invalid)
    }

    fn finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> EventEnvelope {
        let subject = SubjectNamespace::parse("test/agent").expect("subject");
        let workspace = WorkspaceHint::from_digest(StableDigest::from_bytes([3; 32]));
        let session = SessionRef::new(
            SessionKey::new(
                subject,
                InstallId::from_digest(StableDigest::from_bytes([4; 32])),
                AuthorityId::from_digest(StableDigest::from_bytes([5; 32])),
                StableDigest::from_bytes([6; 32]),
            ),
            workspace.clone(),
        );
        EventEnvelope {
            stream: StreamRef {
                observer: ObserverId::parse("test/hook").expect("observer"),
                instance: ObserverInstanceId::from_digest(StableDigest::from_bytes([1; 32])),
                epoch: StreamEpoch::from_digest(StableDigest::from_bytes([2; 32])),
            },
            event_id: EventId::from_digest(StableDigest::from_bytes([7; 32])),
            sequence: Some(StreamSequence::new(1)),
            op: StreamOp::Upsert(
                BoundedVec::try_from_vec(vec![AgentObservation {
                    observed_at: Timestamp::from_unix_millis(10),
                    valid_until: Some(Timestamp::from_unix_millis(20)),
                    presence: None,
                    session: Some(session),
                    agent: None,
                    turn: None,
                    workspace: Some(workspace),
                    kind: ObservationKind::Activity(ActivityOp::Set(
                        ReportedActivityState::Working,
                    )),
                    evidence: EvidenceClaim {
                        support: CapabilitySupport::Confirmed,
                        authority: EvidenceAuthority::Authoritative,
                        provenance: EvidenceProvenance::InstrumentedHook,
                    },
                }])
                .expect("observations"),
            ),
        }
    }

    #[test]
    fn observation_frame_round_trips_without_raw_payload_fields() {
        let frame = ObservationFrame::new(event()).expect("valid frame");
        let bytes = frame.encode().expect("encode");
        assert!(bytes.len() <= MAX_LIVE_FRAME_BYTES);
        assert_eq!(ObservationFrame::decode(&bytes).expect("decode"), frame);
    }

    #[test]
    fn frame_rejects_unsupported_hook_operations() {
        let mut event = event();
        event.op = StreamOp::Gap {
            expected: None,
            received: None,
        };
        assert_eq!(ObservationFrame::new(event), Err(FrameError::Unsupported));
    }

    fn observation_for(kind: ObservationKind) -> AgentObservation {
        let base = event();
        let StreamOp::Upsert(observations) = base.op else {
            unreachable!("fixture event is an upsert");
        };
        let mut observation = observations[0].clone();
        observation.valid_until = None;
        observation.kind = kind;
        let session = observation.session.as_ref().expect("session").clone();
        observation.presence =
            matches!(observation.kind, ObservationKind::Presence(_)).then(|| {
                PresenceRef::new(
                    StableDigest::from_bytes([8; 32]),
                    Some(session.key().subject().clone()),
                    Some(session.workspace().clone()),
                )
            });
        observation.agent = matches!(observation.kind, ObservationKind::Agent(_)).then(|| {
            AgentRef::new(
                AgentKey::new(session.key().clone(), StableDigest::from_bytes([9; 32])),
                Some(AgentKey::new(
                    session.key().clone(),
                    StableDigest::from_bytes([10; 32]),
                )),
                Some(AgentKind::Subagent),
            )
        });
        observation.turn = matches!(
            observation.kind,
            ObservationKind::Turn(
                TurnOp::Started | TurnOp::Updated | TurnOp::Completed | TurnOp::Failed
            )
        )
        .then(|| {
            TurnKey::new(
                session.key().clone(),
                AuthorityId::from_digest(StableDigest::from_bytes([11; 32])),
                StableDigest::from_bytes([12; 32]),
            )
        });
        observation
    }

    #[test]
    fn every_normalized_observation_variant_round_trips_through_the_live_frame() {
        let variants = vec![
            ObservationKind::Presence(PresenceOp::Seen),
            ObservationKind::Presence(PresenceOp::Released),
            ObservationKind::Session(SessionOp::Observed),
            ObservationKind::Lifecycle(LifecycleOp::Clear),
            ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open)),
            ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Ended)),
            ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Failed)),
            ObservationKind::Activity(ActivityOp::Clear),
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::WaitingPermission)),
            ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Idle)),
            ObservationKind::Turn(TurnOp::Started),
            ObservationKind::Turn(TurnOp::Updated),
            ObservationKind::Turn(TurnOp::Completed),
            ObservationKind::Turn(TurnOp::Failed),
            ObservationKind::Turn(TurnOp::UnattributedEvidence),
            ObservationKind::Permission(PermissionOp::Requested),
            ObservationKind::Permission(PermissionOp::Granted),
            ObservationKind::Permission(PermissionOp::Denied),
            ObservationKind::Tool(ToolOp::Started),
            ObservationKind::Tool(ToolOp::Completed),
            ObservationKind::Tool(ToolOp::Failed),
            ObservationKind::Agent(AgentOp::Observed),
            ObservationKind::Agent(AgentOp::Released),
            ObservationKind::Change(ChangeObservation {
                kind: ChangeKind::Added,
            }),
            ObservationKind::Change(ChangeObservation {
                kind: ChangeKind::Modified,
            }),
            ObservationKind::Change(ChangeObservation {
                kind: ChangeKind::Deleted,
            }),
            ObservationKind::Change(ChangeObservation {
                kind: ChangeKind::Renamed,
            }),
            ObservationKind::Artifact(ArtifactObservation {
                key: ArtifactKey::from_digest(StableDigest::from_bytes([13; 32])),
                kind: ArtifactKind::Document,
            }),
            ObservationKind::Artifact(ArtifactObservation {
                key: ArtifactKey::from_digest(StableDigest::from_bytes([14; 32])),
                kind: ArtifactKind::Link,
            }),
            ObservationKind::Artifact(ArtifactObservation {
                key: ArtifactKey::from_digest(StableDigest::from_bytes([15; 32])),
                kind: ArtifactKind::Media,
            }),
            ObservationKind::Artifact(ArtifactObservation {
                key: ArtifactKey::from_digest(StableDigest::from_bytes([16; 32])),
                kind: ArtifactKind::Other,
            }),
            ObservationKind::Presentation(PresentationOp::Set),
            ObservationKind::Presentation(PresentationOp::Clear),
            ObservationKind::Diagnostic(SanitizedDiagnostic {
                code: DiagnosticCode::UnsupportedEvent,
            }),
            ObservationKind::Diagnostic(SanitizedDiagnostic {
                code: DiagnosticCode::MissingStableIdentity,
            }),
            ObservationKind::Diagnostic(SanitizedDiagnostic {
                code: DiagnosticCode::PartialCoverage,
            }),
            ObservationKind::Diagnostic(SanitizedDiagnostic {
                code: DiagnosticCode::ProviderUnavailable,
            }),
        ];

        for (index, kind) in variants.into_iter().enumerate() {
            let mut event = event();
            event.event_id = EventId::from_digest(StableDigest::from_bytes([index as u8; 32]));
            event.op = StreamOp::Upsert(
                BoundedVec::try_from_vec(vec![observation_for(kind.clone())]).expect("observation"),
            );
            let frame = ObservationFrame::new(event).expect("valid frame");
            let decoded = ObservationFrame::decode(&frame.encode().expect("encode"))
                .expect("decode normalized variant");
            assert_eq!(decoded, frame, "failed to round-trip {kind:?}");
        }
    }

    #[test]
    fn live_ack_mapping_preserves_generation_and_fails_closed_for_unknown_tags() {
        for (ack, expected) in [
            (
                LiveAck::Accepted {
                    receiver_generation: 42,
                },
                PublishOutcome::Accepted {
                    receiver_generation: 42,
                },
            ),
            (LiveAck::NotMember, PublishOutcome::NotMember),
            (LiveAck::Busy, PublishOutcome::Busy),
            (LiveAck::VersionMismatch, PublishOutcome::Incompatible),
            (LiveAck::Invalid, PublishOutcome::Rejected),
        ] {
            assert_eq!(decode_ack(&encode_ack(ack)), expected);
        }
        let mut unknown = encode_ack(LiveAck::Accepted {
            receiver_generation: u64::MAX,
        });
        unknown[0] = u8::MAX;
        assert_eq!(decode_ack(&unknown), PublishOutcome::Rejected);
    }

    #[test]
    fn frame_decoder_rejects_truncation_trailing_bytes_and_invalid_workspace_scope() {
        let frame = ObservationFrame::new(event()).expect("valid frame");
        let encoded = frame.encode().expect("encoded frame");
        for end in 0..encoded.len() {
            assert!(
                ObservationFrame::decode(&encoded[..end]).is_err(),
                "truncated frame of {end} bytes was accepted"
            );
        }

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            ObservationFrame::decode(&trailing),
            Err(FrameError::Invalid)
        );

        let mut wrong_magic = encoded;
        wrong_magic[0] = b'X';
        assert_eq!(
            ObservationFrame::decode(&wrong_magic),
            Err(FrameError::Invalid)
        );

        let mut multiple_workspaces = event();
        let StreamOp::Upsert(ref observations) = multiple_workspaces.op else {
            unreachable!("fixture is an upsert");
        };
        let mut second = observations[0].clone();
        second.workspace = Some(WorkspaceHint::from_digest(StableDigest::from_bytes(
            [9; 32],
        )));
        multiple_workspaces.op = StreamOp::Upsert(
            BoundedVec::try_from_vec(vec![observations[0].clone(), second])
                .expect("two observations"),
        );
        assert_eq!(
            ObservationFrame::new(multiple_workspaces),
            Err(FrameError::Invalid)
        );
    }
}
