------------------ MODULE ManagedCredentialRollout ------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT MaxRevision
ASSUME MaxRevision \in Nat /\ MaxRevision >= 3

Targets == {1, 2}

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
        /\ serviceRevision = [t \in Targets |-> 1] /\ serviceLifecycle = [t \in Targets |-> "Active"]
        /\ serviceBusy = {} /\ processUp = TRUE
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

\* Idle writer coordinates are unobservable. Canonicalizing them after the
\* command closes removes equivalent TLC states without restricting commands.
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
    /\ pendingExpectedSource' = 1 /\ pendingExpectedChild' = 1
    /\ UNCHANGED <<adopted,
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
    /\ pendingOperation' = "None"
    /\ pendingExpectedSource' = 1 /\ pendingExpectedChild' = 1 /\ staleWriterRejected' = TRUE
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    committed,
                    outbox, adopted, acknowledged, deliveryPhase,
                    deliveryEvent, serviceRevision, serviceLifecycle,
                    serviceBusy, processUp, failedDeliverySeen,
                    duplicateDeliverySeen>>

SetBusy(target, value) ==
    /\ target \in Targets /\ value \in BOOLEAN
    /\ serviceBusy' = IF value THEN serviceBusy \cup {target} ELSE serviceBusy \ {target}
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, deliveryPhase, deliveryEvent,
                    serviceRevision, serviceLifecycle, processUp,
                    staleWriterRejected, failedDeliverySeen,
                    duplicateDeliverySeen>>

BecomeAvailable(target) == /\ processUp /\ SetBusy(target, FALSE)

AttemptDelivery(event) ==
    /\ processUp /\ deliveryPhase = "None"
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

\* Session adoption is independent per target. One busy/failed target never
\* prevents its sibling from adopting, and only the complete target set can
\* authorize ACK. This is a bounded consumer set, not Session query refinement.
AdoptTarget(target) ==
    /\ processUp /\ target \in Targets \ serviceBusy
    /\ deliveryPhase = "Attempted" /\ deliveryEvent \in outbox
    /\ serviceRevision[target] < sourceRevision
    /\ serviceRevision' = [serviceRevision EXCEPT ![target] = sourceRevision]
    /\ serviceLifecycle' = [serviceLifecycle EXCEPT ![target] = lifecycle]
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource, pendingExpectedChild,
                    committed, outbox, acknowledged, adopted, deliveryPhase,
                    deliveryEvent, serviceBusy, processUp,
                    staleWriterRejected, failedDeliverySeen, duplicateDeliverySeen>>

AdoptDelivery ==
    /\ processUp /\ deliveryPhase = "Attempted"
    /\ deliveryEvent \in outbox
    /\ \A target \in Targets: serviceRevision[target] >= deliveryEvent.revision
    /\ deliveryPhase' = "Adopted"
    /\ adopted' = adopted \cup {deliveryEvent}
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource, pendingExpectedChild,
                    committed, outbox, acknowledged, serviceRevision, serviceLifecycle,
                    deliveryEvent, serviceBusy, processUp,
                    staleWriterRejected, failedDeliverySeen, duplicateDeliverySeen>>

AckCommit ==
    /\ processUp /\ deliveryPhase = "Adopted"
    /\ deliveryEvent \in outbox /\ deliveryEvent \in adopted
    /\ \A target \in Targets: serviceRevision[target] >= deliveryEvent.revision
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
    /\ processUp /\ event \in acknowledged
    /\ duplicateDeliverySeen' = TRUE
    /\ UNCHANGED <<sourceRevision, childRevision, lifecycle,
                    pendingOperation, pendingExpectedSource,
                    pendingExpectedChild, committed, outbox, adopted,
                    acknowledged, deliveryPhase, deliveryEvent, serviceBusy,
                    processUp, staleWriterRejected, failedDeliverySeen,
                    serviceRevision, serviceLifecycle>>

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
        \/ (\E target \in Targets, value \in BOOLEAN: SetBusy(target, value))
        \/ (\E target \in Targets: AdoptTarget(target))
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
          /\ serviceRevision \in [Targets -> 1..MaxRevision]
          /\ serviceLifecycle \in [Targets -> Lifecycles]
          /\ serviceBusy \subseteq Targets /\ processUp \in BOOLEAN
          /\ staleWriterRejected \in BOOLEAN /\ failedDeliverySeen \in BOOLEAN
          /\ duplicateDeliverySeen \in BOOLEAN

CompleteOutboxAccounting == committed = outbox \cup acknowledged

AtomicPairFence == sourceRevision = childRevision
OutboxOnlyNamesCommittedFences == outbox \subseteq committed
AcknowledgedOnlyNamesCommittedFences == acknowledged \subseteq committed
AcknowledgedOnlyAfterAdoption == acknowledged \subseteq adopted
AdoptionCoversEveryNamedFence ==
    \A event \in adopted, target \in Targets: event.revision <= serviceRevision[target]
InFlightDeliveryIsDurable ==
    deliveryPhase \in {"Attempted", "Adopted"} => deliveryEvent \in outbox
ServiceNeverRunsAheadOfTruth == \A target \in Targets: serviceRevision[target] <= sourceRevision
DeletedIsAbsorbing == lifecycle = "Deleted" => pendingOperation = "None"
ArchivedOnlyTransitionsToDelete ==
    lifecycle = "Archived" /\ pendingOperation # "None"
        => pendingOperation = "Delete"
RevokedServiceNeverLeadsAuthority ==
    \A target \in Targets: serviceLifecycle[target] = "Deleted" => lifecycle = "Deleted"

RolloutSafety ==
    /\ TypeOK /\ CompleteOutboxAccounting /\ AtomicPairFence /\ OutboxOnlyNamesCommittedFences
    /\ AcknowledgedOnlyNamesCommittedFences /\ AcknowledgedOnlyAfterAdoption
    /\ AdoptionCoversEveryNamedFence /\ InFlightDeliveryIsDurable
    /\ ServiceNeverRunsAheadOfTruth /\ DeletedIsAbsorbing
    /\ ArchivedOnlyTransitionsToDelete /\ RevokedServiceNeverLeadsAuthority

Spec == Init /\ [][Next]_vars

\* Conditional progress: the process and every target eventually stay available.
\* Intermittent availability alone need not overlap delivery; it is insufficient.
\* Under that explicit environmental assumption,
\* and commit/delivery/adoption/ack work that remains enabled is retried fairly.
FairSpec ==
    /\ Spec
    /\ <>[](processUp /\ serviceBusy = {})
    /\ WF_vars(Restart)
    /\ SF_vars(Commit)
    /\ SF_vars(RejectStaleWriter)
    /\ \A target \in Targets: SF_vars(BecomeAvailable(target))
    /\ \A target \in Targets: SF_vars(AdoptTarget(target))
    /\ SF_vars(AttemptAny)
    /\ SF_vars(AdoptDelivery)
    /\ SF_vars(AckCommit)

\* Commit increases a revision bounded by MaxRevision, so only finitely many
\* events can be added. Complete drain after every nonempty observation is
\* equivalent to per-event eventual ACK under CompleteOutboxAccounting. This
\* avoids one redundant temporal product graph for each possible event value.
EveryCommittedEventEventuallyAcknowledged == outbox # {} ~> outbox = {}
=====================================================================
