----------------------- MODULE SessionActivityKernel -----------------------
EXTENDS Naturals

\* Canonical abstract projection of the Session root's operation-to-activity
\* receipt protocol. ActivityIds are stable operation identities. The durable
\* root mutation receipt is the only mapping; exact open/settle retries stutter.
CONSTANTS ActivityIds, MaxActivityEpoch

VARIABLES
    kActivityEpoch,
    kActiveActivityEpochs,
    kSettledActivities,
    kNextActivityEpoch

kVars == <<kActivityEpoch, kActiveActivityEpochs,
           kSettledActivities, kNextActivityEpoch>>

KernelAssumptions == MaxActivityEpoch \in Nat

Init ==
    /\ kActivityEpoch = [activity \in ActivityIds |-> 0]
    /\ kActiveActivityEpochs = {}
    /\ kSettledActivities = {}
    /\ kNextActivityEpoch = 1

\* The exact form also serves root-CAS compositions: policy-only mutations may
\* advance the root revision without issuing an activity. The sole receipt
\* update accepts that later coordinate without introducing another clock.
OpenAt(activity, epoch) ==
    /\ activity \in ActivityIds
    /\ kActivityEpoch[activity] = 0
    /\ epoch \in kNextActivityEpoch..MaxActivityEpoch
    /\ kActivityEpoch' =
         [kActivityEpoch EXCEPT ![activity] = epoch]
    /\ kActiveActivityEpochs' =
         kActiveActivityEpochs \cup {epoch}
    /\ kNextActivityEpoch' = epoch + 1
    /\ UNCHANGED kSettledActivities

\* Expansion is exactly the original Next-epoch action and standalone Spec.
Open(activity) == OpenAt(activity, kNextActivityEpoch)

ReplayOpen(activity) ==
    /\ activity \in ActivityIds
    /\ kActivityEpoch[activity] > 0
    /\ UNCHANGED kVars

Settle(activity) ==
    /\ activity \in ActivityIds
    /\ kActivityEpoch[activity] \in kActiveActivityEpochs
    /\ kActiveActivityEpochs' =
         kActiveActivityEpochs \ {kActivityEpoch[activity]}
    /\ kSettledActivities' = kSettledActivities \cup {activity}
    /\ UNCHANGED <<kActivityEpoch, kNextActivityEpoch>>

ReplaySettle(activity) ==
    /\ activity \in kSettledActivities
    /\ kActivityEpoch[activity] \notin kActiveActivityEpochs
    /\ UNCHANGED kVars

Next ==
    \/ \E activity \in ActivityIds: Open(activity)
    \/ \E activity \in ActivityIds: ReplayOpen(activity)
    \/ \E activity \in ActivityIds: Settle(activity)
    \/ \E activity \in ActivityIds: ReplaySettle(activity)

Spec == Init /\ [][Next]_kVars

TypeOK ==
    /\ kActivityEpoch \in [ActivityIds -> 0..MaxActivityEpoch]
    /\ kActiveActivityEpochs \subseteq 1..MaxActivityEpoch
    /\ kSettledActivities \subseteq ActivityIds
    /\ kNextActivityEpoch \in 1..(MaxActivityEpoch + 1)

ReceiptsAreInjective ==
    \A left, right \in ActivityIds:
        kActivityEpoch[left] > 0 /\ kActivityEpoch[left] = kActivityEpoch[right]
            => left = right

ActiveEpochHasExactReceipt ==
    \A epoch \in kActiveActivityEpochs:
        \E activity \in ActivityIds: kActivityEpoch[activity] = epoch

SettledActivityHasDurableReceipt ==
    \A activity \in kSettledActivities:
        /\ kActivityEpoch[activity] > 0
        /\ kActivityEpoch[activity] \notin kActiveActivityEpochs

IssuedEpochsAreMonotonic ==
    \A activity \in ActivityIds:
        kActivityEpoch[activity] < kNextActivityEpoch

Safety ==
    /\ TypeOK
    /\ ReceiptsAreInjective
    /\ ActiveEpochHasExactReceipt
    /\ SettledActivityHasDurableReceipt
    /\ IssuedEpochsAreMonotonic

=============================================================================
