-------------------- MODULE DispatchMaintenanceScheduling --------------------
EXTENDS Naturals, FiniteSets, Sequences

\* Scheduler composition, not Dispatch state: immutable adapter ownership,
\* execution/maintenance-only entry, optional terminal timer, duplicate/lost
\* hints, periodic fallback and cancellation. No store operation is in flight
\* in this abstraction; storage completion and physical termination are NOT
\* proved. TerminalReconciliationScan separately owns bounded cursor progress.
CONSTANTS OwnsMaintenance, RunsExecution, TimerEnabled
ASSUME /\ OwnsMaintenance \in BOOLEAN /\ RunsExecution \in BOOLEAN
       /\ TimerEnabled \in BOOLEAN
VARIABLES hint, poll, timerDue, cancelled, stopped, maintained, drained, spun
vars == <<hint, poll, timerDue, cancelled, stopped, maintained, drained, spun>>
Init == /\ hint = FALSE /\ poll = TRUE /\ timerDue = TRUE
        /\ cancelled = FALSE /\ stopped = FALSE
        /\ maintained = FALSE /\ drained = FALSE /\ spun = FALSE

\* Hints can repeat or disappear. They never authorize maintenance themselves.
Hint == /\ ~cancelled /\ ~stopped /\ hint' \in BOOLEAN
        /\ UNCHANGED <<poll, timerDue, cancelled, stopped, maintained, drained, spun>>
Poll == /\ ~cancelled /\ ~stopped /\ ~poll
        /\ poll' = TRUE /\ timerDue' = TRUE
        /\ UNCHANGED <<hint, cancelled, stopped, maintained, drained, spun>>

\* The owner guard must cover both work and the selectable terminal deadline.
\* Otherwise an unsupported operation repeats, or a past deadline busy-spins
\* even though the body correctly skipped remote maintenance.
TimerReady == OwnsMaintenance /\ TimerEnabled /\ timerDue
Tick == /\ ~cancelled /\ ~stopped
        /\ (hint \/ poll \/ TimerReady)
        /\ maintained' = (maintained \/ OwnsMaintenance)
        /\ drained' = (drained \/ (RunsExecution /\ (hint \/ poll)))
        /\ spun' = (spun \/ (~OwnsMaintenance /\ ~hint /\ ~poll))
        /\ hint' = FALSE /\ poll' = FALSE
        /\ timerDue' = IF OwnsMaintenance THEN FALSE ELSE timerDue
        /\ UNCHANGED <<cancelled, stopped>>
Cancel == /\ ~cancelled /\ cancelled' = TRUE
          /\ UNCHANGED <<hint, poll, timerDue, stopped, maintained, drained, spun>>
Stop == /\ cancelled /\ ~stopped /\ stopped' = TRUE
        /\ UNCHANGED <<hint, poll, timerDue, cancelled, maintained, drained, spun>>
Next == Hint \/ Poll \/ Tick \/ Cancel \/ Stop \/ (stopped /\ UNCHANGED vars)
Spec == Init /\ [][Next]_vars /\ WF_vars(Poll) /\ WF_vars(Tick) /\ WF_vars(Stop)
TypeOK == /\ hint \in BOOLEAN /\ poll \in BOOLEAN /\ timerDue \in BOOLEAN
          /\ cancelled \in BOOLEAN /\ stopped \in BOOLEAN
          /\ maintained \in BOOLEAN /\ drained \in BOOLEAN /\ spun \in BOOLEAN
RemoteNeverMaintains == ~OwnsMaintenance => ~maintained
RemoteNeverSpins == ~spun
MaintenanceOnlyNeverDrains == ~RunsExecution => ~drained
Safety == TypeOK /\ RemoteNeverMaintains /\ RemoteNeverSpins /\ MaintenanceOnlyNeverDrains
ShutdownCompletes == cancelled ~> stopped
ExecutionProgress == RunsExecution => <>(drained \/ cancelled)
NoRemoteDrainWitness == ~(~OwnsMaintenance /\ drained)
NoOwnerMaintenanceWitness == ~(OwnsMaintenance /\ maintained)

\* Actual async-call observations refine this I/O contract in addition to the
\* atomic scheduling model above. Begin/return are recorded at the existing
\* Dispatch/WorkerResolver ports; cancel/Pending/release/stop are measured by
\* polling the real public shutdown Future, not fabricated scheduler states.
\* No liveness is inferred from an I/O port which never returns. These finite
\* traces do not establish all Tokio interleavings or Rust-to-Spec refinement.
MaintenanceOps == {"gc", "relay", "scan"}
ReturnKinds == {"ok", "error"}
IoBalance(trace, count, op) ==
    Cardinality({i \in 1..count : trace[i].operation = op /\ trace[i].kind = "begin"})
    - Cardinality({i \in 1..count : trace[i].operation = op /\ trace[i].kind \in ReturnKinds})
ActiveIo(trace, count) == {op \in MaintenanceOps : IoBalance(trace, count, op) = 1}
LastReturned(trace, count) ==
    LET indices == {i \in 1..count : trace[i].kind \in ReturnKinds}
    IN IF indices = {} THEN "none"
       ELSE trace[CHOOSE last \in indices : \A prior \in indices : prior <= last].operation
ControlCount(trace, kind) == Cardinality({i \in 1..Len(trace) : trace[i].kind = kind})

MaintenanceObservation(owns, topology, terminal, pause, fail, trace) ==
    /\ owns \in BOOLEAN /\ terminal \in BOOLEAN /\ fail \in BOOLEAN
    /\ topology \in {"service", "pool", "reconciler"}
    /\ pause \in MaintenanceOps \cup {"none"}
    /\ Len(trace) > 0
    /\ \A count \in 0..Len(trace) :
         /\ \A op \in MaintenanceOps : IoBalance(trace, count, op) \in {0, 1}
         /\ Cardinality(ActiveIo(trace, count)) <= 1
    /\ \A i \in 1..Len(trace) :
         LET event == trace[i]
             active == ActiveIo(trace, i - 1)
             previous == LastReturned(trace, i - 1)
         IN /\ event.kind \in {"begin", "ok", "error", "drain", "hint", "poll",
                               "cancel", "pending", "release", "stop"}
            /\ IF event.kind \in {"begin", "ok", "error"}
               THEN event.operation \in MaintenanceOps
               ELSE event.operation = "none"
            /\ (event.kind = "begin" =>
                  /\ owns /\ active = {}
                  /\ CASE event.operation = "gc" -> previous \in {"none", "relay", "scan"}
                       [] event.operation = "relay" -> previous = "gc"
                       [] event.operation = "scan" -> previous = "relay" /\ terminal)
            /\ (event.kind \in ReturnKinds => active = {event.operation})
            /\ (event.kind = "drain" => topology # "reconciler"
                  /\ (topology = "service" => active = {}))
            /\ (event.kind \in {"pending", "release"} =>
                  /\ active = {pause}
                  /\ \E before \in 1..(i-1) : trace[before].kind = "cancel")
            /\ (event.kind = "stop" => active = {} /\ i = Len(trace))
    /\ ControlCount(trace, "cancel") = 1
    /\ ControlCount(trace, "stop") = 1
    /\ trace[Len(trace)].kind = "stop"
    /\ IF pause = "none"
       THEN /\ ~fail /\ ControlCount(trace, "pending") = 0
            /\ ControlCount(trace, "release") = 0
            /\ (owns => \E i \in 1..Len(trace) : trace[i].kind = "begin")
            /\ (topology # "reconciler" => ControlCount(trace, "drain") > 0)
       ELSE /\ owns /\ (pause = "scan" => terminal)
            /\ ControlCount(trace, "pending") = 1
            /\ ControlCount(trace, "release") = 1
            /\ \E pending, release \in 1..Len(trace) :
                 /\ trace[pending].kind = "pending"
                 /\ trace[release].kind = "release" /\ pending < release
    /\ ControlCount(trace, "error") = IF fail THEN 1 ELSE 0
=============================================================================
