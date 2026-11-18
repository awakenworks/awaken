---------------------------- MODULE S3Publication ----------------------------
EXTENDS Naturals, Sequences, FiniteSets

\* Two requests may carry the same operation and claim, or address distinct
\* Threads of one Session. Each Run has one operation in this protocol model.
\* StoreHistory separately explores multiple ordinals and terminal transitions.
Actors == {1, 2}
\* The two actors carry distinct bodies. Both may target either Run. A retry
\* of an exact body is represented by its own actor. One Run can be taken over.
Operations == {1, 2}
Payloads == {1, 2}
CONSTANTS NoReceipt, NoDraft

VARIABLES head, history, receipts, pending, objects, acknowledged, recoveredPrefix,
          acceptedAuthority

vars == <<head, history, receipts, pending, objects, acknowledged, recoveredPrefix,
          acceptedAuthority>>

Shared == INSTANCE StoreHistory WITH
    Threads <- Operations, Runs <- Operations, Operations <- Operations,
    Payloads <- Payloads, RunStates <- {"running"}, TerminalStates <- {},
    NoReceipt <- NoReceipt, NoOperation <- 0, NoPayload <- 0,
    ObservationTimes <- {0}, UnknownTime <- 1,
    history <- history, receipts <- receipts,
    recoveredPrefix <- recoveredPrefix

EmptyRequest ==
    [phase |-> "idle", operation |-> 1, payload |-> 1, epoch |-> 1,
     binding |-> "local", version |-> 0, expected |-> 0, proposal |-> NoDraft]

Init ==
    /\ history = <<>>
    /\ receipts = [o \in Operations |-> NoReceipt]
    /\ head = [generation |-> 0, epochs |-> [o \in Operations |-> 1],
               checkpoints |-> Operations]
    /\ pending = [a \in Actors |-> EmptyRequest]
    /\ objects = {}
    /\ acknowledged = [o \in Operations |-> NoReceipt]
    /\ recoveredPrefix = <<>>
    /\ acceptedAuthority = TRUE

Authorized(request) ==
    /\ request.binding = "local"
    /\ request.epoch = head.epochs[request.operation]

ReadGrant(a, operation, payload, binding) ==
    /\ pending[a].phase = "idle"
    /\ pending' = [pending EXCEPT ![a] =
        [EmptyRequest EXCEPT !.phase = "claimed", !.operation = operation,
         !.payload = payload, !.epoch = head.epochs[operation],
         !.binding = binding, !.version = Shared!ThreadVersion(operation)]]
    /\ UNCHANGED <<head, history, receipts, objects, acknowledged, recoveredPrefix, acceptedAuthority>>

Prepare(a) ==
    LET request == pending[a]
        op == request.operation
        proposed == Shared!AppendRecord(op, op, "running", op, request.payload, 0)
        receipt == Shared!OperationReceipt(op, request.payload, Len(proposed), request.version, 0, 0)
    IN /\ request.phase = "claimed"
       /\ Authorized(request)
       /\ Shared!OperationAcceptable(op, op, "running", op, request.version, 0)
       /\ pending' = [pending EXCEPT ![a].phase = "prepared",
            ![a].expected = head.generation,
            ![a].proposal = [history |-> proposed,
                receipts |-> [receipts EXCEPT ![op] = receipt],
                head |-> [head EXCEPT !.generation = @ + 1, !.checkpoints = @ \ {op}]] ]
       /\ UNCHANGED <<head, history, receipts, objects, acknowledged, recoveredPrefix, acceptedAuthority>>

Upload(a) ==
    /\ pending[a].phase = "prepared"
    /\ objects' = objects \union {<<pending[a].operation, pending[a].payload>>}
    /\ pending' = [pending EXCEPT ![a].phase = "uploaded"]
    /\ UNCHANGED <<head, history, receipts, acknowledged, recoveredPrefix, acceptedAuthority>>

\* The server checks the captured version. The client evaluated authority in
\* Prepare on that exact head. The server does not check the lease clock.
Publish(a) ==
    LET request == pending[a]
    IN /\ request.phase = "uploaded"
       /\ request.expected = head.generation
       /\ head' = request.proposal.head
       /\ history' = request.proposal.history
       /\ receipts' = request.proposal.receipts
       /\ pending' = [pending EXCEPT ![a].phase = "waiting"]
       /\ acceptedAuthority' = Authorized(request)
       /\ UNCHANGED <<objects, acknowledged, recoveredPrefix>>

Acknowledge(a) ==
    LET request == pending[a]
    IN /\ request.phase = "waiting"
       /\ acknowledged' = [acknowledged EXCEPT
            ![request.operation] = request.proposal.receipts[request.operation]]
       /\ pending' = [pending EXCEPT ![a] = EmptyRequest]
       /\ UNCHANGED <<head, history, receipts, objects, recoveredPrefix, acceptedAuthority>>

\* An uncertain response may follow publication or an upload that never won.
\* Retry keeps operation identity and payload. It may reacquire a current grant.
LoseResponse(a) ==
    /\ pending[a].phase \in {"uploaded", "waiting"}
    /\ pending' = [pending EXCEPT ![a].phase =
                       IF @ = "waiting" THEN "uncertain-committed" ELSE "uncertain-unapplied",
                       ![a].proposal = NoDraft, ![a].expected = 0]
    /\ UNCHANGED <<head, history, receipts, objects, acknowledged, recoveredPrefix, acceptedAuthority>>

Retry(a, epoch) ==
    /\ pending[a].phase \in {"uncertain-committed", "uncertain-unapplied"}
    /\ pending' = [pending EXCEPT ![a].phase = "claimed", ![a].epoch = epoch]
    /\ UNCHANGED <<head, history, receipts, objects, acknowledged, recoveredPrefix, acceptedAuthority>>

Replay(a) ==
    LET request == pending[a]
        receipt == receipts[request.operation]
    IN /\ request.phase = "claimed"
       /\ Authorized(request)
       /\ receipt /= NoReceipt
       /\ receipt.payload = request.payload
       /\ acknowledged' = [acknowledged EXCEPT ![request.operation] = receipt]
       /\ pending' = [pending EXCEPT ![a] = EmptyRequest]
       /\ UNCHANGED <<head, history, receipts, objects, recoveredPrefix, acceptedAuthority>>

Reject(a) ==
    LET request == pending[a]
        receipt == receipts[request.operation]
    IN /\ \/ /\ request.phase = "claimed"
             /\ \/ ~Authorized(request)
                \/ IF receipt = NoReceipt
                   THEN request.version /= Shared!ThreadVersion(request.operation)
                   ELSE receipt.payload /= request.payload
          \/ /\ request.phase = "uploaded"
             /\ request.expected /= head.generation
       /\ pending' = [pending EXCEPT ![a] = EmptyRequest]
       /\ UNCHANGED <<head, history, receipts, objects, acknowledged, recoveredPrefix, acceptedAuthority>>

Takeover(op) ==
    /\ head.epochs[op] = 1
    /\ head' = [head EXCEPT !.epochs[op] = 2, !.generation = @ + 1]
    /\ UNCHANGED <<history, receipts, pending, objects, acknowledged, recoveredPrefix,
                   acceptedAuthority>>

Recover ==
    /\ Shared!Recover
    /\ UNCHANGED <<head, pending, objects, acknowledged, acceptedAuthority>>

Crash(a) ==
    /\ pending' = [pending EXCEPT ![a] = EmptyRequest]
    /\ recoveredPrefix' = <<>>
    /\ UNCHANGED <<head, history, receipts, objects, acknowledged, acceptedAuthority>>

Next ==
    \/ \E a \in Actors, o \in Operations : ReadGrant(a, o, a, "local")
    \/ ReadGrant(2, 1, 2, "foreign")
    \/ \E a \in Actors :
         Prepare(a) \/ Upload(a) \/ Publish(a) \/ Acknowledge(a)
         \/ LoseResponse(a) \/ Replay(a) \/ Reject(a) \/ Crash(a)
         \/ \E e \in {pending[a].epoch, head.epochs[pending[a].operation]} : Retry(a, e)
    \/ Takeover(1)
    \/ Recover

Spec == Init /\ [][Next]_vars
RefinesStoreHistory == Shared!Spec

Records == {history[i] : i \in 1..Len(history)}

PublishedReferencesExist ==
    \A record \in Records : <<record.operation, record.payload>> \in objects

EveryPublishedOperationHasReceipt ==
    \A record \in Records :
        receipts[record.operation] /= NoReceipt

CommittedCheckpointsRetired ==
    \A record \in Records : record.run \notin head.checkpoints

AcknowledgedReceiptsStayPublished ==
    \A op \in Operations :
      acknowledged[op] /= NoReceipt => receipts[op] = acknowledged[op]

PublicationWasAuthorized == acceptedAuthority

\* Reachability witnesses. Their negations must fail independently of mutants.
NoConcurrentDrafts ==
    ~(\A a \in Actors : pending[a].phase = "uploaded")
NoLostAcknowledgement ==
    ~(\E a \in Actors : pending[a].phase = "uncertain-committed")
NoStagedTakeover ==
    ~(\E a \in Actors : pending[a].phase = "uploaded" /\
                        pending[a].epoch /= head.epochs[pending[a].operation])

\* Read-only checks over one finite, actually observed streaming execution.
\* This does not claim universal Rust-to-model refinement or a latency bound.
ObservedStreamCoordinate(frame) == <<frame.step, frame.response, frame.attempt>>
ObservedStreamPositions(frames) ==
    /\ \A i \in 1..Len(frames) : frames[i].sequence = i
    /\ \A i \in 1..Len(frames) :
         frames[i].after =
           Cardinality({j \in 1..i :
             ObservedStreamCoordinate(frames[j]) = ObservedStreamCoordinate(frames[i])})

StreamOrderRefinement(trace) ==
    /\ trace.stream_count >= 2
    /\ trace.frames_per_stream > 0
    /\ trace.peak_writers >= 2
    /\ trace.watermark = trace.stream_count * trace.frames_per_stream
    /\ Len(trace.live) = trace.watermark
    /\ Cardinality({ObservedStreamCoordinate(trace.live[i]) :
                    i \in 1..Len(trace.live)}) = trace.stream_count
    /\ \A i \in 1..Len(trace.live) :
         Cardinality({j \in 1..Len(trace.live) :
           ObservedStreamCoordinate(trace.live[j]) =
             ObservedStreamCoordinate(trace.live[i])}) = trace.frames_per_stream
    /\ trace.frozen = trace.live
    /\ Len(trace.tail) = 1
    /\ ObservedStreamPositions(trace.live \o trace.tail)
    /\ trace.checkpoint_before
    /\ ~trace.checkpoint_after

=============================================================================
