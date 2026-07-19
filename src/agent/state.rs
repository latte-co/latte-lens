use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{
    ActivityOp, AgentKey, AgentKind, AgentObservation, AgentRef, ArtifactKey, BoundedVec,
    CapabilitySupport, DecisionDisposition, DecisionTrace, DecisionValue, EventId,
    EvidenceAuthority, EvidenceClaim, EvidenceDomain, EvidenceExpiryKey, EvidenceExpiryUpdate,
    EvidenceProvenance, InstanceContract, LifecycleOp, MAX_METADATA_AGENTS, MetadataSnapshot,
    ObservationEnvelope, ObservationKind, ObservedEntityKey, ObservedEntityKind, ObserverId,
    ObserverInstanceId, PresenceOp, PresenceRef, ReportedActivityState, ReportedSessionLifecycle,
    SessionDiscovery, SessionKey, SessionLifecycleHint, SessionMetadata, SessionMetadataDelta,
    SessionRef, SnapshotCompleteness, SnapshotEnvelope, SnapshotId, SnapshotScope, StreamEpoch,
    StreamOp, StreamRef, StreamSequence, Timestamp, TurnKey, ValidatedEnvelope, WorkspaceScope,
    project_metadata,
};

pub const MAX_VIEW_SESSIONS: usize = 256;
pub const MAX_VIEW_PRESENCES: usize = 64;
const MAX_SEEN_EVENTS_PER_STREAM: usize = 4_096;
const MAX_PENDING_SNAPSHOT_ITEMS: usize = 256;
const MAX_OBSERVERS_PER_SESSION: usize = 8;
const MAX_LIVE_TURNS_PER_SESSION: usize = 256;
const MAX_LIVE_ARTIFACTS_PER_SESSION: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SessionLifecycle {
    Unknown,
    Open,
    Ended,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ActivityState {
    Unknown,
    Working,
    WaitingPermission,
    Idle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationMode {
    MetadataOnly,
    LiveObserved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationFreshness {
    Unknown,
    Current,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewCompleteness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentViewSession {
    pub session: SessionKey,
    pub subject: super::SubjectNamespace,
    pub observers: BoundedVec<ObserverId, 8>,
    pub discovery: SessionDiscovery,
    pub mode: ObservationMode,
    pub lifecycle: SessionLifecycle,
    pub activity: ActivityState,
    pub freshness: ObservationFreshness,
    pub completeness: ViewCompleteness,
    pub reconciling: bool,
    pub gap_count: u32,
    pub dropped_live_events: u32,
    pub first_observed_at: Timestamp,
    pub last_observed_at: Timestamp,
    pub live_observing_since: Option<Timestamp>,
    pub known_agents: usize,
    pub live_agents: usize,
    pub agents_truncated: bool,
    pub changes: usize,
    pub artifacts: usize,
    pub turns: usize,
    pub agents: BoundedVec<AgentViewAgent, MAX_METADATA_AGENTS>,
    pub coverage: ObservationCoverage,
    pub decisions: BoundedVec<DecisionTrace, 12>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentViewAgent {
    pub key: AgentKey,
    pub parent: Option<AgentKey>,
    pub kind: Option<AgentKind>,
    pub live: bool,
}

impl AgentViewSession {
    pub fn short_key(&self) -> String {
        self.session.stable_id().to_hex().chars().take(8).collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationCoverage {
    pub metadata_first_observed_at: Timestamp,
    pub live_observing_since: Option<Timestamp>,
    pub start_event_seen: bool,
    pub terminal_event_seen: bool,
    pub observers: BoundedVec<ObserverCoverage, 8>,
    pub observers_truncated: bool,
    pub agents_truncated: bool,
    pub dropped_live_events: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverCoverage {
    pub observer: ObserverId,
    pub instance: Option<ObserverInstanceId>,
    pub observing_since: Timestamp,
    pub snapshot_completeness: Option<SnapshotCompleteness>,
    pub last_reconciled_at: Option<Timestamp>,
    pub stream_gap_count: u32,
    pub dropped_events: u32,
    pub reconciling: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentViewPresence {
    pub presence: PresenceRef,
    pub observer: ObserverId,
    pub freshness: ObservationFreshness,
    pub last_observed_at: Timestamp,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentViewState {
    pub sessions: BoundedVec<AgentViewSession, MAX_VIEW_SESSIONS>,
    pub unattributed_presences: BoundedVec<AgentViewPresence, MAX_VIEW_PRESENCES>,
    pub known_count: usize,
    pub live_count: usize,
    pub visible_count: usize,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyDisposition {
    Applied,
    Duplicate,
    StaleSequence,
    UnsequencedAfterSequenced,
    Expired,
    WrongGeneration,
    WrongEpoch,
    GapDetected,
    AwaitingSnapshot,
    UnsupportedCapability,
    EqualAuthorityConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyResult {
    pub disposition: ApplyDisposition,
    pub changed: bool,
    pub metadata_deltas: BoundedVec<SessionMetadataDelta, 64>,
    pub expiry_updates: BoundedVec<EvidenceExpiryUpdate, 64>,
}

impl ApplyResult {
    fn empty(disposition: ApplyDisposition) -> Self {
        Self {
            disposition,
            changed: false,
            metadata_deltas: BoundedVec::new(),
            expiry_updates: BoundedVec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SourceKey {
    observer: ObserverId,
    instance: ObserverInstanceId,
}

impl SourceKey {
    fn from_stream(stream: &StreamRef) -> Self {
        Self {
            observer: stream.observer.clone(),
            instance: stream.instance.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CandidateValue {
    Lifecycle(SessionLifecycle),
    Activity(ActivityState),
    Present,
}

impl CandidateValue {
    const fn decision(self) -> DecisionValue {
        match self {
            Self::Lifecycle(value) => DecisionValue::Lifecycle(value),
            Self::Activity(value) => DecisionValue::Activity(value),
            Self::Present => DecisionValue::Present,
        }
    }
}

#[derive(Clone, Debug)]
struct EvidenceCandidate {
    source: SourceKey,
    domain: EvidenceDomain,
    value: CandidateValue,
    support: CapabilitySupport,
    authority: EvidenceAuthority,
    provenance: EvidenceProvenance,
    observed_at: Timestamp,
    valid_until: Option<Timestamp>,
}

#[derive(Clone, Debug)]
struct SessionRecord {
    session: SessionRef,
    observers: BTreeSet<ObserverId>,
    discovery: SessionDiscovery,
    mode: ObservationMode,
    metadata_terminal: SessionLifecycle,
    first_observed_at: Timestamp,
    last_observed_at: Timestamp,
    live_observing_since: Option<Timestamp>,
    candidates: BTreeMap<(SourceKey, EvidenceDomain), EvidenceCandidate>,
    live_sources: BTreeMap<SourceKey, Timestamp>,
    known_agents: BTreeMap<AgentKey, AgentLiveState>,
    agents_truncated: bool,
    observers_truncated: bool,
    artifacts: BTreeMap<ArtifactKey, BTreeMap<SourceKey, EvidenceClaim>>,
    changes: BTreeMap<SourceKey, (usize, EvidenceClaim)>,
    turns: BTreeMap<super::TurnKey, BTreeMap<SourceKey, EvidenceClaim>>,
    expired_domains: BTreeSet<EvidenceDomain>,
    metadata_partial: bool,
    dropped_live_events: u32,
}

#[derive(Clone, Debug)]
struct AgentLiveState {
    agent: AgentRef,
    sources: BTreeMap<SourceKey, EvidenceClaim>,
}

#[derive(Clone, Debug)]
struct PresenceLiveState {
    view: AgentViewPresence,
    evidence: EvidenceClaim,
}

impl SessionRecord {
    fn from_metadata(metadata: &SessionMetadata) -> Self {
        Self {
            session: metadata.session.clone(),
            observers: metadata.observers.iter().cloned().collect(),
            discovery: metadata.discovery,
            mode: ObservationMode::MetadataOnly,
            metadata_terminal: if metadata.revived {
                SessionLifecycle::Unknown
            } else {
                metadata
                    .terminal
                    .map_or(SessionLifecycle::Unknown, |terminal| {
                        lifecycle_from_hint(terminal.lifecycle)
                    })
            },
            first_observed_at: metadata.first_observed_at,
            last_observed_at: metadata.last_observed_at,
            live_observing_since: None,
            candidates: BTreeMap::new(),
            live_sources: BTreeMap::new(),
            known_agents: metadata
                .known_agents
                .iter()
                .map(|agent| {
                    (
                        agent.key.clone(),
                        AgentLiveState {
                            agent: AgentRef::new(
                                agent.key.clone(),
                                agent.parent.clone(),
                                agent.kind,
                            ),
                            sources: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
            agents_truncated: metadata.agents_truncated,
            observers_truncated: metadata.observers_truncated,
            artifacts: BTreeMap::new(),
            changes: BTreeMap::new(),
            turns: BTreeMap::new(),
            expired_domains: BTreeSet::new(),
            metadata_partial: true,
            dropped_live_events: 0,
        }
    }

    fn from_live(session: SessionRef, observed_at: Timestamp, start: bool) -> Self {
        Self {
            session,
            observers: BTreeSet::new(),
            discovery: if start {
                SessionDiscovery::StartConfirmed
            } else {
                SessionDiscovery::DiscoveredMidSession
            },
            mode: ObservationMode::LiveObserved,
            metadata_terminal: SessionLifecycle::Unknown,
            first_observed_at: observed_at,
            last_observed_at: observed_at,
            live_observing_since: Some(observed_at),
            candidates: BTreeMap::new(),
            live_sources: BTreeMap::new(),
            known_agents: BTreeMap::new(),
            agents_truncated: false,
            observers_truncated: false,
            artifacts: BTreeMap::new(),
            changes: BTreeMap::new(),
            turns: BTreeMap::new(),
            expired_domains: BTreeSet::new(),
            metadata_partial: false,
            dropped_live_events: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct StreamState {
    epoch: StreamEpoch,
    last_sequence: Option<StreamSequence>,
    seen_events: VecDeque<EventId>,
    reconciling: bool,
    completeness: SnapshotCompleteness,
    gap_count: u32,
    last_reconciled_at: Option<Timestamp>,
}

#[derive(Clone, Debug)]
struct PendingSnapshot {
    snapshot_id: SnapshotId,
    next_chunk: u16,
    captured_at: Timestamp,
    scope: SnapshotScope,
    completeness: SnapshotCompleteness,
    watermark: Option<StreamSequence>,
    observations: Vec<AgentObservation>,
}

#[derive(Debug)]
pub struct AgentState {
    generation: u64,
    sessions: BTreeMap<SessionKey, SessionRecord>,
    presences: BTreeMap<(PresenceRef, SourceKey), PresenceLiveState>,
    streams: BTreeMap<SourceKey, StreamState>,
    pending_snapshots: BTreeMap<SourceKey, PendingSnapshot>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new(0)
    }
}

impl AgentState {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            sessions: BTreeMap::new(),
            presences: BTreeMap::new(),
            streams: BTreeMap::new(),
            pending_snapshots: BTreeMap::new(),
        }
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn select_generation(&mut self, generation: u64) {
        self.generation = generation;
        self.sessions.clear();
        self.presences.clear();
        self.streams.clear();
        self.pending_snapshots.clear();
    }

    pub fn bootstrap_metadata(
        &mut self,
        generation: u64,
        snapshot: MetadataSnapshot,
    ) -> ApplyResult {
        if generation != self.generation {
            return ApplyResult::empty(ApplyDisposition::WrongGeneration);
        }
        let mut changed = false;
        for metadata in snapshot.sessions {
            changed = true;
            self.sessions.insert(
                metadata.session.key().clone(),
                SessionRecord::from_metadata(&metadata),
            );
        }
        ApplyResult {
            disposition: ApplyDisposition::Applied,
            changed,
            metadata_deltas: BoundedVec::new(),
            expiry_updates: BoundedVec::new(),
        }
    }

    pub fn apply_envelope(&mut self, generation: u64, envelope: ValidatedEnvelope) -> ApplyResult {
        if generation != self.generation {
            return ApplyResult::empty(ApplyDisposition::WrongGeneration);
        }
        let metadata_deltas = BoundedVec::try_from_vec(
            project_metadata(&envelope)
                .into_vec()
                .into_iter()
                .map(|mut delta| {
                    delta.generation = generation;
                    delta
                })
                .collect(),
        )
        .expect("metadata projection remains bounded");
        match envelope.into_inner() {
            ObservationEnvelope::Event(event) => {
                self.apply_event(event, metadata_deltas, generation)
            }
            ObservationEnvelope::Snapshot(snapshot) => {
                self.apply_snapshot(snapshot, metadata_deltas, generation)
            }
        }
    }

    /// Re-arbitrate all evidence owned by an instance after a probed contract
    /// changes. Evidence that the new contract can no longer justify is
    /// removed per source; evidence from other instances is preserved.
    pub fn apply_contract_update(
        &mut self,
        generation: u64,
        contract: &InstanceContract,
    ) -> ApplyResult {
        if generation != self.generation {
            return ApplyResult::empty(ApplyDisposition::WrongGeneration);
        }
        let source = SourceKey {
            observer: contract.observer.clone(),
            instance: contract.instance.clone(),
        };
        let mut changed = self.pending_snapshots.remove(&source).is_some();
        if let Some(stream) = self.streams.get_mut(&source) {
            changed |= !stream.reconciling || stream.completeness != SnapshotCompleteness::Partial;
            stream.reconciling = true;
            stream.completeness = SnapshotCompleteness::Partial;
        }

        for record in self.sessions.values_mut() {
            let subject = record.session.key().subject();
            let candidate_keys = record
                .candidates
                .iter()
                .filter(|((candidate_source, _), candidate)| {
                    candidate_source == &source
                        && !contract.permits_evidence(
                            subject,
                            candidate.domain,
                            EvidenceClaim {
                                support: candidate.support,
                                authority: candidate.authority,
                                provenance: candidate.provenance,
                            },
                        )
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in candidate_keys {
                changed |= record.candidates.remove(&key).is_some();
            }

            for state in record.known_agents.values_mut() {
                changed |= remove_contract_invalid_source(
                    &mut state.sources,
                    &source,
                    contract,
                    subject,
                    EvidenceDomain::AgentTopology,
                );
            }
            changed |= retain_contract_valid_entities(
                &mut record.artifacts,
                &source,
                contract,
                subject,
                EvidenceDomain::Artifact,
            );
            changed |= retain_contract_valid_entities(
                &mut record.turns,
                &source,
                contract,
                subject,
                EvidenceDomain::Turn,
            );
            if record.changes.get(&source).is_some_and(|(_, evidence)| {
                !contract.permits_evidence(subject, EvidenceDomain::Change, *evidence)
            }) {
                record.changes.remove(&source);
                changed = true;
            }
        }

        self.presences.retain(|(presence, presence_source), state| {
            let keep = presence_source != &source
                || contract_permits_optional_subject(
                    contract,
                    presence.subject_hint(),
                    EvidenceDomain::Presence,
                    state.evidence,
                );
            changed |= !keep;
            keep
        });
        ApplyResult {
            disposition: ApplyDisposition::Applied,
            changed,
            metadata_deltas: BoundedVec::new(),
            expiry_updates: BoundedVec::new(),
        }
    }

    pub fn expire_evidence(&mut self, generation: u64, keys: &[EvidenceExpiryKey]) -> ApplyResult {
        if generation != self.generation {
            return ApplyResult::empty(ApplyDisposition::WrongGeneration);
        }
        let mut changed = false;
        for key in keys {
            if key.generation != generation {
                continue;
            }
            if let Some(session) = self.sessions.get_mut(&key.session) {
                let source = SourceKey {
                    observer: key.observer.clone(),
                    instance: key.instance.clone(),
                };
                if session.candidates.remove(&(source, key.domain)).is_some() {
                    session.expired_domains.insert(key.domain);
                    changed = true;
                }
            }
        }
        ApplyResult {
            disposition: if changed {
                ApplyDisposition::Applied
            } else {
                ApplyDisposition::Expired
            },
            changed,
            metadata_deltas: BoundedVec::new(),
            expiry_updates: BoundedVec::new(),
        }
    }

    pub fn record_live_drop(&mut self, session: Option<&SessionKey>, count: u32) -> bool {
        if count == 0 {
            return false;
        }
        if let Some(session) = session.and_then(|key| self.sessions.get_mut(key)) {
            session.dropped_live_events = session.dropped_live_events.saturating_add(count);
            return true;
        }
        if session.is_none() {
            for record in self.sessions.values_mut() {
                record.dropped_live_events = record.dropped_live_events.saturating_add(count);
            }
            return !self.sessions.is_empty();
        }
        false
    }

    pub fn view(&self) -> AgentViewState {
        let known_count = self.sessions.len();
        let live_count = self
            .sessions
            .values()
            .filter(|session| session.mode == ObservationMode::LiveObserved)
            .count();
        let mut sessions = self.sessions.values().collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            right
                .last_observed_at
                .cmp(&left.last_observed_at)
                .then_with(|| left.session.key().cmp(right.session.key()))
        });
        let truncated = sessions.len() > MAX_VIEW_SESSIONS;
        let sessions = sessions
            .into_iter()
            .take(MAX_VIEW_SESSIONS)
            .map(|session| self.project_session(session))
            .collect();
        let presences = self
            .presences
            .values()
            .take(MAX_VIEW_PRESENCES)
            .map(|state| state.view.clone())
            .collect();
        AgentViewState {
            sessions: BoundedVec::try_from_vec(sessions).expect("view sessions capped"),
            unattributed_presences: BoundedVec::try_from_vec(presences)
                .expect("view presences capped"),
            known_count,
            live_count,
            visible_count: known_count.min(MAX_VIEW_SESSIONS),
            truncated,
        }
    }

    fn apply_event(
        &mut self,
        event: super::EventEnvelope,
        metadata_deltas: BoundedVec<SessionMetadataDelta, 64>,
        generation: u64,
    ) -> ApplyResult {
        let source = SourceKey::from_stream(&event.stream);
        let precheck = self.prepare_event_stream(&source, &event);
        if let Some(disposition) = precheck.reject {
            return ApplyResult::empty(disposition);
        }
        match event.op {
            StreamOp::Reset | StreamOp::Gap { .. } => {
                if let Some(stream) = self.streams.get_mut(&source) {
                    stream.reconciling = true;
                    stream.gap_count = stream.gap_count.saturating_add(1);
                }
                ApplyResult {
                    disposition: ApplyDisposition::GapDetected,
                    changed: true,
                    metadata_deltas: BoundedVec::new(),
                    expiry_updates: BoundedVec::new(),
                }
            }
            StreamOp::Delete { entity, domains } => {
                if self
                    .streams
                    .get(&source)
                    .is_some_and(|stream| stream.reconciling)
                {
                    return ApplyResult::empty(ApplyDisposition::AwaitingSnapshot);
                }
                let changed = self.apply_delete(&source, &entity, domains.iter().copied());
                ApplyResult {
                    disposition: ApplyDisposition::Applied,
                    changed,
                    metadata_deltas: BoundedVec::new(),
                    expiry_updates: BoundedVec::new(),
                }
            }
            StreamOp::Upsert(observations) => {
                let (changed, expiry_updates, conflict) =
                    self.apply_observations(&event.stream, &observations, generation);
                ApplyResult {
                    disposition: if precheck.gap {
                        ApplyDisposition::GapDetected
                    } else if conflict {
                        ApplyDisposition::EqualAuthorityConflict
                    } else {
                        ApplyDisposition::Applied
                    },
                    changed,
                    metadata_deltas,
                    expiry_updates,
                }
            }
        }
    }

    fn apply_snapshot(
        &mut self,
        snapshot: SnapshotEnvelope,
        metadata_deltas: BoundedVec<SessionMetadataDelta, 64>,
        generation: u64,
    ) -> ApplyResult {
        let source = SourceKey::from_stream(&snapshot.stream);
        let stream = self.streams.entry(source.clone()).or_insert(StreamState {
            epoch: snapshot.stream.epoch.clone(),
            last_sequence: None,
            seen_events: VecDeque::new(),
            reconciling: false,
            completeness: SnapshotCompleteness::Partial,
            gap_count: 0,
            last_reconciled_at: None,
        });
        if stream.epoch != snapshot.stream.epoch {
            stream.epoch = snapshot.stream.epoch.clone();
            stream.last_sequence = None;
            stream.reconciling = true;
            self.pending_snapshots.remove(&source);
        }

        let pending = self
            .pending_snapshots
            .entry(source.clone())
            .or_insert_with(|| PendingSnapshot {
                snapshot_id: snapshot.snapshot_id.clone(),
                next_chunk: 0,
                captured_at: snapshot.captured_at,
                scope: snapshot.scope.clone(),
                completeness: snapshot.completeness,
                watermark: snapshot.watermark,
                observations: Vec::new(),
            });
        let shape_matches = pending.snapshot_id == snapshot.snapshot_id
            && pending.next_chunk == snapshot.chunk_index
            && pending.captured_at == snapshot.captured_at
            && pending.scope == snapshot.scope
            && pending.completeness == snapshot.completeness
            && pending.watermark == snapshot.watermark;
        if !shape_matches
            || pending
                .observations
                .len()
                .saturating_add(snapshot.observations.len())
                > MAX_PENDING_SNAPSHOT_ITEMS
        {
            self.pending_snapshots.remove(&source);
            if let Some(stream) = self.streams.get_mut(&source) {
                stream.reconciling = true;
                stream.gap_count = stream.gap_count.saturating_add(1);
            }
            return ApplyResult::empty(ApplyDisposition::GapDetected);
        }
        pending
            .observations
            .extend(snapshot.observations.iter().cloned());
        pending.next_chunk = pending.next_chunk.saturating_add(1);
        if !snapshot.final_chunk {
            return ApplyResult::empty(ApplyDisposition::AwaitingSnapshot);
        }
        let pending = self
            .pending_snapshots
            .remove(&source)
            .expect("pending snapshot exists");
        let bounded =
            BoundedVec::<_, MAX_PENDING_SNAPSHOT_ITEMS>::try_from_vec(pending.observations.clone())
                .expect("snapshot aggregate capped");
        let (mut changed, expiry_updates, conflict) =
            self.apply_observations(&snapshot.stream, &bounded, generation);
        if pending.completeness == SnapshotCompleteness::Complete {
            changed |= self.apply_complete_snapshot_tombstones(
                &source,
                &pending.scope,
                &pending.observations,
            );
        }
        if let Some(stream) = self.streams.get_mut(&source) {
            changed |= stream.last_sequence != pending.watermark
                || stream.completeness != pending.completeness
                || stream.reconciling != (pending.completeness != SnapshotCompleteness::Complete);
            stream.last_sequence = pending.watermark;
            stream.completeness = pending.completeness;
            stream.reconciling = pending.completeness != SnapshotCompleteness::Complete;
            stream.last_reconciled_at = Some(pending.captured_at);
        }
        ApplyResult {
            disposition: if conflict {
                ApplyDisposition::EqualAuthorityConflict
            } else {
                ApplyDisposition::Applied
            },
            changed,
            metadata_deltas,
            expiry_updates,
        }
    }

    fn prepare_event_stream(
        &mut self,
        source: &SourceKey,
        event: &super::EventEnvelope,
    ) -> Precheck {
        let stream = self.streams.entry(source.clone()).or_insert(StreamState {
            epoch: event.stream.epoch.clone(),
            last_sequence: None,
            seen_events: VecDeque::new(),
            reconciling: false,
            completeness: SnapshotCompleteness::Partial,
            gap_count: 0,
            last_reconciled_at: None,
        });
        if stream.epoch != event.stream.epoch {
            stream.epoch = event.stream.epoch.clone();
            stream.last_sequence = None;
            stream.seen_events.clear();
            stream.reconciling = true;
            stream.gap_count = stream.gap_count.saturating_add(1);
            return Precheck::reject(ApplyDisposition::WrongEpoch);
        }
        if stream.seen_events.contains(&event.event_id) {
            return Precheck::reject(ApplyDisposition::Duplicate);
        }
        // Reset/Gap are stream-control markers, not ordinary data events.
        // They intentionally remain valid after a sequenced snapshot/event
        // even when the control plane has no sequence of its own.
        if matches!(event.op, StreamOp::Reset | StreamOp::Gap { .. }) {
            stream.seen_events.push_back(event.event_id.clone());
            if stream.seen_events.len() > MAX_SEEN_EVENTS_PER_STREAM {
                stream.seen_events.pop_front();
            }
            return Precheck {
                reject: None,
                gap: false,
            };
        }
        let mut gap = false;
        match (stream.last_sequence, event.sequence) {
            (Some(previous), Some(current)) if current <= previous => {
                return Precheck::reject(ApplyDisposition::StaleSequence);
            }
            (Some(_), None) => {
                return Precheck::reject(ApplyDisposition::UnsequencedAfterSequenced);
            }
            (Some(previous), Some(current))
                if current.get() != previous.get().saturating_add(1) =>
            {
                stream.reconciling = true;
                stream.gap_count = stream.gap_count.saturating_add(1);
                gap = true;
            }
            _ => {}
        }
        if event.sequence.is_some() {
            stream.last_sequence = event.sequence;
        }
        stream.seen_events.push_back(event.event_id.clone());
        if stream.seen_events.len() > MAX_SEEN_EVENTS_PER_STREAM {
            stream.seen_events.pop_front();
        }
        Precheck { reject: None, gap }
    }

    fn apply_observations<const N: usize>(
        &mut self,
        stream: &StreamRef,
        observations: &BoundedVec<AgentObservation, N>,
        generation: u64,
    ) -> (bool, BoundedVec<EvidenceExpiryUpdate, 64>, bool) {
        let source = SourceKey::from_stream(stream);
        let mut changed = false;
        let mut expiry_updates = BoundedVec::new();
        for observation in observations {
            if let Some(session_ref) = &observation.session {
                let start = matches!(
                    observation.kind,
                    ObservationKind::Lifecycle(LifecycleOp::Set(ReportedSessionLifecycle::Open))
                );
                let record = self
                    .sessions
                    .entry(session_ref.key().clone())
                    .or_insert_with(|| {
                        SessionRecord::from_live(
                            session_ref.clone(),
                            observation.observed_at,
                            start,
                        )
                    });
                record.mode = ObservationMode::LiveObserved;
                record.live_observing_since = Some(
                    record
                        .live_observing_since
                        .map_or(observation.observed_at, |current| {
                            current.min(observation.observed_at)
                        }),
                );
                record.first_observed_at = record.first_observed_at.min(observation.observed_at);
                record.last_observed_at = record.last_observed_at.max(observation.observed_at);
                if !record.live_sources.contains_key(&source)
                    && record.live_sources.len() >= MAX_OBSERVERS_PER_SESSION
                {
                    record.observers_truncated = true;
                    record.dropped_live_events = record.dropped_live_events.saturating_add(1);
                    changed = true;
                    continue;
                }
                if record.observers.len() < MAX_OBSERVERS_PER_SESSION
                    || record.observers.contains(&stream.observer)
                {
                    record.observers.insert(stream.observer.clone());
                } else {
                    record.observers_truncated = true;
                }
                record
                    .live_sources
                    .entry(source.clone())
                    .and_modify(|first| *first = (*first).min(observation.observed_at))
                    .or_insert(observation.observed_at);
                if start {
                    record.discovery = SessionDiscovery::StartConfirmed;
                }
                changed |= apply_observation_to_record(record, &source, observation);
                if let Some(valid_until) = observation.valid_until {
                    let _ = expiry_updates.try_push(EvidenceExpiryUpdate {
                        key: EvidenceExpiryKey {
                            generation,
                            session: session_ref.key().clone(),
                            observer: stream.observer.clone(),
                            instance: stream.instance.clone(),
                            domain: observation.domain(),
                        },
                        valid_until,
                    });
                }
            } else if let (ObservationKind::Presence(PresenceOp::Seen), Some(presence)) =
                (&observation.kind, &observation.presence)
            {
                self.presences.insert(
                    (presence.clone(), source.clone()),
                    PresenceLiveState {
                        view: AgentViewPresence {
                            presence: presence.clone(),
                            observer: stream.observer.clone(),
                            freshness: ObservationFreshness::Current,
                            last_observed_at: observation.observed_at,
                        },
                        evidence: observation.evidence,
                    },
                );
                changed = true;
            } else if let (ObservationKind::Presence(PresenceOp::Released), Some(presence)) =
                (&observation.kind, &observation.presence)
            {
                changed |= self
                    .presences
                    .remove(&(presence.clone(), source.clone()))
                    .is_some();
            }
        }
        let conflict = self.sessions.values().any(session_has_conflict);
        (changed, expiry_updates, conflict)
    }

    fn apply_delete(
        &mut self,
        source: &SourceKey,
        entity: &ObservedEntityKey,
        domains: impl Iterator<Item = EvidenceDomain>,
    ) -> bool {
        match entity {
            ObservedEntityKey::Session(session) => {
                let Some(record) = self.sessions.get_mut(session) else {
                    return false;
                };
                let mut changed = false;
                for domain in domains {
                    changed |= record
                        .candidates
                        .remove(&(source.clone(), domain))
                        .is_some();
                }
                changed
            }
            ObservedEntityKey::Presence(presence) => self
                .presences
                .remove(&(presence.clone(), source.clone()))
                .is_some(),
            ObservedEntityKey::Agent(agent) => self
                .sessions
                .get_mut(agent.session())
                .and_then(|record| record.known_agents.get_mut(agent))
                .is_some_and(|state| state.sources.remove(source).is_some()),
            ObservedEntityKey::Artifact(artifact) => {
                let mut changed = false;
                for record in self.sessions.values_mut() {
                    changed |= remove_entity_source(&mut record.artifacts, artifact, source);
                }
                changed
            }
            ObservedEntityKey::Turn(turn) => self
                .sessions
                .get_mut(turn.session())
                .is_some_and(|record| remove_entity_source(&mut record.turns, turn, source)),
        }
    }

    fn apply_complete_snapshot_tombstones(
        &mut self,
        source: &SourceKey,
        scope: &SnapshotScope,
        observations: &[AgentObservation],
    ) -> bool {
        let seen_session_domains = observations
            .iter()
            .filter_map(|observation| {
                observation
                    .session
                    .as_ref()
                    .map(|session| (session.key().clone(), observation.domain()))
            })
            .collect::<BTreeSet<_>>();
        let seen_agents = observations
            .iter()
            .filter_map(|observation| observation.agent.as_ref().map(|agent| agent.key().clone()))
            .collect::<BTreeSet<_>>();
        let seen_artifacts = observations
            .iter()
            .filter_map(|observation| match &observation.kind {
                ObservationKind::Artifact(artifact) => observation
                    .session
                    .as_ref()
                    .map(|session| (session.key().clone(), artifact.key.clone())),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let seen_turns = observations
            .iter()
            .filter_map(|observation| observation.turn.clone())
            .collect::<BTreeSet<_>>();
        let seen_presences = observations
            .iter()
            .filter_map(|observation| observation.presence.clone())
            .collect::<BTreeSet<_>>();
        let mut changed = false;
        for record in self.sessions.values_mut() {
            if !scope_covers_session(scope, &record.session) {
                continue;
            }
            let keys = record
                .candidates
                .keys()
                .filter(|(candidate_source, domain)| {
                    candidate_source == source
                        && scope.domains.contains(domain)
                        && !seen_session_domains.contains(&(record.session.key().clone(), *domain))
                })
                .cloned()
                .collect::<Vec<_>>();
            for key in keys {
                changed |= record.candidates.remove(&key).is_some();
            }
            if scope.entity_kinds.contains(&ObservedEntityKind::Agent)
                && scope.domains.contains(&EvidenceDomain::AgentTopology)
            {
                for (agent, state) in &mut record.known_agents {
                    if !seen_agents.contains(agent) {
                        changed |= state.sources.remove(source).is_some();
                    }
                }
            }
            if scope.entity_kinds.contains(&ObservedEntityKind::Artifact)
                && scope.domains.contains(&EvidenceDomain::Artifact)
            {
                record.artifacts.retain(|artifact, sources| {
                    if !seen_artifacts.contains(&(record.session.key().clone(), artifact.clone())) {
                        changed |= sources.remove(source).is_some();
                    }
                    !sources.is_empty()
                });
            }
            if scope.entity_kinds.contains(&ObservedEntityKind::Turn)
                && scope.domains.contains(&EvidenceDomain::Turn)
            {
                record.turns.retain(|turn, sources| {
                    if !seen_turns.contains(turn) {
                        changed |= sources.remove(source).is_some();
                    }
                    !sources.is_empty()
                });
            }
            if scope.domains.contains(&EvidenceDomain::Change)
                && !seen_session_domains
                    .contains(&(record.session.key().clone(), EvidenceDomain::Change))
            {
                changed |= record.changes.remove(source).is_some();
            }
        }
        if scope.entity_kinds.contains(&ObservedEntityKind::Presence)
            && scope.domains.contains(&EvidenceDomain::Presence)
        {
            self.presences.retain(|(presence, presence_source), _| {
                let keep = presence_source != source
                    || !scope_covers_presence(scope, presence)
                    || seen_presences.contains(presence);
                changed |= !keep;
                keep
            });
        }
        changed
    }

    fn project_session(&self, record: &SessionRecord) -> AgentViewSession {
        let (lifecycle, lifecycle_trace, lifecycle_conflict) =
            arbitrate(record, EvidenceDomain::Lifecycle, record.metadata_terminal);
        let (activity, activity_trace, activity_conflict) =
            arbitrate_activity(record, EvidenceDomain::Activity);
        let source_states = record
            .live_sources
            .keys()
            .filter_map(|source| self.streams.get(source))
            .collect::<Vec<_>>();
        let reconciling = source_states.iter().any(|stream| stream.reconciling);
        let gap_count = source_states.iter().fold(0_u32, |total, stream| {
            total.saturating_add(stream.gap_count)
        });
        let complete_snapshot = !source_states.is_empty()
            && source_states.iter().all(|stream| {
                !stream.reconciling && stream.completeness == SnapshotCompleteness::Complete
            });
        let conflict = lifecycle_conflict || activity_conflict;
        let completeness = if conflict
            || record.metadata_partial
            || record.agents_truncated
            || record.dropped_live_events > 0
            || reconciling
        {
            ViewCompleteness::Partial
        } else if complete_snapshot {
            ViewCompleteness::Complete
        } else {
            ViewCompleteness::Unknown
        };
        let has_semantic_evidence = record.candidates.values().any(|candidate| {
            matches!(
                candidate.domain,
                EvidenceDomain::Lifecycle | EvidenceDomain::Activity
            )
        });
        let freshness = if record.expired_domains.contains(&EvidenceDomain::Activity)
            || record.expired_domains.contains(&EvidenceDomain::Lifecycle)
        {
            ObservationFreshness::Stale
        } else if has_semantic_evidence {
            ObservationFreshness::Current
        } else {
            ObservationFreshness::Unknown
        };
        let mut observer_coverage = record
            .live_sources
            .iter()
            .map(|(source, observing_since)| {
                let stream = self.streams.get(source);
                ObserverCoverage {
                    observer: source.observer.clone(),
                    instance: Some(source.instance.clone()),
                    observing_since: *observing_since,
                    snapshot_completeness: stream.map(|state| state.completeness),
                    last_reconciled_at: stream.and_then(|state| state.last_reconciled_at),
                    stream_gap_count: stream.map_or(0, |state| state.gap_count),
                    dropped_events: 0,
                    reconciling: stream.is_some_and(|state| state.reconciling),
                }
            })
            .collect::<Vec<_>>();
        for observer in &record.observers {
            if !observer_coverage
                .iter()
                .any(|coverage| &coverage.observer == observer)
            {
                observer_coverage.push(ObserverCoverage {
                    observer: observer.clone(),
                    instance: None,
                    observing_since: record.first_observed_at,
                    snapshot_completeness: None,
                    last_reconciled_at: None,
                    stream_gap_count: 0,
                    dropped_events: 0,
                    reconciling: false,
                });
            }
        }
        observer_coverage.sort_by(|left, right| {
            left.observer
                .cmp(&right.observer)
                .then_with(|| left.instance.cmp(&right.instance))
        });
        let observers_truncated = record.observers_truncated || observer_coverage.len() > 8;
        observer_coverage.truncate(8);
        let decisions = vec![lifecycle_trace, activity_trace];
        AgentViewSession {
            session: record.session.key().clone(),
            subject: record.session.key().subject().clone(),
            observers: BoundedVec::try_from_vec(record.observers.iter().take(8).cloned().collect())
                .expect("observer view capped"),
            discovery: record.discovery,
            mode: record.mode,
            lifecycle,
            activity,
            freshness,
            completeness,
            reconciling,
            gap_count,
            dropped_live_events: record.dropped_live_events,
            first_observed_at: record.first_observed_at,
            last_observed_at: record.last_observed_at,
            live_observing_since: record.live_observing_since,
            known_agents: record.known_agents.len(),
            live_agents: record
                .known_agents
                .values()
                .filter(|state| !state.sources.is_empty())
                .count(),
            agents_truncated: record.agents_truncated,
            changes: record
                .changes
                .values()
                .fold(0_usize, |total, (count, _)| total.saturating_add(*count)),
            artifacts: record.artifacts.len(),
            turns: record.turns.len(),
            agents: BoundedVec::try_from_vec(
                record
                    .known_agents
                    .values()
                    .map(|state| AgentViewAgent {
                        key: state.agent.key().clone(),
                        parent: state.agent.parent().cloned(),
                        kind: state.agent.kind(),
                        live: !state.sources.is_empty(),
                    })
                    .collect(),
            )
            .expect("agent topology capped"),
            coverage: ObservationCoverage {
                metadata_first_observed_at: record.first_observed_at,
                live_observing_since: record.live_observing_since,
                start_event_seen: record.discovery == SessionDiscovery::StartConfirmed,
                terminal_event_seen: matches!(
                    lifecycle,
                    SessionLifecycle::Ended | SessionLifecycle::Failed
                ),
                observers: BoundedVec::try_from_vec(observer_coverage)
                    .expect("observer coverage capped"),
                observers_truncated,
                agents_truncated: record.agents_truncated,
                dropped_live_events: record.dropped_live_events,
            },
            decisions: BoundedVec::try_from_vec(decisions).expect("decision domains bounded"),
        }
    }
}

fn contract_permits_optional_subject(
    contract: &InstanceContract,
    subject: Option<&super::SubjectNamespace>,
    domain: EvidenceDomain,
    evidence: EvidenceClaim,
) -> bool {
    if let Some(subject) = subject {
        return contract.permits_evidence(subject, domain, evidence);
    }
    contract.capabilities.get(&domain).is_some_and(|claim| {
        claim.support.permits(evidence.support)
            && claim.max_authority.permits(evidence.authority)
            && claim.provenance == evidence.provenance
    })
}

fn remove_contract_invalid_source(
    sources: &mut BTreeMap<SourceKey, EvidenceClaim>,
    source: &SourceKey,
    contract: &InstanceContract,
    subject: &super::SubjectNamespace,
    domain: EvidenceDomain,
) -> bool {
    if sources
        .get(source)
        .is_some_and(|evidence| !contract.permits_evidence(subject, domain, *evidence))
    {
        sources.remove(source);
        true
    } else {
        false
    }
}

fn retain_contract_valid_entities<K: Ord>(
    entities: &mut BTreeMap<K, BTreeMap<SourceKey, EvidenceClaim>>,
    source: &SourceKey,
    contract: &InstanceContract,
    subject: &super::SubjectNamespace,
    domain: EvidenceDomain,
) -> bool {
    let mut changed = false;
    entities.retain(|_, sources| {
        changed |= remove_contract_invalid_source(sources, source, contract, subject, domain);
        !sources.is_empty()
    });
    changed
}

struct Precheck {
    reject: Option<ApplyDisposition>,
    gap: bool,
}

impl Precheck {
    const fn reject(disposition: ApplyDisposition) -> Self {
        Self {
            reject: Some(disposition),
            gap: false,
        }
    }
}

fn apply_observation_to_record(
    record: &mut SessionRecord,
    source: &SourceKey,
    observation: &AgentObservation,
) -> bool {
    record.expired_domains.remove(&observation.domain());
    match &observation.kind {
        ObservationKind::Lifecycle(LifecycleOp::Set(value)) => insert_candidate(
            record,
            source,
            observation,
            CandidateValue::Lifecycle(match value {
                ReportedSessionLifecycle::Open => SessionLifecycle::Open,
                ReportedSessionLifecycle::Ended => SessionLifecycle::Ended,
                ReportedSessionLifecycle::Failed => SessionLifecycle::Failed,
            }),
        ),
        ObservationKind::Lifecycle(LifecycleOp::Clear) => record
            .candidates
            .remove(&(source.clone(), EvidenceDomain::Lifecycle))
            .is_some(),
        ObservationKind::Activity(ActivityOp::Set(value)) => insert_candidate(
            record,
            source,
            observation,
            CandidateValue::Activity(match value {
                ReportedActivityState::Working => ActivityState::Working,
                ReportedActivityState::WaitingPermission => ActivityState::WaitingPermission,
                ReportedActivityState::Idle => ActivityState::Idle,
            }),
        ),
        ObservationKind::Activity(ActivityOp::Clear) => record
            .candidates
            .remove(&(source.clone(), EvidenceDomain::Activity))
            .is_some(),
        ObservationKind::Session(_) => {
            insert_candidate(record, source, observation, CandidateValue::Present)
        }
        ObservationKind::Agent(super::AgentOp::Observed) => observation
            .agent
            .as_ref()
            .is_some_and(|agent| observe_agent(record, source, agent, observation.evidence)),
        ObservationKind::Agent(super::AgentOp::Released) => observation
            .agent
            .as_ref()
            .and_then(|agent| record.known_agents.get_mut(agent.key()))
            .is_some_and(|state| state.sources.remove(source).is_some()),
        ObservationKind::Turn(super::TurnOp::Started | super::TurnOp::Updated) => observation
            .turn
            .as_ref()
            .is_some_and(|turn| insert_turn(record, source, turn, observation.evidence)),
        ObservationKind::Turn(super::TurnOp::Completed | super::TurnOp::Failed) => observation
            .turn
            .as_ref()
            .is_some_and(|turn| remove_entity_source(&mut record.turns, turn, source)),
        ObservationKind::Turn(super::TurnOp::UnattributedEvidence) => false,
        ObservationKind::Artifact(artifact) => {
            insert_artifact(record, source, &artifact.key, observation.evidence)
        }
        ObservationKind::Change(_) => {
            let (count, evidence) = record
                .changes
                .entry(source.clone())
                .or_insert((0, observation.evidence));
            *count = count.saturating_add(1);
            *evidence = observation.evidence;
            true
        }
        ObservationKind::Permission(_)
        | ObservationKind::Tool(_)
        | ObservationKind::Presence(_)
        | ObservationKind::Presentation(_)
        | ObservationKind::Diagnostic(_) => false,
    }
}

fn observe_agent(
    record: &mut SessionRecord,
    source: &SourceKey,
    agent: &AgentRef,
    evidence: EvidenceClaim,
) -> bool {
    if let Some(existing) = record.known_agents.get_mut(agent.key()) {
        let changed = existing.sources.get(source) != Some(&evidence) || existing.agent != *agent;
        existing.agent = agent.clone();
        existing.sources.insert(source.clone(), evidence);
        return changed;
    }
    if record.known_agents.len() >= MAX_METADATA_AGENTS {
        record.agents_truncated = true;
        record.dropped_live_events = record.dropped_live_events.saturating_add(1);
        return true;
    }
    record.known_agents.insert(
        agent.key().clone(),
        AgentLiveState {
            agent: agent.clone(),
            sources: BTreeMap::from([(source.clone(), evidence)]),
        },
    );
    true
}

fn insert_turn(
    record: &mut SessionRecord,
    source: &SourceKey,
    turn: &TurnKey,
    evidence: EvidenceClaim,
) -> bool {
    if let Some(sources) = record.turns.get_mut(turn) {
        return sources.insert(source.clone(), evidence) != Some(evidence);
    }
    if record.turns.len() >= MAX_LIVE_TURNS_PER_SESSION {
        record.dropped_live_events = record.dropped_live_events.saturating_add(1);
        return true;
    }
    record
        .turns
        .insert(turn.clone(), BTreeMap::from([(source.clone(), evidence)]));
    true
}

fn insert_artifact(
    record: &mut SessionRecord,
    source: &SourceKey,
    artifact: &ArtifactKey,
    evidence: EvidenceClaim,
) -> bool {
    if let Some(sources) = record.artifacts.get_mut(artifact) {
        return sources.insert(source.clone(), evidence) != Some(evidence);
    }
    if record.artifacts.len() >= MAX_LIVE_ARTIFACTS_PER_SESSION {
        record.dropped_live_events = record.dropped_live_events.saturating_add(1);
        return true;
    }
    record.artifacts.insert(
        artifact.clone(),
        BTreeMap::from([(source.clone(), evidence)]),
    );
    true
}

fn remove_entity_source<K: Ord + Clone>(
    entities: &mut BTreeMap<K, BTreeMap<SourceKey, EvidenceClaim>>,
    key: &K,
    source: &SourceKey,
) -> bool {
    let Some(sources) = entities.get_mut(key) else {
        return false;
    };
    let changed = sources.remove(source).is_some();
    if sources.is_empty() {
        entities.remove(key);
    }
    changed
}

fn insert_candidate(
    record: &mut SessionRecord,
    source: &SourceKey,
    observation: &AgentObservation,
    value: CandidateValue,
) -> bool {
    let key = (source.clone(), observation.domain());
    if record
        .candidates
        .get(&key)
        .is_some_and(|current| current.observed_at > observation.observed_at)
    {
        return false;
    }
    let candidate = EvidenceCandidate {
        source: source.clone(),
        domain: observation.domain(),
        value,
        support: observation.evidence.support,
        authority: observation.evidence.authority,
        provenance: observation.evidence.provenance,
        observed_at: observation.observed_at,
        valid_until: observation.valid_until,
    };
    let changed = record.candidates.get(&key).is_none_or(|current| {
        current.value != value || current.observed_at != observation.observed_at
    });
    record.candidates.insert(key, candidate);
    changed
}

fn arbitrate(
    record: &SessionRecord,
    domain: EvidenceDomain,
    fallback: SessionLifecycle,
) -> (SessionLifecycle, DecisionTrace, bool) {
    let candidates = record
        .candidates
        .values()
        .filter(|candidate| candidate.domain == domain)
        .collect::<Vec<_>>();
    let authoritative = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.authority == EvidenceAuthority::Authoritative)
        .collect::<Vec<_>>();
    if authoritative.is_empty() {
        return (
            fallback,
            DecisionTrace::unknown(domain, DecisionDisposition::Suppressed),
            false,
        );
    }
    let values = authoritative
        .iter()
        .map(|candidate| candidate.value)
        .collect::<BTreeSet<_>>();
    if values.len() != 1 {
        return (
            SessionLifecycle::Unknown,
            conflict_trace(domain, &authoritative),
            true,
        );
    }
    let winner = authoritative
        .into_iter()
        .max_by_key(|candidate| candidate.observed_at)
        .expect("authoritative candidate exists");
    let CandidateValue::Lifecycle(value) = winner.value else {
        return (
            fallback,
            DecisionTrace::unknown(domain, DecisionDisposition::Suppressed),
            false,
        );
    };
    (value, applied_trace(winner), false)
}

fn arbitrate_activity(
    record: &SessionRecord,
    domain: EvidenceDomain,
) -> (ActivityState, DecisionTrace, bool) {
    let candidates = record
        .candidates
        .values()
        .filter(|candidate| candidate.domain == domain)
        .collect::<Vec<_>>();
    let authoritative = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.authority == EvidenceAuthority::Authoritative)
        .collect::<Vec<_>>();
    if authoritative.is_empty() {
        return (
            ActivityState::Unknown,
            DecisionTrace::unknown(domain, DecisionDisposition::Suppressed),
            false,
        );
    }
    let values = authoritative
        .iter()
        .map(|candidate| candidate.value)
        .collect::<BTreeSet<_>>();
    if values.len() != 1 {
        return (
            ActivityState::Unknown,
            conflict_trace(domain, &authoritative),
            true,
        );
    }
    let winner = authoritative
        .into_iter()
        .max_by_key(|candidate| candidate.observed_at)
        .expect("authoritative candidate exists");
    let CandidateValue::Activity(value) = winner.value else {
        return (
            ActivityState::Unknown,
            DecisionTrace::unknown(domain, DecisionDisposition::Suppressed),
            false,
        );
    };
    (value, applied_trace(winner), false)
}

fn applied_trace(candidate: &EvidenceCandidate) -> DecisionTrace {
    DecisionTrace {
        domain: candidate.domain,
        effective_value: candidate.value.decision(),
        winning_observer: Some(candidate.source.observer.clone()),
        authority: candidate.authority,
        provenance: Some(candidate.provenance),
        observed_at: Some(candidate.observed_at),
        valid_until: candidate.valid_until,
        disposition: DecisionDisposition::Applied,
        competing: BoundedVec::new(),
    }
}

fn conflict_trace(domain: EvidenceDomain, candidates: &[&EvidenceCandidate]) -> DecisionTrace {
    let competing = candidates
        .iter()
        .take(4)
        .map(|candidate| super::CompetingEvidenceSummary {
            observer: candidate.source.observer.clone(),
            domain,
            authority: candidate.authority,
            current: true,
            disposition: DecisionDisposition::EqualAuthorityConflict,
        })
        .collect();
    DecisionTrace {
        domain,
        effective_value: DecisionValue::Unknown,
        winning_observer: None,
        authority: EvidenceAuthority::Authoritative,
        provenance: None,
        observed_at: candidates
            .iter()
            .map(|candidate| candidate.observed_at)
            .max(),
        valid_until: None,
        disposition: DecisionDisposition::EqualAuthorityConflict,
        competing: BoundedVec::try_from_vec(competing).expect("competing evidence capped"),
    }
}

fn session_has_conflict(record: &SessionRecord) -> bool {
    let (_, _, lifecycle) = arbitrate(record, EvidenceDomain::Lifecycle, SessionLifecycle::Unknown);
    let (_, _, activity) = arbitrate_activity(record, EvidenceDomain::Activity);
    lifecycle || activity
}

fn scope_covers_session(scope: &SnapshotScope, session: &SessionRef) -> bool {
    scope.subjects.contains(session.key().subject())
        && match &scope.workspaces {
            WorkspaceScope::Selected => true,
            WorkspaceScope::Explicit(workspaces) => workspaces.contains(session.workspace()),
        }
}

fn scope_covers_presence(scope: &SnapshotScope, presence: &PresenceRef) -> bool {
    presence
        .subject_hint()
        .is_none_or(|subject| scope.subjects.contains(subject))
        && match &scope.workspaces {
            WorkspaceScope::Selected => true,
            WorkspaceScope::Explicit(workspaces) => presence
                .workspace()
                .is_some_and(|workspace| workspaces.contains(workspace)),
        }
}

const fn lifecycle_from_hint(value: SessionLifecycleHint) -> SessionLifecycle {
    match value {
        SessionLifecycleHint::Unknown | SessionLifecycleHint::Open => SessionLifecycle::Unknown,
        SessionLifecycleHint::Ended => SessionLifecycle::Ended,
        SessionLifecycleHint::Failed => SessionLifecycle::Failed,
    }
}
