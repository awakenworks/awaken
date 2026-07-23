------------------------ MODULE RemoteWorkerProtocol ------------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC

\* One Run is sufficient to check the protocol's ownership and prefix
\* invariants. Workers, epochs, versions, operations, and hashes are finite in
\* the TLC configuration; the production protocol remains unbounded.
CONSTANTS Workers, Operations, Hashes, AwaitOperations, TerminalOperations,
          NoWorker, NoOperation, NoHash, Coordinator,
          MaxIncarnation, MaxEpoch, MaxVersion

ASSUME /\ Workers # {} /\ Operations # {} /\ Hashes # {}
       /\ AwaitOperations \subseteq Operations
       /\ TerminalOperations \subseteq Operations
       /\ AwaitOperations \cap TerminalOperations = {}
       /\ NoWorker \notin Workers /\ NoOperation \notin Operations
       /\ NoHash \notin Hashes /\ Coordinator \notin Workers
       /\ MaxIncarnation \in Nat \ {0}
       /\ MaxEpoch \in Nat \ {0}
       /\ MaxVersion \in Nat \ {0}

WorkerStates == {"Absent", "Ready", "Draining", "Quiesced"}
DispatchStates == {"Pending", "Leased", "Awaiting", "Removed"}
RunStates == {"Running", "Awaiting", "Ended"}
ResponseStates == {"Idle", "Available", "Lost", "Acknowledged", "Rejected"}
Signals == {"None", "Input", "Cancellation"}

VARIABLES workerState, workerIncarnation,
          dispatchState, owner, leaseEpoch, leaseLive,
          snapshotLoaded, snapshotEpoch, workerProjection, executing,
          committedLog, runState, endedOnce, receiptHash, receiptVersion,
          pendingOperation, pendingHash, pendingEpoch, pendingExpectedVersion,
          responseState, acknowledgedVersion,
          pendingSignal, consumedSignalVersion, lastTruthWriter

vars ==
    <<workerState, workerIncarnation,
      dispatchState, owner, leaseEpoch, leaseLive,
      snapshotLoaded, snapshotEpoch, workerProjection, executing,
      committedLog, runState, endedOnce, receiptHash, receiptVersion,
      pendingOperation, pendingHash, pendingEpoch, pendingExpectedVersion,
      responseState, acknowledgedVersion,
      pendingSignal, consumedSignalVersion, lastTruthWriter>>

Init ==
    /\ workerState = [w \in Workers |-> "Absent"]
    /\ workerIncarnation = [w \in Workers |-> 0]
    /\ dispatchState = "Pending"
    /\ owner = NoWorker
    /\ leaseEpoch = 0
    /\ leaseLive = FALSE
    /\ snapshotLoaded = [w \in Workers |-> FALSE]
    /\ snapshotEpoch = [w \in Workers |-> 0]
    /\ workerProjection = [w \in Workers |-> <<>>]
    /\ executing = {}
    /\ committedLog = <<>>
    /\ runState = "Running"
    /\ endedOnce = FALSE
    /\ receiptHash = [o \in Operations |-> NoHash]
    /\ receiptVersion = [o \in Operations |-> 0]
    /\ pendingOperation = [w \in Workers |-> NoOperation]
    /\ pendingHash = [w \in Workers |-> NoHash]
    /\ pendingEpoch = [w \in Workers |-> 0]
    /\ pendingExpectedVersion = [w \in Workers |-> 0]
    /\ responseState = [w \in Workers |-> "Idle"]
    /\ acknowledgedVersion = [w \in Workers |-> 0]
    /\ pendingSignal = "None"
    /\ consumedSignalVersion = 0
    /\ lastTruthWriter = Coordinator

Register(w) ==
    /\ w \in Workers
    /\ workerState[w] \in {"Absent", "Quiesced"}
    /\ workerIncarnation[w] < MaxIncarnation
    /\ workerState' = [workerState EXCEPT ![w] = "Ready"]
    /\ workerIncarnation' = [workerIncarnation EXCEPT ![w] = @ + 1]
    /\ UNCHANGED <<dispatchState, owner, leaseEpoch, leaseLive,
                    snapshotLoaded, snapshotEpoch, workerProjection, executing,
                    committedLog, runState, endedOnce, receiptHash,
                    receiptVersion, pendingOperation, pendingHash, pendingEpoch,
                    pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Heartbeat(w, incarnation) ==
    /\ w \in Workers
    /\ incarnation = workerIncarnation[w]
    /\ workerState[w] \in {"Ready", "Draining"}
    /\ UNCHANGED vars

StaleHeartbeat(w, incarnation) ==
    /\ w \in Workers
    /\ incarnation \in 0..MaxIncarnation
    /\ incarnation # workerIncarnation[w]
    /\ UNCHANGED vars

Drain(w) ==
    /\ w \in Workers /\ workerState[w] = "Ready"
    /\ workerState' = [workerState EXCEPT ![w] = "Draining"]
    /\ UNCHANGED <<workerIncarnation, dispatchState, owner, leaseEpoch,
                    leaseLive, snapshotLoaded, snapshotEpoch, workerProjection,
                    executing, committedLog, runState, endedOnce, receiptHash,
                    receiptVersion, pendingOperation, pendingHash, pendingEpoch,
                    pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Quiesce(w) ==
    /\ w \in Workers /\ workerState[w] = "Draining" /\ w \notin executing
    /\ workerState' = [workerState EXCEPT ![w] = "Quiesced"]
    /\ UNCHANGED <<workerIncarnation, dispatchState, owner, leaseEpoch,
                    leaseLive, snapshotLoaded, snapshotEpoch, workerProjection,
                    executing, committedLog, runState, endedOnce, receiptHash,
                    receiptVersion, pendingOperation, pendingHash, pendingEpoch,
                    pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Claim(w) ==
    /\ w \in Workers /\ workerState[w] = "Ready"
    /\ dispatchState \in {"Pending", "Awaiting"}
    /\ runState # "Ended" /\ leaseEpoch < MaxEpoch
    /\ dispatchState' = "Leased" /\ owner' = w
    /\ leaseEpoch' = leaseEpoch + 1 /\ leaseLive' = TRUE
    /\ snapshotLoaded' = [snapshotLoaded EXCEPT ![w] = FALSE]
    /\ executing' = executing \ {w}
    /\ UNCHANGED <<workerState, workerIncarnation, snapshotEpoch,
                    workerProjection, committedLog, runState, endedOnce,
                    receiptHash, receiptVersion, pendingOperation, pendingHash,
                    pendingEpoch, pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Expire ==
    /\ dispatchState = "Leased" /\ leaseLive
    /\ leaseLive' = FALSE
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, snapshotLoaded, snapshotEpoch, workerProjection,
                    executing, committedLog, runState, endedOnce, receiptHash,
                    receiptVersion, pendingOperation, pendingHash, pendingEpoch,
                    pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Reclaim(w) ==
    /\ w \in Workers /\ workerState[w] = "Ready" /\ w # owner
    /\ dispatchState = "Leased" /\ ~leaseLive /\ runState # "Ended"
    /\ leaseEpoch < MaxEpoch
    /\ owner' = w /\ leaseEpoch' = leaseEpoch + 1 /\ leaseLive' = TRUE
    /\ snapshotLoaded' = [snapshotLoaded EXCEPT ![w] = FALSE]
    /\ executing' = executing \ {w}
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, snapshotEpoch,
                    workerProjection, committedLog, runState, endedOnce,
                    receiptHash, receiptVersion, pendingOperation, pendingHash,
                    pendingEpoch, pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Abandon ==
    /\ dispatchState = "Leased" /\ ~leaseLive
    /\ dispatchState' = IF runState = "Awaiting" THEN "Awaiting" ELSE "Pending"
    /\ owner' = NoWorker
    /\ UNCHANGED <<workerState, workerIncarnation, leaseEpoch, leaseLive,
                    snapshotLoaded, snapshotEpoch, workerProjection, executing,
                    committedLog, runState, endedOnce, receiptHash,
                    receiptVersion, pendingOperation, pendingHash, pendingEpoch,
                    pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Snapshot(w) ==
    /\ w = owner /\ leaseLive /\ dispatchState = "Leased"
    /\ workerState[w] = "Ready"
    /\ snapshotLoaded' = [snapshotLoaded EXCEPT ![w] = TRUE]
    /\ snapshotEpoch' = [snapshotEpoch EXCEPT ![w] = leaseEpoch]
    /\ workerProjection' = [workerProjection EXCEPT ![w] = committedLog]
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    responseState, acknowledgedVersion, pendingSignal,
                    consumedSignalVersion, lastTruthWriter>>

Execute(w) ==
    /\ w = owner /\ leaseLive /\ dispatchState = "Leased"
    /\ workerState[w] = "Ready"
    /\ snapshotLoaded[w] /\ snapshotEpoch[w] = leaseEpoch
    /\ w \notin executing
    /\ executing' = executing \cup {w}
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, committedLog, runState, endedOnce,
                    receiptHash, receiptVersion, pendingOperation, pendingHash,
                    pendingEpoch, pendingExpectedVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

CommitRequest(w, operation, hash) ==
    /\ w \in executing /\ operation \in Operations /\ hash \in Hashes
    /\ responseState[w] = "Idle"
    /\ pendingOperation[w] = NoOperation
    /\ \/ receiptHash[operation] # NoHash
       \/ /\ runState = "Running" /\ operation \in AwaitOperations
       \/ /\ runState = "Awaiting" /\ operation \in TerminalOperations
    /\ pendingOperation' = [pendingOperation EXCEPT ![w] = operation]
    /\ pendingHash' = [pendingHash EXCEPT ![w] = hash]
    /\ pendingEpoch' = [pendingEpoch EXCEPT ![w] = snapshotEpoch[w]]
    /\ pendingExpectedVersion' =
        [pendingExpectedVersion EXCEPT ![w] = Len(workerProjection[w])]
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, responseState,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

CurrentFence(w) ==
    /\ w = owner /\ leaseLive /\ dispatchState = "Leased"
    /\ pendingEpoch[w] = leaseEpoch

CommitApplyNew(w) ==
    LET operation == pendingOperation[w] IN
    /\ operation \in Operations
    /\ CurrentFence(w)
    /\ receiptHash[operation] = NoHash
    /\ pendingExpectedVersion[w] = Len(committedLog)
    /\ Len(committedLog) < MaxVersion /\ runState # "Ended"
    /\ committedLog' = Append(committedLog, operation)
    /\ receiptHash' = [receiptHash EXCEPT ![operation] = pendingHash[w]]
    /\ receiptVersion' =
        [receiptVersion EXCEPT ![operation] = Len(committedLog) + 1]
    /\ runState' = IF operation \in TerminalOperations
                    THEN "Ended"
                    ELSE IF operation \in AwaitOperations
                         THEN "Awaiting"
                         ELSE "Running"
    /\ endedOnce' = (endedOnce \/ operation \in TerminalOperations)
    /\ pendingSignal' = "None"
    /\ consumedSignalVersion' =
        IF pendingSignal # "None"
        THEN Len(committedLog) + 1
        ELSE consumedSignalVersion
    /\ responseState' = [responseState EXCEPT ![w] = "Available"]
    /\ lastTruthWriter' = Coordinator
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, pendingOperation, pendingHash,
                    pendingEpoch, pendingExpectedVersion, acknowledgedVersion>>

CommitApplyReplay(w) ==
    LET operation == pendingOperation[w] IN
    /\ operation \in Operations
    /\ CurrentFence(w)
    /\ receiptHash[operation] = pendingHash[w]
    /\ responseState' = [responseState EXCEPT ![w] = "Available"]
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

CommitReject(w) ==
    LET operation == pendingOperation[w] IN
    /\ operation \in Operations
    /\ \/ ~CurrentFence(w)
       \/ /\ receiptHash[operation] # NoHash
          /\ receiptHash[operation] # pendingHash[w]
       \/ pendingExpectedVersion[w] # Len(committedLog)
       \/ runState = "Ended"
       \/ Len(committedLog) = MaxVersion
    /\ responseState' = [responseState EXCEPT ![w] = "Rejected"]
    /\ executing' = executing \ {w}
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

CommitApply(w) ==
    /\ responseState[w] = "Idle"
    /\ (CommitApplyNew(w) \/ CommitApplyReplay(w) \/ CommitReject(w))

ResponseLost(w) ==
    /\ w \in Workers /\ responseState[w] = "Available"
    /\ responseState' = [responseState EXCEPT ![w] = "Lost"]
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Retry(w) ==
    LET operation == pendingOperation[w] IN
    /\ w \in Workers /\ responseState[w] = "Lost"
    /\ receiptHash[operation] = pendingHash[w]
    /\ responseState' = [responseState EXCEPT ![w] = "Available"]
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

DeliverResponse(w) ==
    LET operation == pendingOperation[w] IN
    /\ w \in Workers /\ responseState[w] = "Available"
    /\ receiptHash[operation] = pendingHash[w]
    /\ responseState' = [responseState EXCEPT ![w] = "Acknowledged"]
    /\ workerProjection' =
        [workerProjection EXCEPT
          ![w] = IF Len(@) < receiptVersion[operation]
                  THEN SubSeq(committedLog, 1, receiptVersion[operation])
                  ELSE @]
    /\ acknowledgedVersion' =
        [acknowledgedVersion EXCEPT
          ![w] = IF @ >= receiptVersion[operation]
                  THEN @ ELSE receiptVersion[operation]]
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    executing, committedLog, runState, endedOnce, receiptHash,
                    receiptVersion, pendingOperation, pendingHash, pendingEpoch,
                    pendingExpectedVersion, pendingSignal,
                    consumedSignalVersion, lastTruthWriter>>

ClearResponse(w) ==
    /\ w \in Workers
    /\ \/ responseState[w] = "Rejected"
       \/ /\ responseState[w] = "Acknowledged"
          /\ \/ runState = "Running"
             \/ dispatchState # "Leased"
             \/ w # owner
    /\ responseState' = [responseState EXCEPT ![w] = "Idle"]
    /\ pendingOperation' = [pendingOperation EXCEPT ![w] = NoOperation]
    /\ pendingHash' = [pendingHash EXCEPT ![w] = NoHash]
    /\ pendingEpoch' = [pendingEpoch EXCEPT ![w] = 0]
    /\ pendingExpectedVersion' = [pendingExpectedVersion EXCEPT ![w] = 0]
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion,
                    acknowledgedVersion, pendingSignal, consumedSignalVersion,
                    lastTruthWriter>>

Renew(w, epoch) ==
    /\ w = owner /\ epoch = leaseEpoch /\ leaseLive
    /\ dispatchState = "Leased" /\ workerState[w] = "Ready"
    /\ UNCHANGED vars

Settle(w, epoch) ==
    /\ w = owner /\ epoch = leaseEpoch /\ leaseLive
    /\ dispatchState = "Leased"
    /\ runState \in {"Awaiting", "Ended"}
    /\ responseState[w] = "Acknowledged"
    /\ dispatchState' = IF runState = "Ended" THEN "Removed" ELSE "Awaiting"
    /\ owner' = NoWorker /\ leaseLive' = FALSE
    /\ executing' = executing \ {w}
    /\ UNCHANGED <<workerState, workerIncarnation, leaseEpoch, snapshotLoaded,
                    snapshotEpoch, workerProjection, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    responseState, acknowledgedVersion, pendingSignal,
                    consumedSignalVersion, lastTruthWriter>>

SupplyInput ==
    /\ runState = "Awaiting" /\ dispatchState = "Awaiting"
    /\ pendingSignal = "None"
    /\ pendingSignal' = "Input"
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    responseState, acknowledgedVersion, consumedSignalVersion,
                    lastTruthWriter>>

RequestCancellation ==
    /\ runState # "Ended" /\ pendingSignal = "None"
    /\ pendingSignal' = "Cancellation"
    /\ UNCHANGED <<workerState, workerIncarnation, dispatchState, owner,
                    leaseEpoch, leaseLive, snapshotLoaded, snapshotEpoch,
                    workerProjection, executing, committedLog, runState,
                    endedOnce, receiptHash, receiptVersion, pendingOperation,
                    pendingHash, pendingEpoch, pendingExpectedVersion,
                    responseState, acknowledgedVersion, consumedSignalVersion,
                    lastTruthWriter>>

Next ==
    \/ \E w \in Workers : Register(w)
    \/ \E w \in Workers, incarnation \in 0..MaxIncarnation :
           Heartbeat(w, incarnation) \/ StaleHeartbeat(w, incarnation)
    \/ \E w \in Workers : Drain(w) \/ Quiesce(w)
    \/ \E w \in Workers : Claim(w) \/ Reclaim(w) \/ Snapshot(w) \/ Execute(w)
    \/ Expire \/ Abandon
    \/ \E w \in Workers, operation \in Operations, hash \in Hashes :
           CommitRequest(w, operation, hash)
    \/ \E w \in Workers : CommitApply(w) \/ ResponseLost(w) \/ Retry(w)
                           \/ DeliverResponse(w) \/ ClearResponse(w)
    \/ \E w \in Workers, epoch \in 0..MaxEpoch :
           Renew(w, epoch) \/ Settle(w, epoch)
    \/ SupplyInput \/ RequestCancellation

TypeOK ==
    /\ workerState \in [Workers -> WorkerStates]
    /\ workerIncarnation \in [Workers -> 0..MaxIncarnation]
    /\ dispatchState \in DispatchStates
    /\ owner \in Workers \cup {NoWorker}
    /\ leaseEpoch \in 0..MaxEpoch /\ leaseLive \in BOOLEAN
    /\ snapshotLoaded \in [Workers -> BOOLEAN]
    /\ snapshotEpoch \in [Workers -> 0..MaxEpoch]
    /\ workerProjection \in [Workers -> Seq(Operations)]
    /\ executing \in SUBSET Workers
    /\ committedLog \in Seq(Operations) /\ Len(committedLog) <= MaxVersion
    /\ runState \in RunStates /\ endedOnce \in BOOLEAN
    /\ receiptHash \in [Operations -> Hashes \cup {NoHash}]
    /\ receiptVersion \in [Operations -> 0..MaxVersion]
    /\ pendingOperation \in [Workers -> Operations \cup {NoOperation}]
    /\ pendingHash \in [Workers -> Hashes \cup {NoHash}]
    /\ pendingEpoch \in [Workers -> 0..MaxEpoch]
    /\ pendingExpectedVersion \in [Workers -> 0..MaxVersion]
    /\ responseState \in [Workers -> ResponseStates]
    /\ acknowledgedVersion \in [Workers -> 0..MaxVersion]
    /\ pendingSignal \in Signals
    /\ consumedSignalVersion \in 0..MaxVersion
    /\ lastTruthWriter = Coordinator

IsPrefix(prefix, whole) ==
    /\ Len(prefix) <= Len(whole)
    /\ \A i \in 1..Len(prefix) : prefix[i] = whole[i]

OneLiveClaimOwner ==
    (dispatchState = "Leased" /\ leaseLive) => owner \in Workers

StaleEpochNeverMutatesTruth ==
    \A w \in Workers :
      pendingOperation[w] \in Operations /\ pendingEpoch[w] # leaseEpoch
        => (responseState[w] = "Available"
              => receiptHash[pendingOperation[w]] = pendingHash[w])

StableOperationReceipt ==
    \A operation \in Operations :
      /\ (receiptHash[operation] = NoHash) = (receiptVersion[operation] = 0)
      /\ receiptVersion[operation] > 0
           => committedLog[receiptVersion[operation]] = operation

TerminalRunIsAbsorbing == endedOnce => runState = "Ended"

RecoveryProjectionIsCommittedPrefix ==
    \A w \in Workers : IsPrefix(workerProjection[w], committedLog)

ExecutionRequiresCurrentSnapshot ==
    \A w \in executing :
      /\ snapshotLoaded[w]
      /\ snapshotEpoch[w] > 0
      /\ IsPrefix(workerProjection[w], committedLog)

AcknowledgedWritesAreMonotonic ==
    \A w \in Workers :
      /\ acknowledgedVersion[w] <= Len(workerProjection[w])
      /\ acknowledgedVersion[w] <= Len(committedLog)

PendingSignalConsumedWithCommit ==
    consumedSignalVersion > 0
      => consumedSignalVersion <= Len(committedLog)

CoordinatorOwnsCommittedTruth == lastTruthWriter = Coordinator

Safety ==
    /\ TypeOK
    /\ OneLiveClaimOwner
    /\ StaleEpochNeverMutatesTruth
    /\ StableOperationReceipt
    /\ TerminalRunIsAbsorbing
    /\ RecoveryProjectionIsCommittedPrefix
    /\ ExecutionRequiresCurrentSnapshot
    /\ AcknowledgedWritesAreMonotonic
    /\ PendingSignalConsumedWithCommit
    /\ CoordinatorOwnsCommittedTruth

\* Fairness is attached only to protocol progress actions. Environment actions
\* (registration, external input, cancellation, network recovery, and Worker
\* availability) intentionally have no fairness assumption.
ClaimAny == \E w \in Workers : Claim(w) \/ Reclaim(w)
SnapshotAny == \E w \in Workers : Snapshot(w)
CommitApplyAny == \E w \in Workers : CommitApply(w)
SettleAny == \E w \in Workers, epoch \in 0..MaxEpoch : Settle(w, epoch)
QuiesceAny == \E w \in Workers : Quiesce(w)

ClaimEnabled ==
    /\ dispatchState \in {"Pending", "Awaiting"}
    /\ runState # "Ended" /\ leaseEpoch < MaxEpoch
    /\ \E w \in Workers : workerState[w] = "Ready"

SnapshotEnabled ==
    /\ dispatchState = "Leased" /\ leaseLive
    /\ owner \in Workers /\ workerState[owner] = "Ready"
    /\ ~snapshotLoaded[owner]

CommitApplyEnabled ==
    \E w \in Workers :
      pendingOperation[w] \in Operations /\ responseState[w] = "Idle"

ReclaimEnabled ==
    /\ dispatchState = "Leased" /\ ~leaseLive /\ runState # "Ended"
    /\ leaseEpoch < MaxEpoch
    /\ \E w \in Workers : workerState[w] = "Ready" /\ w # owner

ClaimableEventuallyChanges ==
    ClaimEnabled ~> (~ClaimEnabled \/ (dispatchState = "Leased" /\ leaseLive))

SnapshotEventuallyChanges ==
    SnapshotEnabled
      ~> (~SnapshotEnabled
           \/ \E w \in Workers : w = owner /\ snapshotLoaded[w])

CommitRequestEventuallyChanges ==
    \A w \in Workers :
      (pendingOperation[w] \in Operations /\ responseState[w] = "Idle")
        ~> (responseState[w] # "Idle")

ExpiredClaimEventuallyChanges ==
    ReclaimEnabled ~> (~ReclaimEnabled \/ leaseLive)

TerminalLeaseEventuallyChanges ==
    (runState = "Ended" /\ dispatchState = "Leased" /\ leaseLive
      /\ owner \in Workers /\ responseState[owner] = "Acknowledged")
      ~> (dispatchState = "Removed" \/ ~leaseLive)

DrainEventuallyQuiesces ==
    \A w \in Workers :
      (workerState[w] = "Draining" /\ w \notin executing)
        ~> (workerState[w] = "Quiesced")

Spec ==
    Init
    /\ [][Next]_vars
    /\ WF_vars(ClaimAny)
    /\ WF_vars(SnapshotAny)
    /\ WF_vars(SettleAny)
    /\ (\A w \in Workers : WF_vars(CommitApply(w)))
    /\ (\A w \in Workers : WF_vars(Quiesce(w)))
=============================================================================
