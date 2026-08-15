--------------------- MODULE DeploymentExecutionWorkflow ---------------------
EXTENDS Naturals

\* Revision-fenced scheduled occurrence claim, transactional capacity and Run
\* creation, Worker claim/replacement, terminal commit and settlement.
CONSTANTS Schedulers, Workers, NoScheduler, NoWorker,
          MaxRevision, MaxOccurrence, MaxCapacity, MaxEpoch

ASSUME
    /\ Schedulers # {}
    /\ Workers # {}
    /\ NoScheduler \notin Schedulers
    /\ NoWorker \notin Workers
    /\ MaxRevision \in Nat \ {0}
    /\ MaxOccurrence \in Nat \ {0}
    /\ MaxCapacity \in Nat \ {0}
    /\ MaxEpoch \in Nat \ {0}

VARIABLES deploymentRevision, occurrence, occurrenceOwner,
          claimedRevision, capacityUsed, runState, runRevision,
          worker, workerEpoch, outputCommitted, terminalWorker, terminalEpoch

vars == <<deploymentRevision, occurrence, occurrenceOwner,
          claimedRevision, capacityUsed, runState, runRevision,
          worker, workerEpoch, outputCommitted, terminalWorker, terminalEpoch>>

Init ==
    /\ deploymentRevision = 1
    /\ occurrence = 1
    /\ occurrenceOwner = NoScheduler
    /\ claimedRevision = 0
    /\ capacityUsed = 0
    /\ runState = "Absent"
    /\ runRevision = 0
    /\ worker = NoWorker
    /\ workerEpoch = 0
    /\ outputCommitted = FALSE
    /\ terminalWorker = NoWorker
    /\ terminalEpoch = 0

UpdateDeployment ==
    /\ deploymentRevision < MaxRevision
    /\ runState \in {"Absent", "Settled"}
    /\ deploymentRevision' = deploymentRevision + 1
    /\ occurrence' = IF occurrence < MaxOccurrence THEN occurrence + 1 ELSE occurrence
    /\ occurrenceOwner' = NoScheduler
    /\ claimedRevision' = 0
    /\ runState' = "Absent"
    /\ runRevision' = 0
    /\ outputCommitted' = FALSE
    /\ terminalWorker' = NoWorker
    /\ terminalEpoch' = 0
    /\ UNCHANGED <<capacityUsed, worker, workerEpoch>>

ClaimOccurrence(s) ==
    /\ occurrenceOwner = NoScheduler
    /\ runState = "Absent"
    /\ capacityUsed < MaxCapacity
    /\ s \in Schedulers
    /\ occurrenceOwner' = s
    /\ claimedRevision' = deploymentRevision
    /\ capacityUsed' = capacityUsed + 1
    /\ runState' = "Pending"
    /\ runRevision' = deploymentRevision
    /\ UNCHANGED <<deploymentRevision, occurrence, worker, workerEpoch,
                   outputCommitted, terminalWorker, terminalEpoch>>

ClaimWorker(w) ==
    /\ runState = "Pending"
    /\ w \in Workers
    /\ workerEpoch < MaxEpoch
    /\ worker' = w
    /\ workerEpoch' = workerEpoch + 1
    /\ runState' = "Running"
    /\ UNCHANGED <<deploymentRevision, occurrence, occurrenceOwner,
                   claimedRevision, capacityUsed, runRevision, outputCommitted,
                   terminalWorker, terminalEpoch>>

ReplaceWorker(w) ==
    /\ runState = "Running"
    /\ w \in Workers
    /\ w # worker
    /\ workerEpoch < MaxEpoch
    /\ worker' = w
    /\ workerEpoch' = workerEpoch + 1
    /\ UNCHANGED <<deploymentRevision, occurrence, occurrenceOwner,
                   claimedRevision, capacityUsed, runState, runRevision,
                   outputCommitted, terminalWorker, terminalEpoch>>

CommitTerminal(w, epoch) ==
    /\ runState = "Running"
    /\ w = worker
    /\ epoch = workerEpoch
    /\ runState' = "Ended"
    /\ outputCommitted' = TRUE
    /\ terminalWorker' = w
    /\ terminalEpoch' = epoch
    /\ UNCHANGED <<deploymentRevision, occurrence, occurrenceOwner,
                   claimedRevision, capacityUsed, runRevision, worker, workerEpoch>>

Settle ==
    /\ runState = "Ended"
    /\ runState' = "Settled"
    /\ capacityUsed' = capacityUsed - 1
    /\ worker' = NoWorker
    /\ UNCHANGED <<deploymentRevision, occurrence, occurrenceOwner,
                   claimedRevision, runRevision, workerEpoch, outputCommitted,
                   terminalWorker, terminalEpoch>>

ClaimOccurrenceAny == \E s \in Schedulers: ClaimOccurrence(s)
ClaimWorkerAny == \E w \in Workers: ClaimWorker(w)
ReplaceWorkerAny == \E w \in Workers: ReplaceWorker(w)
CommitTerminalAny == \E w \in Workers, epoch \in 0..MaxEpoch: CommitTerminal(w, epoch)

Next ==
    \/ UpdateDeployment
    \/ ClaimOccurrenceAny
    \/ ClaimWorkerAny
    \/ ReplaceWorkerAny
    \/ CommitTerminalAny
    \/ Settle

Spec == Init /\ [][Next]_vars

HappyNext == ClaimOccurrenceAny \/ ClaimWorkerAny \/ CommitTerminalAny \/ Settle
HappySpec ==
    /\ Init
    /\ [][HappyNext]_vars
    /\ WF_vars(ClaimOccurrenceAny)
    /\ WF_vars(ClaimWorkerAny)
    /\ WF_vars(CommitTerminalAny)
    /\ WF_vars(Settle)

TypeOK ==
    /\ deploymentRevision \in 1..MaxRevision
    /\ occurrence \in 1..MaxOccurrence
    /\ occurrenceOwner \in Schedulers \cup {NoScheduler}
    /\ claimedRevision \in 0..MaxRevision
    /\ capacityUsed \in 0..MaxCapacity
    /\ runState \in {"Absent", "Pending", "Running", "Ended", "Settled"}
    /\ runRevision \in 0..MaxRevision
    /\ worker \in Workers \cup {NoWorker}
    /\ workerEpoch \in 0..MaxEpoch
    /\ outputCommitted \in BOOLEAN
    /\ terminalWorker \in Workers \cup {NoWorker}
    /\ terminalEpoch \in 0..MaxEpoch

RunPinsClaimedRevision == runState # "Absent" => runRevision = claimedRevision
CapacityOwnsLiveRun == runState \in {"Pending", "Running", "Ended"} => capacityUsed = 1
ExecutionRequiresWorkerClaim == runState = "Running" => worker \in Workers /\ workerEpoch > 0
OutputRequiresTerminalCommit == outputCommitted => runState \in {"Ended", "Settled"}
TerminalCommitUsesExactClaim ==
    runState = "Ended" => terminalWorker = worker /\ terminalEpoch = workerEpoch
SettlementReleasesCapacity == runState = "Settled" => capacityUsed = 0 /\ worker = NoWorker

Safety ==
    /\ TypeOK
    /\ RunPinsClaimedRevision
    /\ CapacityOwnsLiveRun
    /\ ExecutionRequiresWorkerClaim
    /\ OutputRequiresTerminalCommit
    /\ TerminalCommitUsesExactClaim
    /\ SettlementReleasesCapacity

EventuallySettled == <> (runState = "Settled")
=============================================================================
