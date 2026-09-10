----------------------- MODULE SessionRunStopEvidence -----------------------
EXTENDS SessionRunProtocol

\* Participant evidence over the EXISTING composition, not another Dispatch.
\* Its aggregate external attempt contains four independent participant kinds.
\* Provider receipt authenticity and complete participant enrollment before
\* effects are trusted boundaries. Production Native refines one strict subset:
\* ModelOnlyV1 closes tool/hook participation before effects and journals every
\* model operation; all wider surfaces remain Unknown. This specification does
\* not infer absence from missing data.
Participants == {"local", "model", "tool", "hook"}
Attempts == Runs \X Workers \X (1..MaxEpoch)
Subject(participant) == <<participant[1], participant[2], participant[3]>>
Members(attempt) ==
    {<<attempt[1], attempt[2], attempt[3], kind>> : kind \in Participants}
Local(attempt) == <<attempt[1], attempt[2], attempt[3], "local">>
Hook(attempt) == <<attempt[1], attempt[2], attempt[3], "hook">>

VARIABLES issuedAttempts, activeParticipants, returnedParticipants, closedAttempts,
          completeReceipts
evidenceVars == <<issuedAttempts, activeParticipants, returnedParticipants,
                  closedAttempts, completeReceipts>>
stopVars == <<vars, evidenceVars>>

StopEvidenceInit ==
    /\ Init
    /\ issuedAttempts = {}
    /\ activeParticipants = {}
    /\ returnedParticipants = {}
    /\ closedAttempts = {}
    /\ completeReceipts = {}

\* R1: the canonical physical admission enrolls every possible participant
\* before any external effect. This bounded overapproximation allows model and
\* tool work to coexist; it does not assume their local Futures are sequential.
StartWithParticipants(run, worker, epoch) ==
    LET attempt == <<run, worker, epoch>> IN
    /\ StartExecution(run, worker, epoch)
    /\ issuedAttempts' = issuedAttempts \cup {attempt}
    /\ activeParticipants' = activeParticipants \cup Members(attempt)
    /\ UNCHANGED <<returnedParticipants, closedAttempts, completeReceipts>>

\* R2: process death, model return and tool return are independent physical
\* facts. Neither death nor a logical committed Ended changes other work.
ParticipantStops(participant) ==
    /\ participant \in activeParticipants
    /\ activeParticipants' = activeParticipants \ {participant}
    /\ UNCHANGED <<vars, issuedAttempts, returnedParticipants, closedAttempts,
                   completeReceipts>>

\* The provider's irreversible launch closure is separate from observing an
\* empty domain. Exact Worker Registry expiry and Kubernetes closure are
\* qualified by their own models/tests; this interface cannot forge that fact.
CloseExecutionDomain(attempt) ==
    /\ attempt \in issuedAttempts
    /\ closedAttempts' = closedAttempts \cup {attempt}
    /\ UNCHANGED <<vars, issuedAttempts, activeParticipants, returnedParticipants,
                   completeReceipts>>

\* R3: a delayed local launch may still arrive while the domain is open.
\* No new incarnation/epoch may be mistaken for reopening this exact domain.
DelayedLocalLaunch(attempt) ==
    /\ attempt \in issuedAttempts
    /\ attempt \notin closedAttempts
    /\ activeParticipants' = activeParticipants \cup {Local(attempt)}
    /\ UNCHANGED <<vars, issuedAttempts, returnedParticipants, closedAttempts,
                   completeReceipts>>

\* R4: trusted provider delivery, independent of Worker survival and leases.
\* Lost delivery is stutter; duplicates and reordered delivery are set union.
\* Remote terminal receipts include a no-restart guarantee for that operation.
ObserveParticipantReceipt(participant) ==
    /\ Subject(participant) \in issuedAttempts
    /\ participant \notin activeParticipants
    /\ participant[4] = "local" => Subject(participant) \in closedAttempts
    /\ returnedParticipants' = returnedParticipants \cup {participant}
    /\ UNCHANGED <<vars, issuedAttempts, activeParticipants, closedAttempts,
                   completeReceipts>>

CompleteEvidence(attempt) == Members(attempt) \subseteq returnedParticipants

\* R5: the trusted source may deliver a complete assertion only for ALL exact
\* receipts. This is repeatably queryable provider evidence, not a new Dispatch
\* database/table. Loss/crash before application leaves the physical slot intact.
\* Query is read-only: a handoff during IO cannot authorize cancelling its work.
ObserveCompleteReceipt(attempt) ==
    /\ CompleteEvidence(attempt)
    /\ completeReceipts' = completeReceipts \cup {attempt}
    /\ UNCHANGED <<vars, issuedAttempts, activeParticipants,
                   returnedParticipants, closedAttempts>>

\* R6: validate the delivered assertion's exact identity before original return.
\* The original FinishExecution keeps the exact owner/epoch clear and its
\* retired watermark. There is no new clear transition or settlement path.
ObserveCompleteReturn(attempt) ==
    /\ attempt \in completeReceipts
    /\ ObserveExternalReturn(attempt[1], attempt[2], attempt[3])
    /\ UNCHANGED evidenceVars

StopEvidenceNext ==
    \/ /\ CancellationNext
       \* Disable only the old abstract start/return; every Dispatch, Session,
       \* Work, cancellation and exact finish transition is reused unchanged.
       /\ UNCHANGED externalAttempts
       /\ UNCHANGED evidenceVars
    \/ \E run \in Runs, worker \in Workers, epoch \in 1..MaxEpoch:
         StartWithParticipants(run, worker, epoch)
    \/ \E participant \in (Runs \X Workers \X (1..MaxEpoch) \X Participants):
         ParticipantStops(participant) \/ ObserveParticipantReceipt(participant)
    \/ \E attempt \in Attempts:
         CloseExecutionDomain(attempt) \/ DelayedLocalLaunch(attempt)
         \/ ObserveCompleteReceipt(attempt) \/ ObserveCompleteReturn(attempt)

StopEvidenceSpec == StopEvidenceInit /\ [][StopEvidenceNext]_stopVars

\* Conditional progress only: do NOT assume that an opaque provider eventually
\* stops, that a partition heals, or that historical missing evidence appears.
\* Once all physical work stopped in a closed domain, fair trusted delivery and
\* original exact retirement suffice. No fairness is imposed on external stop.
StopEvidenceFairness ==
    /\ \A participant \in (Runs \X Workers \X (1..MaxEpoch) \X Participants):
         WF_stopVars(ObserveParticipantReceipt(participant))
    /\ \A attempt \in Attempts:
         /\ WF_stopVars(ObserveCompleteReceipt(attempt))
         /\ WF_stopVars(ObserveCompleteReturn(attempt))
         /\ WF_stopVars(FinishExecution(attempt[1], attempt[2], attempt[3])
                        /\ UNCHANGED evidenceVars)
StopEvidenceLiveSpec == StopEvidenceSpec /\ StopEvidenceFairness

ExactSlot(attempt) ==
    /\ attempt[1] \in executionStarted
    /\ executionOwner[attempt[1]] = attempt[2]
    /\ executionEpoch[attempt[1]] = attempt[3]

StopEvidenceTypeOK ==
    /\ issuedAttempts \subseteq Attempts
    /\ closedAttempts \subseteq issuedAttempts
    /\ activeParticipants \subseteq UNION {Members(a) : a \in issuedAttempts}
    /\ returnedParticipants \subseteq UNION {Members(a) : a \in issuedAttempts}
    /\ completeReceipts \subseteq issuedAttempts

CompleteReceiptIsSound == \A a \in completeReceipts: CompleteEvidence(a)

ReceiptIsSound ==
    /\ returnedParticipants \intersect activeParticipants = {}
    /\ \A p \in returnedParticipants:
         p[4] = "local" => Subject(p) \in closedAttempts

ParticipantsRetainAggregate ==
    \A p \in activeParticipants: Subject(p) \in externalAttempts

AggregateRetirementRequiresAllReceipts ==
    \A a \in issuedAttempts \ externalAttempts: CompleteEvidence(a)

StopEvidenceSafety ==
    /\ StopEvidenceTypeOK
    /\ ReceiptIsSound
    /\ CompleteReceiptIsSound
    /\ ParticipantsRetainAggregate
    /\ AggregateRetirementRequiresAllReceipts
    /\ Safety

CompletePhysicalStopEventuallyRetires ==
    \A a \in Attempts:
        (a \in closedAttempts /\ Members(a) \intersect activeParticipants = {})
          ~> (a \notin externalAttempts /\ ~ExactSlot(a))

\* One Run, two Workers, three epochs isolate participant evidence from the
\* separately checked same-Thread peer and three-domain scheduling dimensions.
StopEvidenceScope ==
    /\ RetirementScope
    /\ activityEpoch[ChildRunA] = 0
    /\ activityEpoch[ChildRunB] = 0

\* Positive witnesses reject a vacuous implementation that never starts,
\* never receives evidence, or blocks even after complete proof.
NoLocalOnlyStop == ~\E a \in externalAttempts:
    /\ Local(a) \in returnedParticipants
    /\ Members(a) \intersect activeParticipants # {}
NoRemoteOnlyStop == ~\E a \in externalAttempts:
    /\ Members(a) \ {Local(a)} \subseteq returnedParticipants
    /\ Local(a) \in activeParticipants
\* A local Future drop/unwind is not ParticipantStops. In particular a completed
\* tool does not terminate its AfterTool work. Runtime's observer tests exercise
\* that local distinction; this model assumes complete pre-effect enrollment.
NoHookOnlyOutstanding == ~\E a \in externalAttempts:
    /\ Members(a) \ {Hook(a)} \subseteq returnedParticipants
    /\ Hook(a) \in activeParticipants
NoCompleteEvidenceBeforeClear == ~\E a \in externalAttempts: a \in completeReceipts
NoSuccessorWithOldReceipt == ~\E old, next \in issuedAttempts:
    /\ old[1] = next[1] /\ old[3] < next[3]
    /\ old \in completeReceipts /\ ExactSlot(next)
NoTerminalWithOutstandingWork == ~\E a \in externalAttempts:
    /\ committedDisposition[a[1]] = "Ended"
    /\ Members(a) \intersect activeParticipants # {}
NoEmptyOpenDomain == ~\E a \in externalAttempts:
    /\ a \notin closedAttempts
    /\ Members(a) \intersect activeParticipants = {}

=============================================================================
