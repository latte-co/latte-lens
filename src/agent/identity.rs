use std::{error::Error, fmt};

use super::{BoundExceeded, BoundedText};

const MAX_NAMESPACE_BYTES: usize = 64;

/// Failure to construct a stable, bounded namespace identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceError {
    Empty,
    TooLong,
    InvalidCharacter,
    InvalidSeparator,
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid observability namespace: {self:?}")
    }
}

impl Error for NamespaceError {}

fn validate_namespace(value: &str) -> Result<(), NamespaceError> {
    if value.is_empty() {
        return Err(NamespaceError::Empty);
    }
    if value.len() > MAX_NAMESPACE_BYTES {
        return Err(NamespaceError::TooLong);
    }
    if value.starts_with('/') || value.ends_with('/') || value.contains("//") {
        return Err(NamespaceError::InvalidSeparator);
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'_' | b'-' | b'/')
    }) {
        return Err(NamespaceError::InvalidCharacter);
    }
    Ok(())
}

macro_rules! namespace_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, NamespaceError> {
                let value = value.into();
                validate_namespace(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

namespace_id!(SubjectNamespace);
namespace_id!(ObserverId);

/// A keyed digest safe to retain in the core model.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StableDigest([u8; 32]);

impl StableDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }

    pub fn parse_hex(value: &str) -> Result<Self, StableDigestParseError> {
        if value.len() != 64 {
            return Err(StableDigestParseError);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(pair[0]).ok_or(StableDigestParseError)? << 4)
                | hex_nibble(pair[1]).ok_or(StableDigestParseError)?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StableDigestParseError;

impl fmt::Display for StableDigestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid stable digest")
    }
}

impl Error for StableDigestParseError {}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Debug for StableDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StableDigest(<redacted>)")
    }
}

macro_rules! digest_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(StableDigest);

        impl $name {
            pub const fn from_digest(digest: StableDigest) -> Self {
                Self(digest)
            }

            pub const fn digest(&self) -> &StableDigest {
                &self.0
            }
        }
    };
}

digest_id!(InstallId);
digest_id!(AuthorityId);
digest_id!(ObserverInstanceId);
digest_id!(StreamEpoch);
digest_id!(EventId);
digest_id!(SnapshotId);
digest_id!(WorkspaceHint);
digest_id!(ArtifactKey);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const fn from_unix_millis(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_unix_millis(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamSequence(u64);

impl StreamSequence {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContractRevision(u64);

impl ContractRevision {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionKey {
    subject: SubjectNamespace,
    install_id: InstallId,
    authority_id: AuthorityId,
    stable_id: StableDigest,
}

impl SessionKey {
    pub const fn new(
        subject: SubjectNamespace,
        install_id: InstallId,
        authority_id: AuthorityId,
        stable_id: StableDigest,
    ) -> Self {
        Self {
            subject,
            install_id,
            authority_id,
            stable_id,
        }
    }

    pub const fn subject(&self) -> &SubjectNamespace {
        &self.subject
    }

    pub const fn install_id(&self) -> &InstallId {
        &self.install_id
    }

    pub const fn authority_id(&self) -> &AuthorityId {
        &self.authority_id
    }

    pub const fn stable_id(&self) -> &StableDigest {
        &self.stable_id
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionRef {
    key: SessionKey,
    workspace: WorkspaceHint,
}

impl SessionRef {
    pub const fn new(key: SessionKey, workspace: WorkspaceHint) -> Self {
        Self { key, workspace }
    }

    pub const fn key(&self) -> &SessionKey {
        &self.key
    }

    pub const fn workspace(&self) -> &WorkspaceHint {
        &self.workspace
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AgentKey {
    session: SessionKey,
    stable_id: StableDigest,
}

impl AgentKey {
    pub const fn new(session: SessionKey, stable_id: StableDigest) -> Self {
        Self { session, stable_id }
    }

    pub const fn session(&self) -> &SessionKey {
        &self.session
    }

    pub const fn stable_id(&self) -> &StableDigest {
        &self.stable_id
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AgentKind {
    Primary,
    Subagent,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AgentRef {
    key: AgentKey,
    parent: Option<AgentKey>,
    kind: Option<AgentKind>,
}

impl AgentRef {
    pub const fn new(key: AgentKey, parent: Option<AgentKey>, kind: Option<AgentKind>) -> Self {
        Self { key, parent, kind }
    }

    pub const fn key(&self) -> &AgentKey {
        &self.key
    }

    pub const fn parent(&self) -> Option<&AgentKey> {
        self.parent.as_ref()
    }

    pub const fn kind(&self) -> Option<AgentKind> {
        self.kind
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TurnKey {
    session: SessionKey,
    authority_id: AuthorityId,
    stable_id: StableDigest,
}

impl TurnKey {
    pub const fn new(
        session: SessionKey,
        authority_id: AuthorityId,
        stable_id: StableDigest,
    ) -> Self {
        Self {
            session,
            authority_id,
            stable_id,
        }
    }

    pub const fn session(&self) -> &SessionKey {
        &self.session
    }

    pub const fn authority_id(&self) -> &AuthorityId {
        &self.authority_id
    }

    pub const fn stable_id(&self) -> &StableDigest {
        &self.stable_id
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PresenceRef {
    stable_id: StableDigest,
    subject_hint: Option<SubjectNamespace>,
    workspace: Option<WorkspaceHint>,
}

impl PresenceRef {
    pub const fn new(
        stable_id: StableDigest,
        subject_hint: Option<SubjectNamespace>,
        workspace: Option<WorkspaceHint>,
    ) -> Self {
        Self {
            stable_id,
            subject_hint,
            workspace,
        }
    }

    pub const fn subject_hint(&self) -> Option<&SubjectNamespace> {
        self.subject_hint.as_ref()
    }

    pub const fn stable_id(&self) -> &StableDigest {
        &self.stable_id
    }

    pub const fn workspace(&self) -> Option<&WorkspaceHint> {
        self.workspace.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubjectDescriptor {
    pub namespace: SubjectNamespace,
    pub display_name: BoundedText<128>,
}

impl SubjectDescriptor {
    pub fn new(
        namespace: SubjectNamespace,
        display_name: impl Into<String>,
    ) -> Result<Self, BoundExceeded> {
        Ok(Self {
            namespace,
            display_name: BoundedText::try_new(display_name)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObserverDescriptor {
    pub id: ObserverId,
    pub display_name: BoundedText<128>,
    pub adapter_version: BoundedText<64>,
}

impl ObserverDescriptor {
    pub fn new(
        id: ObserverId,
        display_name: impl Into<String>,
        adapter_version: impl Into<String>,
    ) -> Result<Self, BoundExceeded> {
        Ok(Self {
            id,
            display_name: BoundedText::try_new(display_name)?,
            adapter_version: BoundedText::try_new(adapter_version)?,
        })
    }
}

/// Raw native identity visible only while an adapter calls [`IdentityKeyer`].
///
/// It deliberately implements neither `Debug` nor `Display`, so accidental
/// formatting cannot move a native identifier into logs or diagnostics.
///
/// ```compile_fail
/// use latte_lens::agent::SensitiveId;
/// let native = SensitiveId::new(b"native-session-canary");
/// let _ = format!("{native:?}");
/// ```
pub struct SensitiveId<'a>(&'a [u8]);

impl<'a> SensitiveId<'a> {
    pub const fn new(value: &'a [u8]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(&self) -> &'a [u8] {
        self.0
    }
}

/// Raw workspace locator visible only at the identity boundary.
///
/// ```compile_fail
/// use latte_lens::agent::SensitiveWorkspaceLocator;
/// let locator = SensitiveWorkspaceLocator::new(b"/raw/workspace/canary");
/// let _ = locator.to_string();
/// ```
pub struct SensitiveWorkspaceLocator<'a>(&'a [u8]);

impl<'a> SensitiveWorkspaceLocator<'a> {
    pub const fn new(value: &'a [u8]) -> Self {
        Self(value)
    }

    pub const fn as_bytes(&self) -> &'a [u8] {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityError {
    Empty,
    TooLarge,
    Unavailable,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "identity keying failed: {self:?}")
    }
}

impl Error for IdentityError {}

/// The only boundary allowed to turn raw native identities into core keys.
pub trait IdentityKeyer: Send + Sync {
    fn event_id(
        &self,
        observer: &ObserverId,
        instance: &ObserverInstanceId,
        epoch: &StreamEpoch,
        native_or_composite_id: SensitiveId<'_>,
    ) -> Result<EventId, IdentityError>;

    fn session_key(
        &self,
        subject: &SubjectNamespace,
        authority: &AuthorityId,
        native_id: SensitiveId<'_>,
    ) -> Result<SessionKey, IdentityError>;

    fn presence_ref(
        &self,
        observer: &ObserverId,
        instance: &ObserverInstanceId,
        native_presence_id: SensitiveId<'_>,
        subject_hint: Option<&SubjectNamespace>,
        workspace: Option<WorkspaceHint>,
    ) -> Result<PresenceRef, IdentityError>;

    fn agent_key(
        &self,
        session: &SessionKey,
        native_id: SensitiveId<'_>,
    ) -> Result<AgentKey, IdentityError>;

    fn turn_key(
        &self,
        session: &SessionKey,
        authority: &AuthorityId,
        native_id: SensitiveId<'_>,
    ) -> Result<TurnKey, IdentityError>;

    fn workspace_hint(
        &self,
        locator: SensitiveWorkspaceLocator<'_>,
    ) -> Result<WorkspaceHint, IdentityError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_are_bounded_and_vendor_neutral() {
        assert_eq!(
            SubjectNamespace::parse("test/agent")
                .expect("subject")
                .as_str(),
            "test/agent"
        );
        assert!(ObserverId::parse("Terminal/Runtime").is_err());
        assert!(ObserverId::parse("terminal//runtime").is_err());
        assert!(ObserverId::parse("x".repeat(65)).is_err());
    }

    #[test]
    fn digest_debug_output_never_exposes_bytes() {
        let digest = StableDigest::from_bytes([0xab; 32]);
        assert_eq!(format!("{digest:?}"), "StableDigest(<redacted>)");
        assert!(!format!("{digest:?}").contains("abababab"));
    }

    #[test]
    fn stable_digest_hex_round_trip_is_strict() {
        let digest = StableDigest::from_bytes([0xab; 32]);
        assert_eq!(StableDigest::parse_hex(&digest.to_hex()), Ok(digest));
        assert!(StableDigest::parse_hex(&"A".repeat(64)).is_err());
        assert!(StableDigest::parse_hex("00").is_err());
    }
}
