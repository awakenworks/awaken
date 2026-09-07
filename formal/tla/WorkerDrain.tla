--------------------------- MODULE WorkerDrain ---------------------------
EXTENDS Naturals, TLC

CONSTANT MaxClaims

VARIABLE gate, claimInProgress, claimAttempts, claims,
         drainRequested, registryState, drainAcknowledged,
         attemptsAtRegistryDrain, claimsAtDrain
VARIABLE startupPhase, probePassed, withdrawal, withdrawalCalls, returned

drainVars == <<gate, claimInProgress, claimAttempts, claims,
          drainRequested, registryState, drainAcknowledged,
          attemptsAtRegistryDrain, claimsAtDrain>>
startupVars == <<startupPhase, probePassed, withdrawal, withdrawalCalls, returned>>
vars == <<drainVars, startupVars>>

Init == /\ gate = "Closed" /\ claimInProgress = FALSE
        /\ claimAttempts = 0 /\ claims = 0
        /\ drainRequested = FALSE /\ registryState = "Starting"
        /\ drainAcknowledged = FALSE
        /\ attemptsAtRegistryDrain = 0 /\ claimsAtDrain = 0
        /\ startupPhase = "probing" /\ probePassed = FALSE
        /\ withdrawal = "none" /\ withdrawalCalls = 0 /\ returned = FALSE

StartClaim == /\ gate = "Open" /\ ~claimInProgress
              /\ claimAttempts < MaxClaims
              /\ claimInProgress' = TRUE
              /\ claimAttempts' = claimAttempts + 1
              /\ UNCHANGED <<gate, claims, drainRequested, registryState,
                              drainAcknowledged, attemptsAtRegistryDrain,
                              claimsAtDrain>>

FinishClaim == /\ claimInProgress
               /\ claimInProgress' = FALSE /\ claims' = claims + 1
               /\ UNCHANGED <<gate, claimAttempts, drainRequested,
                               registryState, drainAcknowledged,
                               attemptsAtRegistryDrain, claimsAtDrain>>

RequestDrain == /\ ~drainRequested
                /\ drainRequested' = TRUE
                /\ UNCHANGED <<gate, claimInProgress, claimAttempts, claims,
                                registryState, drainAcknowledged,
                                attemptsAtRegistryDrain, claimsAtDrain>>

CloseAdmission == /\ drainRequested /\ gate = "Open" /\ ~claimInProgress
                  /\ gate' = "Closed"
                  /\ UNCHANGED <<claimInProgress, claimAttempts, claims,
                                  drainRequested, registryState,
                                  drainAcknowledged, attemptsAtRegistryDrain,
                                  claimsAtDrain>>

PublishRegistryDrain == /\ drainRequested /\ gate = "Closed"
                        /\ registryState = "Ready"
                        /\ registryState' = "Draining"
                        /\ attemptsAtRegistryDrain' = claimAttempts
                        /\ UNCHANGED <<gate, claimInProgress, claimAttempts,
                                        claims, drainRequested,
                                        drainAcknowledged, claimsAtDrain>>

AcknowledgeDrain == /\ registryState = "Draining"
                    /\ ~drainAcknowledged
                    /\ drainAcknowledged' = TRUE
                    /\ claimsAtDrain' = claims
                    /\ UNCHANGED <<gate, claimInProgress, claimAttempts,
                                    claims, drainRequested, registryState,
                                    attemptsAtRegistryDrain>>

RepeatDrain == /\ drainRequested /\ gate = "Closed"
               /\ UNCHANGED vars

DrainNext == StartClaim \/ FinishClaim \/ RequestDrain \/ CloseAdmission
        \/ PublishRegistryDrain \/ AcknowledgeDrain \/ RepeatDrain

(* Extend the one registration/drain owner. Expected error, dependency unwind,
   probe timeout and observed shutdown all consume startup before withdrawal.
   Initial Ready delivery and local acceptance are separate, so cancellation
   can leave a remote Ready record without ever opening local claim admission.
   Receipt freshness is assumed here; WorkerExecutionAuthority owns its clock.
   No arbitrary caller abort, process death, network delivery or OS proof. *)
ProbeCompletes == /\ startupPhase = "probing" /\ probePassed' = TRUE
                 /\ startupPhase' = "requesting"
                 /\ UNCHANGED <<drainVars, withdrawal, withdrawalCalls, returned>>
SendInitialReady == /\ startupPhase = "requesting" /\ registryState' = "Ready"
                   /\ startupPhase' = "awaiting"
                   /\ UNCHANGED <<gate, claimInProgress, claimAttempts, claims,
                        drainRequested, drainAcknowledged, attemptsAtRegistryDrain,
                        claimsAtDrain, probePassed, withdrawal, withdrawalCalls, returned>>
AcceptInitialReady == /\ startupPhase = "awaiting" /\ gate' = "Open"
                     /\ startupPhase' = "serving"
                     /\ UNCHANGED <<claimInProgress, claimAttempts, claims,
                        drainRequested, registryState, drainAcknowledged,
                        attemptsAtRegistryDrain, claimsAtDrain, probePassed,
                        withdrawal, withdrawalCalls, returned>>
FailStartup == /\ startupPhase \in {"probing", "requesting", "awaiting"}
               /\ startupPhase' = "failed"
               /\ UNCHANGED <<drainVars, probePassed, withdrawal, withdrawalCalls, returned>>
BeginWithdrawal == /\ withdrawal = "none"
                   /\ (startupPhase = "failed" \/ drainAcknowledged)
                   /\ gate = "Closed" /\ ~claimInProgress
                   /\ withdrawal' = "pending" /\ withdrawalCalls' = withdrawalCalls + 1
                   /\ UNCHANGED <<drainVars, startupPhase, probePassed, returned>>
CompleteWithdrawal(outcome) == /\ withdrawal = "pending"
    /\ withdrawal' = outcome /\ returned' = TRUE
    /\ registryState' = (IF outcome = "applied" THEN "Dead" ELSE registryState)
    /\ UNCHANGED <<gate, claimInProgress, claimAttempts, claims, drainRequested,
        drainAcknowledged, attemptsAtRegistryDrain, claimsAtDrain,
        startupPhase, probePassed, withdrawalCalls>>
FinishWithdrawal == \E outcome \in {"applied", "unknown"} : CompleteWithdrawal(outcome)
Next == (startupPhase = "serving" /\ DrainNext /\ UNCHANGED startupVars)
        \/ ProbeCompletes \/ SendInitialReady \/ AcceptInitialReady \/ FailStartup
        \/ BeginWithdrawal \/ FinishWithdrawal
        \/ (returned /\ UNCHANGED vars)

TypeOK == /\ gate \in {"Open", "Closed"} /\ claimInProgress \in BOOLEAN
          /\ claimAttempts \in 0..MaxClaims /\ claims \in 0..MaxClaims
          /\ claims <= claimAttempts /\ drainRequested \in BOOLEAN
          /\ registryState \in {"Starting", "Ready", "Draining", "Dead"}
          /\ drainAcknowledged \in BOOLEAN
          /\ attemptsAtRegistryDrain \in 0..MaxClaims
          /\ claimsAtDrain \in 0..MaxClaims
          /\ startupPhase \in {"probing", "requesting", "awaiting", "serving", "failed"}
          /\ probePassed \in BOOLEAN /\ returned \in BOOLEAN
          /\ withdrawal \in {"none", "pending", "applied", "unknown"}
          /\ withdrawalCalls \in 0..2
DrainIsAbsorbing == drainAcknowledged => gate = "Closed"
NoClaimAfterDrainAcknowledgement == drainAcknowledged => claims = claimsAtDrain
RegistryDrainFollowsLocalFence == registryState = "Draining" => gate = "Closed"
NoClaimAttemptAfterRegistryDrain ==
    registryState = "Draining" => claimAttempts = attemptsAtRegistryDrain

StartupAdmissionRequiresProbe == gate = "Open" => probePassed
FailedStartupNeverClaims == startupPhase = "failed" => claims = 0 /\ gate = "Closed"
OneWithdrawal == withdrawalCalls <= 1
ReturnFollowsWithdrawal == returned => withdrawal \in {"applied", "unknown"}
RemoteDeathRequiresReceipt == registryState = "Dead" => withdrawal = "applied"
StartupFailureTerminates == (startupPhase = "failed") ~> returned
NoFailedReturnReached == ~(startupPhase = "failed" /\ returned)
NoReadyClaimReached == claims = 0
NoUnknownWithdrawalReached == withdrawal # "unknown"
NoPendingHeartbeatCancelled == ~(startupPhase = "failed" /\ registryState = "Ready")

(* Fairness represents polling the owner and the bounded withdrawal timeout,
   not successful remote cleanup. An unknown response still permits local exit. *)
Spec == Init /\ [][Next]_vars /\ WF_vars(BeginWithdrawal) /\ WF_vars(FinishWithdrawal)
\* F3 a requested drain converges if in-flight claims and the existing drain
\* acknowledgements can progress. Permanent I/O failure is outside this bound.
ProgressSpec == Spec /\ WF_vars(FinishClaim) /\ WF_vars(CloseAdmission)
                    /\ WF_vars(PublishRegistryDrain) /\ WF_vars(AcknowledgeDrain)
DrainEventuallyAcknowledged == drainRequested ~> drainAcknowledged
=============================================================================
