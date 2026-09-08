------------------- MODULE McpCredentialDelivery -------------------
EXTENDS Naturals, TLC

CONSTANT MaxGeneration
ASSUME MaxGeneration \in Nat /\ MaxGeneration >= 3

Modes == {"ClientInjection", "GatewayMediation"}
Holders == {"Workload", "Worker", "Platform"}
Mechanisms == {"ProcessEnvironment", "PrivateFile", "ProcessProtocolField",
               "WorkerRelay", "PlatformRelay"}

Allowed(mode, holder, mechanism) ==
    \/ mode = "ClientInjection"
       /\ holder = "Workload"
       /\ mechanism = "ProcessProtocolField"
    \/ mode = "GatewayMediation"
       /\ holder = "Platform"
       /\ mechanism = "PlatformRelay"

VARIABLE selectedMode, selectedHolder, selectedMechanism,
         desiredGeneration, desiredRevision, leaseEpoch,
         stagedGeneration, stagedRevision, stagedEpoch,
         acceptingGeneration, acceptingRevision,
         revokedRevisions, processUp, processGeneration,
         rejected, lastCall, lastActivation

vars == <<selectedMode, selectedHolder, selectedMechanism,
          desiredGeneration, desiredRevision, leaseEpoch,
          stagedGeneration, stagedRevision, stagedEpoch,
          acceptingGeneration, acceptingRevision,
          revokedRevisions, processUp, processGeneration,
          rejected, lastCall, lastActivation>>

Init == /\ selectedMode = "None"
        /\ selectedHolder = "None"
        /\ selectedMechanism = "None"
        /\ desiredGeneration = 0 /\ desiredRevision = 0 /\ leaseEpoch = 0
        /\ stagedGeneration = 0 /\ stagedRevision = 0 /\ stagedEpoch = 0
        /\ acceptingGeneration = 0 /\ acceptingRevision = 0
        /\ revokedRevisions = {} /\ processUp = TRUE /\ processGeneration = 0
        /\ rejected = FALSE
        /\ lastActivation = [staged |-> <<0, 0, 0>>, requested |-> <<0, 0, 0>>]
        /\ lastCall = [generation |-> 0, revision |-> 0, accepting |-> 0,
                        source |-> 0, process |-> 0, live |-> FALSE,
                        revoked |-> {}, mode |-> "None", holder |-> "None",
                        mechanism |-> "None"]

Request(mode, holder, mechanism) ==
    /\ processUp /\ selectedMode = "None"
    /\ mode \in Modes /\ holder \in Holders /\ mechanism \in Mechanisms
    /\ IF Allowed(mode, holder, mechanism)
          THEN /\ selectedMode' = mode
               /\ selectedHolder' = holder
               /\ selectedMechanism' = mechanism
               /\ desiredGeneration' = 1 /\ desiredRevision' = 1
               /\ leaseEpoch' = 1
               /\ UNCHANGED rejected
          ELSE /\ selectedMode' = selectedMode
               /\ selectedHolder' = selectedHolder
               /\ selectedMechanism' = selectedMechanism
               /\ desiredGeneration' = desiredGeneration
               /\ desiredRevision' = desiredRevision
               /\ leaseEpoch' = leaseEpoch
               /\ rejected' = TRUE
    /\ UNCHANGED <<stagedGeneration, stagedRevision, stagedEpoch,
                    acceptingGeneration, acceptingRevision,
                    revokedRevisions, processUp, processGeneration,
                    lastCall, lastActivation>>

Stage ==
    /\ processUp /\ selectedMode \in Modes
    /\ desiredRevision \notin revokedRevisions
    /\ stagedGeneration' = desiredGeneration
    /\ stagedRevision' = desiredRevision
    /\ stagedEpoch' = leaseEpoch
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredGeneration, desiredRevision, leaseEpoch,
                    acceptingGeneration, acceptingRevision,
                    revokedRevisions, processUp, processGeneration,
                    rejected, lastCall, lastActivation>>

RejectStaleStage(generation, revision, epoch) ==
    /\ processUp /\ selectedMode \in Modes
    /\ generation \in 1..MaxGeneration /\ revision \in 1..MaxGeneration
    /\ epoch \in 1..MaxGeneration
    /\ (generation # desiredGeneration \/ revision # desiredRevision \/ epoch # leaseEpoch
        \/ revision \in revokedRevisions)
    /\ rejected' = TRUE
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredGeneration, desiredRevision, leaseEpoch,
                    stagedGeneration, stagedRevision, stagedEpoch,
                    acceptingGeneration, acceptingRevision,
                    revokedRevisions, processUp, processGeneration,
                    lastCall, lastActivation>>

Activate ==
    /\ processUp /\ selectedMode \in Modes
    /\ Allowed(selectedMode, selectedHolder, selectedMechanism)
    /\ stagedGeneration = desiredGeneration
    /\ stagedRevision = desiredRevision /\ stagedEpoch = leaseEpoch
    /\ stagedRevision \notin revokedRevisions
    /\ lastActivation' = [staged |-> <<stagedGeneration, stagedRevision, stagedEpoch>>,
                           requested |-> <<desiredGeneration, desiredRevision, leaseEpoch>>]
    /\ acceptingGeneration' = stagedGeneration
    /\ acceptingRevision' = stagedRevision
    /\ processGeneration' = stagedGeneration
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredGeneration, desiredRevision, leaseEpoch,
                    stagedGeneration, stagedRevision, stagedEpoch,
                    revokedRevisions, processUp, rejected, lastCall>>

\* Three authorities do not share a revision counter. Ordinary replacement
\* changes only attachment generation. Credential adoption changes generation
\* and source revision while retaining the Session lease. Recovery advances
\* only its lease epoch, retaining the desired generation/source pair.
\* Keep an earlier staged receipt to explore its delayed arrival after changes.
Replace ==
    /\ processUp /\ selectedMode \in Modes /\ desiredGeneration < MaxGeneration
    /\ desiredGeneration' = desiredGeneration + 1
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredRevision, leaseEpoch, stagedGeneration, stagedRevision, stagedEpoch,
                    acceptingGeneration, acceptingRevision, revokedRevisions, processUp,
                    processGeneration, rejected, lastCall, lastActivation>>

Rotate ==
    /\ processUp /\ selectedMode \in Modes
    /\ desiredGeneration < MaxGeneration /\ desiredRevision < MaxGeneration
    /\ desiredGeneration' = desiredGeneration + 1
    /\ desiredRevision' = desiredRevision + 1
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    leaseEpoch, stagedGeneration, stagedRevision, stagedEpoch,
                    acceptingGeneration, acceptingRevision, revokedRevisions, processUp,
                    processGeneration, rejected, lastCall, lastActivation>>

Reclaim ==
    /\ processUp /\ selectedMode \in Modes /\ leaseEpoch < MaxGeneration
    /\ leaseEpoch' = leaseEpoch + 1
    \* This is the new process's accepting fence, not loss of durable Active
    \* attachment state: claim_recovery retains that state in the Session root.
    /\ acceptingGeneration' = 0 /\ acceptingRevision' = 0 /\ processGeneration' = 0
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredGeneration, desiredRevision, stagedGeneration, stagedRevision,
                    stagedEpoch, revokedRevisions, processUp, rejected, lastCall, lastActivation>>

Revoke(revision) ==
    /\ revision \in 1..MaxGeneration
    /\ revokedRevisions' = revokedRevisions \cup {revision}
    /\ IF acceptingRevision = revision
          THEN /\ acceptingGeneration' = 0 /\ acceptingRevision' = 0
               /\ processGeneration' = 0
          ELSE /\ UNCHANGED <<acceptingGeneration, acceptingRevision,
                              processGeneration>>
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredGeneration, desiredRevision, leaseEpoch,
                    stagedGeneration, stagedRevision, stagedEpoch,
                    processUp, rejected, lastCall, lastActivation>>

\* C1 live process; C2 positive accepting generation; C3 exact requested
\* generation/revision; C4 rebuilt process; C5 adopted revision not revoked;
\* C6 frozen delivery pair allowed. E1 records the ACTUAL request and authority
\* at effect entry. Historical calls remain legal after a later revocation.
\* The observer never writes a precomputed "safe" flag. Removing a guard must
\* produce a concrete record rejected by NoUnauthorizedEffect below.
Call(generation, revision) ==
    /\ generation \in 1..MaxGeneration /\ revision \in 1..MaxGeneration
    /\ processUp /\ acceptingGeneration > 0
    /\ generation = acceptingGeneration /\ revision = acceptingRevision
    /\ processGeneration = acceptingGeneration
    /\ acceptingRevision \notin revokedRevisions
    /\ Allowed(selectedMode, selectedHolder, selectedMechanism)
    /\ lastCall' = [generation |-> generation, revision |-> revision,
                    accepting |-> acceptingGeneration, source |-> acceptingRevision,
                    process |-> processGeneration, live |-> processUp,
                    revoked |-> revokedRevisions, mode |-> selectedMode,
                    holder |-> selectedHolder, mechanism |-> selectedMechanism]
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredGeneration, desiredRevision, leaseEpoch,
                    stagedGeneration, stagedRevision, stagedEpoch,
                    acceptingGeneration, acceptingRevision,
                    revokedRevisions, processUp, processGeneration, rejected, lastActivation>>

Crash == /\ processUp /\ processUp' = FALSE /\ processGeneration' = 0
         /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                         desiredGeneration, desiredRevision, leaseEpoch,
                         stagedGeneration, stagedRevision, stagedEpoch,
                         acceptingGeneration, acceptingRevision,
                         revokedRevisions, rejected, lastCall, lastActivation>>

Restart == /\ ~processUp /\ processUp' = TRUE
           /\ stagedGeneration' = 0 /\ stagedRevision' = 0 /\ stagedEpoch' = 0
           /\ acceptingGeneration' = 0 /\ acceptingRevision' = 0
           /\ processGeneration' = 0
           /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                           desiredGeneration, desiredRevision, leaseEpoch,
                           revokedRevisions, rejected, lastCall, lastActivation>>

RequestAny == \E mode \in Modes, holder \in Holders, mechanism \in Mechanisms:
                  Request(mode, holder, mechanism)
RejectAny == \E generation \in 1..MaxGeneration,
                 revision \in 1..MaxGeneration,
                 epoch \in 1..MaxGeneration:
                 RejectStaleStage(generation, revision, epoch)
CallAny == \E generation \in 1..MaxGeneration, revision \in 1..MaxGeneration: Call(generation, revision)
RevokeAny == \E revision \in 1..MaxGeneration: Revoke(revision)

Next == RequestAny \/ Stage \/ RejectAny \/ Activate \/ Replace \/ Rotate \/ Reclaim
        \/ RevokeAny \/ CallAny \/ Crash \/ Restart

CallObservations == [generation: 0..MaxGeneration, revision: 0..MaxGeneration,
                     accepting: 0..MaxGeneration, source: 0..MaxGeneration,
                     process: 0..MaxGeneration, live: BOOLEAN,
                     revoked: SUBSET (1..MaxGeneration), mode: Modes \cup {"None"},
                     holder: Holders \cup {"None"}, mechanism: Mechanisms \cup {"None"}]

TypeOK == /\ selectedMode \in Modes \cup {"None"}
          /\ selectedHolder \in Holders \cup {"None"}
          /\ selectedMechanism \in Mechanisms \cup {"None"}
          /\ desiredGeneration \in 0..MaxGeneration
          /\ desiredRevision \in 0..MaxGeneration
          /\ leaseEpoch \in 0..MaxGeneration
          /\ stagedGeneration \in 0..MaxGeneration
          /\ stagedRevision \in 0..MaxGeneration
          /\ stagedEpoch \in 0..MaxGeneration
          /\ acceptingGeneration \in 0..MaxGeneration
          /\ acceptingRevision \in 0..MaxGeneration
          /\ processGeneration \in 0..MaxGeneration
          /\ revokedRevisions \subseteq 1..MaxGeneration
          /\ processUp \in BOOLEAN /\ rejected \in BOOLEAN
          /\ lastCall \in CallObservations
          /\ lastActivation \in [staged: (0..MaxGeneration) \X (0..MaxGeneration) \X (0..MaxGeneration),
                                  requested: (0..MaxGeneration) \X (0..MaxGeneration) \X (0..MaxGeneration)]

SelectedModeIsAlwaysAdmissible ==
    selectedMode \in Modes => Allowed(selectedMode, selectedHolder, selectedMechanism)
GatewayNeverSelectsWorkload ==
    selectedMode = "GatewayMediation" => selectedHolder = "Platform"
ClientInjectionIsExplicit ==
    selectedMode = "ClientInjection" =>
        selectedHolder = "Workload" /\ selectedMechanism = "ProcessProtocolField"
AcceptingRevisionIsLive ==
    acceptingGeneration > 0 => acceptingRevision \notin revokedRevisions
AcceptingGenerationNeverExceedsDesired ==
    acceptingGeneration <= desiredGeneration /\ acceptingRevision <= desiredRevision
AcceptingGenerationUsesRebuiltProcess ==
    processUp /\ acceptingGeneration > 0 => processGeneration = acceptingGeneration
ActivationUsesExactStage == lastActivation.staged = lastActivation.requested
NoIndependentReplacementReached ==
    lastActivation.requested[1] <= lastActivation.requested[2]
NoIndependentRotationReached ==
    lastActivation.requested[2] <= lastActivation.requested[3]
NoIndependentReclaimReached ==
    lastActivation.requested[3] <= lastActivation.requested[1]

NoUnauthorizedEffect ==
    lastCall.generation = 0 \/
      /\ lastCall.live
      /\ lastCall.accepting > 0
      /\ lastCall.generation = lastCall.accepting
      /\ lastCall.revision = lastCall.source
      /\ lastCall.process = lastCall.generation
      /\ lastCall.revision \notin lastCall.revoked
      /\ Allowed(lastCall.mode, lastCall.holder, lastCall.mechanism)

\* These deliberately false invariants are checked only by the shared mutation
\* runner as existential witnesses. Both useful modes, replacement, and a call
\* retained across later revocation must be reachable; reject-all cannot pass.
NoClientCallReached == lastCall.generation = 0 \/ lastCall.mode # "ClientInjection"
NoGatewayCallReached == lastCall.generation = 0 \/ lastCall.mode # "GatewayMediation"
NoReplacementCallReached == lastCall.generation = 0 \/ lastCall.generation < 2
NoHistoricalRevokedCallReached ==
    lastCall.generation = 0 \/ lastCall.revision \notin revokedRevisions

Spec == Init /\ [][Next]_vars
=====================================================================
