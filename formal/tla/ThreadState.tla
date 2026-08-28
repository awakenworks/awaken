----------------------------- MODULE ThreadState -----------------------------
EXTENDS Naturals, Sequences

\* Atomic ThreadCommit state-region model. Payload values are deliberately
\* erased: the production/Kani materialization selector proves Remove,
\* replacement and object-merge classification. This model owns batch
\* admission, Run-scope binding, crash-before-commit and deterministic replay.
CONSTANTS Keys, Scopes, RunScope, MaxVersion

MergePolicies == {"Disjoint", "Commutative", "Exclusive"}
Actions == {"Set", "Remove"}
Values == {"Absent", "Present"}
Outcomes == {"None", "Committed", "Rejected", "Crashed"}
Slots == Scopes \X Keys

Commands == {
    [scope |-> scope, key |-> key, merge |-> merge,
     action |-> action, runBound |-> bound]
    : scope \in Scopes, key \in Keys, merge \in MergePolicies,
      action \in Actions, bound \in BOOLEAN
}

SingleBatches == {<<command>> : command \in Commands}
PairBatches == {<<left, right>> : left \in Commands, right \in Commands}
Batches == SingleBatches \cup PairBatches
NoBatch == <<>>

Slot(command) == <<command.scope, command.key>>
IsExclusiveSet(command) ==
    command.merge = "Exclusive" /\ command.action = "Set"

Conflict(batch) ==
    Len(batch) = 2
    /\ Slot(batch[1]) = Slot(batch[2])
    /\ IsExclusiveSet(batch[1])
    /\ IsExclusiveSet(batch[2])

RunBindingValid(batch) ==
    \A index \in 1..Len(batch):
        (batch[index].scope = RunScope) => batch[index].runBound

ValidBatch(batch) == ~Conflict(batch) /\ RunBindingValid(batch)

Apply(command, materialized) ==
    [materialized EXCEPT
        ![Slot(command)] = IF command.action = "Remove" THEN "Absent" ELSE "Present"]

ApplyBatch(batch, materialized) ==
    IF Len(batch) = 1
    THEN Apply(batch[1], materialized)
    ELSE Apply(batch[2], Apply(batch[1], materialized))

VARIABLES
    materialized,
    replayed,
    version,
    prepared,
    beforeMaterialized,
    beforeVersion,
    outcome

vars == <<materialized, replayed, version, prepared,
          beforeMaterialized, beforeVersion, outcome>>

EmptyStore == [slot \in Slots |-> "Absent"]

Init ==
    /\ materialized = EmptyStore
    /\ replayed = EmptyStore
    /\ version = 0
    /\ prepared = NoBatch
    /\ beforeMaterialized = EmptyStore
    /\ beforeVersion = 0
    /\ outcome = "None"

Prepare(batch) ==
    /\ batch \in Batches
    /\ prepared = NoBatch
    /\ version < MaxVersion
    /\ prepared' = batch
    /\ beforeMaterialized' = materialized
    /\ beforeVersion' = version
    /\ outcome' = "None"
    /\ UNCHANGED <<materialized, replayed, version>>

CrashPrepared ==
    /\ prepared # NoBatch
    /\ prepared' = NoBatch
    /\ outcome' = "Crashed"
    /\ UNCHANGED <<materialized, replayed, version,
                   beforeMaterialized, beforeVersion>>

RejectInvalid ==
    /\ prepared # NoBatch
    /\ ~ValidBatch(prepared)
    /\ prepared' = NoBatch
    /\ outcome' = "Rejected"
    /\ UNCHANGED <<materialized, replayed, version,
                   beforeMaterialized, beforeVersion>>

CommitValid ==
    /\ prepared # NoBatch
    /\ ValidBatch(prepared)
    /\ materialized' = ApplyBatch(prepared, materialized)
    /\ replayed' = ApplyBatch(prepared, replayed)
    /\ version' = version + 1
    /\ prepared' = NoBatch
    /\ outcome' = "Committed"
    /\ UNCHANGED <<beforeMaterialized, beforeVersion>>

Next ==
    \/ \E batch \in Batches: Prepare(batch)
    \/ CrashPrepared
    \/ RejectInvalid
    \/ CommitValid

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ materialized \in [Slots -> Values]
    /\ replayed \in [Slots -> Values]
    /\ version \in 0..MaxVersion
    /\ prepared \in Batches \cup {NoBatch}
    /\ beforeMaterialized \in [Slots -> Values]
    /\ beforeVersion \in 0..MaxVersion
    /\ outcome \in Outcomes

ReplayMatchesMaterialized == replayed = materialized

RejectedAndCrashAreAtomicStutters ==
    outcome \in {"Rejected", "Crashed"} =>
        /\ materialized = beforeMaterialized
        /\ replayed = beforeMaterialized
        /\ version = beforeVersion

PreparedStateIsInvisible ==
    prepared # NoBatch =>
        /\ materialized = beforeMaterialized
        /\ replayed = beforeMaterialized
        /\ version = beforeVersion

CommittedVersionAdvancesExactlyOnce ==
    outcome = "Committed" => version = beforeVersion + 1

Safety ==
    /\ TypeOK
    /\ ReplayMatchesMaterialized
    /\ RejectedAndCrashAreAtomicStutters
    /\ PreparedStateIsInvisible
    /\ CommittedVersionAdvancesExactlyOnce

=============================================================================
