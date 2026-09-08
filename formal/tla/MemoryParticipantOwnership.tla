--------------------- MODULE MemoryParticipantOwnership ---------------------
EXTENDS Naturals

\* Live Provider only: one physical root, one Copy/FUSE mount operation, two
\* callers. The retained mount future and returned guard are one participant.
\* Current marker/Session admission and root identity are assumed ports, NOT a
\* second lifecycle authority. The typed mount outcome and retained unknown
\* recovery identity are modeled here. Worker death, backend commit fencing,
\* kernel teardown and Rust refinement are outside this model.
VARIABLE s
Callers == {1, 2}
Init == s \in [phase : {"empty"}, held : {FALSE}, stage : {"idle"},
    borrower : {0}, confirmed : {FALSE}, mode : {"copy", "fuse"},
    current : {TRUE}, root : {TRUE}, matching : {TRUE}, calls : {0},
    ack : {FALSE}, cleaned : {FALSE}, purpose : {"replay", "terminal"},
    cancelled : {FALSE}, failed : {FALSE}]
Admitted == s.current /\ s.root
Borrow(c) == /\ s.borrower = 0
             /\ s' = [s EXCEPT !.borrower = c]
Cancel == /\ s.borrower \in Callers
          /\ s' = [s EXCEPT !.borrower = 0, !.stage = "idle", !.cancelled = TRUE]
Observe == \/ s' = [s EXCEPT !.current = ~@]
           \/ s' = [s EXCEPT !.root = ~@]
           \/ s' = [s EXCEPT !.matching = ~@]
Start == /\ s.borrower \in Callers /\ s.stage = "idle" /\ s.phase = "empty"
         /\ Admitted
         /\ s' = [s EXCEPT !.phase = "pending", !.held = TRUE,
                          !.stage = "mount", !.calls = @ + 1]
Resume == /\ s.borrower \in Callers /\ s.stage = "idle" /\ s.phase = "pending"
          /\ Admitted
          /\ s' = [s EXCEPT !.stage = "mount"]
\* A previously admitted external operation may return after authority loss.
\* Its guard is retained BEFORE another admission; response is not permission.
ReturnMount == /\ s.stage = "mount"
               /\ s' = [s EXCEPT !.phase = "live", !.stage = "idle"]
\* A proven no-effect failure can retire immediately. An ambiguous failure is
\* retained under the same owner and cannot be replayed as a fresh mount.
ReturnNoEffect == /\ s.stage = "mount"
                  /\ s' = [s EXCEPT !.phase = "noeffect", !.held = FALSE,
                                   !.stage = "idle", !.cleaned = TRUE,
                                   !.borrower = 0]
ReturnUnknown == /\ s.stage = "mount"
                 /\ s' = [s EXCEPT !.phase = "unknown", !.stage = "idle"]
BeginUnknownRecovery == /\ s.borrower \in Callers /\ s.stage = "idle"
                        /\ s.phase = "unknown" /\ Admitted
                        /\ s' = [s EXCEPT !.stage = "recover"]
ReturnUnknownRecovery == /\ s.stage = "recover"
                         /\ s' = [s EXCEPT !.stage = "finish", !.cleaned = TRUE]
FailUnknownRecovery == /\ s.stage = "recover"
                       /\ s' = [s EXCEPT !.stage = "idle", !.failed = TRUE]
Confirm == /\ s.borrower \in Callers /\ s.stage = "idle" /\ s.phase = "live"
           /\ Admitted /\ ~s.confirmed /\ ~s.cleaned
           /\ s' = [s EXCEPT !.confirmed = TRUE]
Reuse == /\ s.borrower \in Callers /\ s.phase = "live" /\ s.confirmed
         /\ s.mode = "fuse" /\ s.matching /\ Admitted /\ s.stage = "idle"
         /\ s' = [s EXCEPT !.borrower = 0]
CanRetire == ~s.confirmed \/ s.purpose = "terminal"
             \/ (s.mode = "fuse" /\ ~s.matching)
BeginTeardown == /\ s.borrower \in Callers /\ s.stage = "idle" /\ s.phase = "live"
                 /\ Admitted /\ CanRetire
                 /\ s' = [s EXCEPT !.stage = "teardown"]
ReturnTeardown == /\ s.stage = "teardown"
                  /\ s' = [s EXCEPT !.stage = "finish", !.cleaned = TRUE]
FailTeardown == /\ s.stage = "teardown"
                /\ s' = [s EXCEPT !.stage = "idle", !.failed = TRUE]
Retire == /\ s.stage = "finish" /\ Admitted
          /\ s' = [s EXCEPT !.phase = "released", !.held = FALSE,
                           !.stage = "idle", !.borrower = 0]
\* The Host's existing successful Copy CAS acknowledgement is distinct from
\* replay cleanup; it is not allowed while a mount operation is unfinished.
AckCopy == /\ s.borrower \in Callers /\ s.stage = "idle" /\ Admitted
           /\ s.phase = "live" /\ s.confirmed /\ s.mode = "copy"
           /\ s' = [s EXCEPT !.phase = "released", !.held = FALSE,
                            !.ack = TRUE, !.borrower = 0]
Prune == /\ s.borrower = 0 /\ s.phase \in {"empty", "released"}
         /\ s' = [s EXCEPT !.held = FALSE]
Next == (\E c \in Callers: Borrow(c)) \/ Cancel \/ Observe \/ Start \/ Resume
        \/ ReturnMount \/ ReturnNoEffect \/ ReturnUnknown
        \/ BeginUnknownRecovery \/ ReturnUnknownRecovery \/ FailUnknownRecovery
        \/ Confirm \/ Reuse \/ BeginTeardown \/ ReturnTeardown \/ FailTeardown
        \/ Retire \/ AckCopy \/ Prune
Spec == Init /\ [][Next]_s
TypeOK == /\ s.phase \in {"empty", "pending", "unknown", "live", "noeffect", "released"}
          /\ s.stage \in {"idle", "mount", "recover", "teardown", "finish"}
          /\ s.borrower \in Callers \cup {0}
          /\ s.calls \in 0..2
RetainedWhilePhysical == s.phase \in {"pending", "unknown", "live"} => s.held
SingleExternalMount == s.calls <= 1
BorrowIsExclusive == [][s.borrower # 0 => s'.borrower \in {0, s.borrower}]_s
EffectsRequireAdmission ==
    [][(s'.stage \in {"mount", "recover", "teardown"} /\ s'.stage # s.stage) => Admitted]_s
RetirementRequiresAdmission ==
    [][(s.phase # "released" /\ s'.phase = "released") => Admitted]_s
RetirementRequiresEvidence == s.phase = "released" => s.cleaned \/ s.ack
CopyReplayKeepsBaseline ==
    [][(s.phase = "live" /\ s.confirmed /\ s.mode = "copy" /\ s.purpose = "replay")
       => s'.stage # "teardown"]_s
AcknowledgementRequiresSettledMount ==
    [][(~s.ack /\ s'.ack) => s.phase = "live" /\ s.confirmed]_s
NoCancelledPending == ~(s.cancelled /\ s.phase = "pending" /\ s.borrower = 0)
NoFailedRetained == ~(s.failed /\ s.held)
NoUnknownRetained == ~(s.phase = "unknown" /\ s.held)
NoLateMountReturn == ~(s.phase = "live" /\ ~s.current /\ s.held)
NoCopyAck == ~s.ack
NoCleanedTerminal == ~(s.phase = "released" /\ s.cleaned /\ s.purpose = "terminal")
=============================================================================
