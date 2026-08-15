----------------------- MODULE ServiceLifecycle -----------------------
EXTENDS FiniteSets, TLC

CONSTANTS Tasks, Callers, NoCaller

VARIABLE accepting, spawned, registered, owner, draining,
         stopped, waiting, returned, timedOut

vars == <<accepting, spawned, registered, owner, draining,
          stopped, waiting, returned, timedOut>>

Init ==
    /\ accepting = TRUE
    /\ spawned = {}
    /\ registered = {}
    /\ owner = NoCaller
    /\ draining = {}
    /\ stopped = {}
    /\ waiting = {}
    /\ returned = {}
    /\ timedOut = {}

Spawn(task) ==
    /\ accepting
    /\ task \in Tasks \ spawned
    /\ spawned' = spawned \cup {task}
    /\ registered' = registered \cup {task}
    /\ UNCHANGED <<accepting, owner, draining, stopped,
                    waiting, returned, timedOut>>

\* The async shutdown guard is the admission point. The first caller becomes
\* drain owner and atomically transfers the registry. Every concurrent caller
\* only waits; it cannot inspect the now-empty registry and return early.
CallShutdown(caller) ==
    /\ caller \in Callers
    /\ caller # owner
    /\ caller \notin waiting \cup returned
    /\ IF owner = NoCaller
          THEN /\ accepting' = FALSE
               /\ owner' = caller
               /\ draining' = registered
               /\ registered' = {}
               /\ UNCHANGED waiting
          ELSE /\ waiting' = waiting \cup {caller}
               /\ UNCHANGED <<accepting, owner, draining, registered>>
    /\ UNCHANGED <<spawned, stopped, returned, timedOut>>

AcquireWaiting(caller) ==
    /\ owner = NoCaller
    /\ caller \in waiting
    /\ owner' = caller
    /\ waiting' = waiting \ {caller}
    /\ accepting' = FALSE
    /\ draining' = registered
    /\ registered' = {}
    /\ UNCHANGED <<spawned, stopped, returned, timedOut>>

TaskStops(task) ==
    /\ owner # NoCaller
    /\ task \in draining
    /\ draining' = draining \ {task}
    /\ stopped' = stopped \cup {task}
    /\ UNCHANGED <<accepting, spawned, registered, owner,
                    waiting, returned, timedOut>>

Timeout(task) ==
    /\ owner # NoCaller
    /\ task \in draining
    /\ draining' = draining \ {task}
    /\ stopped' = stopped \cup {task}
    /\ timedOut' = timedOut \cup {task}
    /\ UNCHANGED <<accepting, spawned, registered, owner,
                    waiting, returned>>

FinishShutdown(caller) ==
    /\ owner = caller
    /\ caller \in Callers
    /\ draining = {}
    /\ owner' = NoCaller
    /\ returned' = returned \cup {caller}
    /\ UNCHANGED <<accepting, spawned, registered, draining,
                    stopped, waiting, timedOut>>

BoundedStutter ==
    /\ ~accepting
    /\ owner = NoCaller
    /\ waiting = {}
    /\ UNCHANGED vars

Next ==
    \/ \E task \in Tasks: Spawn(task)
    \/ \E caller \in Callers: CallShutdown(caller)
    \/ \E caller \in Callers: AcquireWaiting(caller)
    \/ \E task \in Tasks: TaskStops(task) \/ Timeout(task)
    \/ \E caller \in Callers: FinishShutdown(caller)
    \/ BoundedStutter

TypeOK ==
    /\ accepting \in BOOLEAN
    /\ spawned \subseteq Tasks
    /\ registered \subseteq Tasks
    /\ draining \subseteq Tasks
    /\ stopped \subseteq Tasks
    /\ timedOut \subseteq Tasks
    /\ owner \in Callers \cup {NoCaller}
    /\ waiting \subseteq Callers
    /\ returned \subseteq Callers

TaskPartition ==
    /\ spawned = registered \cup draining \cup stopped
    /\ registered \cap draining = {}
    /\ registered \cap stopped = {}
    /\ draining \cap stopped = {}

ShutdownFencesRegistration == ~accepting => registered = {}
DrainHasOneOwner == draining # {} => owner # NoCaller
OwnerCannotWaitOrReturn == owner # NoCaller => owner \notin waiting \cup returned
WaitingCannotReturn == waiting \cap returned = {}
TimedOutTaskIsStopped == timedOut \subseteq stopped

Safety ==
    /\ TypeOK
    /\ TaskPartition
    /\ ShutdownFencesRegistration
    /\ DrainHasOneOwner
    /\ OwnerCannotWaitOrReturn
    /\ WaitingCannotReturn
    /\ TimedOutTaskIsStopped

Spec == Init /\ [][Next]_vars
=======================================================================
