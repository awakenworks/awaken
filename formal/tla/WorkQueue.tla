------------------------------ MODULE WorkQueue ------------------------------
EXTENDS Naturals

\* Standalone exhaustive instance of the one WorkQueueKernel. Product-level
\* protocols map the same kernel onto their bounded Session Work rows.
CONSTANTS WorkItems, Workers, NoWorker, MaxEpoch, MaxHeartbeat, MaxTime

VARIABLES state, owner, epoch, heartbeat, expires, now

vars == <<state, owner, epoch, heartbeat, expires, now>>

Kernel == INSTANCE WorkQueueKernel WITH
    WorkItems <- WorkItems,
    Workers <- Workers,
    NoWorker <- NoWorker,
    MaxEpoch <- MaxEpoch,
    MaxHeartbeat <- MaxHeartbeat,
    MaxTime <- MaxTime,
    kState <- state,
    kOwner <- owner,
    kEpoch <- epoch,
    kHeartbeat <- heartbeat,
    kExpires <- expires,
    kNow <- now

KernelAssumptions == Kernel!KernelAssumptions
Init == Kernel!Init
NoActive == Kernel!NoActive
Claim(worker, item) == Kernel!Claim(worker, item)
Reclaim(worker, item) == Kernel!Reclaim(worker, item)
Ack(worker, item) == Kernel!Ack(worker, item)
Heartbeat(worker, item, expected) == Kernel!Heartbeat(worker, item, expected)
Stop(worker, item, expectedEpoch) == Kernel!Stop(worker, item, expectedEpoch)
RemoveEnvironment == Kernel!RemoveEnvironment
AdvanceTime == Kernel!AdvanceTime
Next == Kernel!Next
Spec == Init /\ [][Next]_vars

TypeOK == Kernel!TypeOK
SingleActive == Kernel!SingleActive
ActiveHasOneOwner == Kernel!ActiveHasOneOwner
ActiveEpochIsPositive == Kernel!ActiveEpochIsPositive
TerminalHasNoLeaseAuthority == Kernel!TerminalHasNoLeaseAuthority
Safety == Kernel!Safety

=============================================================================
