-------------------------- MODULE AcpProjectedBindingUse --------------------------
EXTENDS Naturals, FiniteSets

\* Two local consumers share one async gate. The external publisher may replace
\* the complete file twice, including deletion, corruption, revision/CLI/status
\* changes. Each awaited probe may return this backend or a foreign identity.
\* This models per-use validation, NOT authentication of the publisher,
\* arbitrary ABA/rollback detection, OS IO, final check-to-launch or Rust refinement.
Consumers == {1, 2}
Images == {"active", "rotated", "otherCli", "disabled", "absent", "invalid"}
VARIABLES file, writes, gate, phase, before, checked, probeMatches, grants
vars == <<file, writes, gate, phase, before, checked, probeMatches, grants>>

Init ==
    /\ file = "active" /\ writes = 0 /\ gate = 0
    /\ phase = [i \in Consumers |-> "idle"]
    /\ before = [i \in Consumers |-> "absent"]
    /\ checked = [i \in Consumers |-> "absent"]
    /\ probeMatches = [i \in Consumers |-> FALSE]
    /\ grants = {}

Replace(image) ==
    /\ writes < 2 /\ image \in Images
    /\ file' = image /\ writes' = writes + 1
    /\ UNCHANGED <<gate, phase, before, checked, probeMatches, grants>>

Read(i) ==
    /\ phase[i] = "idle" /\ gate = 0
    /\ gate' = i /\ phase' = [phase EXCEPT ![i] = "probe"]
    /\ before' = [before EXCEPT ![i] = file]
    /\ UNCHANGED <<file, writes, checked, probeMatches, grants>>

Probe(i, matching) ==
    /\ phase[i] = "probe"
    /\ phase' = [phase EXCEPT ![i] = "recheck"]
    /\ probeMatches' = [probeMatches EXCEPT ![i] = matching]
    /\ UNCHANGED <<file, writes, gate, before, checked, grants>>

Finish(i) ==
    /\ phase[i] = "recheck"
    /\ checked' = [checked EXCEPT ![i] = file]
    /\ grants' = IF before[i] = "active" /\ probeMatches[i] /\ file = before[i] THEN grants \cup {i} ELSE grants
    /\ phase' = [phase EXCEPT ![i] = "done"] /\ gate' = 0
    /\ UNCHANGED <<file, writes, before, probeMatches>>

Cancel(i) ==
    /\ phase[i] \in {"probe", "recheck"}
    /\ phase' = [phase EXCEPT ![i] = "cancelled"] /\ gate' = 0
    /\ UNCHANGED <<file, writes, before, checked, probeMatches, grants>>

Next == (\E image \in Images: Replace(image)) \/
    (\E i \in Consumers: Read(i) \/ (\E matching \in BOOLEAN: Probe(i, matching)) \/ Finish(i) \/ Cancel(i))
TypeOK ==
    /\ file \in Images /\ writes \in 0..2 /\ gate \in 0..2
    /\ phase \in [Consumers -> {"idle", "probe", "recheck", "done", "cancelled"}]
    /\ before \in [Consumers -> Images] /\ checked \in [Consumers -> Images]
    /\ probeMatches \in [Consumers -> BOOLEAN]
    /\ grants \subseteq Consumers
OneConsumer == Cardinality({i \in Consumers: phase[i] \in {"probe", "recheck"}}) <= 1
ExactPostProbeEvidence == \A i \in grants: before[i] = "active" /\ checked[i] = before[i]
ProbeReplyIsBackendScoped == \A i \in grants: probeMatches[i]
CancelledReleasesGate == \A i \in Consumers: phase[i] = "cancelled" => gate # i
NoForeignProbeDenial == ~(\E i \in Consumers: phase[i] = "done" /\
    before[i] = "active" /\ checked[i] = "active" /\ ~probeMatches[i] /\ i \notin grants)
NoUsefulGrant == grants = {}
NoRevokedDenial == ~(\E i \in Consumers: phase[i] = "done" /\ before[i] = "active" /\
    checked[i] = "disabled" /\ i \notin grants)
NoConsumerAfterCancellation == ~(phase[1] = "cancelled" /\ 2 \in grants)
Spec == Init /\ [][Next]_vars
=============================================================================
