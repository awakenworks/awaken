-------------------------- MODULE AcpLocalBindingBatch --------------------------
EXTENDS Naturals, FiniteSets

\* One resolver map, not a credential store or heartbeat protocol. Two writers
\* each submit one exclusive key and one shared key; the initial map may already
\* contain revision 1. Revisions 1/2 and Active/Disabled are finite representatives.
\* Check and Commit are separate steps inside the SAME Rust Mutex critical section.
\* This checks a bounded abstract protocol, not Rust/Mutex refinement, remote
\* delivery, source rotation, crash recovery, or async login-probe freshness.
Writers == {1, 2}
Keys == {"a", "b", "z"}
Entry(revision, status) == [revision |-> revision, status |-> status]
Empty == Entry(0, "Active")
Own(i) == IF i = 1 THEN "a" ELSE "b"
BatchKeys(i) == {Own(i), "z"}

VARIABLES seeded, secondRevision, bindings, mutex, phase, permitted
vars == <<seeded, secondRevision, bindings, mutex, phase, permitted>>
Initial(k) == IF seeded /\ k \in {"a", "z"} THEN Entry(1, "Active") ELSE Empty
Requested(i, k) == Entry(IF k = "z" /\ i = 2 THEN secondRevision ELSE 1, "Disabled")
Compatible(i) == \A k \in BatchKeys(i):
    bindings[k].revision = 0 \/ bindings[k].revision = Requested(i, k).revision

Init ==
    /\ seeded \in BOOLEAN /\ secondRevision \in {1, 2}
    /\ bindings = [k \in Keys |-> Initial(k)]
    /\ mutex = 0 /\ phase = [i \in Writers |-> "idle"]
    /\ permitted = [i \in Writers |-> FALSE]

Check(i) ==
    /\ phase[i] = "idle" /\ mutex = 0
    /\ mutex' = i /\ phase' = [phase EXCEPT ![i] = "checked"]
    /\ permitted' = [permitted EXCEPT ![i] = Compatible(i)]
    /\ UNCHANGED <<seeded, secondRevision, bindings>>

Commit(i) ==
    /\ phase[i] = "checked"
    /\ bindings' = IF permitted[i]
        THEN [k \in Keys |-> IF k \in BatchKeys(i) THEN Requested(i, k) ELSE bindings[k]]
        ELSE bindings
    /\ phase' = [phase EXCEPT ![i] = IF permitted[i] THEN "accepted" ELSE "rejected"]
    /\ mutex' = 0
    /\ UNCHANGED <<seeded, secondRevision, permitted>>

Next == \E i \in Writers: Check(i) \/ Commit(i)
TypeOK ==
    /\ seeded \in BOOLEAN /\ secondRevision \in {1, 2}
    /\ bindings \in [Keys -> [revision : 0..2, status : {"Active", "Disabled"}]]
    /\ mutex \in 0..2
    /\ phase \in [Writers -> {"idle", "checked", "accepted", "rejected"}]
    /\ permitted \in [Writers -> BOOLEAN]
AcceptedBatchComplete == \A i \in Writers:
    phase[i] = "accepted" => (\A k \in BatchKeys(i): bindings[k] = Requested(i, k))
EveryEntryHasAcceptedProvenance == \A k \in Keys:
    bindings[k] = Initial(k) \/
    (\E i \in Writers: phase[i] = "accepted" /\ k \in BatchKeys(i)
        /\ bindings[k] = Requested(i, k))
RejectedBatchHasNoEffect == [][(\E i \in Writers:
    phase[i] # "rejected" /\ phase'[i] = "rejected") => bindings' = bindings]_vars

\* Positive reachability oracles are deliberately FALSE on useful executions;
\* the common semantic mutation runner must find each named counterexample.
NoCompatibleUnion == ~(\A i \in Writers: phase[i] = "accepted")
NoConflictingRace == ~(~seeded /\ secondRevision = 2 /\
    (\E i \in Writers: phase[i] = "accepted") /\
    (\E i \in Writers: phase[i] = "rejected"))
NoStatusUpdate == ~(seeded /\ phase[1] = "accepted" /\ bindings["a"].status = "Disabled")
Spec == Init /\ [][Next]_vars
=============================================================================
