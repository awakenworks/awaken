-------------------------- MODULE WorkQueueKernel --------------------------
EXTENDS Naturals

\* Canonical parameterized Managed environment WorkQueue transition kernel.
\* Standalone model checking and Session protocol composition map these
\* variables onto their own state; neither owns a copied claim/reclaim/release
\* relation. Lease duration is one abstract store-clock tick.
CONSTANTS WorkItems, Workers, NoWorker, MaxEpoch, MaxHeartbeat, MaxTime

WorkStates == {"Queued", "Starting", "Active", "Stopping", "Stopped", "Removed"}
TerminalStates == {"Stopped", "Removed"}

KernelAssumptions ==
    /\ NoWorker \notin Workers
    /\ MaxEpoch \in Nat
    /\ MaxHeartbeat \in Nat
    /\ MaxTime \in Nat \ {0}

VARIABLES kState, kOwner, kEpoch, kHeartbeat, kExpires, kNow

kVars == <<kState, kOwner, kEpoch, kHeartbeat, kExpires, kNow>>

Init ==
    /\ kState = [i \in WorkItems |-> "Queued"]
    /\ kOwner = [i \in WorkItems |-> NoWorker]
    /\ kEpoch = [i \in WorkItems |-> 0]
    /\ kHeartbeat = [i \in WorkItems |-> 0]
    /\ kExpires = [i \in WorkItems |-> 0]
    /\ kNow = 0

NoActive == \A i \in WorkItems: kState[i] # "Active"

Claim(worker, item) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ NoActive
    /\ kState[item] = "Queued"
    /\ kEpoch[item] < MaxEpoch
    /\ kNow < MaxTime
    /\ kState' = [kState EXCEPT ![item] = "Active"]
    /\ kOwner' = [kOwner EXCEPT ![item] = worker]
    /\ kEpoch' = [kEpoch EXCEPT ![item] = @ + 1]
    /\ kHeartbeat' = [kHeartbeat EXCEPT ![item] = 0]
    /\ kExpires' = [kExpires EXCEPT ![item] = kNow + 1]
    /\ UNCHANGED kNow

\* Production performs expiry comparison and replacement claim in one store
\* transaction. A physically slow predecessor may continue outside this
\* durable state, but the incremented epoch fences all later mutations.
Reclaim(worker, item) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ kState[item] = "Active"
    /\ kExpires[item] <= kNow
    /\ kEpoch[item] < MaxEpoch
    /\ kNow < MaxTime
    /\ kState' = kState
    /\ kOwner' = [kOwner EXCEPT ![item] = worker]
    /\ kEpoch' = [kEpoch EXCEPT ![item] = @ + 1]
    /\ kHeartbeat' = [kHeartbeat EXCEPT ![item] = 0]
    /\ kExpires' = [kExpires EXCEPT ![item] = kNow + 1]
    /\ UNCHANGED kNow

Ack(worker, item) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ kOwner[item] = worker
    /\ kState[item] # "Removed"
    /\ kState' = [kState EXCEPT ![item] = IF @ = "Queued" THEN "Starting" ELSE @]
    /\ UNCHANGED <<kOwner, kEpoch, kHeartbeat, kExpires, kNow>>

Heartbeat(worker, item, expected) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ kState[item] = "Active"
    /\ kOwner[item] = worker
    /\ expected = kHeartbeat[item]
    /\ kHeartbeat[item] < MaxHeartbeat
    /\ kNow < MaxTime
    /\ kHeartbeat' = [kHeartbeat EXCEPT ![item] = @ + 1]
    /\ kExpires' = [kExpires EXCEPT ![item] = kNow + 1]
    /\ UNCHANGED <<kState, kOwner, kEpoch, kNow>>

\* Session work release consumes the complete persisted lease receipt. Owner
\* identity alone is insufficient because one Worker incarnation may reacquire
\* a higher epoch after a pause.
Stop(worker, item, expectedEpoch) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ expectedEpoch \in 0..MaxEpoch
    /\ kOwner[item] = worker
    /\ kEpoch[item] = expectedEpoch
    /\ kState[item] # "Removed"
    /\ kState' = [kState EXCEPT ![item] = "Stopped"]
    /\ kOwner' = [kOwner EXCEPT ![item] = NoWorker]
    /\ kHeartbeat' = [kHeartbeat EXCEPT ![item] = 0]
    /\ kExpires' = [kExpires EXCEPT ![item] = 0]
    /\ UNCHANGED <<kEpoch, kNow>>

RemoveEnvironment ==
    /\ \E i \in WorkItems: kState[i] # "Removed"
    /\ kState' = [i \in WorkItems |-> "Removed"]
    /\ kOwner' = [i \in WorkItems |-> NoWorker]
    /\ kHeartbeat' = [i \in WorkItems |-> 0]
    /\ kExpires' = [i \in WorkItems |-> 0]
    /\ UNCHANGED <<kEpoch, kNow>>

AdvanceTime ==
    /\ kNow < MaxTime
    /\ kNow' = kNow + 1
    /\ UNCHANGED <<kState, kOwner, kEpoch, kHeartbeat, kExpires>>

Next ==
    \/ \E worker \in Workers, item \in WorkItems: Claim(worker, item)
    \/ \E worker \in Workers, item \in WorkItems: Reclaim(worker, item)
    \/ \E worker \in Workers, item \in WorkItems: Ack(worker, item)
    \/ \E worker \in Workers, item \in WorkItems, expected \in 0..MaxHeartbeat:
         Heartbeat(worker, item, expected)
    \/ \E worker \in Workers, item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
         Stop(worker, item, expectedEpoch)
    \/ RemoveEnvironment
    \/ AdvanceTime

Spec == Init /\ [][Next]_kVars

TypeOK ==
    /\ kState \in [WorkItems -> WorkStates]
    /\ kOwner \in [WorkItems -> Workers \cup {NoWorker}]
    /\ kEpoch \in [WorkItems -> 0..MaxEpoch]
    /\ kHeartbeat \in [WorkItems -> 0..MaxHeartbeat]
    /\ kExpires \in [WorkItems -> 0..MaxTime]
    /\ kNow \in 0..MaxTime

SingleActive ==
    \A i \in WorkItems, j \in WorkItems:
        kState[i] = "Active" /\ kState[j] = "Active" => i = j

ActiveHasOneOwner ==
    \A i \in WorkItems: (kState[i] = "Active") \equiv (kOwner[i] \in Workers)

ActiveEpochIsPositive ==
    \A i \in WorkItems: kState[i] = "Active" => kEpoch[i] > 0

TerminalHasNoLeaseAuthority ==
    \A i \in WorkItems: kState[i] \in TerminalStates =>
        /\ kOwner[i] = NoWorker
        /\ kHeartbeat[i] = 0
        /\ kExpires[i] = 0

Safety ==
    /\ TypeOK
    /\ SingleActive
    /\ ActiveHasOneOwner
    /\ ActiveEpochIsPositive
    /\ TerminalHasNoLeaseAuthority

=============================================================================
