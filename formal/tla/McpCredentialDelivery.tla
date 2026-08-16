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
         rejected, unsafeEffectSeen

vars == <<selectedMode, selectedHolder, selectedMechanism,
          desiredGeneration, desiredRevision, leaseEpoch,
          stagedGeneration, stagedRevision, stagedEpoch,
          acceptingGeneration, acceptingRevision,
          revokedRevisions, processUp, processGeneration,
          rejected, unsafeEffectSeen>>

Init == /\ selectedMode = "None"
        /\ selectedHolder = "None"
        /\ selectedMechanism = "None"
        /\ desiredGeneration = 0 /\ desiredRevision = 0 /\ leaseEpoch = 0
        /\ stagedGeneration = 0 /\ stagedRevision = 0 /\ stagedEpoch = 0
        /\ acceptingGeneration = 0 /\ acceptingRevision = 0
        /\ revokedRevisions = {} /\ processUp = TRUE /\ processGeneration = 0
        /\ rejected = FALSE /\ unsafeEffectSeen = FALSE

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
                    unsafeEffectSeen>>

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
                    rejected, unsafeEffectSeen>>

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
                    unsafeEffectSeen>>

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
                    revokedRevisions, processUp, rejected, unsafeEffectSeen>>

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
                    rejected, unsafeEffectSeen>>

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
                    processUp, rejected, unsafeEffectSeen>>

Call ==
    /\ processUp /\ acceptingGeneration > 0
    /\ processGeneration = acceptingGeneration
    /\ acceptingRevision \notin revokedRevisions
    /\ Allowed(selectedMode, selectedHolder, selectedMechanism)
    /\ unsafeEffectSeen' = FALSE
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
                         revokedRevisions, rejected, unsafeEffectSeen>>

Restart == /\ ~processUp /\ processUp' = TRUE
           /\ stagedGeneration' = 0 /\ stagedRevision' = 0 /\ stagedEpoch' = 0
           /\ acceptingGeneration' = 0 /\ acceptingRevision' = 0
           /\ processGeneration' = 0
           /\ UNCHANGED <<selectedMode, selectedHolder, selectedMechanism,
                           desiredGeneration, desiredRevision, leaseEpoch,
                           revokedRevisions, rejected, unsafeEffectSeen>>

RequestAny == \E mode \in Modes, holder \in Holders, mechanism \in Mechanisms:
                  Request(mode, holder, mechanism)
RejectAny == \E generation \in 1..MaxGeneration,
                 revision \in 1..MaxGeneration,
                 epoch \in 1..MaxGeneration:
                 RejectStaleStage(generation, revision, epoch)
RevokeAny == \E revision \in 1..MaxGeneration: Revoke(revision)

Next == RequestAny \/ Stage \/ RejectAny \/ Activate \/ Rotate
        \/ RevokeAny \/ Call \/ Crash \/ Restart

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
          /\ unsafeEffectSeen \in BOOLEAN

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
NoUnauthorizedEffect == ~unsafeEffectSeen

Spec == Init /\ [][Next]_vars
=====================================================================
