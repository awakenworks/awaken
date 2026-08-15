--------------------- MODULE CredentialRotationWorkflow ---------------------
EXTENDS Naturals

\* Publication, Worker observation, claim-time pinning, materialization,
\* rotation/revocation, execution and output publication across one credential.
CONSTANT MaxRevision
ASSUME MaxRevision \in Nat \ {0, 1}

VARIABLES published, observed, pinned, materialized, revoked,
          claimEpoch, executionState, outputRevision, revokedAtOutput

vars == <<published, observed, pinned, materialized, revoked,
          claimEpoch, executionState, outputRevision, revokedAtOutput>>

NoRevision == 0

Init ==
    /\ published = 1
    /\ observed = NoRevision
    /\ pinned = NoRevision
    /\ materialized = NoRevision
    /\ revoked = {}
    /\ claimEpoch = 0
    /\ executionState = "Idle"
    /\ outputRevision = NoRevision
    /\ revokedAtOutput = {}

Observe ==
    /\ observed # published
    /\ observed' = published
    /\ UNCHANGED <<published, pinned, materialized, revoked,
                   claimEpoch, executionState, outputRevision, revokedAtOutput>>

Claim ==
    /\ executionState = "Idle"
    /\ observed = published
    /\ published \notin revoked
    /\ pinned' = published
    /\ claimEpoch' = claimEpoch + 1
    /\ executionState' = "Claimed"
    /\ UNCHANGED <<published, observed, materialized, revoked, outputRevision,
                   revokedAtOutput>>

Materialize ==
    /\ executionState = "Claimed"
    /\ pinned = observed
    /\ pinned \notin revoked
    /\ materialized' = pinned
    /\ executionState' = "Ready"
    /\ UNCHANGED <<published, observed, pinned, revoked, claimEpoch, outputRevision,
                   revokedAtOutput>>

Execute ==
    /\ executionState = "Ready"
    /\ materialized = pinned
    /\ pinned \notin revoked
    /\ executionState' = "Executing"
    /\ UNCHANGED <<published, observed, pinned, materialized, revoked,
                   claimEpoch, outputRevision, revokedAtOutput>>

PublishOutput ==
    /\ executionState = "Executing"
    /\ pinned \notin revoked
    /\ outputRevision' = pinned
    /\ revokedAtOutput' = revoked
    /\ executionState' = "Completed"
    /\ UNCHANGED <<published, observed, pinned, materialized, revoked, claimEpoch>>

Rotate ==
    /\ published < MaxRevision
    /\ published' = published + 1
    /\ UNCHANGED <<observed, pinned, materialized, revoked,
                   claimEpoch, executionState, outputRevision, revokedAtOutput>>

Revoke(r) ==
    /\ r \in 1..MaxRevision
    /\ r \notin revoked
    /\ revoked' = revoked \cup {r}
    /\ UNCHANGED <<published, observed, pinned, materialized,
                   claimEpoch, executionState, outputRevision, revokedAtOutput>>

AbortStale ==
    /\ executionState \in {"Claimed", "Ready", "Executing"}
    /\ (pinned \in revoked \/ pinned # observed)
    /\ executionState' = "Rejected"
    /\ materialized' = NoRevision
    /\ UNCHANGED <<published, observed, pinned, revoked, claimEpoch, outputRevision,
                   revokedAtOutput>>

RevokeAny == \E r \in 1..MaxRevision: Revoke(r)

Next ==
    \/ Observe
    \/ Claim
    \/ Materialize
    \/ Execute
    \/ PublishOutput
    \/ Rotate
    \/ RevokeAny
    \/ AbortStale

Spec == Init /\ [][Next]_vars

HappyNext == Observe \/ Claim \/ Materialize \/ Execute \/ PublishOutput
HappySpec ==
    /\ Init
    /\ [][HappyNext]_vars
    /\ WF_vars(Observe)
    /\ WF_vars(Claim)
    /\ WF_vars(Materialize)
    /\ WF_vars(Execute)
    /\ WF_vars(PublishOutput)

TypeOK ==
    /\ published \in 1..MaxRevision
    /\ observed \in 0..MaxRevision
    /\ pinned \in 0..MaxRevision
    /\ materialized \in 0..MaxRevision
    /\ revoked \subseteq 1..MaxRevision
    /\ claimEpoch \in Nat
    /\ executionState \in {"Idle", "Claimed", "Ready", "Executing", "Completed", "Rejected"}
    /\ outputRevision \in 0..MaxRevision
    /\ revokedAtOutput \subseteq 1..MaxRevision

ClaimPinsObservedPublication ==
    executionState \in {"Claimed", "Ready", "Executing", "Completed"} => pinned > 0

MaterializationIsExact ==
    executionState \in {"Ready", "Executing", "Completed"} => materialized = pinned

RevokedCredentialCannotPublish == outputRevision > 0 => outputRevision \notin revokedAtOutput

CompletedOutputMatchesPin == executionState = "Completed" => outputRevision = pinned

Safety ==
    /\ TypeOK
    /\ ClaimPinsObservedPublication
    /\ MaterializationIsExact
    /\ RevokedCredentialCannotPublish
    /\ CompletedOutputMatchesPin

EventuallyCompleted == <> (executionState = "Completed")
=============================================================================
