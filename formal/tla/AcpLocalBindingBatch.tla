-------------------------- MODULE AcpLocalBindingBatch --------------------------
EXTENDS Naturals, FiniteSets, Sequences

\* One resolver map: two writers, three keys, revisions 1/2 and independent
\* statuses. Check, per-key writes and return retain the SAME mutex; observers
\* take it too. A later successful batch may replace an earlier status.
\* This finite protocol does not refine Rust, std::Mutex, delivery or probe IO.
Writers == {1, 2}
Keys == {"a", "b", "z"}
Statuses == {"Active", "Disabled"}
Entry(revision, status) == [revision |-> revision, status |-> status]
Empty == Entry(0, "Active")
Own(i) == IF i = 1 THEN "a" ELSE "b"
BatchKeys(i) == {Own(i), "z"}

VARIABLES seeded, secondRevision, initialStatus, requestedStatus,
          bindings, mutex, phase, permitted, before, committed,
          observed, observedOrder, observationDone
vars == <<seeded, secondRevision, initialStatus, requestedStatus,
          bindings, mutex, phase, permitted, before, committed,
          observed, observedOrder, observationDone>>
Initial(k) == IF seeded /\ k \in {"a", "z"}
    THEN Entry(1, initialStatus) ELSE Empty
InitialMap == [k \in Keys |-> Initial(k)]
Requested(i, k) == Entry(IF k = "z" /\ i = 2 THEN secondRevision ELSE 1,
                         requestedStatus[i])
CompatibleAt(state, i) == \A k \in BatchKeys(i):
    state[k].revision = 0 \/ state[k].revision = Requested(i, k).revision
Apply(state, i) == [k \in Keys |->
    IF k \in BatchKeys(i) THEN Requested(i, k) ELSE state[k]]
\* At most two distinct writers commit; replay is a specification oracle.
Replay(order) == IF Len(order) = 0 THEN InitialMap
    ELSE IF Len(order) = 1 THEN Apply(InitialMap, order[1])
    ELSE Apply(Apply(InitialMap, order[1]), order[2])

Init ==
    /\ seeded \in BOOLEAN /\ secondRevision \in {1, 2}
    /\ initialStatus \in Statuses /\ requestedStatus \in [Writers -> Statuses]
    /\ bindings = InitialMap /\ mutex = 0
    /\ phase = [i \in Writers |-> "idle"]
    /\ permitted = [i \in Writers |-> FALSE]
    /\ before = [i \in Writers |-> InitialMap] /\ committed = <<>>
    /\ observed = InitialMap /\ observedOrder = <<>> /\ observationDone = FALSE

Check(i) ==
    /\ phase[i] = "idle" /\ mutex = 0
    /\ mutex' = i /\ phase' = [phase EXCEPT ![i] = "checked"]
    /\ permitted' = [permitted EXCEPT ![i] = CompatibleAt(bindings, i)]
    /\ before' = [before EXCEPT ![i] = bindings]
    /\ UNCHANGED <<seeded, secondRevision, initialStatus, requestedStatus,
        bindings, committed, observed, observedOrder, observationDone>>

WriteFirst(i) ==
    /\ phase[i] = "checked" /\ permitted[i]
    /\ bindings' = [bindings EXCEPT ![Own(i)] = Requested(i, Own(i))]
    /\ phase' = [phase EXCEPT ![i] = "prefix_written"]
    /\ UNCHANGED <<seeded, secondRevision, initialStatus, requestedStatus,
        mutex, permitted, before, committed, observed, observedOrder, observationDone>>

WriteLast(i) ==
    /\ phase[i] = "prefix_written"
    /\ bindings' = [bindings EXCEPT !["z"] = Requested(i, "z")]
    /\ phase' = [phase EXCEPT ![i] = "ready"]
    /\ UNCHANGED <<seeded, secondRevision, initialStatus, requestedStatus,
        mutex, permitted, before, committed, observed, observedOrder, observationDone>>

Commit(i) ==
    /\ phase[i] = "ready"
    /\ phase' = [phase EXCEPT ![i] = "accepted"] /\ mutex' = 0
    /\ committed' = Append(committed, i)
    /\ UNCHANGED <<seeded, secondRevision, initialStatus, requestedStatus,
        bindings, permitted, before, observed, observedOrder, observationDone>>

Reject(i) ==
    /\ phase[i] = "checked" /\ ~permitted[i]
    /\ phase' = [phase EXCEPT ![i] = "rejected"] /\ mutex' = 0
    /\ UNCHANGED <<seeded, secondRevision, initialStatus, requestedStatus,
        bindings, permitted, before, committed, observed, observedOrder, observationDone>>

Observe ==
    /\ ~observationDone /\ mutex = 0
    /\ observed' = bindings /\ observedOrder' = committed /\ observationDone' = TRUE
    /\ UNCHANGED <<seeded, secondRevision, initialStatus, requestedStatus,
        bindings, mutex, phase, permitted, before, committed>>

Next == (\E i \in Writers:
    Check(i) \/ WriteFirst(i) \/ WriteLast(i) \/ Commit(i) \/ Reject(i)) \/ Observe
MapType == [Keys -> [revision : 0..2, status : Statuses]]
Orders == {<<>>, <<1>>, <<2>>, <<1, 2>>, <<2, 1>>}
TypeOK ==
    /\ seeded \in BOOLEAN /\ secondRevision \in {1, 2}
    /\ initialStatus \in Statuses /\ requestedStatus \in [Writers -> Statuses]
    /\ bindings \in MapType /\ before \in [Writers -> MapType]
    /\ mutex \in 0..2
    /\ phase \in [Writers -> {"idle", "checked", "prefix_written", "ready", "accepted", "rejected"}]
    /\ permitted \in [Writers -> BOOLEAN]
    /\ committed \in Orders /\ observedOrder \in Orders
    /\ observed \in MapType /\ observationDone \in BOOLEAN

VisibleStateIsSerial == mutex = 0 => bindings = Replay(committed)
ObservationIsCommittedPrefix == observed = Replay(observedOrder)
DecisionIsExact == [][\A i \in Writers:
    (phase[i] = "idle" /\ phase'[i] = "checked")
      => permitted'[i] = CompatibleAt(bindings, i)]_vars
SuccessfulCommitIsExact == [][\A i \in Writers:
    (phase[i] = "ready" /\ phase'[i] = "accepted")
      => /\ CompatibleAt(before[i], i)
         /\ bindings' = Apply(before[i], i)]_vars
RejectedBatchHasNoEffect == [][(\E i \in Writers:
    phase[i] = "checked" /\ phase'[i] = "rejected") => bindings' = bindings]_vars

\* False reachability oracles exclude vacuous safety and the old assumption
\* that a successful status value can never be replaced by another success.
NoCompatibleUnion == ~(\A i \in Writers: phase[i] = "accepted")
NoConflictingRace == ~(~seeded /\ secondRevision = 2 /\
    (\E i \in Writers: phase[i] = "accepted") /\
    (\E i \in Writers: phase[i] = "rejected"))
NoStatusUpdate == ~(seeded /\ phase[1] = "accepted" /\
    bindings["a"].status # initialStatus)
NoForwardStatusReplacement == ~(committed = <<1, 2>> /\
    requestedStatus[1] # requestedStatus[2] /\ bindings["z"] = Requested(2, "z"))
NoReverseStatusReplacement == ~(committed = <<2, 1>> /\
    requestedStatus[1] # requestedStatus[2] /\ bindings["z"] = Requested(1, "z"))
NoObserverBetweenCommits == ~(observationDone /\ Len(observedOrder) = 1 /\ Len(committed) = 2)
Spec == Init /\ [][Next]_vars
=============================================================================
