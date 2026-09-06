---------------------- MODULE LocalProcessLifecycle ----------------------
EXTENDS FiniteSets, TLC

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
launchVars == <<launchStage, joined, goIssued, workloadStarted>>
scopeVars == <<scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt, launchVars>>
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

Spec == Init /\ [][Next /\ UNCHANGED scopeVars]_<<vars, scopeVars>>

\* Descriptor-pinned cgroup adapter: ONE owner, one launch admission and one
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
ScopeGo ==
  /\ phase = "owned" /\ launchStage = "acknowledged" /\ joined /\ scopePermit
  /\ goIssued' = TRUE /\ scopePermit' = FALSE /\ launchStage' = "granted"
  /\ UNCHANGED <<vars, scopeClosed, scopeKillAccepted, scopeReceipt, joined, workloadStarted>>
ScopeExec ==
  /\ leaderAlive /\ goIssued /\ joined /\ ~workloadStarted
  /\ workloadStarted' = TRUE
  /\ UNCHANGED <<vars, scopeClosed, scopePermit, scopeKillAccepted, scopeReceipt,
                  launchStage, joined, goIssued>>
ScopeCancelLaunch ==
  /\ scopePermit /\ ~goIssued /\ scopePermit' = FALSE
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
  /\ launchStage' = IF goIssued THEN launchStage ELSE "cancelled"
  /\ UNCHANGED <<scopeClosed, scopeKillAccepted, scopeReceipt, joined, goIssued, workloadStarted>>
ScopeNext == ScopeAcquire \/ ScopeSpawn \/ ScopeRelease \/ ScopeClose
  \/ ScopeJoin \/ ScopeAck \/ ScopeGo \/ ScopeExec \/ ScopeCancelLaunch \/ ScopeCrash
  \/ (\E accepted \in BOOLEAN : ScopeKill(accepted)) \/ ScopeObserveEmpty
  \/ (((workloadStarted /\ Fork) \/ LeaderExit \/ DescendantExit) /\ UNCHANGED scopeVars)
ScopeTypeOK == TypeOK /\ scopeClosed \in BOOLEAN /\ scopePermit \in BOOLEAN
  /\ scopeKillAccepted \in BOOLEAN /\ scopeReceipt \in BOOLEAN
  /\ launchStage \in {"idle", "bootstrap", "joined", "acknowledged", "granted", "cancelled"}
  /\ joined \in BOOLEAN /\ goIssued \in BOOLEAN /\ workloadStarted \in BOOLEAN
ScopeCloseExcludesLaunch == scopeClosed => ~scopePermit
ScopeReceiptRequiresEmpty == scopeReceipt => ~leaderAlive /\ ~descendantAlive
ScopeReceiptRequiresAcceptedStop == scopeReceipt => scopeClosed /\ scopeKillAccepted
ScopeWorkloadRequiresGrant == workloadStarted => joined /\ goIssued
ScopeCancelNeverGrants == launchStage = "cancelled" => ~goIssued /\ ~workloadStarted
ScopePendingJoinExcludesClose == launchStage \in {"bootstrap", "joined", "acknowledged"}
  => scopePermit /\ ~scopeClosed
ScopeSpec == Init /\ [][ScopeNext]_<<vars, scopeVars>>
=============================================================================
