----------------------- MODULE CredentialEffectBoundary -----------------------
EXTENDS Naturals

VARIABLES phase, exactClaim, materialized, published
vars == <<phase, exactClaim, materialized, published>>

Init ==
  /\ phase = "Idle"
  /\ exactClaim \in BOOLEAN
  /\ materialized = FALSE
  /\ published = FALSE

Prepare ==
  /\ phase = "Idle"
  /\ phase' = "Prepared"
  /\ UNCHANGED <<exactClaim, materialized, published>>

MaterializeExact ==
  /\ phase = "Prepared"
  /\ exactClaim
  /\ phase' = "Materialized"
  /\ materialized' = TRUE
  /\ UNCHANGED <<exactClaim, published>>

RejectInexact ==
  /\ phase = "Prepared"
  /\ ~exactClaim
  /\ phase' = "Rejected"
  /\ UNCHANGED <<exactClaim, materialized, published>>

Publish ==
  /\ phase = "Materialized"
  /\ phase' = "Published"
  /\ published' = TRUE
  /\ UNCHANGED <<exactClaim, materialized>>

Revoke ==
  /\ phase \in {"Prepared", "Materialized", "Published"}
  /\ phase' = "Revoked"
  /\ published' = FALSE
  /\ UNCHANGED <<exactClaim, materialized>>

Next == Prepare \/ MaterializeExact \/ RejectInexact \/ Publish \/ Revoke
Spec == Init /\ [][Next]_vars

TypeOK == phase \in {"Idle", "Prepared", "Materialized", "Published", "Rejected", "Revoked"}
MaterializationRequiresExactClaim == materialized => exactClaim
PublicationRequiresMaterialization == published => materialized /\ exactClaim /\ phase = "Published"
RejectedNeverPublishes == phase = "Rejected" => ~materialized /\ ~published
Safety == TypeOK /\ MaterializationRequiresExactClaim /\ PublicationRequiresMaterialization /\ RejectedNeverPublishes
=============================================================================
