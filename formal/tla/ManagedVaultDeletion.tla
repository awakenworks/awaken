----------------------- MODULE ManagedVaultDeletion -----------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT Children, MaxRevision
ASSUME /\ Cardinality(Children) = 2
       /\ MaxRevision \in Nat /\ MaxRevision >= 3

ChildStates == {"Absent", "Active", "Archived", "Deleted"}

\* Two bounded child slots represent zero, one, or two real children because
\* Init may mark either slot Absent. Source and child revisions are tracked
\* independently and must remain one exact pair. A pre-fence writer may exist,
\* but after RequestDeletion it can only be rejected.
VARIABLES rootState, rootRevision, deleteOperation, deleteExpectedRevision,
          childState, sourceRevision, childRevision, frozenRevision,
          preparedWriters, preparedExpected,
          rolloutRequired, rolloutSourceFence, rolloutChildFence,
          deliveryAttempted, rolloutAdopted, rolloutAcked,
          requestCommits, completionCommits, processUp,
          staleWriterRejected, staleRootRejected, deliveryFailureSeen

vars == <<rootState, rootRevision, deleteOperation, deleteExpectedRevision,
          childState, sourceRevision, childRevision, frozenRevision,
          preparedWriters, preparedExpected,
          rolloutRequired, rolloutSourceFence, rolloutChildFence,
          deliveryAttempted, rolloutAdopted, rolloutAcked,
          requestCommits, completionCommits, processUp,
          staleWriterRejected, staleRootRejected, deliveryFailureSeen>>

ZeroRevision == [c \in Children |-> 0]

Init ==
    /\ rootState = "Active" /\ rootRevision = 1
    /\ deleteOperation = 0 /\ deleteExpectedRevision = 0
    /\ childState \in [Children -> ChildStates]
    /\ sourceRevision \in [Children -> 0..(MaxRevision - 1)]
    /\ childRevision = sourceRevision
    /\ \A c \in Children:
          /\ (childState[c] = "Absent") <=> (sourceRevision[c] = 0)
          /\ childState[c] # "Absent" => sourceRevision[c] > 0
    /\ frozenRevision = ZeroRevision
    /\ preparedWriters = {} /\ preparedExpected = ZeroRevision
    /\ rolloutRequired = {} /\ rolloutSourceFence = ZeroRevision
    /\ rolloutChildFence = ZeroRevision
    /\ deliveryAttempted = {} /\ rolloutAdopted = {} /\ rolloutAcked = {}
    /\ requestCommits = 0 /\ completionCommits = 0
    /\ processUp = TRUE /\ staleWriterRejected = FALSE
    /\ staleRootRejected = FALSE /\ deliveryFailureSeen = FALSE

PrepareChild(c) ==
    /\ processUp /\ rootState = "Active" /\ c \in Children
    /\ childState[c] \in {"Absent", "Active"} /\ c \notin preparedWriters
    /\ sourceRevision[c] < MaxRevision - 1
    /\ preparedWriters' = preparedWriters \cup {c}
    /\ preparedExpected' = [preparedExpected EXCEPT ![c] = sourceRevision[c]]
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, rolloutRequired,
                    rolloutSourceFence, rolloutChildFence, deliveryAttempted,
                    rolloutAdopted, rolloutAcked, requestCommits,
                    completionCommits, processUp, staleWriterRejected,
                    staleRootRejected, deliveryFailureSeen>>

CommitPreparedChild(c) ==
    /\ processUp /\ rootState = "Active" /\ c \in preparedWriters
    /\ preparedExpected[c] = sourceRevision[c]
    /\ sourceRevision[c] < MaxRevision - 1
    /\ sourceRevision' = [sourceRevision EXCEPT ![c] = @ + 1]
    /\ childRevision' = [childRevision EXCEPT ![c] = @ + 1]
    /\ childState' = [childState EXCEPT ![c] = "Active"]
    /\ preparedWriters' = preparedWriters \ {c}
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, frozenRevision, preparedExpected,
                    rolloutRequired, rolloutSourceFence, rolloutChildFence,
                    deliveryAttempted, rolloutAdopted, rolloutAcked,
                    requestCommits, completionCommits, processUp,
                    staleWriterRejected, staleRootRejected,
                    deliveryFailureSeen>>

RejectStalePreparedChild(c) ==
    /\ processUp /\ c \in preparedWriters
    /\ (rootState # "Active" \/ preparedExpected[c] # sourceRevision[c])
    /\ preparedWriters' = preparedWriters \ {c}
    /\ staleWriterRejected' = TRUE
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedExpected,
                    rolloutRequired, rolloutSourceFence, rolloutChildFence,
                    deliveryAttempted, rolloutAdopted, rolloutAcked,
                    requestCommits, completionCommits, processUp,
                    staleRootRejected, deliveryFailureSeen>>

UpdateRoot ==
    /\ processUp /\ rootState = "Active" /\ rootRevision < MaxRevision - 2
    /\ rootRevision' = rootRevision + 1
    /\ UNCHANGED <<rootState, deleteOperation, deleteExpectedRevision,
                    childState, sourceRevision, childRevision, frozenRevision,
                    preparedWriters, preparedExpected, rolloutRequired,
                    rolloutSourceFence, rolloutChildFence, deliveryAttempted,
                    rolloutAdopted, rolloutAcked, requestCommits,
                    completionCommits, processUp, staleWriterRejected,
                    staleRootRejected, deliveryFailureSeen>>

ArchiveRoot ==
    /\ processUp /\ rootState = "Active" /\ rootRevision < MaxRevision - 2
    /\ rootState' = "Archived" /\ rootRevision' = rootRevision + 1
    /\ UNCHANGED <<deleteOperation, deleteExpectedRevision, childState,
                    sourceRevision, childRevision, frozenRevision,
                    preparedWriters, preparedExpected, rolloutRequired,
                    rolloutSourceFence, rolloutChildFence, deliveryAttempted,
                    rolloutAdopted, rolloutAcked, requestCommits,
                    completionCommits, processUp, staleWriterRejected,
                    staleRootRejected, deliveryFailureSeen>>

RequestDeletion(expectedRevision) ==
    /\ processUp /\ rootState \in {"Active", "Archived"}
    /\ expectedRevision = rootRevision /\ rootRevision < MaxRevision - 1
    /\ rootState' = "Requested" /\ rootRevision' = rootRevision + 1
    /\ deleteOperation' = rootRevision
    /\ deleteExpectedRevision' = expectedRevision
    /\ frozenRevision' = sourceRevision /\ requestCommits' = 1
    /\ UNCHANGED <<childState, sourceRevision, childRevision,
                    preparedWriters, preparedExpected, rolloutRequired,
                    rolloutSourceFence, rolloutChildFence, deliveryAttempted,
                    rolloutAdopted, rolloutAcked, completionCommits,
                    processUp, staleWriterRejected, staleRootRejected,
                    deliveryFailureSeen>>

RejectStaleRootRequest(expectedRevision) ==
    /\ processUp /\ rootState \in {"Active", "Archived"}
    /\ expectedRevision \in 1..MaxRevision
    /\ expectedRevision # rootRevision
    /\ staleRootRejected' = TRUE
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, deliveryAttempted, rolloutAdopted,
                    rolloutAcked, requestCommits, completionCommits,
                    processUp, staleWriterRejected, deliveryFailureSeen>>

ReplayDeletionRequest(operation) ==
    /\ processUp /\ rootState \in {"Requested", "Completed"}
    /\ operation = deleteOperation /\ UNCHANGED vars

RejectConflictingReplay(operation) ==
    /\ processUp /\ rootState \in {"Requested", "Completed"}
    /\ operation \in 1..MaxRevision /\ operation # deleteOperation
    /\ staleRootRejected' = TRUE
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, deliveryAttempted, rolloutAdopted,
                    rolloutAcked, requestCommits, completionCommits,
                    processUp, staleWriterRejected, deliveryFailureSeen>>

\* Tombstone publication advances both child fences and creates the exact
\* required rollout in the same abstract repository commit.
ReconcileChild(c) ==
    /\ processUp /\ rootState = "Requested" /\ c \in Children
    /\ childState[c] \in {"Active", "Archived"}
    /\ c \notin preparedWriters /\ sourceRevision[c] < MaxRevision
    /\ LET next == sourceRevision[c] + 1
       IN /\ childState' = [childState EXCEPT ![c] = "Deleted"]
          /\ sourceRevision' = [sourceRevision EXCEPT ![c] = next]
          /\ childRevision' = [childRevision EXCEPT ![c] = next]
          /\ rolloutRequired' = rolloutRequired \cup {c}
          /\ rolloutSourceFence' = [rolloutSourceFence EXCEPT ![c] = next]
          /\ rolloutChildFence' = [rolloutChildFence EXCEPT ![c] = next]
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, frozenRevision, preparedWriters,
                    preparedExpected, deliveryAttempted, rolloutAdopted,
                    rolloutAcked, requestCommits, completionCommits,
                    processUp, staleWriterRejected, staleRootRejected,
                    deliveryFailureSeen>>

AttemptRollout(c) ==
    /\ processUp /\ rootState = "Requested" /\ c \in rolloutRequired
    /\ c \notin rolloutAcked /\ c \notin deliveryAttempted
    /\ deliveryAttempted' = deliveryAttempted \cup {c}
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, rolloutAdopted, rolloutAcked,
                    requestCommits, completionCommits, processUp,
                    staleWriterRejected, staleRootRejected,
                    deliveryFailureSeen>>

FailRollout(c) ==
    /\ c \in deliveryAttempted /\ c \notin rolloutAdopted
    /\ deliveryAttempted' = deliveryAttempted \ {c}
    /\ deliveryFailureSeen' = TRUE
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, rolloutAdopted, rolloutAcked,
                    requestCommits, completionCommits, processUp,
                    staleWriterRejected, staleRootRejected>>

AdoptRollout(c) ==
    /\ processUp /\ rootState = "Requested" /\ c \in deliveryAttempted
    /\ c \in rolloutRequired
    /\ rolloutSourceFence[c] = sourceRevision[c]
    /\ rolloutChildFence[c] = childRevision[c]
    /\ childState[c] = "Deleted"
    /\ rolloutAdopted' = rolloutAdopted \cup {c}
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, deliveryAttempted, rolloutAcked,
                    requestCommits, completionCommits, processUp,
                    staleWriterRejected, staleRootRejected,
                    deliveryFailureSeen>>

AckRollout(c) ==
    /\ processUp /\ rootState = "Requested" /\ c \in rolloutAdopted
    /\ c \in deliveryAttempted /\ c \in rolloutRequired
    /\ rolloutSourceFence[c] = sourceRevision[c]
    /\ rolloutChildFence[c] = childRevision[c]
    /\ rolloutAcked' = rolloutAcked \cup {c}
    /\ deliveryAttempted' = deliveryAttempted \ {c}
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, rolloutAdopted, requestCommits,
                    completionCommits, processUp, staleWriterRejected,
                    staleRootRejected, deliveryFailureSeen>>

AllChildrenSettled == \A c \in Children: childState[c] \in {"Absent", "Deleted"}
AllRequiredRolloutsAcked == rolloutAcked = rolloutRequired

CompleteRoot(expectedRevision, operation) ==
    /\ processUp /\ rootState = "Requested"
    /\ expectedRevision = rootRevision /\ operation = deleteOperation
    /\ rootRevision < MaxRevision
    /\ preparedWriters = {} /\ AllChildrenSettled /\ AllRequiredRolloutsAcked
    /\ rootState' = "Completed" /\ rootRevision' = rootRevision + 1
    /\ completionCommits' = 1
    /\ UNCHANGED <<deleteOperation, deleteExpectedRevision, childState,
                    sourceRevision, childRevision, frozenRevision,
                    preparedWriters, preparedExpected, rolloutRequired,
                    rolloutSourceFence, rolloutChildFence, deliveryAttempted,
                    rolloutAdopted, rolloutAcked, requestCommits, processUp,
                    staleWriterRejected, staleRootRejected,
                    deliveryFailureSeen>>

RejectStaleCompletion(expectedRevision, operation) ==
    /\ processUp /\ rootState = "Requested"
    /\ expectedRevision \in 1..MaxRevision
    /\ operation \in 1..MaxRevision
    /\ (expectedRevision # rootRevision \/ operation # deleteOperation)
    /\ staleRootRejected' = TRUE
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, deliveryAttempted, rolloutAdopted,
                    rolloutAcked, requestCommits, completionCommits,
                    processUp, staleWriterRejected, deliveryFailureSeen>>

\* In-flight attempts are process-local; exact adoption is external durable
\* truth and survives a crash before AckRollout.
Crash ==
    /\ processUp /\ processUp' = FALSE /\ deliveryAttempted' = {}
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, rolloutAdopted, rolloutAcked,
                    requestCommits, completionCommits, staleWriterRejected,
                    staleRootRejected, deliveryFailureSeen>>

Restart ==
    /\ ~processUp /\ processUp' = TRUE
    /\ UNCHANGED <<rootState, rootRevision, deleteOperation,
                    deleteExpectedRevision, childState, sourceRevision,
                    childRevision, frozenRevision, preparedWriters,
                    preparedExpected, rolloutRequired, rolloutSourceFence,
                    rolloutChildFence, deliveryAttempted, rolloutAdopted,
                    rolloutAcked, requestCommits, completionCommits,
                    staleWriterRejected, staleRootRejected,
                    deliveryFailureSeen>>

PrepareAny == \E c \in Children: PrepareChild(c)
CommitPreparedAny == \E c \in Children: CommitPreparedChild(c)
RejectPreparedAny == \E c \in Children: RejectStalePreparedChild(c)
RequestAny == \E expected \in 1..MaxRevision: RequestDeletion(expected)
RejectRequestAny == \E expected \in 1..MaxRevision: RejectStaleRootRequest(expected)
ReplayAny == \E operation \in 1..MaxRevision: ReplayDeletionRequest(operation)
RejectReplayAny == \E operation \in 1..MaxRevision: RejectConflictingReplay(operation)
ReconcileAny == \E c \in Children: ReconcileChild(c)
AttemptAny == \E c \in Children: AttemptRollout(c)
FailAny == \E c \in Children: FailRollout(c)
AdoptAny == \E c \in Children: AdoptRollout(c)
AckAny == \E c \in Children: AckRollout(c)
CompleteAny ==
    \E expected \in 1..MaxRevision, operation \in 1..MaxRevision:
        CompleteRoot(expected, operation)
RejectCompletionAny ==
    \E expected \in 1..MaxRevision, operation \in 1..MaxRevision:
        RejectStaleCompletion(expected, operation)

Next == PrepareAny \/ CommitPreparedAny \/ RejectPreparedAny
        \/ UpdateRoot \/ ArchiveRoot
        \/ RequestAny \/ RejectRequestAny \/ ReplayAny \/ RejectReplayAny
        \/ ReconcileAny \/ AttemptAny \/ FailAny \/ AdoptAny \/ AckAny
        \/ CompleteAny \/ RejectCompletionAny \/ Crash \/ Restart

TypeOK ==
    /\ rootState \in {"Active", "Archived", "Requested", "Completed"}
    /\ rootRevision \in 1..MaxRevision
    /\ deleteOperation \in 0..MaxRevision
    /\ deleteExpectedRevision \in 0..MaxRevision
    /\ childState \in [Children -> ChildStates]
    /\ sourceRevision \in [Children -> 0..MaxRevision]
    /\ childRevision \in [Children -> 0..MaxRevision]
    /\ frozenRevision \in [Children -> 0..MaxRevision]
    /\ preparedWriters \subseteq Children
    /\ preparedExpected \in [Children -> 0..MaxRevision]
    /\ rolloutRequired \subseteq Children
    /\ rolloutSourceFence \in [Children -> 0..MaxRevision]
    /\ rolloutChildFence \in [Children -> 0..MaxRevision]
    /\ deliveryAttempted \subseteq Children
    /\ rolloutAdopted \subseteq Children /\ rolloutAcked \subseteq Children
    /\ requestCommits \in 0..1 /\ completionCommits \in 0..1
    /\ processUp \in BOOLEAN /\ staleWriterRejected \in BOOLEAN
    /\ staleRootRejected \in BOOLEAN /\ deliveryFailureSeen \in BOOLEAN

AtomicChildPair == \A c \in Children: sourceRevision[c] = childRevision[c]
AbsentChildrenHaveNoFence ==
    \A c \in Children: (childState[c] = "Absent") <=> (sourceRevision[c] = 0)
RequestedRootFencesPreparedWriters ==
    rootState \in {"Requested", "Completed"}
        => \A c \in Children:
            \/ sourceRevision[c] = frozenRevision[c]
            \/ /\ c \in rolloutRequired
               /\ sourceRevision[c] = frozenRevision[c] + 1
               /\ childState[c] = "Deleted"
ExactRolloutFence ==
    \A c \in rolloutRequired:
        /\ childState[c] = "Deleted"
        /\ rolloutSourceFence[c] = sourceRevision[c]
        /\ rolloutChildFence[c] = childRevision[c]
AdoptionAndAckAreOrdered ==
    /\ rolloutAdopted \subseteq rolloutRequired
    /\ rolloutAcked \subseteq rolloutAdopted
CompletionRequiresEveryExactEffect ==
    rootState = "Completed" =>
        /\ AllChildrenSettled /\ preparedWriters = {}
        /\ AllRequiredRolloutsAcked
        /\ \A c \in rolloutAcked:
              rolloutSourceFence[c] = sourceRevision[c]
              /\ rolloutChildFence[c] = childRevision[c]
RootIdentityIsStable ==
    rootState \in {"Requested", "Completed"}
        => /\ deleteOperation = deleteExpectedRevision
           /\ deleteOperation > 0 /\ requestCommits = 1
CompletedIsAbsorbingState == rootState = "Completed" => completionCommits = 1

DeletionSafety ==
    /\ TypeOK /\ AtomicChildPair /\ AbsentChildrenHaveNoFence
    /\ RequestedRootFencesPreparedWriters /\ ExactRolloutFence
    /\ AdoptionAndAckAreOrdered /\ CompletionRequiresEveryExactEffect
    /\ RootIdentityIsStable /\ CompletedIsAbsorbingState

Spec == Init /\ [][Next]_vars

\* Conditional progress only: restart, stale-writer rejection, child retirement,
\* delivery/adoption/ack and the final exact CAS must all be scheduled fairly.
FairSpec ==
    /\ Spec
    /\ WF_vars(Restart)
    /\ SF_vars(RejectPreparedAny)
    /\ SF_vars(ReconcileAny)
    /\ SF_vars(AttemptAny)
    /\ SF_vars(AdoptAny)
    /\ SF_vars(AckAny)
    /\ SF_vars(CompleteAny)

CompletedIsAbsorbing ==
    [] (rootState = "Completed" => [] (rootState = "Completed"))

RequestedDeletionEventuallyCompletes ==
    rootState = "Requested" ~> rootState = "Completed"
=============================================================================
