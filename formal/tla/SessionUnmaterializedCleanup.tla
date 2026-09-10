---------------- MODULE SessionUnmaterializedCleanup ----------------
EXTENDS Naturals

CONSTANT SupportsJoin
VARIABLES inFlight, physical, joined, absentObserved, completed
vars == <<inFlight, physical, joined, absentObserved, completed>>

\* The Session is already terminal and new admission is closed. Its original
\* Create is still in flight and its handle receipt can be lost. The physical
\* adapter, not Session CAS or an HTTP GET, must establish the join witness.
Init == /\ inFlight = TRUE /\ physical = FALSE /\ joined = FALSE
        /\ absentObserved = FALSE /\ completed = FALSE

FinishCreate == /\ inFlight
                /\ inFlight' = FALSE /\ physical' = TRUE
                /\ absentObserved' = FALSE
                /\ UNCHANGED <<joined, completed>>

\* Physical boundary: same-process Kubernetes zero-participant preparation now
\* implements this exact attempt closure. Unsupported topology or a replacement
\* process selects SupportsJoin=FALSE; polling or a point-in-time GET cannot
\* substitute for this action.
Join == /\ SupportsJoin /\ ~inFlight /\ ~joined
        /\ joined' = TRUE
        /\ UNCHANGED <<inFlight, physical, absentObserved, completed>>

DisposeExact == /\ joined /\ physical /\ ~completed
                /\ physical' = FALSE
                /\ UNCHANGED <<inFlight, joined, absentObserved, completed>>

ObserveAbsent == /\ ~physical /\ ~absentObserved
                 /\ absentObserved' = TRUE
                 /\ UNCHANGED <<inFlight, physical, joined, completed>>

Complete == /\ absentObserved /\ ~completed
            /\ joined
            /\ completed' = TRUE
            /\ UNCHANGED <<inFlight, physical, joined, absentObserved>>

Next == FinishCreate \/ Join \/ DisposeExact \/ ObserveAbsent \/ Complete
Spec == Init /\ [][Next]_vars /\ WF_vars(FinishCreate) /\ WF_vars(Join)
        /\ WF_vars(DisposeExact) /\ WF_vars(ObserveAbsent) /\ WF_vars(Complete)

TypeOK == /\ inFlight \in BOOLEAN /\ physical \in BOOLEAN /\ joined \in BOOLEAN
          /\ absentObserved \in BOOLEAN /\ completed \in BOOLEAN
NoUndrainedCompletion == completed => ~inFlight
NoPhysicalAfterCompletion == completed => ~physical
UnsupportedStaysPending == ~SupportsJoin => ~completed
ConditionalConvergence == SupportsJoin => <>completed
=====================================================================
