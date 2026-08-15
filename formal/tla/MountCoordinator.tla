----------------------- MODULE MountCoordinator -----------------------
EXTENDS Naturals

\* One atomic operation represents the critical section protected by the
\* production Mutex. A successful acquire either creates one new mount or adds
\* one reference to the existing mount. Factory failure, count overflow, and an
\* unknown release are rejected stutters over the published mount registry.
CONSTANTS Stores, MaxRefs, MaxEpoch

KernelAssumptions ==
    /\ MaxRefs \in Nat \ {0}
    /\ MaxEpoch \in Nat \ {0}

VARIABLES refs, mounted, mountEpoch, unmountEpoch

vars == <<refs, mounted, mountEpoch, unmountEpoch>>

Init ==
    /\ refs = [store \in Stores |-> 0]
    /\ mounted = [store \in Stores |-> FALSE]
    /\ mountEpoch = [store \in Stores |-> 0]
    /\ unmountEpoch = [store \in Stores |-> 0]

AcquireNew(store) ==
    /\ store \in Stores
    /\ refs[store] = 0
    /\ ~mounted[store]
    /\ mountEpoch[store] < MaxEpoch
    /\ refs' = [refs EXCEPT ![store] = 1]
    /\ mounted' = [mounted EXCEPT ![store] = TRUE]
    /\ mountEpoch' = [mountEpoch EXCEPT ![store] = @ + 1]
    /\ UNCHANGED unmountEpoch

AcquireShared(store) ==
    /\ store \in Stores
    /\ mounted[store]
    /\ refs[store] \in 1..(MaxRefs - 1)
    /\ refs' = [refs EXCEPT ![store] = @ + 1]
    /\ UNCHANGED <<mounted, mountEpoch, unmountEpoch>>

\* A failed factory call publishes neither a mount nor a phantom reference.
AcquireMountFailure(store) ==
    /\ store \in Stores
    /\ refs[store] = 0
    /\ UNCHANGED vars

\* checked_add rejects capacity instead of wrapping the live count.
RejectOverflow(store) ==
    /\ store \in Stores
    /\ refs[store] = MaxRefs
    /\ UNCHANGED vars

ReleaseSome(store) ==
    /\ store \in Stores
    /\ refs[store] \in 2..MaxRefs
    /\ refs' = [refs EXCEPT ![store] = @ - 1]
    /\ UNCHANGED <<mounted, mountEpoch, unmountEpoch>>

ReleaseLast(store) ==
    /\ store \in Stores
    /\ refs[store] = 1
    /\ mounted[store]
    /\ unmountEpoch[store] < MaxEpoch
    /\ refs' = [refs EXCEPT ![store] = 0]
    /\ mounted' = [mounted EXCEPT ![store] = FALSE]
    /\ unmountEpoch' = [unmountEpoch EXCEPT ![store] = @ + 1]
    /\ UNCHANGED mountEpoch

ReleaseUnknown(store) ==
    /\ store \in Stores
    /\ refs[store] = 0
    /\ UNCHANGED vars

Next ==
    \/ \E store \in Stores: AcquireNew(store)
    \/ \E store \in Stores: AcquireShared(store)
    \/ \E store \in Stores: AcquireMountFailure(store)
    \/ \E store \in Stores: RejectOverflow(store)
    \/ \E store \in Stores: ReleaseSome(store)
    \/ \E store \in Stores: ReleaseLast(store)
    \/ \E store \in Stores: ReleaseUnknown(store)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ refs \in [Stores -> 0..MaxRefs]
    /\ mounted \in [Stores -> BOOLEAN]
    /\ mountEpoch \in [Stores -> 0..MaxEpoch]
    /\ unmountEpoch \in [Stores -> 0..MaxEpoch]

MountedIffReferenced ==
    \A store \in Stores: mounted[store] \equiv refs[store] > 0

\* Every store has either no outstanding physical mount, or exactly the one
\* represented by the current positive reference count.
AtMostOneLiveMount ==
    \A store \in Stores:
        mountEpoch[store] = unmountEpoch[store] +
            (IF mounted[store] THEN 1 ELSE 0)

Safety == TypeOK /\ MountedIffReferenced /\ AtMostOneLiveMount

=======================================================================
