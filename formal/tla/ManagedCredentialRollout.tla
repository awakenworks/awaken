------------------ MODULE ManagedCredentialRollout ------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT MaxRevision
ASSUME MaxRevision \in Nat /\ MaxRevision >= 3

Lifecycles == {"Active", "Archived", "Deleted"}
Operations == {"Update", "Archive", "Delete"}
EventSet == [revision: 2..MaxRevision, lifecycle: Lifecycles]

VARIABLE sourceRevision, childRevision, lifecycle,
         pendingOperation, pendingExpectedSource, pendingExpectedChild,
         committed, outbox, adopted, acknowledged,
         deliveryPhase, deliveryEvent,
         serviceRevision, serviceLifecycle, serviceBusy, processUp,
         staleWriterRejected, failedDeliverySeen, duplicateDeliverySeen

vars == <<sourceRevision, childRevision, lifecycle,
          pendingOperation, pendingExpectedSource, pendingExpectedChild,
          committed, outbox, adopted, acknowledged,
          deliveryPhase, deliveryEvent,
          serviceRevision, serviceLifecycle, serviceBusy, processUp,
          staleWriterRejected, failedDeliverySeen, duplicateDeliverySeen>>

NoEvent == [revision |-> 1, lifecycle |-> "Active"]

Init == /\ sourceRevision = 1 /\ childRevision = 1
        /\ lifecycle = "Active" /\ pendingOperation = "None"
        /\ pendingExpectedSource = 1 /\ pendingExpectedChild = 1
        /\ committed = {} /\ outbox = {} /\ adopted = {} /\ acknowledged = {}
        /\ deliveryPhase = "None" /\ deliveryEvent = NoEvent
        /\ serviceRevision = 1 /\ serviceLifecycle = "Active"
        /\ serviceBusy = FALSE /\ processUp = TRUE
        /\ staleWriterRejected = FALSE /\ failedDeliverySeen = FALSE
        /\ duplicateDeliverySeen = FALSE

LifecycleAllows(op) ==
    \/ lifecycle = "Active" /\ op \in Operations
    \/ lifecycle = "Archived" /\ op = "Delete"

Begin(op, expectedSource, expectedChild) ==
    /\ processUp /\ pendingOperation = "None" /\ op \in Operations
    /\ LifecycleAllows(op)
    /\ expectedSource \in 1..MaxRevision /\ expectedChild \in 1..MaxRevision
    /\ pendingOperation' = op
    /\ pendingExpectedSource' = expectedSource
    /\ pendingExpectedChild' = expectedChild
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle, committed,
                    outbox, adopted, acknowledged, deliveryPhase,
                    deliveryEvent, serviceRevision, serviceLifecycle,
                    serviceBusy, processUp, staleWriterRejected,
                    failedDeliverySeen, duplicateDeliverySeen>>

NextLifecycle ==
    CASE pendingOperation = "Update" -> "Active"
      [] pendingOperation = "Archive" -> "Archived"
      [] pendingOperation = "Delete" -> "Deleted"

Commit ==
    /\ processUp /\ pendingOperation # "None"
    /\ pendingExpectedSource = sourceRevision
    /\ pendingExpectedChild = childRevision
    /\ sourceRevision < MaxRevision /\ childRevision < MaxRevision
    /\ LET event == [revision |-> sourceRevision + 1, lifecycle |-> NextLifecycle]
       IN /\ sourceRevision' = sourceRevision + 1
          /\ childRevision' = childRevision + 1
          /\ lifecycle' = NextLifecycle
          /\ committed' = committed \cup {event}
          /\ outbox' = outbox \cup {event}
    /\ pendingOperation' = "None"
    /\ UNCHANGED <<pendingExpectedSource, pendingExpectedChild, adopted,
                    acknowledged, deliveryPhase, deliveryEvent,
                    serviceRevision, serviceLifecycle, serviceBusy,
                    processUp, staleWriterRejected, failedDeliverySeen,
                    duplicateDeliverySeen>>

RejectStaleWriter ==
    /\ processUp /\ pendingOperation # "None"
    /\ (pendingExpectedSource # sourceRevision
        \/ pendingExpectedChild # childRevision
        \/ sourceRevision = MaxRevision
        \/ childRevision = MaxRevision)
    /\ pendingOperation' = "None" /\ staleWriterRejected' = TRUE
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingExpectedSource, pendingExpectedChild, committed,
                    outbox, adopted, acknowledged, deliveryPhase,
                    deliveryEvent, serviceRevision, serviceLifecycle,
                    serviceBusy, processUp, failedDeliverySeen,
                    duplicateDeliverySeen>>

SetBusy(value) ==
    /\ value \in BOOLEAN /\ serviceBusy' = value
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, deliveryPhase, deliveryEvent,
                    serviceRevision, serviceLifecycle, processUp,
                    staleWriterRejected, failedDeliverySeen,
                    duplicateDeliverySeen>>

BecomeAvailable == /\ processUp /\ SetBusy(FALSE)

AttemptDelivery(event) ==
    /\ processUp /\ ~serviceBusy /\ deliveryPhase = "None"
    /\ event \in outbox
    /\ deliveryPhase' = "Attempted" /\ deliveryEvent' = event
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, serviceRevision, serviceLifecycle,
                    serviceBusy, processUp, staleWriterRejected,
                    failedDeliverySeen, duplicateDeliverySeen>>

DeliveryFailed ==
    /\ deliveryPhase = "Attempted"
    /\ deliveryPhase' = "None" /\ deliveryEvent' = NoEvent
    /\ failedDeliverySeen' = TRUE
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, serviceRevision, serviceLifecycle,
                    serviceBusy, processUp, staleWriterRejected,
                    duplicateDeliverySeen>>

\* A stale event reselects current durable truth. The event is remembered as
\* adopted only after the service carries an equal-or-newer exact pair fence.
AdoptDelivery ==
    /\ processUp /\ ~serviceBusy /\ deliveryPhase = "Attempted"
    /\ deliveryEvent \in outbox
    /\ deliveryPhase' = "Adopted"
    /\ serviceRevision' = sourceRevision /\ serviceLifecycle' = lifecycle
    /\ adopted' = adopted \cup {deliveryEvent}
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, acknowledged,
                    deliveryEvent, serviceBusy, processUp,
                    staleWriterRejected, failedDeliverySeen,
                    duplicateDeliverySeen>>

AckCommit ==
    /\ processUp /\ deliveryPhase = "Adopted"
    /\ deliveryEvent \in outbox /\ deliveryEvent \in adopted
    /\ serviceRevision >= deliveryEvent.revision
    /\ acknowledged' = acknowledged \cup {deliveryEvent}
    /\ outbox' = outbox \ {deliveryEvent}
    /\ deliveryPhase' = "None" /\ deliveryEvent' = NoEvent
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, adopted,
                    serviceRevision, serviceLifecycle, serviceBusy,
                    processUp, staleWriterRejected, failedDeliverySeen,
                    duplicateDeliverySeen>>

\* A response lost after AckCommit may be delivered again by an upstream
\* sender. It reselects current truth but cannot recreate or remove an outbox row.
DuplicateDelivery(event) ==
    /\ processUp /\ ~serviceBusy /\ event \in acknowledged
    /\ serviceRevision' = sourceRevision /\ serviceLifecycle' = lifecycle
    /\ duplicateDeliverySeen' = TRUE
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, deliveryPhase, deliveryEvent, serviceBusy,
                    processUp, staleWriterRejected, failedDeliverySeen>>

\* Crash after adoption but before the local ack forgets only the in-flight
\* delivery cursor. Durable service adoption and the outbox event both survive,
\* so restart retries the same event.
Crash ==
    /\ processUp /\ processUp' = FALSE
    /\ deliveryPhase' = "None" /\ deliveryEvent' = NoEvent
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, serviceRevision, serviceLifecycle,
                    serviceBusy, staleWriterRejected, failedDeliverySeen,
                    duplicateDeliverySeen>>

Restart ==
    /\ ~processUp /\ processUp' = TRUE
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, deliveryPhase, deliveryEvent,
                    serviceRevision, serviceLifecycle, serviceBusy,
                    staleWriterRejected, failedDeliverySeen,
                    duplicateDeliverySeen>>

BeginAny ==
    \E op \in Operations, source \in 1..MaxRevision, child \in 1..MaxRevision:
        Begin(op, source, child)
AttemptAny == \E event \in EventSet: AttemptDelivery(event)
DuplicateAny == \E event \in EventSet: DuplicateDelivery(event)

Next == BeginAny \/ Commit \/ RejectStaleWriter
        \/ (\E value \in BOOLEAN: SetBusy(value))
        \/ AttemptAny \/ DeliveryFailed \/ AdoptDelivery \/ AckCommit
        \/ DuplicateAny \/ Crash \/ Restart

TypeOK == /\ sourceRevision \in 1..MaxRevision
          /\ childRevision \in 1..MaxRevision
          /\ lifecycle \in Lifecycles
          /\ pendingOperation \in Operations \cup {"None"}
          /\ pendingExpectedSource \in 1..MaxRevision
          /\ pendingExpectedChild \in 1..MaxRevision
          /\ committed \subseteq EventSet /\ outbox \subseteq EventSet
          /\ adopted \subseteq EventSet /\ acknowledged \subseteq EventSet
          /\ deliveryPhase \in {"None", "Attempted", "Adopted"}
          /\ deliveryEvent \in EventSet \cup {NoEvent}
          /\ serviceRevision \in 1..MaxRevision
          /\ serviceLifecycle \in Lifecycles
          /\ serviceBusy \in BOOLEAN /\ processUp \in BOOLEAN
          /\ staleWriterRejected \in BOOLEAN /\ failedDeliverySeen \in BOOLEAN
          /\ duplicateDeliverySeen \in BOOLEAN

AtomicPairFence == sourceRevision = childRevision
OutboxOnlyNamesCommittedFences == outbox \subseteq committed
AcknowledgedOnlyNamesCommittedFences == acknowledged \subseteq committed
AcknowledgedOnlyAfterAdoption == acknowledged \subseteq adopted
AdoptionCoversEveryNamedFence ==
    \A event \in adopted: event.revision <= serviceRevision
InFlightDeliveryIsDurable ==
    deliveryPhase \in {"Attempted", "Adopted"} => deliveryEvent \in outbox
ServiceNeverRunsAheadOfTruth == serviceRevision <= sourceRevision
DeletedIsAbsorbing == lifecycle = "Deleted" => pendingOperation = "None"
ArchivedOnlyTransitionsToDelete ==
    lifecycle = "Archived" /\ pendingOperation # "None"
        => pendingOperation = "Delete"
RevokedServiceNeverLeadsAuthority ==
    serviceLifecycle = "Deleted" => lifecycle = "Deleted"

RolloutSafety ==
    /\ TypeOK /\ AtomicPairFence /\ OutboxOnlyNamesCommittedFences
    /\ AcknowledgedOnlyNamesCommittedFences /\ AcknowledgedOnlyAfterAdoption
    /\ AdoptionCoversEveryNamedFence /\ InFlightDeliveryIsDurable
    /\ ServiceNeverRunsAheadOfTruth /\ DeletedIsAbsorbing
    /\ ArchivedOnlyTransitionsToDelete /\ RevokedServiceNeverLeadsAuthority

Spec == Init /\ [][Next]_vars

\* Conditional progress: the process restarts, a busy target becomes available,
\* and commit/delivery/adoption/ack work that remains enabled is retried fairly.
FairSpec ==
    /\ Spec
    /\ WF_vars(Restart)
    /\ SF_vars(Commit)
    /\ SF_vars(RejectStaleWriter)
    /\ SF_vars(BecomeAvailable)
    /\ SF_vars(AttemptAny)
    /\ SF_vars(AdoptDelivery)
    /\ SF_vars(AckCommit)

EveryCommittedEventEventuallyAcknowledged ==
    \A event \in EventSet: event \in outbox ~> event \in acknowledged
=====================================================================
