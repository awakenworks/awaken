--------------------------- MODULE WorkerDrain ---------------------------
EXTENDS Naturals, TLC

CONSTANT MaxClaims

VARIABLE gate, claimInProgress, claimAttempts, claims,
         drainRequested, registryState, drainAcknowledged,
         attemptsAtRegistryDrain, claimsAtDrain
VARIABLE shutdown
VARIABLE startupPhase, probePassed, withdrawal, withdrawalCalls, returned
VARIABLE protocolCompatible, ttlPositive, protocolAccepted

drainVars == <<gate, claimInProgress, claimAttempts, claims,
          drainRequested, registryState, drainAcknowledged,
          attemptsAtRegistryDrain, claimsAtDrain, shutdown>>
startupVars == <<startupPhase, probePassed, withdrawal, withdrawalCalls, returned>>
protocolVars == <<protocolCompatible, ttlPositive, protocolAccepted>>
vars == <<drainVars, startupVars, protocolVars>>

Init == /\ gate = "Closed" /\ claimInProgress = FALSE
        /\ claimAttempts = 0 /\ claims = 0
        /\ drainRequested = FALSE /\ registryState = "Starting"
        /\ drainAcknowledged = FALSE
        /\ attemptsAtRegistryDrain = 0 /\ claimsAtDrain = 0
        /\ shutdown = [active |-> {}, graceExpired |-> FALSE,
                        cancelRequested |-> FALSE, cleanupExpired |-> FALSE,
                        cleanupFinished |-> FALSE, quiesced |-> FALSE]
        /\ startupPhase = "checking_protocol" /\ probePassed = FALSE
        /\ protocolCompatible \in BOOLEAN /\ protocolAccepted = FALSE
        /\ ttlPositive \in BOOLEAN
        /\ withdrawal = "none" /\ withdrawalCalls = 0 /\ returned = FALSE

StartClaim == /\ gate = "Open" /\ ~claimInProgress
              /\ claimAttempts < MaxClaims
              /\ claimInProgress' = TRUE
              /\ claimAttempts' = claimAttempts + 1
              /\ UNCHANGED <<gate, claims, drainRequested, registryState,
                              drainAcknowledged, attemptsAtRegistryDrain,
                              claimsAtDrain, shutdown>>

FinishClaim == /\ claimInProgress
               /\ claimInProgress' = FALSE /\ claims' = claims + 1
               /\ shutdown' = [shutdown EXCEPT !.active = @ \cup {claims + 1}]
               /\ UNCHANGED <<gate, claimAttempts, drainRequested,
                               registryState, drainAcknowledged,
                               attemptsAtRegistryDrain, claimsAtDrain>>

RequestDrain == /\ ~drainRequested
                /\ drainRequested' = TRUE
                /\ UNCHANGED <<gate, claimInProgress, claimAttempts, claims,
                                registryState, drainAcknowledged,
                                attemptsAtRegistryDrain, claimsAtDrain, shutdown>>

CloseAdmission == /\ drainRequested /\ gate = "Open" /\ ~claimInProgress
                  /\ gate' = "Closed"
                  /\ UNCHANGED <<claimInProgress, claimAttempts, claims,
                                  drainRequested, registryState,
                                  drainAcknowledged, attemptsAtRegistryDrain,
                                  claimsAtDrain, shutdown>>

PublishRegistryDrain == /\ drainRequested /\ gate = "Closed"
                        /\ registryState = "Ready"
                        /\ registryState' = "Draining"
                        /\ attemptsAtRegistryDrain' = claimAttempts
                        /\ UNCHANGED <<gate, claimInProgress, claimAttempts,
                                        claims, drainRequested,
                                        drainAcknowledged, claimsAtDrain, shutdown>>

AcknowledgeDrain == /\ registryState = "Draining"
                    /\ ~drainAcknowledged
                    /\ drainAcknowledged' = TRUE
                    /\ claimsAtDrain' = claims
                    /\ UNCHANGED <<gate, claimInProgress, claimAttempts,
                                    claims, drainRequested, registryState,
                                    attemptsAtRegistryDrain, shutdown>>

RepeatDrain == /\ drainRequested /\ gate = "Closed"
               /\ UNCHANGED vars

\* G1 actual return before grace -> cleanup finished; G2 expiry with active
\* work -> cancellation plus bounded observation; G3 no return -> finish local
\* cleanup unquiesced. The existing withdrawal owner alone returns the process.
controlVars == <<gate, claimInProgress, claimAttempts, claims, drainRequested,
                 registryState, drainAcknowledged, attemptsAtRegistryDrain,
                 claimsAtDrain>>
AttemptReturns(id) == /\ ~returned /\ id \in shutdown.active
                     /\ shutdown' = [shutdown EXCEPT !.active = @ \ {id}]
                     /\ UNCHANGED controlVars
ExpireGrace == /\ ~returned /\ drainAcknowledged /\ ~shutdown.graceExpired
               /\ shutdown' = [shutdown EXCEPT !.graceExpired = TRUE]
               /\ UNCHANGED controlVars
CancelSurvivors == /\ ~returned /\ shutdown.graceExpired /\ ~shutdown.cancelRequested
                   /\ shutdown' = [shutdown EXCEPT !.cancelRequested = TRUE]
                   /\ UNCHANGED controlVars
ExpireCleanup == /\ ~returned /\ shutdown.cancelRequested /\ ~shutdown.cleanupExpired
                 /\ shutdown' = [shutdown EXCEPT !.cleanupExpired = TRUE]
                 /\ UNCHANGED controlVars
FinishCleanup == /\ ~returned /\ drainAcknowledged /\ ~shutdown.cleanupFinished
                 /\ (shutdown.active = {} \/ (shutdown.cancelRequested /\ shutdown.cleanupExpired))
                 /\ shutdown' = [shutdown EXCEPT !.cleanupFinished = TRUE,
                                     !.quiesced = (shutdown.active = {})]
                 /\ UNCHANGED controlVars

DrainNext == StartClaim \/ FinishClaim \/ RequestDrain \/ CloseAdmission
        \/ PublishRegistryDrain \/ AcknowledgeDrain \/ RepeatDrain
        \/ (\E id \in 1..MaxClaims: AttemptReturns(id))
        \/ ExpireGrace \/ CancelSurvivors \/ ExpireCleanup \/ FinishCleanup

(* Extend the one registration/drain owner. Expected error, dependency unwind,
   probe timeout and observed shutdown all consume startup before withdrawal.
   Initial Ready delivery and local acceptance are separate, so cancellation
   can leave a remote Ready record without ever opening local claim admission.
   Receipt freshness is assumed here; WorkerExecutionAuthority owns its clock.
   No arbitrary caller abort, process death, network delivery or OS proof. *)
(* Post-registration decision table: compatible declaration + positive TTL ->
   probe; missing/malformed/old/future declaration or zero TTL ->
   failed -> sole withdrawal.
   These are abstract wire equivalence classes, not a serde or rolling-server
   upgrade proof. Rejection and observed shutdown must not bypass withdrawal. *)
AcceptProtocol == /\ startupPhase = "checking_protocol"
                  /\ protocolCompatible
                  /\ ttlPositive
                  /\ startupPhase' = "probing" /\ protocolAccepted' = TRUE
                  /\ UNCHANGED <<drainVars, protocolCompatible, ttlPositive, probePassed,
                                  withdrawal, withdrawalCalls, returned>>
RejectProtocol == /\ startupPhase = "checking_protocol"
                  /\ ~(protocolCompatible /\ ttlPositive)
                  /\ startupPhase' = "failed"
                  /\ UNCHANGED <<drainVars, protocolVars, probePassed,
                                  withdrawal, withdrawalCalls, returned>>
ResolveProtocol == AcceptProtocol \/ RejectProtocol

ProbeCompletes == /\ startupPhase = "probing" /\ probePassed' = TRUE
                 /\ startupPhase' = "requesting"
                 /\ UNCHANGED <<drainVars, withdrawal, withdrawalCalls, returned>>
SendInitialReady == /\ startupPhase = "requesting" /\ registryState' = "Ready"
                   /\ startupPhase' = "awaiting"
                   /\ UNCHANGED <<gate, claimInProgress, claimAttempts, claims,
                        drainRequested, drainAcknowledged, attemptsAtRegistryDrain,
                        claimsAtDrain, probePassed, withdrawal, withdrawalCalls, returned, shutdown>>
AcceptInitialReady == /\ startupPhase = "awaiting" /\ gate' = "Open"
                     /\ startupPhase' = "serving"
                     /\ UNCHANGED <<claimInProgress, claimAttempts, claims,
                        drainRequested, registryState, drainAcknowledged,
                        attemptsAtRegistryDrain, claimsAtDrain, probePassed,
                        withdrawal, withdrawalCalls, returned, shutdown>>
FailStartup == /\ startupPhase \in {"probing", "requesting", "awaiting"}
               /\ startupPhase' = "failed"
               /\ UNCHANGED <<drainVars, probePassed, withdrawal, withdrawalCalls, returned>>
BeginWithdrawal == /\ withdrawal = "none"
                   /\ (startupPhase = "failed" \/ shutdown.cleanupFinished)
                   /\ gate = "Closed" /\ ~claimInProgress
                   /\ withdrawal' = "pending" /\ withdrawalCalls' = withdrawalCalls + 1
                   /\ UNCHANGED <<drainVars, startupPhase, probePassed, returned, protocolVars>>
CompleteWithdrawal(outcome) == /\ withdrawal = "pending"
    /\ withdrawal' = outcome /\ returned' = TRUE
    /\ registryState' = (IF outcome = "applied" THEN "Dead" ELSE registryState)
    /\ UNCHANGED <<gate, claimInProgress, claimAttempts, claims, drainRequested,
        drainAcknowledged, attemptsAtRegistryDrain, claimsAtDrain,
        startupPhase, probePassed, withdrawalCalls, protocolVars, shutdown>>
FinishWithdrawal == \E outcome \in {"applied", "unknown"} : CompleteWithdrawal(outcome)
Next == (UNCHANGED protocolVars /\
        ((startupPhase = "serving" /\ ~returned /\ DrainNext /\ UNCHANGED startupVars)
        \/ ProbeCompletes \/ SendInitialReady \/ AcceptInitialReady \/ FailStartup
        \/ BeginWithdrawal \/ FinishWithdrawal
        \/ (returned /\ UNCHANGED vars)))
        \/ ResolveProtocol

TypeOK == /\ gate \in {"Open", "Closed"} /\ claimInProgress \in BOOLEAN
          /\ claimAttempts \in 0..MaxClaims /\ claims \in 0..MaxClaims
          /\ claims <= claimAttempts /\ drainRequested \in BOOLEAN
          /\ registryState \in {"Starting", "Ready", "Draining", "Dead"}
          /\ drainAcknowledged \in BOOLEAN
          /\ attemptsAtRegistryDrain \in 0..MaxClaims
          /\ claimsAtDrain \in 0..MaxClaims
          /\ startupPhase \in {"checking_protocol", "probing", "requesting", "awaiting", "serving", "failed"}
          /\ protocolCompatible \in BOOLEAN /\ protocolAccepted \in BOOLEAN
          /\ ttlPositive \in BOOLEAN
          /\ probePassed \in BOOLEAN /\ returned \in BOOLEAN
          /\ withdrawal \in {"none", "pending", "applied", "unknown"}
          /\ withdrawalCalls \in 0..2
ShutdownTypeOK == shutdown \in [active: SUBSET (1..MaxClaims),
    graceExpired: BOOLEAN, cancelRequested: BOOLEAN, cleanupExpired: BOOLEAN,
    cleanupFinished: BOOLEAN, quiesced: BOOLEAN]
ShutdownDoesNotDiscardUncancelledWork ==
    (returned /\ shutdown.active # {})
        => (shutdown.cancelRequested /\ shutdown.cleanupExpired)
NoFabricatedQuiescence == shutdown.quiesced => shutdown.active = {}
ShutdownEventuallyExits == drainAcknowledged ~> returned
DrainIsAbsorbing == drainAcknowledged => gate = "Closed"
NoClaimAfterDrainAcknowledgement == drainAcknowledged => claims = claimsAtDrain
RegistryDrainFollowsLocalFence == registryState = "Draining" => gate = "Closed"
NoClaimAttemptAfterRegistryDrain ==
    registryState = "Draining" => claimAttempts = attemptsAtRegistryDrain

StartupAdmissionRequiresProbe == gate = "Open" => probePassed
RegistrationEvidenceRequired ==
    /\ (protocolAccepted => protocolCompatible /\ ttlPositive)
    /\ (startupPhase \in {"probing", "requesting", "awaiting", "serving"}
        => protocolAccepted)
    /\ (~(protocolCompatible /\ ttlPositive) => ~probePassed /\ claims = 0 /\ gate = "Closed")
InvalidRegistrationTerminates == ~(protocolCompatible /\ ttlPositive) ~> returned
FailedStartupNeverClaims == startupPhase = "failed" => claims = 0 /\ gate = "Closed"
OneWithdrawal == withdrawalCalls <= 1
ReturnFollowsWithdrawal == returned => withdrawal \in {"applied", "unknown"}
RemoteDeathRequiresReceipt == registryState = "Dead" => withdrawal = "applied"
StartupFailureTerminates == (startupPhase = "failed") ~> returned
NoFailedReturnReached == ~(startupPhase = "failed" /\ returned)
NoReadyClaimReached == claims = 0
NoUnknownWithdrawalReached == withdrawal # "unknown"
NoPendingHeartbeatCancelled == ~(startupPhase = "failed" /\ registryState = "Ready")
NoIncompatibleReturnReached == ~(~protocolCompatible /\ returned)
NoZeroTtlReturnReached == ~(~ttlPositive /\ returned)
NoDrainAcknowledgedReached == ~drainAcknowledged

(* Fairness represents polling the owner and the bounded withdrawal timeout,
   not successful remote cleanup. An unknown response still permits local exit. *)
Spec == Init /\ [][Next]_vars /\ WF_vars(ResolveProtocol)
        /\ WF_vars(BeginWithdrawal) /\ WF_vars(FinishWithdrawal)
\* F3 a requested drain converges if in-flight claims and the existing drain
\* acknowledgements can progress. Permanent I/O failure is outside this bound.
\* Drain actions frame only drainVars; Next supplies the unchanged startup and
\* protocol frame. Their fairness must use that same owned tuple, not evaluate
\* unconstrained startup/protocol next-state values after model composition.
ProgressSpec == Spec /\ WF_drainVars(FinishClaim) /\ WF_drainVars(CloseAdmission)
                    /\ WF_drainVars(PublishRegistryDrain) /\ WF_drainVars(AcknowledgeDrain)
                    /\ WF_drainVars(ExpireGrace) /\ WF_drainVars(CancelSurvivors)
                    /\ WF_drainVars(ExpireCleanup) /\ WF_drainVars(FinishCleanup)
DrainEventuallyAcknowledged == drainRequested ~> drainAcknowledged
=============================================================================
