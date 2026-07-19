-------------------------- MODULE RustCommitSystem --------------------------
EXTENDS Naturals, Sequences, FiniteSets

\* Durable projection of the production Rust ThreadCommit boundary.  Unlike
\* RuntimeSystem, which also owns ingress leases and external invocation history,
\* this model has exactly the fields reconstructible from committed Run state,
\* ActiveToolBatch, and RunDelegations.  Rust integration tests serialize those
\* fields after every real async commit and TLC checks the resulting trace with
\* TraceIsRefinement below.
CONSTANTS Calls, AgentCalls, NoCall, MaxAttempts, MaxVersion

RunStates == {"Running", "Awaiting", "Ended"}
TicketKinds == {"None", "Approval", "Delegation", "Scheduled", "External"}
CallStates == {"Requested", "Executing", "Awaiting", "Completed", "Indeterminate"}
TerminalCallStates == {"Completed", "Indeterminate"}
BatchStates == {"Absent", "Open", "Finalized"}
LinkStates == {"Absent", "Open", "Completed", "CancelRequested"}

KernelAssumptions ==
    /\ AgentCalls \subseteq Calls
    /\ NoCall \notin Calls
    /\ MaxAttempts \in Nat
    /\ MaxVersion \in Nat

ASSUME KernelAssumptions

VARIABLE state

vars == <<state>>

StateValue(run, ticket, ticketCallValue, calls, attemptValues, batch, links, ver) ==
    [runState |-> run,
     ticketKind |-> ticket,
     ticketCall |-> ticketCallValue,
     callState |-> calls,
     attempts |-> attemptValues,
     batchState |-> batch,
     linkState |-> links,
     version |-> ver]

InitialState ==
    StateValue(
        "Running", "None", NoCall,
        [c \in Calls |-> "Requested"],
        [c \in Calls |-> 0],
        "Absent", [c \in Calls |-> "Absent"], 0)

TypeOK(s) ==
    /\ s.runState \in RunStates
    /\ s.ticketKind \in TicketKinds
    /\ s.ticketCall \in Calls \cup {NoCall}
    /\ s.callState \in [Calls -> CallStates]
    /\ s.attempts \in [Calls -> 0..MaxAttempts]
    /\ s.batchState \in BatchStates
    /\ s.linkState \in [Calls -> LinkStates]
    /\ s.version \in 0..MaxVersion

TicketCoherence(s) ==
    /\ (s.runState = "Awaiting") \equiv (s.ticketKind # "None")
    /\ (s.ticketKind = "None") \equiv (s.ticketCall = NoCall)
    /\ s.ticketKind # "None" => s.callState[s.ticketCall] = "Awaiting"
    /\ s.ticketKind = "Delegation" =>
          /\ s.ticketCall \in AgentCalls
          /\ s.linkState[s.ticketCall] = "Open"

BatchCoherence(s) ==
    /\ s.batchState = "Absent" =>
          \A c \in Calls: s.callState[c] = "Requested" /\ s.attempts[c] = 0
    /\ s.batchState = "Finalized" =>
          \A c \in Calls: s.callState[c] \in TerminalCallStates

AttemptCoherence(s) ==
    \A c \in Calls:
        /\ s.callState[c] = "Requested" => s.attempts[c] = 0
        /\ s.callState[c] = "Executing" => s.attempts[c] > 0

DelegationCoherence(s) ==
    \A c \in Calls:
        /\ c \notin AgentCalls => s.linkState[c] = "Absent"
        /\ s.linkState[c] = "Completed" => s.callState[c] = "Completed"
        /\ s.linkState[c] = "CancelRequested" => s.runState = "Ended"
        /\ s.linkState[c] = "Open" =>
              /\ c \in AgentCalls
              /\ s.callState[c] \in {"Executing", "Awaiting"}

EndedIsSealed(s) ==
    s.runState = "Ended" =>
        /\ s.ticketKind = "None"
        /\ s.batchState \in {"Absent", "Finalized"}
        /\ (s.batchState = "Finalized" =>
              \A c \in Calls: s.callState[c] \in TerminalCallStates)
        /\ \A c \in Calls: s.linkState[c] # "Open"

Safety(s) ==
    /\ TypeOK(s)
    /\ TicketCoherence(s)
    /\ BatchCoherence(s)
    /\ AttemptCoherence(s)
    /\ DelegationCoherence(s)
    /\ EndedIsSealed(s)

Bump(s, t) == s.version < MaxVersion /\ t.version = s.version + 1

PersistBatch(s, t) ==
    /\ s.runState = "Running"
    /\ s.batchState = "Absent"
    /\ t = StateValue(
          s.runState, s.ticketKind, s.ticketCall, s.callState, s.attempts,
          "Open", s.linkState, s.version + 1)
    /\ Bump(s, t)

CommitNoop(s, t) ==
    /\ s.runState # "Ended"
    /\ t = StateValue(
          s.runState, s.ticketKind, s.ticketCall, s.callState, s.attempts,
          s.batchState, s.linkState, s.version + 1)
    /\ Bump(s, t)

StartOrRetry(s, t, c) ==
    /\ c \in Calls
    /\ s.runState = "Running"
    /\ s.ticketKind = "None"
    /\ s.batchState = "Open"
    /\ s.callState[c] \in {"Requested", "Executing"}
    /\ s.attempts[c] < MaxAttempts
    /\ t = StateValue(
          s.runState, s.ticketKind, s.ticketCall,
          [s.callState EXCEPT ![c] = "Executing"],
          [s.attempts EXCEPT ![c] = @ + 1],
          s.batchState,
          [s.linkState EXCEPT ![c] = IF c \in AgentCalls THEN "Open" ELSE @],
          s.version + 1)
    /\ Bump(s, t)

AwaitCall(s, t, c, kind) ==
    /\ c \in Calls
    /\ kind \in TicketKinds \ {"None"}
    /\ s.runState = "Running"
    /\ s.ticketKind = "None"
    /\ s.batchState = "Open"
    /\ s.callState[c] \in {"Requested", "Executing"}
    /\ (kind = "Delegation" =>
          (c \in AgentCalls /\ s.linkState[c] = "Open"))
    /\ t = StateValue(
          "Awaiting", kind, c,
          [s.callState EXCEPT ![c] = "Awaiting"],
          s.attempts, s.batchState, s.linkState, s.version + 1)
    /\ Bump(s, t)

ResumeExecuting(s, t, c) ==
    /\ c \in Calls
    /\ s.runState = "Awaiting"
    /\ s.ticketCall = c
    /\ s.callState[c] = "Awaiting"
    /\ s.attempts[c] < MaxAttempts
    /\ t = StateValue(
          "Running", "None", NoCall,
          [s.callState EXCEPT ![c] = "Executing"],
          [s.attempts EXCEPT ![c] = @ + 1],
          s.batchState,
          [s.linkState EXCEPT ![c] = IF c \in AgentCalls THEN "Open" ELSE @],
          s.version + 1)
    /\ Bump(s, t)

CompleteCall(s, t, c) ==
    /\ c \in Calls
    /\ s.callState[c] \in {"Executing", "Awaiting"}
    /\ (s.runState = "Running" \/ s.ticketCall = c)
    /\ t = StateValue(
          IF s.runState = "Awaiting" THEN "Running" ELSE s.runState,
          IF s.ticketCall = c THEN "None" ELSE s.ticketKind,
          IF s.ticketCall = c THEN NoCall ELSE s.ticketCall,
          [s.callState EXCEPT ![c] = "Completed"],
          s.attempts, s.batchState,
          [s.linkState EXCEPT ![c] = IF @ = "Open" THEN "Completed" ELSE @],
          s.version + 1)
    /\ Bump(s, t)

CompleteAndFinalize(s, t, c) ==
    /\ c \in Calls
    /\ s.batchState = "Open"
    /\ s.callState[c] \in {"Executing", "Awaiting"}
    /\ (s.runState = "Running" \/ s.ticketCall = c)
    /\ \A d \in Calls \ {c}: s.callState[d] \in TerminalCallStates
    /\ t = StateValue(
          IF s.runState = "Awaiting" THEN "Running" ELSE s.runState,
          IF s.ticketCall = c THEN "None" ELSE s.ticketKind,
          IF s.ticketCall = c THEN NoCall ELSE s.ticketCall,
          [s.callState EXCEPT ![c] = "Completed"],
          s.attempts, "Finalized",
          [s.linkState EXCEPT ![c] = IF @ = "Open" THEN "Completed" ELSE @],
          s.version + 1)
    /\ Bump(s, t)

CompleteImmediate(s, t, c) ==
    /\ c \in Calls
    /\ s.runState = "Running"
    /\ s.batchState = "Open"
    /\ s.callState[c] = "Requested"
    /\ t = StateValue(
          s.runState, s.ticketKind, s.ticketCall,
          [s.callState EXCEPT ![c] = "Completed"],
          s.attempts, s.batchState, s.linkState, s.version + 1)
    /\ Bump(s, t)

CompleteImmediateAndFinalize(s, t, c) ==
    /\ c \in Calls
    /\ s.runState = "Running"
    /\ s.batchState = "Open"
    /\ s.callState[c] = "Requested"
    /\ \A d \in Calls \ {c}: s.callState[d] \in TerminalCallStates
    /\ t = StateValue(
          s.runState, s.ticketKind, s.ticketCall,
          [s.callState EXCEPT ![c] = "Completed"],
          s.attempts, "Finalized", s.linkState, s.version + 1)
    /\ Bump(s, t)

MarkIndeterminate(s, t, c) ==
    /\ c \in Calls
    /\ s.runState = "Running"
    /\ s.batchState = "Open"
    /\ s.callState[c] \notin TerminalCallStates
    /\ s.linkState[c] # "Open"
    /\ t = StateValue(
          s.runState, s.ticketKind, s.ticketCall,
          [s.callState EXCEPT ![c] = "Indeterminate"],
          s.attempts, s.batchState, s.linkState, s.version + 1)
    /\ Bump(s, t)

FinalizeBatch(s, t) ==
    /\ s.runState = "Running"
    /\ s.batchState = "Open"
    /\ \A c \in Calls: s.callState[c] \in TerminalCallStates
    /\ t = StateValue(
          s.runState, s.ticketKind, s.ticketCall, s.callState, s.attempts,
          "Finalized", s.linkState, s.version + 1)
    /\ Bump(s, t)

EndRun(s, t) ==
    /\ s.runState # "Ended"
    /\ t = StateValue(
          "Ended", "None", NoCall,
          IF s.batchState = "Absent" THEN s.callState
          ELSE [c \in Calls |->
            IF s.callState[c] \in TerminalCallStates THEN s.callState[c]
            ELSE "Indeterminate"],
          s.attempts,
          IF s.batchState = "Absent" THEN "Absent" ELSE "Finalized",
          [c \in Calls |-> IF s.linkState[c] = "Open"
                            THEN "CancelRequested" ELSE s.linkState[c]],
          s.version + 1)
    /\ Bump(s, t)

NextState(s, t) ==
    \/ PersistBatch(s, t)
    \/ CommitNoop(s, t)
    \/ \E c \in Calls: StartOrRetry(s, t, c)
    \/ \E c \in Calls, kind \in TicketKinds \ {"None"}:
           AwaitCall(s, t, c, kind)
    \/ \E c \in Calls: ResumeExecuting(s, t, c)
    \/ \E c \in Calls: CompleteCall(s, t, c)
    \/ \E c \in Calls: CompleteAndFinalize(s, t, c)
    \/ \E c \in Calls: CompleteImmediate(s, t, c)
    \/ \E c \in Calls: CompleteImmediateAndFinalize(s, t, c)
    \/ \E c \in Calls: MarkIndeterminate(s, t, c)
    \/ FinalizeBatch(s, t)
    \/ EndRun(s, t)

Init == state = InitialState
Next == NextState(state, state')
Spec == Init /\ [][Next]_vars
StateSafety == Safety(state)

\* A finite production trace is accepted only when it starts at the formal
\* initial state, every observed commit is one modeled durable transition, and
\* every projected state satisfies the proved invariant.  Generated trace
\* modules expose this predicate as a TLC invariant.
TraceIsRefinement(trace) ==
    /\ Len(trace) > 0
    /\ trace[1] = InitialState
    /\ \A i \in 1..Len(trace): Safety(trace[i])
    /\ \A i \in 1..(Len(trace) - 1): NextState(trace[i], trace[i + 1])

=============================================================================
