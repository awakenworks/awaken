------------------------------- MODULE ToolBatch -------------------------------
EXTENDS Naturals, RuntimeVocabulary

\* A finite safety model for a model-emitted batch of tool calls. Durable tool
\* state, the Run disposition, and the approval ticket change only in commit
\* actions. Invoke is the sole external-effect action and is enabled only after
\* the matching durable Executing transition.
CONSTANTS Calls, MaxAttempts, NeverReplay, ReplaySafe, NoCall, ReplayCalls

Policy(c) == IF c \in ReplayCalls THEN ReplaySafe ELSE NeverReplay


VARIABLES
    runState,
    callState,
    attempts,
    invokedAttempt,
    ticket,
    decision,
    published,
    commitVersion,
    endedOnce

vars == <<
    runState,
    callState,
    attempts,
    invokedAttempt,
    ticket,
    decision,
    published,
    commitVersion,
    endedOnce
>>

Init ==
    /\ runState = "Running"
    /\ callState = [c \in Calls |-> "Requested"]
    /\ attempts = [c \in Calls |-> 0]
    /\ invokedAttempt = [c \in Calls |-> 0]
    /\ ticket = NoCall
    /\ decision = [c \in Calls |-> "None"]
    /\ published = FALSE
    /\ commitVersion = 0
    /\ endedOnce = FALSE

UnchangedCallData == UNCHANGED <<attempts, invokedAttempt, decision>>
Committed == commitVersion' = commitVersion + 1

RequestToolPermission(c) ==
    /\ runState = "Running"
    /\ ticket = NoCall
    /\ callState[c] = "Requested"
    \* The production dispatcher pauses at the first gated call. This keeps one
    \* Run ticket authoritative while still modelling multiple calls per batch.
    /\ \A d \in Calls: callState[d] # "Executing"
    /\ runState' = "Awaiting"
    /\ callState' = [callState EXCEPT ![c] = "AwaitingToolPermission"]
    /\ ticket' = c
    /\ UnchangedCallData
    /\ UNCHANGED <<published, endedOnce>>
    /\ Committed

DirectStart(c) ==
    /\ runState = "Running"
    /\ ticket = NoCall
    /\ callState[c] = "Requested"
    /\ attempts[c] < MaxAttempts
    /\ callState' = [callState EXCEPT ![c] = "Executing"]
    /\ attempts' = [attempts EXCEPT ![c] = @ + 1]
    /\ UNCHANGED <<runState, invokedAttempt, ticket, decision, published, endedOnce>>
    /\ Committed

Approve(c) ==
    /\ runState = "Awaiting"
    /\ ticket = c
    /\ callState[c] = "AwaitingToolPermission"
    /\ attempts[c] < MaxAttempts
    /\ runState' = "Running"
    /\ callState' = [callState EXCEPT ![c] = "Executing"]
    /\ attempts' = [attempts EXCEPT ![c] = @ + 1]
    /\ ticket' = NoCall
    /\ decision' = [decision EXCEPT ![c] = "Approved"]
    /\ UNCHANGED <<invokedAttempt, published, endedOnce>>
    /\ Committed

Deny(c) ==
    /\ runState = "Awaiting"
    /\ ticket = c
    /\ callState[c] = "AwaitingToolPermission"
    /\ runState' = "Running"
    /\ callState' = [callState EXCEPT ![c] = "Completed"]
    /\ ticket' = NoCall
    /\ decision' = [decision EXCEPT ![c] = "Denied"]
    /\ UNCHANGED <<attempts, invokedAttempt, published, endedOnce>>
    /\ Committed

SupplyResult(c) ==
    /\ runState = "Awaiting"
    /\ ticket = c
    /\ callState[c] = "AwaitingToolPermission"
    /\ runState' = "Running"
    /\ callState' = [callState EXCEPT ![c] = "Completed"]
    /\ ticket' = NoCall
    /\ decision' = [decision EXCEPT ![c] = "ResultSupplied"]
    /\ UNCHANGED <<attempts, invokedAttempt, published, endedOnce>>
    /\ Committed

\* External execution is not a durable write. The attempt number was already
\* committed, and each attempt can enter the executor at most once.
Invoke(c) ==
    /\ callState[c] = "Executing"
    /\ invokedAttempt[c] < attempts[c]
    /\ invokedAttempt' = [invokedAttempt EXCEPT ![c] = attempts[c]]
    /\ UNCHANGED <<
        runState, callState, attempts, ticket, decision,
        published, commitVersion, endedOnce
       >>

Complete(c) ==
    /\ callState[c] = "Executing"
    /\ invokedAttempt[c] = attempts[c]
    /\ callState' = [callState EXCEPT ![c] = "Completed"]
    /\ UNCHANGED <<
        runState, attempts, invokedAttempt, ticket, decision, published, endedOnce
       >>
    /\ Committed

RecoverReplaySafe(c) ==
    /\ callState[c] = "Executing"
    /\ Policy(c) = ReplaySafe
    /\ attempts[c] < MaxAttempts
    /\ attempts' = [attempts EXCEPT ![c] = @ + 1]
    /\ UNCHANGED <<
        runState, callState, invokedAttempt, ticket, decision, published, endedOnce
       >>
    /\ Committed

RecoverIndeterminate(c) ==
    /\ callState[c] = "Executing"
    /\ \/ Policy(c) = NeverReplay
       \/ attempts[c] = MaxAttempts
    /\ callState' = [callState EXCEPT ![c] = "Indeterminate"]
    /\ UNCHANGED <<
        runState, attempts, invokedAttempt, ticket, decision, published, endedOnce
       >>
    /\ Committed

FinalizeBatch ==
    /\ ~published
    /\ \A c \in Calls: callState[c] \in TerminalToolCallStates
    /\ published' = TRUE
    /\ UNCHANGED <<
        runState, callState, attempts, invokedAttempt, ticket, decision, endedOnce
       >>
    /\ Committed

EndRun ==
    /\ runState = "Running"
    /\ published
    /\ runState' = "Ended"
    /\ endedOnce' = TRUE
    /\ UNCHANGED <<
        callState, attempts, invokedAttempt, ticket, decision, published
       >>
    /\ Committed

Cancel ==
    /\ runState # "Ended"
    /\ runState' = "Ended"
    /\ callState' = [c \in Calls |->
          IF callState[c] \in TerminalToolCallStates
          THEN callState[c]
          ELSE "Indeterminate"]
    /\ ticket' = NoCall
    /\ endedOnce' = TRUE
    /\ UNCHANGED <<attempts, invokedAttempt, decision, published>>
    /\ Committed

Next ==
    \/ \E c \in Calls: RequestToolPermission(c)
    \/ \E c \in Calls: DirectStart(c)
    \/ \E c \in Calls: Approve(c)
    \/ \E c \in Calls: Deny(c)
    \/ \E c \in Calls: SupplyResult(c)
    \/ \E c \in Calls: Invoke(c)
    \/ \E c \in Calls: Complete(c)
    \/ \E c \in Calls: RecoverReplaySafe(c)
    \/ \E c \in Calls: RecoverIndeterminate(c)
    \/ FinalizeBatch
    \/ EndRun
    \/ Cancel

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ runState \in CoreRunStates
    /\ callState \in [Calls -> ToolCallStates]
    /\ attempts \in [Calls -> 0..MaxAttempts]
    /\ invokedAttempt \in [Calls -> 0..MaxAttempts]
    /\ ticket \in Calls \cup {NoCall}
    /\ decision \in [Calls -> ToolDecisionStates]
    /\ published \in BOOLEAN
    /\ commitVersion \in Nat
    /\ endedOnce \in BOOLEAN

TicketIffAwaiting ==
    (runState = "Awaiting") \equiv
        (ticket \in Calls /\ callState[ticket] = "AwaitingToolPermission")

ExecutionWasCommitted ==
    \A c \in Calls:
        invokedAttempt[c] > 0 =>
            /\ attempts[c] >= invokedAttempt[c]
            /\ commitVersion > 0

ToolPermissionCannotBeBypassed ==
    \A c \in Calls:
        callState[c] = "AwaitingToolPermission" =>
            /\ attempts[c] = 0
            /\ invokedAttempt[c] = 0

DeniedCallsNeverRun ==
    \A c \in Calls: decision[c] = "Denied" => invokedAttempt[c] = 0

TerminalCallsDoNotReopen ==
    \A c \in Calls:
        callState[c] \in TerminalToolCallStates => invokedAttempt[c] <= attempts[c]

PublicationBarrier ==
    published => \A c \in Calls: callState[c] \in TerminalToolCallStates

EndedIsAbsorbing == endedOnce => runState = "Ended"

Safety ==
    /\ TypeOK
    /\ TicketIffAwaiting
    /\ ExecutionWasCommitted
    /\ ToolPermissionCannotBeBypassed
    /\ DeniedCallsNeverRun
    /\ TerminalCallsDoNotReopen
    /\ PublicationBarrier
    /\ EndedIsAbsorbing

=============================================================================
