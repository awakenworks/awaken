---------------------- MODULE LocalProcessLifecycle ----------------------
EXTENDS FiniteSets, TLC

\* One local ProcessHandle, two concurrent waiters and the signal caller.
\* Poll's OS call is synchronous; only mutex acquisition/backoff can suspend.
\* An OS termination request is distinct from observed exit. Worker death does
\* not run Rust Drop; descendant survival is intentionally a reachable state.
CONSTANTS Waiters, NoLock, SignalCaller
ASSUME /\ Waiters # {} /\ NoLock \notin Waiters
       /\ SignalCaller \notin Waiters /\ SignalCaller # NoLock

VARIABLES phase, privateGroup, dropPolicy, leaderAlive, descendantAlive,
          stopRequested, lock, waiter, observedExit
vars == <<phase, privateGroup, dropPolicy, leaderAlive, descendantAlive,
          stopRequested, lock, waiter, observedExit>>

Init ==
  /\ phase = "new" /\ privateGroup = FALSE /\ dropPolicy = FALSE
  /\ leaderAlive = FALSE /\ descendantAlive = FALSE
  /\ stopRequested = FALSE /\ lock = NoLock
  /\ waiter = [w \in Waiters |-> "ready"] /\ observedExit = {}

Spawn ==
  /\ phase = "new" /\ phase' = "owned"
  /\ privateGroup' = TRUE /\ dropPolicy' = TRUE
  /\ leaderAlive' = TRUE
  /\ UNCHANGED <<descendantAlive, stopRequested, lock, waiter, observedExit>>

SpawnFails ==
  /\ phase = "new" /\ phase' = "failed"
  /\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,
                  stopRequested, lock, waiter, observedExit>>

Fork ==
  /\ leaderAlive /\ ~descendantAlive /\ descendantAlive' = TRUE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  stopRequested, lock, waiter, observedExit>>

Acquire(w) ==
  /\ phase = "owned" /\ waiter[w] = "ready" /\ lock = NoLock
  /\ lock' = w /\ waiter' = [waiter EXCEPT ![w] = "polling"]
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, observedExit>>

Poll(w) ==
  /\ phase = "owned" /\ lock = w
  /\ lock' = NoLock
  /\ waiter' = [waiter EXCEPT ![w] = IF leaderAlive THEN "backoff" ELSE "done"]
  /\ observedExit' = IF leaderAlive THEN observedExit ELSE observedExit \cup {w}
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested>>

Wake(w) ==
  /\ phase = "owned" /\ waiter[w] = "backoff"
  /\ waiter' = [waiter EXCEPT ![w] = "ready"]
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, lock, observedExit>>

Cancel(w) ==
  /\ phase = "owned" /\ waiter[w] \notin {"done", "cancelled"}
  /\ waiter' = [waiter EXCEPT ![w] = "cancelled"]
  /\ lock' = IF lock = w THEN NoLock ELSE lock
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, observedExit>>

SignalAcquire ==
  /\ phase = "owned" /\ lock = NoLock /\ lock' = SignalCaller
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, stopRequested, waiter, observedExit>>

SignalFinish ==
  /\ phase = "owned" /\ lock = SignalCaller
  /\ lock' = NoLock /\ stopRequested' = TRUE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  descendantAlive, waiter, observedExit>>

\* Last-handle Drop requires that there are no outstanding borrowed calls.
Drop ==
  /\ phase = "owned" /\ lock = NoLock
  /\ \A w \in Waiters : waiter[w] \in {"done", "cancelled"}
  /\ phase' = "dropped" /\ stopRequested' = dropPolicy
  /\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,
                  lock, waiter, observedExit>>

Crash ==
  /\ phase = "owned" /\ phase' = "crashed" /\ lock' = NoLock
  /\ waiter' = [w \in Waiters |-> IF waiter[w] = "done" THEN "done" ELSE "cancelled"]
  /\ UNCHANGED <<privateGroup, dropPolicy, leaderAlive, descendantAlive,
                  stopRequested, observedExit>>

\* OS progress is nondeterministic, not an instant effect of signal/Drop.
LeaderExit ==
  /\ leaderAlive /\ leaderAlive' = FALSE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, descendantAlive,
                  stopRequested, lock, waiter, observedExit>>

DescendantExit ==
  /\ descendantAlive /\ descendantAlive' = FALSE
  /\ UNCHANGED <<phase, privateGroup, dropPolicy, leaderAlive,
                  stopRequested, lock, waiter, observedExit>>

Next == Spawn \/ SpawnFails \/ Fork \/ SignalAcquire \/ SignalFinish
        \/ Drop \/ Crash \/ LeaderExit \/ DescendantExit
        \/ \E w \in Waiters : Acquire(w) \/ Poll(w) \/ Wake(w) \/ Cancel(w)

TypeOK ==
  /\ phase \in {"new", "owned", "failed", "dropped", "crashed"}
  /\ privateGroup \in BOOLEAN /\ dropPolicy \in BOOLEAN
  /\ leaderAlive \in BOOLEAN /\ descendantAlive \in BOOLEAN
  /\ stopRequested \in BOOLEAN
  /\ lock \in Waiters \cup {NoLock, SignalCaller}
  /\ waiter \in [Waiters -> {"ready", "polling", "backoff", "done", "cancelled"}]
  /\ observedExit \subseteq Waiters

ConfiguredBeforeSpawn == phase \in {"owned", "dropped", "crashed"}
                        => privateGroup /\ dropPolicy
NoParkedWaiterOwnsLock == \A w \in Waiters : lock = w => waiter[w] = "polling"
ExitObservationIsStable == observedExit # {} => ~leaderAlive
FailureCreatesNoHandle == phase = "failed" => ~leaderAlive /\ lock = NoLock
DropRequestsTermination == phase = "dropped" => stopRequested
\* Guard against making the model pass by silently assuming OS crash cleanup.
CrashPreservesOSProcesses == [][(phase = "owned" /\ phase' = "crashed")
                            => UNCHANGED <<leaderAlive, descendantAlive>>]_vars

Spec == Init /\ [][Next]_vars
=============================================================================
