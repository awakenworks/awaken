-------------------- MODULE ServiceLifecycleProof --------------------
EXTENDS ServiceLifecycle, TLAPS

\* A follower that arrives during an active drain can only enter the waiting
\* set. It neither replaces the leader nor reports completion.
THEOREM ConcurrentShutdownWaits ==
    \A caller \in Callers:
      owner # NoCaller /\ CallShutdown(caller)
        => /\ caller \in waiting'
           /\ caller \notin returned'
           /\ owner' = owner
BY Z3T(30) DEF CallShutdown

\* No shutdown owner can return while any transferred task remains live.
THEOREM ShutdownReturnRequiresEmptyDrain ==
    \A caller \in Callers:
      FinishShutdown(caller) => draining = {}
BY DEF FinishShutdown

\* Once shutdown closes registration, no later spawn transition is possible.
THEOREM ClosedLifecycleRejectsSpawn ==
    \A task \in Tasks:
      ~accepting /\ Spawn(task) => FALSE
BY DEF Spawn

=======================================================================
