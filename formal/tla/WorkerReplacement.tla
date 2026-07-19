------------------------ MODULE WorkerReplacement ------------------------
EXTENDS FiniteSets, Naturals, TLC

CONSTANTS Workers, Providers, Credentials,
          NoWorker, NoProvider, NoCredential,
          MaxRouteVersion, MaxLeaseEpoch, MaxCredentialEpoch, MaxAuthority

ASSUME /\ Workers # {} /\ Providers # {} /\ Credentials # {}
       /\ NoWorker \notin Workers /\ NoProvider \notin Providers
       /\ NoCredential \notin Credentials
       /\ Credentials = Providers
       /\ MaxRouteVersion \in Nat \ {0}
       /\ MaxLeaseEpoch \in Nat \ {0}
       /\ MaxCredentialEpoch \in Nat \ {0}
       /\ MaxAuthority # {}

Phases == {"Authored", "Resolved", "Dispatched", "Claimed",
           "Materialized", "Executing", "Settled"}

VARIABLES phase, routeVersion, resolvedVersion, resolvedCandidates,
          pinnedVersion, pinnedCandidates, actualProvider, actualCredential,
          actualCredentialEpoch, credentialEpoch, disabled, archived,
          claimWorker, workerAlive, leaseEpoch, leaseLive, secretLeaseEpoch,
          outputCommitted, commitLeaseEpoch, durableContainsSecret,
          authority, failed

vars == <<phase, routeVersion, resolvedVersion, resolvedCandidates,
          pinnedVersion, pinnedCandidates, actualProvider, actualCredential,
          actualCredentialEpoch, credentialEpoch, disabled, archived,
          claimWorker, workerAlive, leaseEpoch, leaseLive, secretLeaseEpoch,
          outputCommitted, commitLeaseEpoch, durableContainsSecret,
          authority, failed>>

Init ==
  /\ phase = "Authored"
  /\ routeVersion = 0 /\ resolvedVersion = 0 /\ resolvedCandidates = {}
  /\ pinnedVersion = 0 /\ pinnedCandidates = {}
  /\ actualProvider = NoProvider /\ actualCredential = NoCredential
  /\ actualCredentialEpoch = 0
  /\ credentialEpoch = [c \in Credentials |-> 0]
  /\ disabled = {} /\ archived = {}
  /\ claimWorker = NoWorker /\ workerAlive = Workers
  /\ leaseEpoch = 0 /\ leaseLive = FALSE /\ secretLeaseEpoch = 0
  /\ outputCommitted = FALSE /\ commitLeaseEpoch = 0
  /\ durableContainsSecret = FALSE
  /\ authority = MaxAuthority /\ failed = FALSE

Resolve(candidates) ==
  /\ phase = "Authored" /\ candidates \in SUBSET Providers /\ candidates # {}
  /\ phase' = "Resolved" /\ resolvedVersion' = routeVersion
  /\ resolvedCandidates' = candidates
  /\ UNCHANGED <<routeVersion, pinnedVersion, pinnedCandidates,
                  actualProvider, actualCredential, actualCredentialEpoch,
                  credentialEpoch, disabled, archived, claimWorker, workerAlive,
                  leaseEpoch, leaseLive, secretLeaseEpoch, outputCommitted,
                  commitLeaseEpoch, durableContainsSecret, authority, failed>>

RouteChanged ==
  /\ routeVersion < MaxRouteVersion /\ routeVersion' = routeVersion + 1
  /\ UNCHANGED <<phase, resolvedVersion, resolvedCandidates, pinnedVersion,
                  pinnedCandidates, actualProvider, actualCredential,
                  actualCredentialEpoch, credentialEpoch, disabled, archived,
                  claimWorker, workerAlive, leaseEpoch, leaseLive,
                  secretLeaseEpoch, outputCommitted, commitLeaseEpoch,
                  durableContainsSecret, authority, failed>>

Dispatch ==
  /\ phase = "Resolved"
  /\ phase' = "Dispatched" /\ pinnedVersion' = resolvedVersion
  /\ pinnedCandidates' = resolvedCandidates
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  actualProvider, actualCredential, actualCredentialEpoch,
                  credentialEpoch, disabled, archived, claimWorker, workerAlive,
                  leaseEpoch, leaseLive, secretLeaseEpoch, outputCommitted,
                  commitLeaseEpoch, durableContainsSecret, authority, failed>>

Claim(w) ==
  /\ phase = "Dispatched" /\ w \in workerAlive /\ leaseEpoch < MaxLeaseEpoch
  /\ phase' = "Claimed" /\ claimWorker' = w
  /\ leaseEpoch' = leaseEpoch + 1 /\ leaseLive' = TRUE
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  disabled, archived, workerAlive, secretLeaseEpoch,
                  outputCommitted, commitLeaseEpoch, durableContainsSecret,
                  authority, failed>>

CanMaterialize(p, c) ==
  /\ p \in pinnedCandidates /\ c \in Credentials
  /\ c = p /\ c \notin disabled /\ c \notin archived
  /\ \/ actualCredential = NoCredential
     \/ /\ actualCredential = c /\ actualProvider = p

Materialize(p, c) ==
  /\ phase = "Claimed" /\ leaseLive /\ claimWorker \in workerAlive
  /\ CanMaterialize(p, c)
  /\ phase' = "Materialized" /\ actualProvider' = p
  /\ actualCredential' = c /\ actualCredentialEpoch' = credentialEpoch[c]
  /\ secretLeaseEpoch' = leaseEpoch
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, credentialEpoch, disabled,
                  archived, claimWorker, workerAlive, leaseEpoch, leaseLive,
                  outputCommitted, commitLeaseEpoch, durableContainsSecret,
                  authority, failed>>

Execute ==
  /\ phase = "Materialized" /\ leaseLive /\ claimWorker \in workerAlive
  /\ actualCredential \notin disabled /\ actualCredential \notin archived
  /\ secretLeaseEpoch = leaseEpoch
  /\ phase' = "Executing"
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  disabled, archived, claimWorker, workerAlive, leaseEpoch,
                  leaseLive, secretLeaseEpoch, outputCommitted,
                  commitLeaseEpoch, durableContainsSecret, authority, failed>>

Settle ==
  /\ phase = "Executing" /\ leaseLive /\ claimWorker \in workerAlive
  /\ phase' = "Settled" /\ outputCommitted' = TRUE
  /\ commitLeaseEpoch' = leaseEpoch /\ leaseLive' = FALSE
  /\ secretLeaseEpoch' = 0
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  disabled, archived, claimWorker, workerAlive, leaseEpoch,
                  durableContainsSecret, authority, failed>>

CredentialRotated(c) ==
  /\ c \in Credentials /\ credentialEpoch[c] < MaxCredentialEpoch
  /\ credentialEpoch' = [credentialEpoch EXCEPT ![c] = @ + 1]
  /\ UNCHANGED <<phase, routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, disabled, archived,
                  claimWorker, workerAlive, leaseEpoch, leaseLive,
                  secretLeaseEpoch, outputCommitted, commitLeaseEpoch,
                  durableContainsSecret, authority, failed>>

CredentialUnavailable(c, nextDisabled, nextArchived) ==
  /\ c \in Credentials /\ c \notin disabled \cup archived
  /\ nextDisabled \in SUBSET Credentials /\ nextArchived \in SUBSET Credentials
  /\ c \in nextDisabled \cup nextArchived
  /\ disabled' = nextDisabled /\ archived' = nextArchived
  /\ IF actualCredential = c /\ phase \in {"Materialized", "Executing"}
        THEN /\ phase' = "Settled" /\ failed' = TRUE
             /\ leaseLive' = FALSE /\ secretLeaseEpoch' = 0
        ELSE /\ UNCHANGED <<phase, failed, leaseLive, secretLeaseEpoch>>
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  claimWorker, workerAlive, leaseEpoch, outputCommitted,
                  commitLeaseEpoch, durableContainsSecret, authority>>

CredentialDisabled(c) == CredentialUnavailable(c, disabled \cup {c}, archived)
CredentialArchived(c) == CredentialUnavailable(c, disabled, archived \cup {c})

WorkerCrashed(w) ==
  /\ w \in workerAlive /\ workerAlive' = workerAlive \ {w}
  /\ IF claimWorker = w THEN leaseLive' = FALSE ELSE UNCHANGED leaseLive
  /\ UNCHANGED <<phase, routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  disabled, archived, claimWorker, leaseEpoch,
                  secretLeaseEpoch, outputCommitted, commitLeaseEpoch,
                  durableContainsSecret, authority, failed>>

LeaseExpired ==
  /\ leaseLive /\ leaseLive' = FALSE
  /\ UNCHANGED <<phase, routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  disabled, archived, claimWorker, workerAlive, leaseEpoch,
                  secretLeaseEpoch, outputCommitted, commitLeaseEpoch,
                  durableContainsSecret, authority, failed>>

RetryClaimed(w) ==
  /\ phase \in {"Claimed", "Materialized", "Executing"} /\ ~leaseLive
  /\ w \in workerAlive /\ leaseEpoch < MaxLeaseEpoch
  /\ phase' = "Claimed" /\ claimWorker' = w
  /\ leaseEpoch' = leaseEpoch + 1 /\ leaseLive' = TRUE
  /\ secretLeaseEpoch' = 0
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  disabled, archived, workerAlive, outputCommitted,
                  commitLeaseEpoch, durableContainsSecret, authority, failed>>

FailClosed ==
  /\ phase = "Claimed"
  /\ ~\E p \in Providers, c \in Credentials : CanMaterialize(p, c)
  /\ phase' = "Settled" /\ failed' = TRUE /\ leaseLive' = FALSE
  /\ secretLeaseEpoch' = 0
  /\ UNCHANGED <<routeVersion, resolvedVersion, resolvedCandidates,
                  pinnedVersion, pinnedCandidates, actualProvider,
                  actualCredential, actualCredentialEpoch, credentialEpoch,
                  disabled, archived, claimWorker, workerAlive, leaseEpoch,
                  outputCommitted, commitLeaseEpoch, durableContainsSecret,
                  authority>>

Next ==
  \/ \E candidates \in SUBSET Providers : Resolve(candidates)
  \/ RouteChanged \/ Dispatch \/ \E w \in Workers : Claim(w)
  \/ \E p \in Providers, c \in Credentials : Materialize(p, c)
  \/ Execute \/ Settle
  \/ \E c \in Credentials : CredentialRotated(c)
  \/ \E c \in Credentials : CredentialDisabled(c) \/ CredentialArchived(c)
  \/ \E w \in Workers : WorkerCrashed(w) \/ RetryClaimed(w)
  \/ LeaseExpired \/ FailClosed

TypeOK ==
  /\ phase \in Phases /\ routeVersion \in 0..MaxRouteVersion
  /\ resolvedVersion \in 0..MaxRouteVersion
  /\ resolvedCandidates \in SUBSET Providers
  /\ pinnedVersion \in 0..MaxRouteVersion
  /\ pinnedCandidates \in SUBSET Providers
  /\ actualProvider \in Providers \cup {NoProvider}
  /\ actualCredential \in Credentials \cup {NoCredential}
  /\ actualCredentialEpoch \in 0..MaxCredentialEpoch
  /\ credentialEpoch \in [Credentials -> 0..MaxCredentialEpoch]
  /\ disabled \in SUBSET Credentials /\ archived \in SUBSET Credentials
  /\ claimWorker \in Workers \cup {NoWorker} /\ workerAlive \in SUBSET Workers
  /\ leaseEpoch \in 0..MaxLeaseEpoch /\ leaseLive \in BOOLEAN
  /\ secretLeaseEpoch \in 0..MaxLeaseEpoch /\ outputCommitted \in BOOLEAN
  /\ commitLeaseEpoch \in 0..MaxLeaseEpoch
  /\ durableContainsSecret \in BOOLEAN /\ authority \in SUBSET MaxAuthority
  /\ failed \in BOOLEAN

NoDurableSecrets == ~durableContainsSecret
AuthorityNeverWidens == authority \subseteq MaxAuthority
DispatchPinsResolution ==
  phase \in {"Dispatched", "Claimed", "Materialized", "Executing", "Settled"}
    => /\ pinnedVersion = resolvedVersion /\ pinnedCandidates = resolvedCandidates
       /\ pinnedCandidates # {}
ActualBindingIsPinned ==
  actualProvider # NoProvider =>
    /\ actualProvider \in pinnedCandidates
    /\ actualCredential \in Credentials
    /\ actualCredential = actualProvider
ActualBindingIsComplete == (actualProvider = NoProvider) = (actualCredential = NoCredential)
UnavailableCredentialNotExecuting ==
  phase \in {"Materialized", "Executing"} =>
    actualCredential \notin disabled \cup archived
SecretLeaseIsClaimFenced ==
  phase \in {"Materialized", "Executing"} => secretLeaseEpoch = leaseEpoch
OutputRequiresCurrentFence ==
  outputCommitted => /\ phase = "Settled" /\ commitLeaseEpoch = leaseEpoch
                     /\ commitLeaseEpoch > 0 /\ ~failed
FailureNeverPublishesOutput == failed => ~outputCommitted

Safety == /\ TypeOK /\ NoDurableSecrets /\ AuthorityNeverWidens
          /\ DispatchPinsResolution /\ ActualBindingIsPinned
          /\ ActualBindingIsComplete /\ UnavailableCredentialNotExecuting
          /\ SecretLeaseIsClaimFenced /\ OutputRequiresCurrentFence
          /\ FailureNeverPublishesOutput

Spec == Init /\ [][Next]_vars
=============================================================================
