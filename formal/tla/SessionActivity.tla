-------------------------- MODULE SessionActivity --------------------------
EXTENDS Naturals

CONSTANTS ActivityIds, MaxActivityEpoch

VARIABLES activityEpoch, activeActivityEpochs, settledActivities,
          nextActivityEpoch

vars == <<activityEpoch, activeActivityEpochs, settledActivities,
          nextActivityEpoch>>

Kernel == INSTANCE SessionActivityKernel WITH
    ActivityIds <- ActivityIds,
    MaxActivityEpoch <- MaxActivityEpoch,
    kActivityEpoch <- activityEpoch,
    kActiveActivityEpochs <- activeActivityEpochs,
    kSettledActivities <- settledActivities,
    kNextActivityEpoch <- nextActivityEpoch

Init == Kernel!Init
Next == Kernel!Next
Spec == Init /\ [][Next]_vars
Safety == Kernel!Safety

=============================================================================
