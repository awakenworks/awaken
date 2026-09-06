---------------------- MODULE LocalProcessLifecycle ----------------------
EXTENDS FiniteSets, Naturals, Sequences, TLC

\* One local ProcessHandle, two concurrent waiters and the signal caller.
\* Poll's OS call is synchronous; only mutex acquisition/backoff can suspend.
\* An OS termination request is distinct from observed exit. Worker death does
\* not run Rust Drop; descendant survival is intentionally a reachable state.
CONSTANTS Waiters, NoLock
ASSUME /\ Waiters # {} /\ NoLock \notin Waiters

VARIABLES phase, privateGroup, dropPolicy, leaderAlive, descendantAlive,
          stopRequested, lock, waiter, observedExit, signalOutcome, signalAccepted
VARIABLES scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt
VARIABLES launchStage, joined, goIssued, workloadStarted
VARIABLES authorityLive, authorityAtGrant, authorityHeld
VARIABLES initState, helperAlive, controlLive, deadlineLive
VARIABLES cutoffClock, cutoff, cutoffQueue, cutoffPartial, cutoffWriteAuthorized
renewalVars == <<cutoffClock, cutoff, cutoffQueue, cutoffPartial, cutoffWriteAuthorized>>
boundaryVars == <<initState, helperAlive, controlLive, deadlineLive>>
authorityVars == <<authorityLive, authorityAtGrant, authorityHeld>>
launchVars == <<launchStage, joined, goIssued, workloadStarted>>
scopeVars == <<scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt, launchVars, authorityVars>>
signalVars == <<signalOutcome, signalAccepted>>
vars == <<phase, privateGroup, dropPolicy, leaderAlive, descendantAlive,
          stopRequested, lock, waiter, observedExit, signalVars>>

Init ==
  /\ phase = "new" /\ privateGroup = FALSE /\ dropPolicy = FALSE
  /\ leaderAlive = FALSE /\ descendantAlive = FALSE
  /\ stopRequested = FALSE /\ lock = NoLock
  /\ waiter = [w \in Waiters |-> "ready"] /\ observedExit = {}
  /\ signalOutcome = "none" /\ signalAccepted = FALSE
  /\ scopeClosed = FALSE /\ scopePermit = FALSE
  /\ scopeKillAccepted = FALSE /\ scopeReceipt = FALSE
  /\ launchStage = "idle" /\ joined = FALSE
  /\ goIssued = FALSE /\ workloadStarted = FALSE
  /\ authorityLive = TRUE /\ authorityAtGrant = FALSE /\ authorityHeld = FALSE
  /\ initState = "absent" /\ helperAlive = FALSE
  /\ controlLive = TRUE /\ deadlineLive = TRUE
  /\ cutoffClock = 0 /\ cutoff = 2 /\ cutoffQueue = <<>>
  /\ cutoffPartial = FALSE /\ cutoffWriteAuthorized = TRUE

Spawn ==
  /\ phase = "new" /\ phase' = "owned"
  /\ privateGroup' = TRUE /\ dropPolicy' = TRUE
  /\ leaderAlive' = TRUE
  /\ UNCHANGED <<descendantAlive, stopRequested, lock, waiter, observedExit, signalVars>>

SpawnFails ==
  /\ phase = "new" /\ phase' = "failed"
  /\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,
                  stopRequested, lock, waiter, observedExit, signalVars>>

Fork ==
  /\ leaderAlive /\ ~descendantAlive /\ descendantAlive' = TRUE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  stopRequested, lock, waiter, observedExit, signalVars>>

Acquire(w) ==
  /\ phase = "owned" /\ waiter[w] = "ready" /\ lock = NoLock
  /\ lock' = w /\ waiter' = [waiter EXCEPT ![w] = "polling"]
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, observedExit, signalVars>>

Poll(w) ==
  /\ phase = "owned" /\ lock = w
  /\ lock' = NoLock
  /\ waiter' = [waiter EXCEPT ![w] = IF leaderAlive THEN "backoff" ELSE "done"]
  /\ observedExit' = IF leaderAlive THEN observedExit ELSE observedExit \cup {w}
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, signalVars>>

Wake(w) ==
  /\ phase = "owned" /\ waiter[w] = "backoff"
  /\ waiter' = [waiter EXCEPT ![w] = "ready"]
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, lock, observedExit, signalVars>>

Cancel(w) ==
  /\ phase = "owned" /\ waiter[w] \notin {"done", "cancelled"}
  /\ waiter' = [waiter EXCEPT ![w] = "cancelled"]
  /\ lock' = IF lock = w THEN NoLock ELSE lock
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, observedExit, signalVars>>

\* Unix killpg does not acquire the Child mutex or reap the leader. The kernel
\* may deny a signal before/after leader exit while a descendant remains alive.
\* Accepted delivery or exact ESRCH is success; neither is a tree-exit receipt.
Signal(outcome) ==
  /\ phase = "owned"
  /\ signalOutcome' = outcome
  /\ signalAccepted' = (outcome \in {"accepted", "absent"})
  /\ stopRequested' = (stopRequested \/ outcome = "accepted")
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, lock, waiter, observedExit>>

\* Last-handle Drop requires that there are no outstanding borrowed calls.
Drop ==
  /\ phase = "owned" /\ lock = NoLock
  /\ \A w \in Waiters : waiter[w] \in {"done", "cancelled"}
  /\ phase' = "dropped" /\ stopRequested' = dropPolicy
  /\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,
                  lock, waiter, observedExit, signalVars>>

Crash ==
  /\ phase = "owned" /\ phase' = "crashed" /\ lock' = NoLock
  /\ waiter' = [w \in Waiters |-> IF waiter[w] = "done" THEN "done" ELSE "cancelled"]
  /\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,
                  stopRequested, observedExit, signalVars>>

\* OS progress is nondeterministic, not an instant effect of signal/Drop.
LeaderExit ==
  /\ leaderAlive /\ leaderAlive' = FALSE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, descendantAlive,
                  stopRequested, lock, waiter, observedExit, signalVars>>

DescendantExit ==
  /\ descendantAlive /\ descendantAlive' = FALSE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  stopRequested, lock, waiter, observedExit, signalVars>>

Next == Spawn \/ SpawnFails \/ Fork
        \/ (\E outcome \in {"accepted", "absent", "denied"} : Signal(outcome))
        \/ Drop \/ Crash \/ LeaderExit \/ DescendantExit
        \/ \E w \in Waiters : Acquire(w) \/ Poll(w) \/ Wake(w) \/ Cancel(w)

TypeOK ==
  /\ phase \in {"new", "owned", "failed", "dropped", "crashed"}
  /\ privateGroup \in BOOLEAN /\ dropPolicy \in BOOLEAN
  /\ leaderAlive \in BOOLEAN /\ descendantAlive \in BOOLEAN
  /\ stopRequested \in BOOLEAN
  /\ lock \in Waiters \cup {NoLock}
  /\ waiter \in [Waiters -> {"ready", "polling", "backoff", "done", "cancelled"}]
  /\ observedExit \subseteq Waiters
  /\ signalOutcome \in {"none", "accepted", "absent", "denied"}
  /\ signalAccepted \in BOOLEAN

ConfiguredBeforeSpawn == phase \in {"owned", "dropped", "crashed"}
                        => privateGroup /\ dropPolicy
NoParkedWaiterOwnsLock == \A w \in Waiters : lock = w => waiter[w] = "polling"
ExitObservationIsStable == observedExit # {} => ~leaderAlive
FailureCreatesNoHandle == phase = "failed" => ~leaderAlive /\ lock = NoLock
DropRequestsTermination == phase = "dropped" => stopRequested
SignalDenialIsNotAcknowledged == signalOutcome = "denied" => ~signalAccepted
\* Guard against making the model pass by silently assuming OS crash cleanup.
CrashPreservesOSProcesses == [][(phase = "owned" /\ phase' = "crashed")
                            => UNCHANGED <<leaderAlive, descendantAlive>>]_vars

Spec == Init /\ [][Next /\ UNCHANGED <<scopeVars, boundaryVars, renewalVars>>]_<<vars, scopeVars, boundaryVars, renewalVars>>

\* Descriptor-pinned process scope: ONE owner, one launch admission and one
\* bounded workload tree. The trusted helper's pre-GO transport/OS lifetime is
\* abstracted; joining and acknowledgement precede the atomic GO/permit release.
\* Cancellation cannot authorize exec; no out-of-band migration is assumed.
\* There is no Session adoption, durable receipt or production Worker proof here.
ScopeAcquire ==
  /\ ~scopeClosed /\ ~scopePermit /\ phase = "new"
  /\ scopePermit' = TRUE
  /\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt, launchVars>>
ScopeSpawn ==
  /\ scopePermit /\ ~scopeClosed /\ Spawn
  /\ launchStage' = "bootstrap"
  /\ UNCHANGED <<scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  joined, goIssued, workloadStarted>>
ScopeJoin ==
  /\ phase = "owned" /\ leaderAlive /\ launchStage = "bootstrap"
  /\ joined' = TRUE /\ launchStage' = "joined"
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  goIssued, workloadStarted>>
ScopeAck ==
  /\ phase = "owned" /\ launchStage = "joined" /\ joined
  /\ launchStage' = "acknowledged"
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  joined, goIssued, workloadStarted>>
\* The adapter retains the same synchronous authorization guard through GO.
\* Separate transitions expose the cross-thread check/grant interleaving:
\* no-await alone is NOT mutual exclusion. Backpressure releases the guard
\* and requires a fresh authorization on retry. Wall-clock lease semantics
\* and the concrete Session adapter remain outside this bounded model.
ScopeAuthorize ==
  /\ phase = "owned" /\ launchStage = "acknowledged" /\ joined /\ scopePermit
  /\ ~authorityHeld /\ authorityLive
  /\ authorityHeld' = TRUE
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  launchVars, authorityLive, authorityAtGrant>>
ScopeRetryAuthorization ==
  /\ authorityHeld /\ authorityHeld' = FALSE
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  launchVars, authorityLive, authorityAtGrant>>
ScopeGo ==
  /\ phase = "owned" /\ launchStage = "acknowledged" /\ joined /\ scopePermit
  /\ authorityHeld /\ authorityHeld' = FALSE
  /\ authorityAtGrant' = authorityLive
  /\ UNCHANGED authorityLive
  /\ goIssued' = TRUE /\ scopePermit' = FALSE /\ launchStage' = "granted"
  /\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt, joined, workloadStarted>>
ScopeExec ==
  /\ leaderAlive /\ goIssued /\ joined /\ ~workloadStarted
  /\ workloadStarted' = TRUE
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  launchStage, joined, goIssued>>
ScopeCancelLaunch ==
  /\ scopePermit /\ ~goIssued /\ scopePermit' = FALSE
  /\ authorityHeld' = FALSE /\ UNCHANGED <<authorityLive, authorityAtGrant>>
  /\ launchStage' = "cancelled"
  /\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt,
                  joined, goIssued, workloadStarted>>
ScopeRelease ==
  /\ phase = "new" /\ scopePermit /\ scopePermit' = FALSE
  /\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt, launchVars>>
ScopeClose ==
  /\ ~scopePermit /\ scopeClosed' = TRUE
  /\ UNCHANGED <<vars, scopePermit, scopeKillAccepted, scopeReceipt, launchVars>>
ScopeKill(accepted) ==
  /\ scopeClosed /\ scopeKillAccepted' = (scopeKillAccepted \/ accepted)
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeReceipt, launchVars>>
ScopeObserveEmpty ==
  /\ scopeClosed /\ scopeKillAccepted /\ ~leaderAlive /\ ~descendantAlive
  /\ scopeReceipt' = TRUE
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, launchVars>>
ScopeCrash ==
  /\ Crash /\ scopePermit' = FALSE
  /\ authorityHeld' = FALSE /\ UNCHANGED <<authorityLive, authorityAtGrant>>
  /\ launchStage' = IF goIssued THEN launchStage ELSE "cancelled"
  /\ UNCHANGED <<scopeClosed, scopeKillAccepted, scopeReceipt, joined, goIssued, workloadStarted>>
\* A revoker uses the same mutex, not merely the same authority value.
ScopeRevokeAuthority ==
  /\ ~authorityHeld /\ authorityLive /\ authorityLive' = FALSE
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  launchVars, authorityAtGrant, authorityHeld>>
ScopeNext == ScopeAuthorize \/ ScopeRetryAuthorization \/ ScopeGo \/ ScopeRevokeAuthority
  \/ ScopeCancelLaunch \/ ScopeCrash
  \/ ((ScopeAcquire \/ ScopeSpawn \/ ScopeRelease \/ ScopeClose
  \/ ScopeJoin \/ ScopeAck \/ ScopeExec
  \/ (\E accepted \in BOOLEAN : ScopeKill(accepted)) \/ ScopeObserveEmpty
  \/ (((workloadStarted /\ Fork) \/ LeaderExit \/ DescendantExit) /\ UNCHANGED scopeVars))
      /\ UNCHANGED authorityVars)
ScopeTypeOK == TypeOK /\ scopeClosed \in BOOLEAN /\ scopePermit \in BOOLEAN
  /\ scopeKillAccepted \in BOOLEAN /\ scopeReceipt \in BOOLEAN
  /\ launchStage \in {"idle", "bootstrap", "joined", "acknowledged", "granted", "cancelled"}
  /\ joined \in BOOLEAN /\ goIssued \in BOOLEAN /\ workloadStarted \in BOOLEAN
  /\ authorityLive \in BOOLEAN /\ authorityAtGrant \in BOOLEAN /\ authorityHeld \in BOOLEAN
ScopeGrantWasAuthorized == goIssued => authorityAtGrant
ScopeCloseExcludesLaunch == scopeClosed => ~scopePermit
ScopeReceiptRequiresEmpty == scopeReceipt => ~leaderAlive /\ ~descendantAlive
ScopeReceiptRequiresAcceptedStop == scopeReceipt => scopeClosed /\ scopeKillAccepted
ScopeWorkloadRequiresGrant == workloadStarted => joined /\ goIssued
ScopeCancelNeverGrants == launchStage = "cancelled" => ~goIssued /\ ~workloadStarted
ScopePendingJoinExcludesClose == launchStage \in {"bootstrap", "joined", "acknowledged"}
  => scopePermit /\ ~scopeClosed
ScopeSpec == Init /\ [][ScopeNext /\ UNCHANGED <<boundaryVars, renewalVars>>]_<<vars, scopeVars, boundaryVars, renewalVars>>

\* Physical Linux boundary composition: reuse the admission/GO authority above,
\* adding only OS observations. No new Session lease or recovery state machine.
\* BOOTTIME expiry is abstracted as irreversible deadlineLive -> FALSE; this
\* fixed-cutoff configuration does NOT model renewal or a wall-clock guarantee.
allBoundaryVars == <<vars, scopeVars, boundaryVars>>
BoundaryInit == Init
BoundaryRunnable == initState = "live" /\ helperAlive /\ controlLive /\ deadlineLive
BoundarySpawn ==
  /\ ScopeSpawn /\ UNCHANGED authorityVars
  /\ initState' = "live" /\ helperAlive' = TRUE
  /\ UNCHANGED <<controlLive, deadlineLive>>
\* Other protocol steps keep OS ownership. Creation is handled above. Only a
\* live helper before cutoff may exec/fork; no process appears after init exit.
\* A poll receipt additionally observes init exit, not just the workload leader.
BoundaryProtocol ==
  /\ ScopeNext
  /\ ~(phase = "new" /\ phase' = "owned")
  /\ ((~workloadStarted /\ workloadStarted') => BoundaryRunnable)
  /\ ((~descendantAlive /\ descendantAlive') => BoundaryRunnable)
  /\ ((~scopeReceipt /\ scopeReceipt') => initState \in {"absent", "exited"})
  /\ UNCHANGED boundaryVars
BoundaryFailure(cause) ==
  /\ initState = "live"
  /\ CASE cause = "helper" -> /\ helperAlive /\ helperAlive' = FALSE
                             /\ UNCHANGED <<controlLive, deadlineLive>>
       [] cause = "disconnect" -> /\ controlLive /\ controlLive' = FALSE
                                 /\ UNCHANGED <<helperAlive, deadlineLive>>
       [] OTHER -> /\ deadlineLive /\ deadlineLive' = FALSE
                   /\ UNCHANGED <<helperAlive, controlLive>>
  /\ UNCHANGED <<vars, scopeVars, initState>>
BoundaryStopRequired ==
  initState = "live" /\ (phase = "crashed" \/ ~helperAlive \/ ~controlLive
    \/ ~deadlineLive \/ scopeKillAccepted \/ launchStage = "cancelled"
    \/ (workloadStarted /\ ~leaderAlive))
\* Parent death, helper failure, EOF or cutoff requests kernel teardown.
\* This step deliberately does not erase live descendants or publish a receipt.
BoundaryBeginStop ==
  /\ BoundaryStopRequired /\ initState' = "stopping"
  /\ UNCHANGED <<vars, scopeVars, helperAlive, controlLive, deadlineLive>>
\* Assumption: the kernel eventually completes init teardown and reaps members.
\* Uninterruptible I/O can postpone this indefinitely; no hard time bound is
\* proved. PID namespaces prevent in-tree forks from escaping to an ancestor.
BoundaryKernelExit ==
  /\ initState = "stopping" /\ initState' = "exited"
  /\ leaderAlive' = FALSE /\ descendantAlive' = FALSE
  /\ helperAlive' = FALSE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, stopRequested, lock, waiter,
                  observedExit, signalVars, scopeVars, controlLive, deadlineLive>>
BoundaryNext == BoundarySpawn \/ BoundaryProtocol \/ BoundaryBeginStop
  \/ BoundaryKernelExit
  \/ \E cause \in {"helper", "disconnect", "deadline"} : BoundaryFailure(cause)
BoundaryTypeOK == ScopeTypeOK
  /\ initState \in {"absent", "live", "stopping", "exited"}
  /\ helperAlive \in BOOLEAN /\ controlLive \in BOOLEAN /\ deadlineLive \in BOOLEAN
BoundaryReceiptObservesInit == scopeReceipt => initState \in {"absent", "exited"}
BoundaryExitedHasNoMembers == initState = "exited"
  => ~leaderAlive /\ ~descendantAlive /\ ~helperAlive
BoundaryExecRequiresLiveCutoff ==
  [][(~workloadStarted /\ workloadStarted') => BoundaryRunnable]_allBoundaryVars
BoundaryFailureEventuallyTerminates ==
  BoundaryStopRequired ~> (initState = "exited")
BoundarySpec == BoundaryInit /\ [][BoundaryNext /\ UNCHANGED renewalVars]_<<allBoundaryVars, renewalVars>>
  /\ WF_allBoundaryVars(BoundaryBeginStop)
  /\ WF_allBoundaryVars(BoundaryKernelExit)

\* Renewal composes the SAME launch/close/current-authority and kernel boundary
\* above, replacing only the abstract expiry event with a bounded absolute clock.
\* Causes/effects: R1 live + newer complete frame => exact incoming cutoff;
\* R2 stale => retain; R3 expiry/disconnect/partial stall => no resurrection;
\* R4 held authorization excludes revoke; R5 backpressure releases that SAME
\* guard (ScopeRetryAuthorization); R6 clock progresses even with a half-frame.
\* Queue capacity 1, deadlines 1..3, clock 0..4. A partial frame represents any
\* proper byte prefix; Rust transport tests own actual framing/short-write
\* shutdown. This is NOT a business lease model or a Rust refinement proof.
allRenewalVars == <<allBoundaryVars, renewalVars>>
RenewalAuthorize ==
  /\ workloadStarted /\ initState = "live" /\ controlLive
  /\ authorityLive /\ ~authorityHeld
  /\ authorityHeld' = TRUE
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  launchVars, authorityLive, authorityAtGrant, boundaryVars, renewalVars>>
RenewalWrite(incoming) ==
  /\ workloadStarted /\ authorityHeld /\ controlLive
  /\ Len(cutoffQueue) < 1 /\ incoming > cutoffClock
  /\ authorityHeld' = FALSE /\ cutoffWriteAuthorized' = authorityLive
  /\ cutoffQueue' = Append(cutoffQueue, incoming)
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  launchVars, authorityLive, authorityAtGrant, boundaryVars,
                  cutoffClock, cutoff, cutoffPartial>>
RenewalReadPrefix ==
  /\ initState = "live" /\ helperAlive /\ controlLive
  /\ cutoffQueue # <<>> /\ ~cutoffPartial
  /\ cutoffPartial' = TRUE
  /\ UNCHANGED <<allBoundaryVars, cutoffClock, cutoff, cutoffQueue, cutoffWriteAuthorized>>
RenewalReceive ==
  /\ initState = "live" /\ helperAlive /\ controlLive /\ deadlineLive
  /\ cutoffQueue # <<>> /\ cutoffPartial
  /\ cutoff' = IF Head(cutoffQueue) > cutoff THEN Head(cutoffQueue) ELSE cutoff
  /\ cutoffQueue' = Tail(cutoffQueue) /\ cutoffPartial' = FALSE
  /\ UNCHANGED <<allBoundaryVars, cutoffClock, cutoffWriteAuthorized>>
RenewalTick ==
  /\ cutoffClock < 4 /\ cutoffClock' = cutoffClock + 1
  /\ deadlineLive' = (cutoffClock' < cutoff)
  /\ UNCHANGED <<vars, scopeVars, initState, helperAlive, controlLive,
                  cutoff, cutoffQueue, cutoffPartial, cutoffWriteAuthorized>>
RenewalNext ==
  \/ ((BoundarySpawn \/ BoundaryProtocol \/ BoundaryBeginStop \/ BoundaryKernelExit
       \/ (\E cause \in {"helper", "disconnect"} : BoundaryFailure(cause)))
      /\ UNCHANGED renewalVars)
  \/ RenewalAuthorize \/ (\E incoming \in 1..3 : RenewalWrite(incoming))
  \/ RenewalReadPrefix \/ RenewalReceive \/ RenewalTick
RenewalTypeOK == BoundaryTypeOK
  /\ cutoffClock \in 0..4 /\ cutoff \in 1..3
  /\ cutoffQueue \in {<<>>} \cup {<<d>> : d \in 1..3}
  /\ cutoffPartial \in BOOLEAN /\ cutoffWriteAuthorized \in BOOLEAN
RenewalWritesRequireCurrentAuthority == cutoffWriteAuthorized
RenewalCutoffNeverDecreases == [][cutoff' >= cutoff]_allRenewalVars
RenewalCannotResurrect ==
  [][(cutoff' > cutoff) => deadlineLive /\ helperAlive /\ controlLive
      /\ initState = "live"]_allRenewalVars
RenewalCannotMintTransitTime ==
  [][(cutoff' > cutoff) => cutoffQueue # <<>> /\ cutoff' = Head(cutoffQueue)]_allRenewalVars
RenewalClockMatchesCutoff == deadlineLive = (cutoffClock < cutoff)
RenewalPartialCannotStallClock == <> (cutoffClock = 4)
RenewalSpec == BoundaryInit /\ [][RenewalNext]_allRenewalVars
  /\ WF_allRenewalVars(RenewalTick)
  /\ WF_allRenewalVars(BoundaryBeginStop /\ UNCHANGED renewalVars)
  /\ WF_allRenewalVars(BoundaryKernelExit /\ UNCHANGED renewalVars)
=============================================================================
