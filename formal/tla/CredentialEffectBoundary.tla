----------------------- MODULE CredentialEffectBoundary -----------------------
EXTENDS Naturals, FiniteSets

\* Existing exact binding/effect boundary, not a second Credential store.
\* Pins abstract equality of opaque identities, never cryptographic correctness.
\* The full configuration covers every initial match/mismatch combination.
\* Dynamic adoption/reclaim may independently advance revision, generation and
\* epoch once. Other identity coordinates are frozen, not current-Vault search.
CONSTANT MaxMismatches
Axes == {"workspace", "session", "source", "revision", "holder", "trustDomain",
         "target", "usage", "envelope", "payload", "generation", "epoch"}
ChangingAxes == {"revision", "generation", "epoch"}
Pins == [Axes -> 1..2]
InitialPin == [axis \in Axes |-> 1]
Requests == {pin \in Pins:
    Cardinality({axis \in Axes: pin[axis] # InitialPin[axis]}) <= MaxMismatches}
ReceiptPins(pin) == {pin} \cup {[pin EXCEPT ![axis] = 3 - @]: axis \in Axes}

VARIABLES phase, requested, current, now, authorized, processUp,
          materialized, published, materialObservation, publicationObservation
vars == <<phase, requested, current, now, authorized, processUp,
          materialized, published, materialObservation, publicationObservation>>

EmptyObservation == [request |-> InitialPin, authority |-> InitialPin,
                     receipt |-> InitialPin, time |-> 0, live |-> FALSE, up |-> FALSE]
Observation(receipt) == [request |-> requested, authority |-> current,
                        receipt |-> receipt, time |-> now,
                        live |-> authorized, up |-> processUp]
\* Normative relation over the actual observation, separate from the guards.
ExactObservation(observation) ==
    /\ observation.request = observation.authority
    /\ observation.receipt = observation.request
    /\ observation.live /\ observation.up /\ observation.time < 2

Init ==
    /\ phase = "Idle"
    /\ requested = InitialPin /\ current = InitialPin
    /\ now = 0 /\ authorized = TRUE /\ processUp = TRUE
    /\ materialized = FALSE /\ published = FALSE
    /\ materialObservation = EmptyObservation
    /\ publicationObservation = EmptyObservation

Prepare(pin) ==
    /\ phase = "Idle" /\ processUp /\ pin \in Requests
    /\ phase' = "Prepared" /\ requested' = pin
    /\ UNCHANGED <<current, now, authorized, processUp, materialized,
                   published, materialObservation, publicationObservation>>

\* C1 complete request equality; C2 live use-time authority; C3 unexpired;
\* C4 process present. E1 records the exact effect. No approval snapshot from
\* Prepare substitutes for the checks after a wait, revocation or reclaim.
MaterializeExact ==
    /\ phase = "Prepared" /\ processUp
    /\ \A axis \in Axes: requested[axis] = current[axis]
    /\ authorized /\ now < 2
    /\ phase' = "Materialized" /\ materialized' = TRUE
    /\ materialObservation' = Observation(requested)
    /\ UNCHANGED <<requested, current, now, authorized, processUp,
                   published, publicationObservation>>

RejectInexact ==
    /\ phase \in {"Prepared", "Materialized"} /\ processUp
    /\ requested # current \/ ~authorized \/ now >= 2
    /\ phase' = "Rejected" /\ published' = FALSE
    /\ UNCHANGED <<requested, current, now, authorized, processUp,
                   materialized, materialObservation, publicationObservation>>

\* A receipt arriving after materialization is a separate untrusted input.
\* Recheck current authority and every receipt coordinate before publication.
Publish(receipt) ==
    /\ phase = "Materialized" /\ processUp /\ receipt \in ReceiptPins(requested)
    /\ \A axis \in Axes: requested[axis] = current[axis]
    /\ \A axis \in Axes: receipt[axis] = requested[axis]
    /\ authorized /\ now < 2
    /\ phase' = "Published" /\ published' = TRUE
    /\ publicationObservation' = Observation(receipt)
    /\ UNCHANGED <<requested, current, now, authorized, processUp,
                   materialized, materialObservation>>

RejectReceipt(receipt) ==
    /\ phase = "Materialized" /\ processUp
    /\ receipt \in ReceiptPins(requested) /\ receipt # requested
    /\ phase' = "Rejected"
    /\ UNCHANGED <<requested, current, now, authorized, processUp,
                   materialized, published, materialObservation, publicationObservation>>

\* Adoption changes accepted authority, not an implicit source fallback.
\* Changes after publication retire its live projection but preserve history.
Advance(axis) ==
    /\ phase # "Idle" /\ axis \in ChangingAxes /\ current[axis] < 2
    /\ current' = [current EXCEPT ![axis] = @ + 1]
    /\ phase' = IF published THEN "Revoked" ELSE phase
    /\ published' = FALSE
    /\ UNCHANGED <<requested, now, authorized, processUp, materialized,
                   materialObservation, publicationObservation>>

Tick ==
    /\ now < 2 /\ now' = now + 1
    /\ phase' = IF published /\ now' = 2 THEN "Revoked" ELSE phase
    /\ published' = (published /\ now' < 2)
    /\ UNCHANGED <<requested, current, authorized, processUp, materialized,
                   materialObservation, publicationObservation>>

Revoke ==
    /\ authorized /\ authorized' = FALSE
    /\ phase' = IF published THEN "Revoked" ELSE phase
    /\ published' = FALSE
    /\ UNCHANGED <<requested, current, now, processUp, materialized,
                   materialObservation, publicationObservation>>

Crash ==
    /\ processUp /\ processUp' = FALSE
    /\ UNCHANGED <<phase, requested, current, now, authorized, materialized,
                   published, materialObservation, publicationObservation>>
Restart ==
    /\ ~processUp /\ processUp' = TRUE
    /\ UNCHANGED <<phase, requested, current, now, authorized, materialized,
                   published, materialObservation, publicationObservation>>

\* Test the phase before enumerating the input domain; this preserves the
\* relation while avoiding 4096 disabled Prepare evaluations in every state.
PrepareAny == /\ phase = "Idle" /\ (\E pin \in Requests: Prepare(pin))
PublishAny == /\ phase = "Materialized"
              /\ (\E receipt \in ReceiptPins(requested): Publish(receipt) \/ RejectReceipt(receipt))
Next == PrepareAny \/ MaterializeExact \/ RejectInexact \/ PublishAny
        \/ (\E axis \in ChangingAxes: Advance(axis)) \/ Tick \/ Revoke \/ Crash \/ Restart
Spec == Init /\ [][Next]_vars

Observations == [request: Pins, authority: Pins, receipt: Pins,
                 time: 0..2, live: BOOLEAN, up: BOOLEAN]
TypeOK ==
    /\ phase \in {"Idle", "Prepared", "Materialized", "Published", "Rejected", "Revoked"}
    /\ requested \in Pins /\ current \in Pins /\ now \in 0..2
    /\ authorized \in BOOLEAN /\ processUp \in BOOLEAN
    /\ materialized \in BOOLEAN /\ published \in BOOLEAN
    /\ materialObservation \in Observations /\ publicationObservation \in Observations
MaterializationRequiresExactClaim == materialized => ExactObservation(materialObservation)
PublicationRequiresMaterialization ==
    published => materialized /\ phase = "Published" /\ ExactObservation(publicationObservation)
PublishedAuthorityIsCurrent == published => requested = current /\ authorized /\ now < 2
RejectedNeverPublishes == phase = "Rejected" => ~published
Safety == TypeOK /\ MaterializationRequiresExactClaim /\ PublicationRequiresMaterialization
          /\ PublishedAuthorityIsCurrent /\ RejectedNeverPublishes

\* Existential witnesses, selected only by the mutation runner. A lawful old
\* materialization stays lawful after revocation; future publication is blocked.
NoPublicationReached == ~published
NoExpiredRejectionReached == ~(phase = "Rejected" /\ now = 2)
NoRevokedRejectionReached == ~(phase = "Rejected" /\ ~authorized)
NoSuccessorPublicationReached == ~published \/ current["epoch"] = 1
NoStaleMaterialRejectionReached ==
    ~(phase = "Rejected" /\ materialized /\ requested # current)
NoRetainedRevokedMaterialReached == ~(materialized /\ ~authorized)
=============================================================================
