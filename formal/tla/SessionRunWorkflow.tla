------------------------- MODULE SessionRunWorkflow -------------------------
EXTENDS Naturals

\* Cross-component product workflow: immutable Session admission, durable Run
\* creation, Worker claim, exact credential materialization, terminal commit,
\* and settlement. Component-internal detail remains in the existing models.
CONSTANTS AgentRevisions, EnvironmentRevisions, CredentialRevisions,
          Workers, NoRevision, NoWorker, MaxEpoch

ASSUME
    /\ AgentRevisions # {}
    /\ EnvironmentRevisions # {}
    /\ CredentialRevisions # {}
    /\ Workers # {}
    /\ NoRevision \notin AgentRevisions \cup EnvironmentRevisions \cup CredentialRevisions
    /\ NoWorker \notin Workers
    /\ MaxEpoch \in Nat \ {0}

VARIABLES sessionState, agentRevision, environmentRevision,
          runState, owner, epoch, credentialRevision, materializedRevision,
          outputCommitted, terminalOwner, terminalEpoch

vars == <<sessionState, agentRevision, environmentRevision,
          runState, owner, epoch, credentialRevision, materializedRevision,
          outputCommitted, terminalOwner, terminalEpoch>>

Init ==
    /\ sessionState = "Absent"
    /\ agentRevision = NoRevision
    /\ environmentRevision = NoRevision
    /\ runState = "Absent"
    /\ owner = NoWorker
    /\ epoch = 0
    /\ credentialRevision \in CredentialRevisions
    /\ materializedRevision = NoRevision
    /\ outputCommitted = FALSE
    /\ terminalOwner = NoWorker
    /\ terminalEpoch = 0

CreateSession(a, e) ==
    /\ sessionState = "Absent"
    /\ a \in AgentRevisions
    /\ e \in EnvironmentRevisions
    /\ sessionState' = "Ready"
    /\ agentRevision' = a
    /\ environmentRevision' = e
    /\ UNCHANGED <<runState, owner, epoch, credentialRevision,
                   materializedRevision, outputCommitted, terminalOwner,
                   terminalEpoch>>

CreateRun ==
    /\ sessionState = "Ready"
    /\ runState = "Absent"
    /\ runState' = "Pending"
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   owner, epoch, credentialRevision, materializedRevision,
                   outputCommitted, terminalOwner, terminalEpoch>>

Claim(w) ==
    /\ runState = "Pending"
    /\ w \in Workers
    /\ epoch < MaxEpoch
    /\ runState' = "Leased"
    /\ owner' = w
    /\ epoch' = epoch + 1
    /\ materializedRevision' = NoRevision
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   credentialRevision, outputCommitted, terminalOwner,
                   terminalEpoch>>

Materialize ==
    /\ runState = "Leased"
    /\ owner \in Workers
    /\ materializedRevision = NoRevision
    /\ materializedRevision' = credentialRevision
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   runState, owner, epoch, credentialRevision, outputCommitted,
                   terminalOwner, terminalEpoch>>

Execute ==
    /\ runState = "Leased"
    /\ owner \in Workers
    /\ materializedRevision = credentialRevision
    /\ runState' = "Running"
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   owner, epoch, credentialRevision, materializedRevision,
                   outputCommitted, terminalOwner, terminalEpoch>>

CommitTerminal(w, claimEpoch) ==
    /\ runState = "Running"
    /\ w = owner
    /\ claimEpoch = epoch
    /\ runState' = "Ended"
    /\ outputCommitted' = TRUE
    /\ terminalOwner' = w
    /\ terminalEpoch' = claimEpoch
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   owner, epoch, credentialRevision, materializedRevision>>

Settle ==
    /\ runState = "Ended"
    /\ outputCommitted
    /\ runState' = "Settled"
    /\ owner' = NoWorker
    /\ materializedRevision' = NoRevision
    /\ UNCHANGED <<sessionState, agentRevision, environmentRevision,
                   epoch, credentialRevision, outputCommitted, terminalOwner,
                   terminalEpoch>>

CreateSessionAny == \E a \in AgentRevisions, e \in EnvironmentRevisions: CreateSession(a, e)
ClaimAny == \E w \in Workers: Claim(w)
CommitTerminalAny == \E w \in Workers, claimEpoch \in 0..MaxEpoch:
    CommitTerminal(w, claimEpoch)

Next ==
    \/ CreateSessionAny
    \/ CreateRun
    \/ ClaimAny
    \/ Materialize
    \/ Execute
    \/ CommitTerminalAny
    \/ Settle

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(CreateSessionAny)
    /\ WF_vars(CreateRun)
    /\ WF_vars(ClaimAny)
    /\ WF_vars(Materialize)
    /\ WF_vars(Execute)
    /\ WF_vars(CommitTerminalAny)
    /\ WF_vars(Settle)

TypeOK ==
    /\ sessionState \in {"Absent", "Ready"}
    /\ agentRevision \in AgentRevisions \cup {NoRevision}
    /\ environmentRevision \in EnvironmentRevisions \cup {NoRevision}
    /\ runState \in {"Absent", "Pending", "Leased", "Running", "Ended", "Settled"}
    /\ owner \in Workers \cup {NoWorker}
    /\ epoch \in 0..MaxEpoch
    /\ credentialRevision \in CredentialRevisions
    /\ materializedRevision \in CredentialRevisions \cup {NoRevision}
    /\ outputCommitted \in BOOLEAN
    /\ terminalOwner \in Workers \cup {NoWorker}
    /\ terminalEpoch \in 0..MaxEpoch

SessionPinsAreImmutable ==
    sessionState = "Ready" =>
        agentRevision \in AgentRevisions /\ environmentRevision \in EnvironmentRevisions

RunRequiresFrozenSession ==
    runState # "Absent" => sessionState = "Ready"

ExecutionRequiresExactClaimAndCredential ==
    runState = "Running" =>
        owner \in Workers /\ epoch > 0 /\ materializedRevision = credentialRevision

OutputRequiresTerminalCommit == outputCommitted => runState \in {"Ended", "Settled"}

TerminalCommitUsesExactClaim ==
    runState = "Ended" => terminalOwner = owner /\ terminalEpoch = epoch

SettledClearsAuthority ==
    runState = "Settled" => owner = NoWorker /\ materializedRevision = NoRevision

Safety ==
    /\ TypeOK
    /\ SessionPinsAreImmutable
    /\ RunRequiresFrozenSession
    /\ ExecutionRequiresExactClaimAndCredential
    /\ OutputRequiresTerminalCommit
    /\ TerminalCommitUsesExactClaim
    /\ SettledClearsAuthority

EventuallySettled == <> (runState = "Settled")
=============================================================================
