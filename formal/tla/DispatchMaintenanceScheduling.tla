-------------------- MODULE DispatchMaintenanceScheduling --------------------
EXTENDS Naturals

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
=============================================================================
