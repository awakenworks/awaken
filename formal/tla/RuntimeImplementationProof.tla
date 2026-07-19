--------------------- MODULE RuntimeImplementationProof ---------------------
EXTENDS RuntimeImplementation, TLAPS

\* The concrete implementation adds only the prepare buffer. The refinement
\* mapping is the projection that forgets pc and the pending operation fields.

LEMMA InitRefines == Init => ABS!Init
BY DEF Init

LEMMA PrepareStutters ==
    \A op \in OpKinds,
       call \in Calls \cup {NoCall},
       candidate \in Owners \cup {NoOwner},
       epoch \in 0..MaxEpoch:
        Prepare(op, call, candidate, epoch) => UNCHANGED implVars
BY DEF Prepare, implVars

LEMMA CrashPreparedStutters == CrashPrepared => UNCHANGED implVars
BY DEF CrashPrepared

LEMMA ApplyClaimRefines == ApplyClaim => ABS!Next
BY Z3T(10) DEF ApplyClaim, ABS!Claim, ABS!Next

LEMMA ApplyReclaimRefines == ApplyReclaim => ABS!Next
BY Z3T(10) DEF ApplyReclaim, ABS!Reclaim, ABS!Next

LEMMA ApplyRequestToolPermissionRefines == ApplyRequestToolPermission => ABS!Next
<1>1. ApplyRequestToolPermission => pendingCall \in Calls
    BY DEF ApplyRequestToolPermission, ABS!RequestToolPermission
<1>2. ApplyRequestToolPermission => ABS!RequestToolPermission(pendingCall)
    BY DEF ApplyRequestToolPermission
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplyApproveRefines == ApplyApprove => ABS!Next
<1>1. ApplyApprove =>
        pendingCall \in Calls /\ pendingOwner \in Owners
    BY DEF ApplyApprove, ABS!Approve
<1>2. ApplyApprove => ABS!Approve(pendingCall, pendingOwner)
    BY DEF ApplyApprove
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplyDenyRefines == ApplyDeny => ABS!Next
<1>1. ApplyDeny => pendingCall \in Calls /\ pendingOwner \in Owners
    BY DEF ApplyDeny, ABS!Deny
<1>2. ApplyDeny => ABS!Deny(pendingCall, pendingOwner)
    BY DEF ApplyDeny
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplySupplyResultRefines == ApplySupplyResult => ABS!Next
<1>1. ApplySupplyResult =>
        pendingCall \in Calls /\ pendingOwner \in Owners
    BY DEF ApplySupplyResult, ABS!SupplyResult
<1>2. ApplySupplyResult => ABS!SupplyResult(pendingCall, pendingOwner)
    BY DEF ApplySupplyResult
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplyDirectStartRefines == ApplyDirectStart => ABS!Next
<1>1. ApplyDirectStart => pendingCall \in Calls
    BY DEF ApplyDirectStart, ABS!DirectStart
<1>2. ApplyDirectStart => ABS!DirectStart(pendingCall)
    BY DEF ApplyDirectStart
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplyCompleteImmediateRefines == ApplyCompleteImmediate => ABS!Next
<1>1. ApplyCompleteImmediate => pendingCall \in Calls
    BY DEF ApplyCompleteImmediate, ABS!CompleteImmediate
<1>2. ApplyCompleteImmediate => ABS!CompleteImmediate(pendingCall)
    BY DEF ApplyCompleteImmediate
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA InvokeRefines ==
    \A call \in Calls: Invoke(call) => ABS!Next
BY Z3T(10) DEF Invoke, ABS!Next

LEMMA ApplyCompleteRefines == ApplyComplete => ABS!Next
<1>1. ApplyComplete => pendingCall \in Calls
    BY DEF ApplyComplete, ABS!Complete
<1>2. ApplyComplete => ABS!Complete(pendingCall)
    BY DEF ApplyComplete
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplyRecoverReplaySafeRefines ==
    ApplyRecoverReplaySafe => ABS!Next
<1>1. ApplyRecoverReplaySafe => pendingCall \in Calls
    BY DEF ApplyRecoverReplaySafe, ABS!RecoverReplaySafe
<1>2. ApplyRecoverReplaySafe => ABS!RecoverReplaySafe(pendingCall)
    BY DEF ApplyRecoverReplaySafe
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplyRecoverIndeterminateRefines ==
    ApplyRecoverIndeterminate => ABS!Next
<1>1. ApplyRecoverIndeterminate => pendingCall \in Calls
    BY DEF ApplyRecoverIndeterminate, ABS!RecoverIndeterminate
<1>2. ApplyRecoverIndeterminate =>
        ABS!RecoverIndeterminate(pendingCall)
    BY DEF ApplyRecoverIndeterminate
<1>. QED BY <1>1, <1>2 DEF ABS!Next

LEMMA ApplyStartChildRefines == ApplyStartChild => ABS!Next
BY Z3T(10) DEF ApplyStartChild, ABS!Next

LEMMA ApplyAwaitChildRefines == ApplyAwaitChild => ABS!Next
BY Z3T(10) DEF ApplyAwaitChild, ABS!Next

LEMMA ApplyResumeChildRefines == ApplyResumeChild => ABS!Next
BY Z3T(10) DEF ApplyResumeChild, ABS!Next

LEMMA ApplyFinishChildRefines == ApplyFinishChild => ABS!Next
BY Z3T(10) DEF ApplyFinishChild, ABS!FinishChild, ABS!Next

LEMMA ApplyDuplicateChildCompletionRefines ==
    ApplyDuplicateChildCompletion => ABS!Next
BY Z3T(10) DEF ApplyDuplicateChildCompletion, ABS!Next

LEMMA ApplyQueueMessageRefines == ApplyQueueMessage => ABS!Next
BY Z3T(10) DEF ApplyQueueMessage, ABS!Next

LEMMA ApplyConsumeMessageRefines == ApplyConsumeMessage => ABS!Next
BY Z3T(10) DEF ApplyConsumeMessage, ABS!Next

LEMMA ApplyFinalizeBatchRefines == ApplyFinalizeBatch => ABS!Next
BY Z3T(10) DEF ApplyFinalizeBatch, ABS!Next

LEMMA ApplyEndRunRefines == ApplyEndRun => ABS!Next
BY Z3T(10) DEF ApplyEndRun, ABS!Next

LEMMA ApplyCancelRefines == ApplyCancel => ABS!Next
BY Z3T(10) DEF ApplyCancel, ABS!Next

LEMMA ApplyStaleSettleRefines == ApplyStaleSettle => ABS!Next
BY Z3T(10) DEF ApplyStaleSettle, ABS!StaleSettle, ABS!Next

LEMMA NextRefines == Next => ABS!Next \/ UNCHANGED implVars
BY PrepareStutters, CrashPreparedStutters, ApplyClaimRefines,
   ApplyReclaimRefines, ApplyRequestToolPermissionRefines, ApplyApproveRefines,
   ApplyDenyRefines, ApplySupplyResultRefines, ApplyDirectStartRefines,
   ApplyCompleteImmediateRefines, InvokeRefines, ApplyCompleteRefines,
   ApplyRecoverReplaySafeRefines,
   ApplyRecoverIndeterminateRefines, ApplyStartChildRefines,
   ApplyAwaitChildRefines, ApplyResumeChildRefines, ApplyFinishChildRefines,
   ApplyDuplicateChildCompletionRefines, ApplyQueueMessageRefines,
   ApplyConsumeMessageRefines, ApplyFinalizeBatchRefines,
   ApplyEndRunRefines, ApplyCancelRefines,
   ApplyStaleSettleRefines DEF Next

LEMMA ConcreteStepRefines == [Next]_vars => [ABS!Next]_implVars
BY NextRefines DEF vars

THEOREM RuntimeRefinement == Spec => ABS!Spec
BY InitRefines, ConcreteStepRefines, PTL
   DEF Spec, ABS!Spec, ABS!vars, implVars

=============================================================================
