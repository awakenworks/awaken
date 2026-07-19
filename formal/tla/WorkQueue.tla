------------------------------ MODULE WorkQueue ------------------------------
EXTENDS Naturals

\* Managed self-hosted environment WorkQueue. One model instance represents one
\* environment; WorkItems are its durable rows. Lease duration is one abstract
\* clock tick. Claim/reclaim are atomic store transactions.
CONSTANTS WorkItems, Workers, NoWorker, MaxEpoch, MaxHeartbeat, MaxTime

WorkStates == {"Queued", "Starting", "Active", "Stopping", "Stopped", "Removed"}
TerminalStates == {"Stopped", "Removed"}

KernelAssumptions ==
    /\ NoWorker \notin Workers
    /\ MaxEpoch \in Nat
    /\ MaxHeartbeat \in Nat
    /\ MaxTime \in Nat \ {0}

VARIABLES state, owner, epoch, heartbeat, expires, now

vars == <<state, owner, epoch, heartbeat, expires, now>>

Init ==
    /\ state = [i \in WorkItems |-> "Queued"]
    /\ owner = [i \in WorkItems |-> NoWorker]
    /\ epoch = [i \in WorkItems |-> 0]
    /\ heartbeat = [i \in WorkItems |-> 0]
    /\ expires = [i \in WorkItems |-> 0]
    /\ now = 0

NoActive == \A i \in WorkItems: state[i] # "Active"

Claim(worker, item) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ NoActive
    /\ state[item] = "Queued"
    /\ epoch[item] < MaxEpoch
    /\ now < MaxTime
    /\ state' = [state EXCEPT ![item] = "Active"]
    /\ owner' = [owner EXCEPT ![item] = worker]
    /\ epoch' = [epoch EXCEPT ![item] = @ + 1]
    /\ heartbeat' = [heartbeat EXCEPT ![item] = 0]
    /\ expires' = [expires EXCEPT ![item] = now + 1]
    /\ UNCHANGED now

\* Production performs expiry reset and the new claim in one transaction. With
\* the single-active cap the expired row is the only possible reclaimed row.
Reclaim(worker, item) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ state[item] = "Active"
    /\ expires[item] <= now
    /\ epoch[item] < MaxEpoch
    /\ now < MaxTime
    /\ state' = state
    /\ owner' = [owner EXCEPT ![item] = worker]
    /\ epoch' = [epoch EXCEPT ![item] = @ + 1]
    /\ heartbeat' = [heartbeat EXCEPT ![item] = 0]
    /\ expires' = [expires EXCEPT ![item] = now + 1]
    /\ UNCHANGED now

Ack(item) ==
    /\ item \in WorkItems
    /\ state[item] # "Removed"
    /\ state' = [state EXCEPT ![item] = IF @ = "Queued" THEN "Starting" ELSE @]
    /\ UNCHANGED <<owner, epoch, heartbeat, expires, now>>

\* `expected` is the opaque expected_last_heartbeat receipt projected to a
\* finite revision. Only the current revision authorizes the atomic extension;
\* a duplicate first heartbeat or any stale receipt is not a transition.
Heartbeat(worker, item, expected) ==
    /\ worker \in Workers
    /\ item \in WorkItems
    /\ state[item] = "Active"
    /\ owner[item] = worker
    /\ expected = heartbeat[item]
    /\ heartbeat[item] < MaxHeartbeat
    /\ now < MaxTime
    /\ heartbeat' = [heartbeat EXCEPT ![item] = @ + 1]
    /\ expires' = [expires EXCEPT ![item] = now + 1]
    /\ UNCHANGED <<state, owner, epoch, now>>

Stop(item) ==
    /\ item \in WorkItems
    /\ state[item] # "Removed"
    /\ state' = [state EXCEPT ![item] = "Stopped"]
    /\ owner' = [owner EXCEPT ![item] = NoWorker]
    /\ heartbeat' = [heartbeat EXCEPT ![item] = 0]
    /\ expires' = [expires EXCEPT ![item] = 0]
    /\ UNCHANGED <<epoch, now>>

RemoveEnvironment ==
    /\ \E i \in WorkItems: state[i] # "Removed"
    /\ state' = [i \in WorkItems |-> "Removed"]
    /\ owner' = [i \in WorkItems |-> NoWorker]
    /\ heartbeat' = [i \in WorkItems |-> 0]
    /\ expires' = [i \in WorkItems |-> 0]
    /\ UNCHANGED <<epoch, now>>

AdvanceTime ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<state, owner, epoch, heartbeat, expires>>

Next ==
    \/ \E worker \in Workers, item \in WorkItems: Claim(worker, item)
    \/ \E worker \in Workers, item \in WorkItems: Reclaim(worker, item)
    \/ \E item \in WorkItems: Ack(item)
    \/ \E worker \in Workers, item \in WorkItems, expected \in 0..MaxHeartbeat:
         Heartbeat(worker, item, expected)
    \/ \E item \in WorkItems: Stop(item)
    \/ RemoveEnvironment
    \/ AdvanceTime

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in [WorkItems -> WorkStates]
    /\ owner \in [WorkItems -> Workers \cup {NoWorker}]
    /\ epoch \in [WorkItems -> 0..MaxEpoch]
    /\ heartbeat \in [WorkItems -> 0..MaxHeartbeat]
    /\ expires \in [WorkItems -> 0..MaxTime]
    /\ now \in 0..MaxTime

SingleActive ==
    \A i \in WorkItems, j \in WorkItems:
        state[i] = "Active" /\ state[j] = "Active" => i = j

ActiveHasOneOwner ==
    \A i \in WorkItems: (state[i] = "Active") \equiv (owner[i] \in Workers)

ActiveEpochIsPositive ==
    \A i \in WorkItems: state[i] = "Active" => epoch[i] > 0

TerminalHasNoLeaseAuthority ==
    \A i \in WorkItems: state[i] \in TerminalStates =>
        /\ owner[i] = NoWorker
        /\ heartbeat[i] = 0
        /\ expires[i] = 0

Safety ==
    /\ TypeOK
    /\ SingleActive
    /\ ActiveHasOneOwner
    /\ ActiveEpochIsPositive
    /\ TerminalHasNoLeaseAuthority

=============================================================================
