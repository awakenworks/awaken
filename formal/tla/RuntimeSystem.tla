----------------------------- MODULE RuntimeSystem -----------------------------
EXTENDS Naturals, RuntimeVocabulary

\* Composed safety specification for the durable Runtime boundary. Unlike the
\* component models, this module shares one parent Run across dispatch, a
\* model-emitted tool batch, delegated child execution, cancellation, and the
\* durable inbox. External invocation is represented by the history variable
\* invokedAttempt; it is deliberately not treated as persisted state.
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

RuntimeAssumptions ==
    /\ MaxEpoch \in Nat
    /\ MaxAttempts \in Nat
    /\ MaxInbox \in Nat
    /\ MaxVersion \in Nat
    /\ NoOwner \notin Owners
    /\ NoCall \notin Calls
    /\ AgentCall \in Calls
    /\ ReplayCalls \subseteq Calls

ASSUME RuntimeAssumptions

TicketKinds == {"None", "Approval", "Delegation"}
DispatchStates == {"Pending", "Leased", "Awaiting", "Removed"}

VARIABLES
    runState,
    ticketKind,
    ticketCall,
    dispatchState,
    owner,
    leaseEpoch,
    callState,
    attempts,
    invokedAttempt,
    decision,
    published,
    childState,
    linkStatus,
    inboxCount,
    commitVersion,
    endedOnce

vars == <<
    runState,
    ticketKind,
    ticketCall,
    dispatchState,
    owner,
    leaseEpoch,
    callState,
    attempts,
    invokedAttempt,
    decision,
    published,
    childState,
    linkStatus,
    inboxCount,
    commitVersion,
    endedOnce
>>

Init ==
    /\ AgentCall \in Calls
    /\ runState = "Running"
    /\ ticketKind = "None"
    /\ ticketCall = NoCall
    /\ dispatchState = "Pending"
    /\ owner = NoOwner
    /\ leaseEpoch = 0
    /\ callState = [c \in Calls |-> "Requested"]
    /\ attempts = [c \in Calls |-> 0]
    /\ invokedAttempt = [c \in Calls |-> 0]
    /\ decision = [c \in Calls |-> "None"]
    /\ published = FALSE
    /\ childState = "Absent"
    /\ linkStatus = "Absent"
    /\ inboxCount = 0
    /\ commitVersion = 0
    /\ endedOnce = FALSE

Committed ==
    /\ commitVersion < MaxVersion
    /\ commitVersion' = commitVersion + 1

Claim(candidate) ==
    /\ candidate \in Owners
    /\ runState = "Running"
    /\ dispatchState = "Pending"
    /\ owner = NoOwner
    /\ leaseEpoch < MaxEpoch
    /\ dispatchState' = "Leased"
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ UNCHANGED <<runState, ticketKind, ticketCall, callState, attempts,
                    invokedAttempt, decision, published, childState,
                    linkStatus, inboxCount,
                    endedOnce>>
    /\ Committed

Reclaim(candidate) ==
    /\ candidate \in Owners
    /\ dispatchState = "Leased"
    /\ candidate # owner
    /\ leaseEpoch < MaxEpoch
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, callState,
                    attempts, invokedAttempt, decision, published, childState,
                    linkStatus, inboxCount,
                    endedOnce>>
    /\ Committed

RequestApproval(c) ==
    /\ c \in Calls
    /\ runState = "Running"
    /\ dispatchState = "Leased"
    /\ ticketKind = "None"
    /\ callState[c] = "Requested"
    /\ \A d \in Calls: callState[d] # "Executing"
    /\ runState' = "Awaiting"
    /\ ticketKind' = "Approval"
    /\ ticketCall' = c
    /\ dispatchState' = "Awaiting"
    /\ owner' = NoOwner
    /\ callState' = [callState EXCEPT ![c] = "AwaitingApproval"]
    /\ UNCHANGED <<leaseEpoch, attempts, invokedAttempt, decision, published,
                    childState, linkStatus,
                    inboxCount, endedOnce>>
    /\ Committed

Approve(c, candidate) ==
    /\ c \in Calls
    /\ candidate \in Owners
    /\ runState = "Awaiting"
    /\ dispatchState = "Awaiting"
    /\ ticketKind = "Approval"
    /\ ticketCall = c
    /\ callState[c] = "AwaitingApproval"
    /\ attempts[c] < MaxAttempts
    /\ leaseEpoch < MaxEpoch
    /\ runState' = "Running"
    /\ ticketKind' = "None"
    /\ ticketCall' = NoCall
    /\ dispatchState' = "Leased"
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ callState' = [callState EXCEPT ![c] = "Executing"]
    /\ attempts' = [attempts EXCEPT ![c] = @ + 1]
    /\ decision' = [decision EXCEPT ![c] = "Approved"]
    /\ UNCHANGED <<invokedAttempt, published, childState, linkStatus, inboxCount, endedOnce>>
    /\ Committed

Deny(c, candidate) ==
    /\ c \in Calls
    /\ candidate \in Owners
    /\ runState = "Awaiting"
    /\ dispatchState = "Awaiting"
    /\ ticketKind = "Approval"
    /\ ticketCall = c
    /\ callState[c] = "AwaitingApproval"
    /\ leaseEpoch < MaxEpoch
    /\ runState' = "Running"
    /\ ticketKind' = "None"
    /\ ticketCall' = NoCall
    /\ dispatchState' = "Leased"
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ callState' = [callState EXCEPT ![c] = "Completed"]
    /\ decision' = [decision EXCEPT ![c] = "Denied"]
    /\ UNCHANGED <<attempts, invokedAttempt, published, childState,
                    linkStatus, inboxCount,
                    endedOnce>>
    /\ Committed

SupplyResult(c, candidate) ==
    /\ c \in Calls
    /\ candidate \in Owners
    /\ runState = "Awaiting"
    /\ dispatchState = "Awaiting"
    /\ ticketKind = "Approval"
    /\ ticketCall = c
    /\ callState[c] = "AwaitingApproval"
    /\ leaseEpoch < MaxEpoch
    /\ runState' = "Running"
    /\ ticketKind' = "None"
    /\ ticketCall' = NoCall
    /\ dispatchState' = "Leased"
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ callState' = [callState EXCEPT ![c] = "Completed"]
    /\ decision' = [decision EXCEPT ![c] = "ResultSupplied"]
    /\ UNCHANGED <<attempts, invokedAttempt, published, childState,
                    linkStatus, inboxCount, endedOnce>>
    /\ Committed

DirectStart(c) ==
    /\ c \in Calls
    /\ runState = "Running"
    /\ dispatchState = "Leased"
    /\ ticketKind = "None"
    /\ callState[c] = "Requested"
    /\ attempts[c] < MaxAttempts
    /\ callState' = [callState EXCEPT ![c] = "Executing"]
    /\ attempts' = [attempts EXCEPT ![c] = @ + 1]
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, invokedAttempt, decision, published, childState,
                    linkStatus, inboxCount,
                    endedOnce>>
    /\ Committed

CompleteImmediate(c) ==
    /\ c \in Calls
    /\ callState[c] = "Requested"
    /\ callState' = [callState EXCEPT ![c] = "Completed"]
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, attempts, invokedAttempt, decision, published,
                    childState, linkStatus, inboxCount, endedOnce>>
    /\ Committed

Invoke(c) ==
    /\ c \in Calls
    /\ callState[c] = "Executing"
    /\ invokedAttempt[c] < attempts[c]
    /\ invokedAttempt' = [invokedAttempt EXCEPT ![c] = attempts[c]]
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, callState, attempts, decision, published,
                    childState, linkStatus,
                    inboxCount, commitVersion, endedOnce>>

Complete(c) ==
    /\ c \in Calls \ {AgentCall}
    /\ callState[c] = "Executing"
    /\ invokedAttempt[c] = attempts[c]
    /\ callState' = [callState EXCEPT ![c] = "Completed"]
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, attempts, invokedAttempt, decision, published,
                    childState, linkStatus,
                    inboxCount, endedOnce>>
    /\ Committed

RecoverReplaySafe(c) ==
    /\ c \in Calls
    /\ c \in ReplayCalls
    /\ c # AgentCall
    /\ callState[c] = "Executing"
    /\ attempts[c] < MaxAttempts
    /\ attempts' = [attempts EXCEPT ![c] = @ + 1]
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, callState, invokedAttempt, decision, published,
                    childState, linkStatus,
                    inboxCount, endedOnce>>
    /\ Committed

RecoverIndeterminate(c) ==
    /\ c \in Calls \ {AgentCall}
    /\ callState[c] = "Executing"
    /\ \/ c \notin ReplayCalls
       \/ attempts[c] = MaxAttempts
    /\ callState' = [callState EXCEPT ![c] = "Indeterminate"]
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, attempts, invokedAttempt, decision, published,
                    childState, linkStatus,
                    inboxCount, endedOnce>>
    /\ Committed

StartChild ==
    /\ runState = "Running"
    /\ dispatchState = "Leased"
    /\ ticketKind = "None"
    /\ callState[AgentCall] = "Executing"
    /\ invokedAttempt[AgentCall] = attempts[AgentCall]
    /\ childState = "Absent"
    /\ childState' = "Running"
    /\ linkStatus' = "Open"
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, callState, attempts, invokedAttempt, decision,
                    published, inboxCount, endedOnce>>
    /\ Committed

AwaitChild ==
    /\ runState = "Running"
    /\ dispatchState = "Leased"
    /\ ticketKind = "None"
    /\ childState = "Running"
    /\ linkStatus = "Open"
    /\ runState' = "Awaiting"
    /\ ticketKind' = "Delegation"
    /\ ticketCall' = AgentCall
    /\ dispatchState' = "Awaiting"
    /\ owner' = NoOwner
    /\ childState' = "Awaiting"
    /\ UNCHANGED <<leaseEpoch, callState, attempts, invokedAttempt, decision,
                    published, linkStatus,
                    inboxCount, endedOnce>>
    /\ Committed

ResumeChild ==
    /\ childState = "Awaiting"
    /\ linkStatus = "Open"
    /\ childState' = "Running"
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, callState, attempts, invokedAttempt, decision,
                    published, linkStatus,
                    inboxCount, endedOnce>>
    /\ Committed

FinishChild(candidate) ==
    /\ candidate \in Owners
    /\ childState \in {"Running", "Awaiting"}
    /\ linkStatus = "Open"
    /\ \/ /\ runState = "Running"
          /\ dispatchState = "Leased"
          /\ ticketKind = "None"
          /\ candidate = owner
       \/ /\ runState = "Awaiting"
          /\ dispatchState = "Awaiting"
          /\ ticketKind = "Delegation"
          /\ ticketCall = AgentCall
          /\ leaseEpoch < MaxEpoch
    /\ runState' = "Running"
    /\ ticketKind' = "None"
    /\ ticketCall' = NoCall
    /\ dispatchState' = "Leased"
    /\ owner' = IF runState = "Awaiting" THEN candidate ELSE owner
    /\ leaseEpoch' = IF runState = "Awaiting"
                       THEN leaseEpoch + 1 ELSE leaseEpoch
    /\ childState' = "Ended"
    /\ linkStatus' = "Completed"
    /\ callState' = [callState EXCEPT ![AgentCall] = "Completed"]
    /\ UNCHANGED <<attempts, invokedAttempt, decision, published, inboxCount,
                    endedOnce>>
    /\ Committed

DuplicateChildCompletion ==
    /\ linkStatus = "Completed"
    /\ UNCHANGED vars

QueueMessage ==
    /\ runState # "Ended"
    /\ inboxCount < MaxInbox
    /\ inboxCount' = inboxCount + 1
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, callState, attempts, invokedAttempt, decision,
                    published, childState, linkStatus, endedOnce>>
    /\ Committed

ConsumeMessage ==
    /\ runState = "Running"
    /\ dispatchState = "Leased"
    /\ inboxCount > 0
    /\ inboxCount' = inboxCount - 1
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, callState, attempts, invokedAttempt, decision,
                    published, childState, linkStatus, endedOnce>>
    /\ Committed

FinalizeBatch ==
    /\ ~published
    /\ \A c \in Calls: callState[c] \in TerminalToolCallStates
    /\ published' = TRUE
    /\ UNCHANGED <<runState, ticketKind, ticketCall, dispatchState, owner,
                    leaseEpoch, callState, attempts, invokedAttempt, decision,
                    childState, linkStatus,
                    inboxCount, endedOnce>>
    /\ Committed

EndRun ==
    /\ runState = "Running"
    /\ published
    /\ runState' = "Ended"
    /\ dispatchState' = "Removed"
    /\ owner' = NoOwner
    /\ endedOnce' = TRUE
    /\ UNCHANGED <<ticketKind, ticketCall, leaseEpoch, callState, attempts,
                    invokedAttempt, decision, published, childState,
                    linkStatus, inboxCount>>
    /\ Committed

Cancel ==
    /\ runState # "Ended"
    /\ runState' = "Ended"
    /\ ticketKind' = "None"
    /\ ticketCall' = NoCall
    /\ dispatchState' = "Removed"
    /\ owner' = NoOwner
    /\ callState' = [c \in Calls |->
          IF callState[c] \in TerminalToolCallStates
          THEN callState[c]
          ELSE "Indeterminate"]
    /\ linkStatus' = IF linkStatus = "Open"
                      THEN "CancelRequested" ELSE linkStatus
    /\ endedOnce' = TRUE
    /\ UNCHANGED <<leaseEpoch, attempts, invokedAttempt, decision, published,
                    childState, inboxCount>>
    /\ Committed

StaleSettle(candidate, epoch) ==
    /\ candidate \in Owners
    /\ epoch \in 0..MaxEpoch
    /\ epoch # leaseEpoch
    /\ UNCHANGED vars

Next ==
    \/ \E candidate \in Owners: Claim(candidate)
    \/ \E candidate \in Owners: Reclaim(candidate)
    \/ \E c \in Calls: RequestApproval(c)
    \/ \E c \in Calls, candidate \in Owners: Approve(c, candidate)
    \/ \E c \in Calls, candidate \in Owners: Deny(c, candidate)
    \/ \E c \in Calls, candidate \in Owners: SupplyResult(c, candidate)
    \/ \E c \in Calls: DirectStart(c)
    \/ \E c \in Calls: CompleteImmediate(c)
    \/ \E c \in Calls: Invoke(c)
    \/ \E c \in Calls: Complete(c)
    \/ \E c \in Calls: RecoverReplaySafe(c)
    \/ \E c \in Calls: RecoverIndeterminate(c)
    \/ StartChild
    \/ AwaitChild
    \/ ResumeChild
    \/ \E candidate \in Owners: FinishChild(candidate)
    \/ DuplicateChildCompletion
    \/ QueueMessage
    \/ ConsumeMessage
    \/ FinalizeBatch
    \/ EndRun
    \/ Cancel
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           StaleSettle(candidate, epoch)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ runState \in CoreRunStates
    /\ ticketKind \in TicketKinds
    /\ ticketCall \in Calls \cup {NoCall}
    /\ dispatchState \in DispatchStates
    /\ owner \in Owners \cup {NoOwner}
    /\ leaseEpoch \in 0..MaxEpoch
    /\ callState \in [Calls -> ToolCallStates]
    /\ attempts \in [Calls -> 0..MaxAttempts]
    /\ invokedAttempt \in [Calls -> 0..MaxAttempts]
    /\ decision \in [Calls -> ToolDecisionStates]
    /\ published \in BOOLEAN
    /\ childState \in DelegatedChildStates
    /\ linkStatus \in DelegationLinkStatuses
    /\ inboxCount \in 0..MaxInbox
    /\ commitVersion \in 0..MaxVersion
    /\ endedOnce \in BOOLEAN

RunDispatchCoherence ==
    /\ (runState = "Awaiting") \equiv (dispatchState = "Awaiting")
    /\ (runState = "Ended") \equiv (dispatchState = "Removed")
    /\ (runState = "Running") \equiv
          (dispatchState \in {"Pending", "Leased"})
    /\ (dispatchState = "Leased") \equiv (owner \in Owners)

TicketCoherence ==
    /\ (ticketKind = "None") \equiv (ticketCall = NoCall)
    /\ (runState = "Awaiting") \equiv (ticketKind # "None")
    /\ (ticketKind = "Approval") =>
          callState[ticketCall] = "AwaitingApproval"
    /\ (ticketKind = "Delegation") =>
          (ticketCall = AgentCall /\ childState \in {"Running", "Awaiting"}
           /\ linkStatus = "Open")

ExecutionWasCommitted ==
    \A c \in Calls:
        /\ invokedAttempt[c] <= attempts[c]
        /\ invokedAttempt[c] > 0 => commitVersion > 0

AttemptsAreCommitted ==
    \A c \in Calls: attempts[c] > 0 => commitVersion > 0

ApprovalCannotBeBypassed ==
    \A c \in Calls:
        callState[c] = "AwaitingApproval" =>
            /\ attempts[c] = 0
            /\ invokedAttempt[c] = 0
            /\ decision[c] = "None"

DeniedCallsNeverRun ==
    \A c \in Calls: decision[c] = "Denied" => invokedAttempt[c] = 0

RequestedCallsAreFresh ==
    \A c \in Calls:
        callState[c] = "Requested" =>
            /\ attempts[c] = 0
            /\ invokedAttempt[c] = 0
            /\ decision[c] = "None"

DeniedCallsAreTerminal ==
    \A c \in Calls:
        decision[c] = "Denied" => callState[c] = "Completed"

PublicationBarrier ==
    published => \A c \in Calls: callState[c] \in TerminalToolCallStates

DelegationCompletionIsToolOwned ==
    /\ (linkStatus = "Completed") =>
          (childState = "Ended" /\ callState[AgentCall] = "Completed")
    /\ (childState = "Ended") => linkStatus = "Completed"

OpenDelegationHasLiveChild ==
    linkStatus = "Open" => childState \in {"Running", "Awaiting"}

CancellationIsDurable ==
    (linkStatus = "CancelRequested") =>
        (runState = "Ended" /\ childState \in {"Running", "Awaiting"})

MessagesCannotApprove ==
    \A c \in Calls:
        decision[c] = "Approved" =>
            (attempts[c] > 0 /\ callState[c] # "AwaitingApproval")

EndedIsAbsorbing == endedOnce => runState = "Ended"

Safety ==
    /\ TypeOK
    /\ RunDispatchCoherence
    /\ TicketCoherence
    /\ ExecutionWasCommitted
    /\ AttemptsAreCommitted
    /\ ApprovalCannotBeBypassed
    /\ DeniedCallsNeverRun
    /\ RequestedCallsAreFresh
    /\ DeniedCallsAreTerminal
    /\ PublicationBarrier
    /\ DelegationCompletionIsToolOwned
    /\ OpenDelegationHasLiveChild
    /\ CancellationIsDurable
    /\ MessagesCannotApprove
    /\ EndedIsAbsorbing

=============================================================================
