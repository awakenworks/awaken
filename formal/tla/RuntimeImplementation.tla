------------------------- MODULE RuntimeImplementation -------------------------
EXTENDS Naturals

\* Fine-grained implementation model. Every durable abstract action is split
\* into Prepare and Apply steps, matching the Rust pattern of computing a
\* ThreadCommit/repository CAS before the atomic write. A crash may discard a
\* prepared operation. Executor entry remains a separate post-commit action.
CONSTANTS
    Calls,
    AgentCall,
    ReplayCalls,
    Owners,
    NoOwner,
    NoCall,
    MaxEpoch,
    MaxAttempts,
    MaxInbox,
    MaxVersion

OpKinds == {
    "Claim", "Reclaim", "RequestToolPermission", "Approve", "Deny", "SupplyResult",
    "DirectStart", "CompleteImmediate", "Complete", "RecoverReplaySafe",
    "RecoverIndeterminate", "StartChild", "AwaitChild", "ResumeChild",
    "FinishChild", "DuplicateChildCompletion",
    "QueueMessage", "ConsumeMessage", "FinalizeBatch", "EndRun",
    "Cancel", "StaleSettle"
}

CallOps == {
    "RequestToolPermission", "Approve", "Deny", "SupplyResult", "DirectStart",
    "CompleteImmediate", "Complete",
    "RecoverReplaySafe", "RecoverIndeterminate"
}
OwnerOps == {
    "Claim", "Reclaim", "Approve", "Deny", "SupplyResult", "FinishChild",
    "StaleSettle"
}

VARIABLES
    iRunState,
    iTicketKind,
    iTicketCall,
    iDispatchState,
    iOwner,
    iLeaseEpoch,
    iCallState,
    iAttempts,
    iInvokedAttempt,
    iDecision,
    iPublished,
    iChildState,
    iLinkStatus,
    iInboxCount,
    iCommitVersion,
    iEndedOnce,
    pc,
    pendingOp,
    pendingCall,
    pendingOwner,
    pendingEpoch

implVars == <<
    iRunState,
    iTicketKind,
    iTicketCall,
    iDispatchState,
    iOwner,
    iLeaseEpoch,
    iCallState,
    iAttempts,
    iInvokedAttempt,
    iDecision,
    iPublished,
    iChildState,
    iLinkStatus,
    iInboxCount,
    iCommitVersion,
    iEndedOnce
>>

vars == <<implVars, pc, pendingOp, pendingCall, pendingOwner, pendingEpoch>>

ABS == INSTANCE RuntimeSystem WITH
    Calls <- Calls,
    AgentCall <- AgentCall,
    ReplayCalls <- ReplayCalls,
    Owners <- Owners,
    NoOwner <- NoOwner,
    NoCall <- NoCall,
    MaxEpoch <- MaxEpoch,
    MaxAttempts <- MaxAttempts,
    MaxInbox <- MaxInbox,
    MaxVersion <- MaxVersion,
    runState <- iRunState,
    ticketKind <- iTicketKind,
    ticketCall <- iTicketCall,
    dispatchState <- iDispatchState,
    owner <- iOwner,
    leaseEpoch <- iLeaseEpoch,
    callState <- iCallState,
    attempts <- iAttempts,
    invokedAttempt <- iInvokedAttempt,
    decision <- iDecision,
    published <- iPublished,
    childState <- iChildState,
    linkStatus <- iLinkStatus,
    inboxCount <- iInboxCount,
    commitVersion <- iCommitVersion,
    endedOnce <- iEndedOnce

ASSUME ABS!RuntimeAssumptions

Init ==
    /\ ABS!Init
    /\ pc = "Idle"
    /\ pendingOp = "None"
    /\ pendingCall = NoCall
    /\ pendingOwner = NoOwner
    /\ pendingEpoch = 0

Prepared(op) == pc = "Prepared" /\ pendingOp = op

ClearPrepared ==
    /\ pc' = "Idle"
    /\ pendingOp' = "None"
    /\ pendingCall' = NoCall
    /\ pendingOwner' = NoOwner
    /\ pendingEpoch' = 0

Prepare(op, call, candidate, epoch) ==
    /\ pc = "Idle"
    /\ op \in OpKinds
    /\ call \in Calls \cup {NoCall}
    /\ candidate \in Owners \cup {NoOwner}
    /\ epoch \in 0..MaxEpoch
    /\ IF op \in CallOps THEN call \in Calls ELSE call = NoCall
    /\ IF op \in OwnerOps THEN candidate \in Owners ELSE candidate = NoOwner
    /\ IF op = "StaleSettle" THEN TRUE ELSE epoch = 0
    /\ pc' = "Prepared"
    /\ pendingOp' = op
    /\ pendingCall' = call
    /\ pendingOwner' = candidate
    /\ pendingEpoch' = epoch
    /\ UNCHANGED implVars

\* Process loss before the CAS/ThreadCommit is an abstract stuttering step.
CrashPrepared ==
    /\ pc = "Prepared"
    /\ ClearPrepared
    /\ UNCHANGED implVars

ApplyClaim ==
    /\ Prepared("Claim")
    /\ ABS!Claim(pendingOwner)
    /\ ClearPrepared

ApplyReclaim ==
    /\ Prepared("Reclaim")
    /\ ABS!Reclaim(pendingOwner)
    /\ ClearPrepared

ApplyRequestToolPermission ==
    /\ Prepared("RequestToolPermission")
    /\ ABS!RequestToolPermission(pendingCall)
    /\ ClearPrepared

ApplyApprove ==
    /\ Prepared("Approve")
    /\ ABS!Approve(pendingCall, pendingOwner)
    /\ ClearPrepared

ApplyDeny ==
    /\ Prepared("Deny")
    /\ ABS!Deny(pendingCall, pendingOwner)
    /\ ClearPrepared

ApplySupplyResult ==
    /\ Prepared("SupplyResult")
    /\ ABS!SupplyResult(pendingCall, pendingOwner)
    /\ ClearPrepared

ApplyDirectStart ==
    /\ Prepared("DirectStart")
    /\ ABS!DirectStart(pendingCall)
    /\ ClearPrepared

ApplyCompleteImmediate ==
    /\ Prepared("CompleteImmediate")
    /\ ABS!CompleteImmediate(pendingCall)
    /\ ClearPrepared

\* Executor entry is not folded into ApplyDirectStart. This is the concrete
\* side-effect boundary whose abstract image is Invoke.
Invoke(call) ==
    /\ pc = "Idle"
    /\ ABS!Invoke(call)
    /\ UNCHANGED <<pc, pendingOp, pendingCall, pendingOwner, pendingEpoch>>

ApplyComplete ==
    /\ Prepared("Complete")
    /\ ABS!Complete(pendingCall)
    /\ ClearPrepared

ApplyRecoverReplaySafe ==
    /\ Prepared("RecoverReplaySafe")
    /\ ABS!RecoverReplaySafe(pendingCall)
    /\ ClearPrepared

ApplyRecoverIndeterminate ==
    /\ Prepared("RecoverIndeterminate")
    /\ ABS!RecoverIndeterminate(pendingCall)
    /\ ClearPrepared

ApplyStartChild ==
    /\ Prepared("StartChild")
    /\ ABS!StartChild
    /\ ClearPrepared

ApplyAwaitChild ==
    /\ Prepared("AwaitChild")
    /\ ABS!AwaitChild
    /\ ClearPrepared

ApplyResumeChild ==
    /\ Prepared("ResumeChild")
    /\ ABS!ResumeChild
    /\ ClearPrepared

ApplyFinishChild ==
    /\ Prepared("FinishChild")
    /\ ABS!FinishChild(pendingOwner)
    /\ ClearPrepared

ApplyDuplicateChildCompletion ==
    /\ Prepared("DuplicateChildCompletion")
    /\ ABS!DuplicateChildCompletion
    /\ ClearPrepared

ApplyQueueMessage ==
    /\ Prepared("QueueMessage")
    /\ ABS!QueueMessage
    /\ ClearPrepared

ApplyConsumeMessage ==
    /\ Prepared("ConsumeMessage")
    /\ ABS!ConsumeMessage
    /\ ClearPrepared

ApplyFinalizeBatch ==
    /\ Prepared("FinalizeBatch")
    /\ ABS!FinalizeBatch
    /\ ClearPrepared

ApplyEndRun ==
    /\ Prepared("EndRun")
    /\ ABS!EndRun
    /\ ClearPrepared

ApplyCancel ==
    /\ Prepared("Cancel")
    /\ ABS!Cancel
    /\ ClearPrepared

ApplyStaleSettle ==
    /\ Prepared("StaleSettle")
    /\ ABS!StaleSettle(pendingOwner, pendingEpoch)
    /\ ClearPrepared

Next ==
    \/ \E op \in OpKinds,
          call \in Calls \cup {NoCall},
          candidate \in Owners \cup {NoOwner},
          epoch \in 0..MaxEpoch:
           Prepare(op, call, candidate, epoch)
    \/ CrashPrepared
    \/ ApplyClaim
    \/ ApplyReclaim
    \/ ApplyRequestToolPermission
    \/ ApplyApprove
    \/ ApplyDeny
    \/ ApplySupplyResult
    \/ ApplyDirectStart
    \/ ApplyCompleteImmediate
    \/ \E call \in Calls: Invoke(call)
    \/ ApplyComplete
    \/ ApplyRecoverReplaySafe
    \/ ApplyRecoverIndeterminate
    \/ ApplyStartChild
    \/ ApplyAwaitChild
    \/ ApplyResumeChild
    \/ ApplyFinishChild
    \/ ApplyDuplicateChildCompletion
    \/ ApplyQueueMessage
    \/ ApplyConsumeMessage
    \/ ApplyFinalizeBatch
    \/ ApplyEndRun
    \/ ApplyCancel
    \/ ApplyStaleSettle

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ ABS!TypeOK
    /\ pc \in {"Idle", "Prepared"}
    /\ pendingOp \in OpKinds \cup {"None"}
    /\ pendingCall \in Calls \cup {NoCall}
    /\ pendingOwner \in Owners \cup {NoOwner}
    /\ pendingEpoch \in 0..MaxEpoch

PreparedShape ==
    (pc = "Idle") \equiv
        (pendingOp = "None" /\ pendingCall = NoCall /\
         pendingOwner = NoOwner /\ pendingEpoch = 0)

MappedSafety == ABS!Safety

\* This temporal property is the executable refinement obligation: Prepare and
\* CrashPrepared project to stuttering; every Apply/Invoke projects to one
\* RuntimeSystem action.
RefinesRuntimeSystem == ABS!Spec

=============================================================================
