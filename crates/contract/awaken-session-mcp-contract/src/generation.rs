// @formal evidence session_realization.generation_authority_requires_every_immutable_axis
//! The complete optimistic-concurrency witness for one MCP attachment
//! generation, and the exact ownership fence carrying it.
//!
//! Separated from the aggregate it fences: the aggregate owns state
//! transitions, this module owns what a conditional mutation must prove before
//! one is applied.

use super::{
    CredentialAccess, McpAttachmentDraft, McpAttachmentId, McpAttachmentOrigin, McpTarget,
};
use awaken_lease_contract::fence::{Epoch, FenceSuccession, Lease, Tenure, TenureScope, ZeroEpoch};
use awaken_time::Expiry;
use serde::{Deserialize, Serialize};

/// The digest of one attachment generation's exact desired content.
///
/// Only this module can mint one, and only from real content, so a generation
/// witness can never be assembled around an invented digest. The logical name
/// is deliberately not a coordinate: it is the attachment's identity, already
/// carried by [`McpAttachmentId`], not part of what was observed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpAttachmentContentDigest(String);

impl McpAttachmentContentDigest {
    /// A one-byte equality representative for bounded proofs of predicates
    /// that only compare digests. Proof builds only: every other build can
    /// still mint a digest solely from real content. Digesting real content
    /// inside such a proof runs URL parsing and hashing under the harness's
    /// unwind bound and proves nothing about the comparison.
    #[cfg(kani)]
    #[must_use]
    pub fn proof_representative(same: bool) -> Self {
        Self(if same { "a" } else { "b" }.into())
    }

    /// Digest the complete desired content of one attachment generation.
    #[must_use]
    pub fn of_content(
        target: &McpTarget,
        prompts_as_skills: bool,
        credential: Option<&CredentialAccess>,
        origin: McpAttachmentOrigin,
    ) -> Self {
        Self(awaken_agent_contract::stable_fingerprint(&(
            "mcp-attachment-content-v1",
            target,
            prompts_as_skills,
            credential,
            origin,
        )))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl McpAttachmentDraft {
    /// The digest of this draft's desired content.
    #[must_use]
    pub fn content_digest(&self) -> McpAttachmentContentDigest {
        McpAttachmentContentDigest::of_content(
            &self.target,
            self.prompts_as_skills,
            self.credential.as_ref(),
            self.origin,
        )
    }
}

/// Complete optimistic-concurrency identity for one durable MCP attachment
/// generation.
///
/// A counter alone is not a generation. The version says how far *some* head
/// advanced but not whose, so a witness read from one attachment can gate a
/// write to another; and it says nothing about what was observed, so a
/// generation whose desired content was rewritten under the same number would
/// still pass. Every conditional mutation of the aggregate therefore carries
/// the immutable attachment identity, its monotonic version, and the digest of
/// the content observed together — see
/// [`awaken_resource_contract::CompleteGeneration`].
///
/// The fields are private: a witness is minted by the aggregate when it
/// allocates a generation and is thereafter only carried, never rebuilt from
/// parts that were read at different times.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpGeneration {
    attachment_id: McpAttachmentId,
    version: u64,
    content_digest: McpAttachmentContentDigest,
}

impl McpGeneration {
    /// Mint the witness for one allocated generation of `content`.
    #[must_use]
    pub fn new(
        attachment_id: McpAttachmentId,
        version: u64,
        content_digest: McpAttachmentContentDigest,
    ) -> Self {
        Self {
            attachment_id,
            version,
            content_digest,
        }
    }

    /// Mint the witness for the generation `version` of `draft`'s content.
    #[must_use]
    pub fn of_draft(
        attachment_id: McpAttachmentId,
        version: u64,
        draft: &McpAttachmentDraft,
    ) -> Self {
        Self::new(attachment_id, version, draft.content_digest())
    }

    /// Mint the witness for a generation whose desired content is exactly one
    /// target: Session-authored, no credential, no prompt import. This is a
    /// real digest of that content, not a placeholder — a caller holding any
    /// other content axis must use [`McpGeneration::of_draft`] instead, or the
    /// witness it mints will (correctly) never match the durable head.
    #[must_use]
    pub fn of_bare_target(
        attachment_id: McpAttachmentId,
        version: u64,
        target: &McpTarget,
    ) -> Self {
        Self::new(
            attachment_id,
            version,
            McpAttachmentContentDigest::of_content(
                target,
                false,
                None,
                McpAttachmentOrigin::Session,
            ),
        )
    }

    /// The same attachment and content observed at another version. Only a
    /// caller that already holds the head's content may shift its version —
    /// this never invents a digest.
    #[must_use]
    pub fn at_version(&self, version: u64) -> Self {
        Self::new(
            self.attachment_id.clone(),
            version,
            self.content_digest.clone(),
        )
    }

    #[must_use]
    pub const fn attachment_id(&self) -> &McpAttachmentId {
        &self.attachment_id
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn digest(&self) -> &McpAttachmentContentDigest {
        &self.content_digest
    }
}

impl awaken_resource_contract::sealed::Sealed for McpGeneration {}

impl awaken_resource_contract::CompleteGeneration for McpGeneration {
    fn subject_id(&self) -> &str {
        &self.attachment_id.0
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn content_digest(&self) -> &str {
        self.content_digest.as_str()
    }
}

/// The lease family of one Runtime MCP projection.
///
/// The subject is the Session together with the complete generation witness:
/// a projection realized for one attachment generation never fences another
/// generation, another attachment or another Session, whatever its epoch. The
/// holder is the runtime incarnation that realizes it. Consumer:
/// [`McpGenerationRef`].
#[derive(Debug)]
pub struct McpRealization;

impl TenureScope for McpRealization {
    type Subject = (crate::SessionId, McpGeneration);
    type Holder = String;
}

/// Exact identity and continuing ownership fence for one Runtime projection.
/// Runtime adapters must compare the complete tenure; a logical name is never a
/// sufficient route or cleanup identity.
///
/// The value *is* a [`Lease`] over [`McpRealization`]: the Session and the
/// generation witness are its subject, so they cannot disagree with the lease
/// coordinates, and every comparison that ignores the renewable expiry goes
/// through [`McpGenerationRef::tenure`] or [`McpGenerationRef::succession`].
/// The persisted wire is unchanged (see [`McpGenerationRefWire`]), and it is
/// the only door: [`McpGenerationRef::from_persisted`] is the admission both
/// the decoder and every constructor use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "McpGenerationRefWire", into = "McpGenerationRefWire")]
pub struct McpGenerationRef {
    lease: Lease<McpRealization>,
}

/// The persisted coordinates of [`McpGenerationRef`], byte-for-byte the shape
/// the value had while its coordinates were public fields.
///
/// Converting back into the fence validates exactly as decoding does (a zero
/// epoch, which no realization lease grants, is refused). Consumers: the serde
/// codec of [`McpGenerationRef`], and adapters and tests that must name one
/// persisted coordinate (a tampered epoch, a foreign Session) rather than a
/// successor the kernel mints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpGenerationRefWire {
    pub session_id: crate::SessionId,
    pub generation: McpGeneration,
    pub runtime_incarnation: String,
    pub lease_epoch: u64,
    pub lease_expires_at_unix_ms: u64,
}

impl TryFrom<McpGenerationRefWire> for McpGenerationRef {
    type Error = ZeroEpoch;

    fn try_from(wire: McpGenerationRefWire) -> Result<Self, Self::Error> {
        Self::from_persisted(
            wire.session_id,
            wire.generation,
            wire.runtime_incarnation,
            wire.lease_epoch,
            wire.lease_expires_at_unix_ms,
        )
        .ok_or(ZeroEpoch)
    }
}

impl From<McpGenerationRef> for McpGenerationRefWire {
    fn from(reference: McpGenerationRef) -> Self {
        let lease_epoch = reference.lease_epoch().get();
        let lease_expires_at_unix_ms = reference.lease_expires_at_unix_ms();
        let tenure = reference.lease.tenure();
        let (session_id, generation) = tenure.subject().clone();
        Self {
            session_id,
            generation,
            runtime_incarnation: tenure.holder().clone(),
            lease_epoch,
            lease_expires_at_unix_ms,
        }
    }
}

impl McpGenerationRef {
    /// The projection a durable realization claim authorizes for `generation`
    /// in `session_id`. `None` when the claim's persisted epoch is zero, which
    /// no realization lease grants. This is the only claim-to-fence mapping.
    #[must_use]
    pub fn from_claim(
        session_id: &crate::SessionId,
        generation: &McpGeneration,
        claim: &crate::McpRealizationClaim,
    ) -> Option<Self> {
        Self::from_persisted(
            session_id.clone(),
            generation.clone(),
            claim.runtime_incarnation.clone(),
            claim.lease_epoch,
            claim.lease_expires_at_unix_ms,
        )
    }

    /// The one admission of this fence, shared by the wire decoder and every
    /// constructor: the projection of `generation` in `session_id`, held by
    /// `runtime_incarnation` under realization lease `lease_epoch` until
    /// `lease_expires_at_unix_ms`. `None` for a zero epoch, which no
    /// realization lease grants.
    #[must_use]
    pub fn from_persisted(
        session_id: crate::SessionId,
        generation: McpGeneration,
        runtime_incarnation: String,
        lease_epoch: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Option<Self> {
        Some(Self {
            lease: Lease::new(
                Tenure::granted(
                    (session_id, generation),
                    runtime_incarnation,
                    Epoch::from_persisted(lease_epoch)?,
                ),
                Expiry::from_epoch_ms(lease_expires_at_unix_ms),
            ),
        })
    }

    /// The persisted coordinates of this fence.
    #[must_use]
    pub fn to_wire(&self) -> McpGenerationRefWire {
        self.clone().into()
    }

    /// The same tenure asserted until another persisted expiry, later (a
    /// renewal) or earlier (a regression the fence law refuses as a
    /// successor). The tenure is unchanged, so no admission is repeated.
    #[must_use]
    pub fn expiring_at(&self, lease_expires_at_unix_ms: u64) -> Self {
        Self {
            lease: Lease::new(
                self.lease.tenure().clone(),
                Expiry::from_epoch_ms(lease_expires_at_unix_ms),
            ),
        }
    }

    /// This fence with persisted coordinates edited, re-admitted through
    /// [`McpGenerationRef::from_persisted`]; `None` when the edit leaves a zero
    /// epoch. Consumers: decision tables that name the one coordinate they
    /// tamper with, rather than a successor the kernel would mint.
    #[must_use]
    pub fn with_persisted(&self, edit: impl FnOnce(&mut McpGenerationRefWire)) -> Option<Self> {
        let mut wire = self.to_wire();
        edit(&mut wire);
        Self::try_from(wire).ok()
    }

    #[must_use]
    pub const fn lease(&self) -> &Lease<McpRealization> {
        &self.lease
    }

    /// The complete identity of this projection, ignoring the renewable
    /// expiry: Session, generation, runtime incarnation and epoch.
    #[must_use]
    pub const fn tenure(&self) -> &Tenure<McpRealization> {
        self.lease.tenure()
    }

    /// How `successor` takes over this projection under the one fence law.
    #[must_use]
    pub fn succession(&self, successor: &Self) -> Option<FenceSuccession> {
        self.lease.succession(&successor.lease)
    }

    /// The same projection granted to `runtime_incarnation` at the next
    /// epoch, keeping the expiry: every fence minted under this one is
    /// superseded. `None` once the epoch is exhausted.
    #[must_use]
    pub fn reassigned(&self, runtime_incarnation: impl Into<String>) -> Option<Self> {
        Some(Self {
            lease: Lease::new(
                self.lease.tenure().reassigned(runtime_incarnation.into())?,
                self.lease.expires(),
            ),
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &crate::SessionId {
        &self.lease.tenure().subject().0
    }

    #[must_use]
    pub fn generation(&self) -> &McpGeneration {
        &self.lease.tenure().subject().1
    }

    #[must_use]
    pub fn attachment_id(&self) -> &McpAttachmentId {
        self.generation().attachment_id()
    }

    #[must_use]
    pub fn runtime_incarnation(&self) -> &str {
        self.lease.tenure().holder()
    }

    #[must_use]
    pub const fn lease_epoch(&self) -> Epoch {
        self.lease.tenure().epoch()
    }

    #[must_use]
    pub const fn lease_expires(&self) -> Expiry {
        self.lease.expires()
    }

    /// The persisted expiry millisecond, for the wire and for predicates that
    /// still judge realization leases in persisted coordinates.
    #[must_use]
    pub fn lease_expires_at_unix_ms(&self) -> u64 {
        self.lease.expires().to_epoch_ms()
    }
}

/// Test-only complete witness for one generation of a bare http attachment.
#[cfg(test)]
pub(crate) fn test_generation(attachment: &str, version: u64, url: &str) -> McpGeneration {
    McpGeneration::of_bare_target(
        McpAttachmentId(attachment.into()),
        version,
        &McpTarget::parse_http(url).unwrap(),
    )
}

#[cfg(test)]
pub(crate) fn test_generation_at_version(
    generation: &McpGeneration,
    version: u64,
) -> McpGeneration {
    generation.at_version(version)
}

#[cfg(test)]
mod tests {
    use super::super::tests::draft;
    use super::super::{McpAttachmentError, McpAttachmentState, SessionMcpAttachmentSet};
    use super::*;
    use crate::McpRealizationClaim;
    use awaken_resource_contract::CompleteGeneration;

    /// The reason `McpGeneration` is a triple and not a counter.
    ///
    /// Cause graph: C1 the observed witness is the durable head on all three
    /// coordinates; C2 the version advanced under it (an A -> B -> A content
    /// cycle leaves the digest equal but the head ahead); C3 the content
    /// changed under an unchanged version; C4 the witness names another
    /// attachment. Effects: E1 the conditional mutation applies; E2 it is
    /// refused as stale with the aggregate unchanged.
    ///
    /// | Rule | identity | version | digest | Effect |
    /// |---|---|---|---|---|
    /// | W1 | exact | exact | exact | E1 |
    /// | W2 | exact | behind | exact (A->B->A) | E2 |
    /// | W3 | exact | exact | other | E2 |
    /// | W4 | other | exact | exact | E2 |
    #[test]
    fn a_conditional_mutation_needs_every_coordinate_of_the_observed_head() {
        let set = || {
            SessionMcpAttachmentSet::from_initial(
                vec![draft("a", "https://a.test/mcp", None)],
                None,
            )
            .expect("W fixture")
        };
        let claim = || McpRealizationClaim {
            realization_id: "realization-1".into(),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 4,
            lease_expires_at_unix_ms: 100,
            stage_idempotency_key: "stage-1".into(),
        };
        let head = set().attachments[0].generation.clone();
        assert_eq!(head.subject_id(), head.attachment_id().0);
        assert_eq!(head.version(), 1);
        assert_eq!(
            <McpGeneration as CompleteGeneration>::content_digest(&head),
            head.digest().as_str()
        );

        // W1: the exact observed head applies.
        let mut applied = set();
        applied.claim_realization(&head, claim()).expect("W1/E1");
        assert_eq!(applied.attachments[0].state, McpAttachmentState::Realizing);

        // W2: a real A -> B -> A cycle. Generation 1 holds content A and is
        // retired; generation 2 holds content B; generation 3 re-mints content
        // A. The digest a reader captured at generation 1 is byte-identical to
        // the live head's, so a digest-keyed precondition would resolve the
        // stale witness onto generation 3 and apply the write. Only the
        // version coordinate separates them.
        let mut cycled = set();
        let one = cycled.attachments[0].generation.clone();
        cycled.claim_realization(&one, claim()).expect("W2 A claim");
        cycled.activate(&one, "realization-1").expect("W2 A active");
        cycled
            .request_full_replacement(vec![draft("a", "https://b.test/mcp", None)], None)
            .expect("W2 A -> B");
        let two = cycled.attachments[1].generation.clone();
        cycled.claim_realization(&two, claim()).expect("W2 B claim");
        cycled.activate(&two, "realization-1").expect("W2 B active");
        cycled.finish_drain(&one).expect("W2 retire A");
        cycled
            .request_full_replacement(vec![draft("a", "https://a.test/mcp", None)], None)
            .expect("W2 B -> A");
        let live = cycled
            .attachments
            .last()
            .expect("W2 live head")
            .generation
            .clone();
        assert_eq!(live.version(), 3, "W2 the head advanced");
        assert_eq!(
            <McpGeneration as CompleteGeneration>::content_digest(&live),
            <McpGeneration as CompleteGeneration>::content_digest(&one),
            "W2 the content cycled back to its original digest"
        );
        assert_ne!(live, one, "W2 the complete witnesses still differ");
        let before = cycled.clone();
        assert!(
            cycled.claim_realization(&one, claim()).is_err(),
            "W2/E2 the stale witness is refused"
        );
        assert_eq!(cycled, before, "W2/E2 no mutation");
        // Re-reading the head produces a witness that does apply.
        cycled
            .claim_realization(&live, claim())
            .expect("W2 re-read");
        assert_eq!(
            cycled.attachments.last().expect("W2 live head").state,
            McpAttachmentState::Realizing,
            "W2/E1"
        );

        // W3: same identity and version, different observed content.
        let mut drifted = set();
        let other_content = McpGeneration::new(
            head.attachment_id().clone(),
            head.version(),
            draft("a", "https://b.test/mcp", None).content_digest(),
        );
        let before = drifted.clone();
        assert_eq!(
            drifted.claim_realization(&other_content, claim()),
            Err(McpAttachmentError::StaleGeneration),
            "W3/E2"
        );
        assert_eq!(drifted, before, "W3/E2 no mutation");

        // W4: a witness read from another attachment cannot gate this one.
        let mut foreign_target = set();
        let foreign = McpGeneration::new(
            McpAttachmentId("foreign".into()),
            head.version(),
            head.digest().clone(),
        );
        let before = foreign_target.clone();
        assert_eq!(
            foreign_target.claim_realization(&foreign, claim()),
            Err(McpAttachmentError::UnknownAttachment("foreign".into())),
            "W4/E2"
        );
        assert_eq!(foreign_target, before, "W4/E2 no mutation");
    }
}
