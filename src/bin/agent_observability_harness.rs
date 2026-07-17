//! Synthetic PTY harness for the Agent observability vertical slice.
//!
//! This binary is available only through the explicit harness feature. It
//! drives the production App/runtime completion seam and production terminal
//! loop without adding fake adapters or hidden controls to `latte-lens`.

use std::{collections::BTreeMap, io, path::PathBuf, sync::Arc, thread, time::Duration};

use anyhow::Result;
use clap::Parser;
use latte_lens::{agent::*, app::App};
#[cfg(not(windows))]
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};

#[derive(Debug, Parser)]
#[command(name = "latte-lens-agent-harness")]
struct Cli {
    #[arg(default_value = ".")]
    path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let fixture = HarnessFixture::new();
    let (handle, endpoint) = agent_runtime_channel(16, 16);
    let worker = fixture.spawn(endpoint);
    let mut app = App::new(cli.path)?;
    app.attach_agent_runtime(
        AgentRuntime::from_channel(handle),
        WorkspaceSelector::new(
            BoundedVec::try_from_vec(vec![fixture.workspace.clone()])
                .expect("one synthetic workspace"),
        ),
    )
    .map_err(|error| anyhow::anyhow!("cannot attach synthetic Agent runtime: {error:?}"))?;

    let terminal_result = ratatui::run(|terminal| -> io::Result<()> {
        let _terminal_input = TerminalInputGuard::enable()?;
        app.run(terminal)
    });
    let _ = worker.join();
    terminal_result?;
    Ok(())
}

struct HarnessFixture {
    workspace: WorkspaceHint,
    metadata: MetadataSnapshot,
    live_states: Vec<ValidatedEnvelope>,
    gap: ValidatedEnvelope,
    expiry: EvidenceExpiryKey,
    dropped_session: SessionKey,
    downgraded_contract: InstanceContract,
}

impl HarnessFixture {
    fn new() -> Self {
        let observer = ObserverId::parse("synthetic/harness").expect("observer");
        let instance = ObserverInstanceId::from_digest(digest(10));
        let epoch = StreamEpoch::from_digest(digest(11));
        let subject = SubjectNamespace::parse("synthetic/harness").expect("subject");
        let workspace = WorkspaceHint::from_digest(digest(3));
        let session = SessionRef::new(
            SessionKey::new(
                subject.clone(),
                InstallId::from_digest(digest(4)),
                AuthorityId::from_digest(digest(5)),
                digest(6),
            ),
            workspace.clone(),
        );
        let second_session = SessionRef::new(
            SessionKey::new(
                subject.clone(),
                InstallId::from_digest(digest(7)),
                AuthorityId::from_digest(digest(8)),
                digest(9),
            ),
            workspace.clone(),
        );
        let claim = CapabilityClaim {
            support: CapabilitySupport::Confirmed,
            max_authority: EvidenceAuthority::Authoritative,
            provenance: EvidenceProvenance::InstrumentedHook,
            reason: BoundedText::try_new("synthetic PTY harness").expect("reason"),
            lease_backed: false,
        };
        let subjects = BoundedSet::try_from_iter([subject]).expect("subjects");
        let acquisition =
            BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).expect("acquisition");
        let capabilities = BTreeMap::from([(EvidenceDomain::Activity, claim)]);
        let stream_semantics = StreamSemantics {
            supported: true,
            sequenced: true,
            reports_reset: false,
            reports_gap: true,
        };
        let contract = InstanceContract {
            observer: observer.clone(),
            instance: instance.clone(),
            revision: ContractRevision::new(1),
            observer_version: None,
            subjects: subjects.clone(),
            acquisition: acquisition.clone(),
            capabilities: capabilities.clone(),
            snapshot_semantics: SnapshotSemantics::unsupported(),
            stream_semantics,
            requires_instrumentation: true,
            stability: InterfaceStability::Stable,
        };
        let template = InstanceContractTemplate {
            observer: observer.clone(),
            subjects,
            acquisition,
            capabilities,
            snapshot_semantics: SnapshotSemantics::unsupported(),
            stream_semantics,
            requires_instrumentation: true,
            stability: InterfaceStability::Stable,
        };
        let mut registry = AdapterRegistry::new();
        registry
            .register(Arc::new(HarnessAdapter {
                descriptor: ObserverDescriptor::new(observer.clone(), "Synthetic PTY harness", "1")
                    .expect("descriptor"),
                template,
            }))
            .expect("unique observer");
        let stream = StreamRef {
            observer: observer.clone(),
            instance: instance.clone(),
            epoch,
        };
        let live_states = [
            ReportedActivityState::Working,
            ReportedActivityState::WaitingPermission,
            ReportedActivityState::Idle,
            ReportedActivityState::Working,
            ReportedActivityState::Idle,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, state)| {
            let observation = AgentObservation {
                observed_at: Timestamp::from_unix_millis(3 + index as u64),
                valid_until: Some(Timestamp::from_unix_millis(10_000 + index as u64)),
                presence: None,
                session: Some(session.clone()),
                agent: None,
                turn: None,
                workspace: Some(workspace.clone()),
                kind: ObservationKind::Activity(ActivityOp::Set(state)),
                evidence: EvidenceClaim {
                    support: CapabilitySupport::Confirmed,
                    authority: EvidenceAuthority::Authoritative,
                    provenance: EvidenceProvenance::InstrumentedHook,
                },
            };
            registry
                .validate_envelope(
                    ObservationEnvelope::Event(EventEnvelope {
                        stream: stream.clone(),
                        event_id: EventId::from_digest(digest(12 + index as u8)),
                        sequence: Some(StreamSequence::new(index as u64 + 1)),
                        op: StreamOp::Upsert(
                            BoundedVec::try_from_vec(vec![observation]).expect("one observation"),
                        ),
                    }),
                    &contract,
                )
                .expect("validated activity fixture")
        })
        .collect::<Vec<_>>();
        let gap = registry
            .validate_envelope(
                ObservationEnvelope::Event(EventEnvelope {
                    stream,
                    event_id: EventId::from_digest(digest(20)),
                    sequence: None,
                    op: StreamOp::Gap {
                        expected: Some(StreamSequence::new(4)),
                        received: Some(StreamSequence::new(5)),
                    },
                }),
                &contract,
            )
            .expect("validated gap fixture");
        let expiry = EvidenceExpiryKey {
            generation: 1,
            session: session.key().clone(),
            observer: observer.clone(),
            instance,
            domain: EvidenceDomain::Activity,
        };
        let mut downgraded_contract = contract.clone();
        downgraded_contract
            .capabilities
            .get_mut(&EvidenceDomain::Activity)
            .expect("activity capability")
            .support = CapabilitySupport::Partial;
        let dropped_session = session.key().clone();
        let metadata = MetadataSnapshot {
            workspaces: BoundedVec::new(),
            sessions: BoundedVec::try_from_vec(vec![
                SessionMetadata {
                    session,
                    observers: BoundedVec::try_from_vec(vec![observer]).expect("observer"),
                    observers_truncated: false,
                    discovery: SessionDiscovery::DiscoveredMidSession,
                    first_observed_at: Timestamp::from_unix_millis(1),
                    last_observed_at: Timestamp::from_unix_millis(2),
                    lifecycle_hint: SessionLifecycleHint::Open,
                    last_activity_hint: ActivityStateHint::Working,
                    last_event_kind: ObservationKindTag::Activity,
                    known_agents: BoundedVec::new(),
                    agents_truncated: false,
                    start_observed: false,
                    terminal: None,
                    generation: 1,
                    partial: true,
                    revived: false,
                },
                SessionMetadata {
                    session: second_session,
                    observers: BoundedVec::new(),
                    observers_truncated: false,
                    discovery: SessionDiscovery::StartConfirmed,
                    first_observed_at: Timestamp::from_unix_millis(1),
                    last_observed_at: Timestamp::from_unix_millis(2),
                    lifecycle_hint: SessionLifecycleHint::Open,
                    last_activity_hint: ActivityStateHint::WaitingPermission,
                    last_event_kind: ObservationKindTag::Session,
                    known_agents: BoundedVec::new(),
                    agents_truncated: true,
                    start_observed: true,
                    terminal: None,
                    generation: 1,
                    partial: true,
                    revived: false,
                },
            ])
            .expect("two sessions"),
            truncated: false,
            corrupt_records_ignored: 0,
        };
        Self {
            workspace,
            metadata,
            live_states,
            gap,
            expiry,
            dropped_session,
            downgraded_contract,
        }
    }

    fn spawn(&self, endpoint: AgentRuntimeEndpoint) -> thread::JoinHandle<()> {
        let metadata = self.metadata.clone();
        let live_states = self.live_states.clone();
        let gap = self.gap.clone();
        let expiry = self.expiry.clone();
        let dropped_session = self.dropped_session.clone();
        let downgraded_contract = self.downgraded_contract.clone();
        let empty_metadata = MetadataSnapshot {
            workspaces: BoundedVec::new(),
            sessions: BoundedVec::new(),
            truncated: false,
            corrupt_records_ignored: 0,
        };
        thread::Builder::new()
            .name("latte-lens-agent-harness".to_owned())
            .spawn(move || {
                let mut generation = 0_u64;
                let mut metadata_refreshes = 0_u8;
                let mut saturated = false;
                let mut saturation_ticks = 0_u16;
                let mut overflow_sent = false;
                while !endpoint.shutdown_requested() {
                    if saturated {
                        saturation_ticks = saturation_ticks.saturating_add(1);
                        if saturation_ticks >= 250 && !overflow_sent {
                            overflow_sent = true;
                            let _ = endpoint.complete(AgentRuntimeCompletion::EnvelopeReceived {
                                generation,
                                workspace_hints: BoundedVec::new(),
                                envelope: Box::new(live_states[4].clone()),
                            });
                        }
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    match endpoint.try_request() {
                        Some(AgentRuntimeRequest::SelectWorkspace {
                            generation: selected,
                            ..
                        }) => {
                            generation = selected;
                            let _ = endpoint.complete(AgentRuntimeCompletion::MetadataLoaded {
                                generation,
                                snapshot: empty_metadata.clone(),
                            });
                        }
                        Some(AgentRuntimeRequest::RefreshMetadata {
                            generation: requested,
                        }) if requested == generation => {
                            metadata_refreshes = metadata_refreshes.saturating_add(1);
                            let completion = match metadata_refreshes {
                                // Entering Agents performs the first metadata
                                // refresh. The second loads the persisted
                                // sessions; each later `r` advances one
                                // explicit reducer/UI state in the PTY journey.
                                2 => {
                                    let _ =
                                        endpoint.complete(AgentRuntimeCompletion::LiveDropped {
                                            generation: generation.saturating_add(1),
                                            sessions: BoundedVec::new(),
                                            unattributed: 1,
                                        });
                                    Some(AgentRuntimeCompletion::MetadataLoaded {
                                        generation,
                                        snapshot: metadata.clone(),
                                    })
                                }
                                3..=5 => Some(AgentRuntimeCompletion::EnvelopeReceived {
                                    generation,
                                    workspace_hints: BoundedVec::new(),
                                    envelope: Box::new(
                                        live_states[usize::from(metadata_refreshes - 3)].clone(),
                                    ),
                                }),
                                6 => Some(AgentRuntimeCompletion::EvidenceExpired {
                                    generation,
                                    keys: BoundedVec::try_from_vec(vec![expiry.clone()])
                                        .expect("one expiry"),
                                }),
                                7 => Some(AgentRuntimeCompletion::LiveDropped {
                                    generation,
                                    sessions: BoundedVec::try_from_vec(vec![SessionDropCount {
                                        session: dropped_session.clone(),
                                        count: 2,
                                    }])
                                    .expect("one dropped session"),
                                    unattributed: 1,
                                }),
                                8 => Some(AgentRuntimeCompletion::EnvelopeReceived {
                                    generation,
                                    workspace_hints: BoundedVec::new(),
                                    envelope: Box::new(gap.clone()),
                                }),
                                9 => Some(AgentRuntimeCompletion::ContractUpdated {
                                    generation,
                                    contract: Box::new(downgraded_contract.clone()),
                                }),
                                10 => {
                                    saturated = true;
                                    Some(AgentRuntimeCompletion::EnvelopeReceived {
                                        generation,
                                        workspace_hints: BoundedVec::new(),
                                        envelope: Box::new(live_states[3].clone()),
                                    })
                                }
                                _ => None,
                            };
                            if let Some(completion) = completion {
                                let _ = endpoint.complete(completion);
                            }
                        }
                        Some(
                            AgentRuntimeRequest::RefreshProviders { .. }
                            | AgentRuntimeRequest::RefreshMetadata { .. }
                            | AgentRuntimeRequest::PersistMetadata { .. }
                            | AgentRuntimeRequest::ScheduleExpiry { .. },
                        )
                        | None => thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
            .expect("start harness worker")
    }
}

struct HarnessAdapter {
    descriptor: ObserverDescriptor,
    template: InstanceContractTemplate,
}

impl CodeAgentAdapter for HarnessAdapter {
    fn descriptor(&self) -> ObserverDescriptor {
        self.descriptor.clone()
    }

    fn contract_template(&self, _observer_version: Option<&str>) -> InstanceContractTemplate {
        self.template.clone()
    }

    fn decode(
        &self,
        input: AdapterInput<'_>,
        _identity: &dyn IdentityKeyer,
    ) -> std::result::Result<DecodeOutcome, AdapterError> {
        input.validate_bounds()?;
        Ok(DecodeOutcome::Ignore(IgnoreReason::NoObservableFact))
    }
}

const fn digest(byte: u8) -> StableDigest {
    StableDigest::from_bytes([byte; 32])
}

struct TerminalInputGuard {
    #[cfg(not(windows))]
    keyboard_enhanced: bool,
}

impl TerminalInputGuard {
    fn enable() -> io::Result<Self> {
        let mut stdout = io::stdout();
        execute!(stdout, EnableMouseCapture)?;
        #[cfg(not(windows))]
        let keyboard_enhanced = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
        Ok(Self {
            #[cfg(not(windows))]
            keyboard_enhanced,
        })
    }
}

impl Drop for TerminalInputGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        #[cfg(not(windows))]
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(stdout, DisableMouseCapture);
    }
}
