------------------------- MODULE ConfigActivation -------------------------
EXTENDS Naturals, TLC

CONSTANT MaxGeneration

\* ADR-0071: one Agent in one execution Workspace. Control prepares a source
\* revision, commits it with publication CAS, then Coordinator may receive that
\* durable revision late or repeatedly. Current is a lifecycle high-water mark;
\* exact historical registrations survive withdrawal. There is no whole-catalog
\* install generation and no requirement that delivery match the latest draft.
\* Fingerprint/content identity remains an assumption of this abstraction. The
\* production write derives its source fence from the publication and its direct
\* Kani receipt gate discharges source pairing outside this finite model.
VARIABLE authorGeneration, preparedGeneration, publications, registeredRevisions,
         installedGeneration, lastInstalledGeneration, currentAvailable, lastStep

vars == <<authorGeneration, preparedGeneration, publications, registeredRevisions,
          installedGeneration, lastInstalledGeneration, currentAvailable, lastStep>>
Revisions == 1..MaxGeneration
Idle == [kind |-> "idle", source |-> 0, observedAuthor |-> 0,
         priorGeneration |-> 0, priorAvailable |-> FALSE,
         publicationsBefore |-> {}, registrationsBefore |-> {}]
Observe(kind, source) ==
    [kind |-> kind, source |-> source, observedAuthor |-> authorGeneration,
     priorGeneration |-> installedGeneration, priorAvailable |-> currentAvailable,
     publicationsBefore |-> publications, registrationsBefore |-> registeredRevisions]

Init == /\ authorGeneration = 0 /\ preparedGeneration = 0
        /\ publications = {} /\ registeredRevisions = {}
        /\ installedGeneration = 0 /\ lastInstalledGeneration = 0
        /\ currentAvailable = FALSE /\ lastStep = Idle

Edit == /\ authorGeneration < MaxGeneration
        /\ authorGeneration' = authorGeneration + 1
        /\ lastStep' = Idle
        /\ UNCHANGED <<preparedGeneration, publications, registeredRevisions,
                       installedGeneration, lastInstalledGeneration, currentAvailable>>

Prepare == /\ authorGeneration > 0
           /\ preparedGeneration' = authorGeneration
           /\ lastStep' = Idle
           /\ UNCHANGED <<authorGeneration, publications, registeredRevisions,
                          installedGeneration, lastInstalledGeneration, currentAvailable>>

\* P1 exact source at durable CAS -> accepted and retained. P2 source changed
\* after prepare -> conflict, durable set AND catalog unchanged. Observation is
\* independent of the admission predicate, so deleting that predicate is killed.
PublicationAttempt ==
    /\ preparedGeneration > 0
    /\ IF preparedGeneration = authorGeneration
          THEN /\ publications' = publications \cup {preparedGeneration}
               /\ lastStep' = Observe("accepted", preparedGeneration)
          ELSE /\ UNCHANGED publications
               /\ lastStep' = Observe("rejected", preparedGeneration)
    /\ UNCHANGED <<authorGeneration, preparedGeneration, registeredRevisions,
                   installedGeneration, lastInstalledGeneration, currentAvailable>>

\* R1 durable + unseen + newer lifecycle -> current; R2 exact retry -> stutter;
\* R3 durable + unseen + equal/older lifecycle -> retain history only. A newer
\* draft does not invalidate a publication already accepted by durable CAS.
Register(revision) ==
    /\ revision \in Revisions
    /\ revision \in publications
    /\ lastInstalledGeneration' = installedGeneration
    /\ IF revision \in registeredRevisions
          THEN /\ lastStep' = Observe("replay", revision)
               /\ UNCHANGED <<registeredRevisions, installedGeneration, currentAvailable>>
          ELSE /\ registeredRevisions' = registeredRevisions \cup {revision}
               /\ IF revision > installedGeneration
                     THEN /\ installedGeneration' = revision
                          /\ currentAvailable' = TRUE
                          /\ lastStep' = Observe("current", revision)
                     ELSE /\ UNCHANGED <<installedGeneration, currentAvailable>>
                          /\ lastStep' = Observe("historical", revision)
    /\ UNCHANGED <<authorGeneration, preparedGeneration, publications>>

\* W1 older withdrawal -> stutter; W2 equal tombstone -> idempotent; W3 equal
\* available or newer -> unavailable at that lifecycle revision. Every branch
\* retains exact history. Positive authored commands abstract port validation.
Withdraw(revision) ==
    /\ revision \in 1..authorGeneration
    /\ lastInstalledGeneration' = installedGeneration
    /\ IF revision < installedGeneration
          THEN /\ UNCHANGED <<installedGeneration, currentAvailable>>
               /\ lastStep' = Observe("withdraw-old", revision)
          ELSE /\ installedGeneration' = revision
               /\ currentAvailable' = FALSE
               /\ lastStep' = Observe("withdraw", revision)
    /\ UNCHANGED <<authorGeneration, preparedGeneration, publications, registeredRevisions>>

Next == Edit \/ Prepare \/ PublicationAttempt
        \/ (\E revision \in Revisions: Register(revision) \/ Withdraw(revision))

TypeOK ==
    /\ authorGeneration \in 0..MaxGeneration
    /\ preparedGeneration \in 0..MaxGeneration
    /\ publications \subseteq Revisions /\ registeredRevisions \subseteq Revisions
    /\ installedGeneration \in 0..MaxGeneration
    /\ lastInstalledGeneration \in 0..MaxGeneration
    /\ currentAvailable \in BOOLEAN
    /\ lastStep \in [kind : {"idle", "accepted", "rejected", "replay", "current",
                             "historical", "withdraw-old", "withdraw"},
                    source : 0..MaxGeneration, observedAuthor : 0..MaxGeneration,
                    priorGeneration : 0..MaxGeneration, priorAvailable : BOOLEAN,
                    publicationsBefore : SUBSET Revisions, registrationsBefore : SUBSET Revisions]

AcceptedPublicationMatchedSource ==
    lastStep.kind = "accepted" => lastStep.source = lastStep.observedAuthor
RejectedPublicationPreservesDurableSet ==
    lastStep.kind = "rejected" => publications = lastStep.publicationsBefore
PublicationIsAtomicCAS ==
    /\ (lastStep.kind = "accepted" =>
           publications = lastStep.publicationsBefore \cup {lastStep.source})
    /\ (lastStep.kind = "rejected" => lastStep.source # lastStep.observedAuthor)
    /\ (lastStep.kind \in {"accepted", "rejected"} =>
           /\ registeredRevisions = lastStep.registrationsBefore
           /\ installedGeneration = lastStep.priorGeneration
           /\ currentAvailable = lastStep.priorAvailable)
RegisteredHistoryWasDurablyPublished == registeredRevisions \subseteq publications
AvailableCurrentHasExactRegistration ==
    currentAvailable => installedGeneration \in registeredRevisions
InstalledNeverExceedsAuthored == installedGeneration <= authorGeneration
ActivationGenerationNeverRollsBack == installedGeneration >= lastInstalledGeneration
RegisteredCurrentRequiresStrictlyNewer ==
    lastStep.kind = "current" =>
        /\ lastStep.source > lastStep.priorGeneration
        /\ installedGeneration = lastStep.source /\ currentAvailable
RegistrationPreservesCurrentUnlessNewer ==
    lastStep.kind \in {"replay", "historical"} =>
        /\ installedGeneration = lastStep.priorGeneration
        /\ currentAvailable = lastStep.priorAvailable
ExactHistoryNeverDisappears == lastStep.registrationsBefore \subseteq registeredRevisions
WithdrawalPreservesExactHistory ==
    lastStep.kind \in {"withdraw", "withdraw-old"} =>
        /\ registeredRevisions = lastStep.registrationsBefore
        /\ publications = lastStep.publicationsBefore
        /\ (lastStep.kind = "withdraw" =>
               /\ installedGeneration = lastStep.source /\ ~currentAvailable)
        /\ (lastStep.kind = "withdraw-old" =>
               /\ installedGeneration = lastStep.priorGeneration
               /\ currentAvailable = lastStep.priorAvailable)

\* The mutation family checks these inverse invariants separately: each MUST
\* fail on the unmodified model, establishing non-vacuous admission, rejection,
\* delayed delivery, historical retention and equal-revision withdrawal fencing.
NoAcceptedPublicationWitness == lastStep.kind # "accepted"
NoStaleRejectionWitness == lastStep.kind # "rejected"
NoDelayedCurrentRegistrationWitness ==
    ~(lastStep.kind = "current" /\ lastStep.source < lastStep.observedAuthor)
NoHistoricalRegistrationWitness == lastStep.kind # "historical"
NoWithdrawalFenceWitness ==
    ~(lastStep.kind = "historical" /\ lastStep.source = lastStep.priorGeneration
      /\ ~lastStep.priorAvailable)

Spec == Init /\ [][Next]_vars
=============================================================================
