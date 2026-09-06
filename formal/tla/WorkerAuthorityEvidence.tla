--------------------- MODULE WorkerAuthorityEvidence ---------------------
EXTENDS Naturals, TLC

\* The Worker supervisor owns two independent proof lanes, NOT two Worker
\* authorities. Registry expiry and sandbox-policy validity are external facts.
\* This finite model checks cadence independence and conservative expiry. It
\* abstracts local fence/cleanup as atomic; WorkerDrain owns that ordering.
CONSTANT Window
VARIABLE registryAge, evidenceAge, probeAge, probing, open
vars == <<registryAge, evidenceAge, probeAge, probing, open>>

Init == /\ registryAge = 0 /\ evidenceAge = 0 /\ probeAge = 0
        /\ probing = FALSE /\ open = TRUE

Tick == /\ open
        /\ registryAge' = registryAge + 1
        /\ evidenceAge' = evidenceAge + 1
        /\ probeAge' = IF probing THEN probeAge + 1 ELSE 0
        /\ open' = (registryAge' < Window /\ evidenceAge' < Window)
        /\ probing' = (probing /\ open')

\* Critically, no probe state is a prerequisite to proving registry authority.
Heartbeat == /\ open /\ registryAge < Window
             /\ registryAge' = 0
             /\ UNCHANGED <<evidenceAge, probeAge, probing, open>>

BeginProbe == /\ open /\ ~probing
              /\ probing' = TRUE /\ probeAge' = 0
              /\ UNCHANGED <<registryAge, evidenceAge, open>>

\* Request-start anchoring includes the response latency in the new proof age.
ProbeSuccess == /\ open /\ probing /\ evidenceAge < Window
                /\ evidenceAge' = probeAge /\ probing' = FALSE
                /\ UNCHANGED <<registryAge, probeAge, open>>

ProbeTimeout == /\ open /\ probing
                /\ probing' = FALSE
                /\ UNCHANGED <<registryAge, evidenceAge, probeAge, open>>

Reject == /\ open /\ open' = FALSE /\ probing' = FALSE
          /\ UNCHANGED <<registryAge, evidenceAge, probeAge>>

Next == Tick \/ Heartbeat \/ BeginProbe \/ ProbeSuccess \/ ProbeTimeout \/ Reject
Spec == Init /\ [][Next]_vars /\ WF_vars(Tick)

TypeOK == /\ registryAge \in 0..Window /\ evidenceAge \in 0..Window
          /\ probeAge \in 0..Window /\ probing \in BOOLEAN /\ open \in BOOLEAN
BothProofsRequired == open => registryAge < Window /\ evidenceAge < Window
ProbeCannotBlockHeartbeat == open => ENABLED Heartbeat
NoProofResurrection == [](~open => []~open)
\* Conditional liveness: if successful environment evidence stops arriving,
\* fair passage of time closes admission despite arbitrarily many heartbeats.
MissingEvidenceEventuallyCloses ==
    (<>[][~ProbeSuccess]_vars) => <>[]~open
=============================================================================
