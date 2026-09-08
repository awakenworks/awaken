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
         rejected, lastCall

vars == <<selectedMode, selectedHolder, selectedMechanism,
          desiredGeneration, desiredRevision, leaseEpoch,
          stagedGeneration, stagedRevision, stagedEpoch,
          acceptingGeneration, acceptingRevision,
          revokedRevisions, processUp, processGeneration,
          rejected, lastCall>>

Init == /\ selectedMode = "None"
        /\ selectedHolder = "None"
        /\ selectedMechanism = "None"
        /\ desiredGeneration = 0 /\ desiredRevision = 0 /\ leaseEpoch = 0
        /\ stagedGeneration = 0 /\ stagedRevision = 0 /\ stagedEpoch = 0
        /\ acceptingGeneration = 0 /\ acceptingRevision = 0
        /\ revokedRevisions = {} /\ processUp = TRUE /\ processGeneration = 0
        /\ rejected = FALSE
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
                    lastCall>>

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
                    rejected, lastCall>>

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
                    lastCall>>

Activate ==
    /\ processUp /\ selectedMode \in Modes
    /\ Allowed(selectedMode, selectedHolder, selectedMechanism)
    /\ stagedGeneration = desiredGeneration
    /\ stagedRevision = desiredRevision /\ stagedEpoch = leaseEpoch
    /\ stagedRevision \notin revokedRevisions
    /\ acceptingGeneration' = stagedGeneration
    /\ acceptingRevision' = stagedRevision
    /\ processGeneration' = stagedGeneration
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    desiredGeneration, desiredRevision, leaseEpoch,
                    stagedGeneration, stagedRevision, stagedEpoch,
                    revokedRevisions, processUp, rejected, lastCall>>

Rotate ==
    /\ processUp /\ selectedMode \in Modes
    /\ desiredGeneration < MaxGeneration /\ desiredRevision < MaxGeneration
    /\ desiredGeneration' = desiredGeneration + 1
    /\ desiredRevision' = desiredRevision + 1
    /\ leaseEpoch' = leaseEpoch + 1
    /\ stagedGeneration' = 0 /\ stagedRevision' = 0 /\ stagedEpoch' = 0
    /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                    acceptingGeneration, acceptingRevision,
                    revokedRevisions, processUp, processGeneration,
                    rejected, lastCall>>

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
                    processUp, rejected, lastCall>>

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
                    revokedRevisions, processUp, processGeneration, rejected>>

Crash == /\ processUp /\ processUp' = FALSE /\ processGeneration' = 0
         /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                         desiredGeneration, desiredRevision, leaseEpoch,
                         stagedGeneration, stagedRevision, stagedEpoch,
                         acceptingGeneration, acceptingRevision,
                         revokedRevisions, rejected, lastCall>>

Restart == /\ ~processUp /\ processUp' = TRUE
           /\ stagedGeneration' = 0 /\ stagedRevision' = 0 /\ stagedEpoch' = 0
           /\ acceptingGeneration' = 0 /\ acceptingRevision' = 0
           /\ processGeneration' = 0
           /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                           desiredGeneration, desiredRevision, leaseEpoch,
                           revokedRevisions, rejected, lastCall>>

RequestAny == \E mode \in Modes, holder \in Holders, mechanism \in Mechanisms:
                  Request(mode, holder, mechanism)
RejectAny == \E generation \in 1..MaxGeneration,
                 revision \in 1..MaxGeneration,
                 epoch \in 1..MaxGeneration:
                 RejectStaleStage(generation, revision, epoch)
CallAny == \E generation \in 1..MaxGeneration, revision \in 1..MaxGeneration: Call(generation, revision)
RevokeAny == \E revision \in 1..MaxGeneration: Revoke(revision)

Next == RequestAny \/ Stage \/ RejectAny \/ Activate \/ Rotate
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
