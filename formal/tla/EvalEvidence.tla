--------------------------- MODULE EvalEvidence ---------------------------
EXTENDS Naturals

CONSTANTS Cases, FrozenVersion

VARIABLES datasetVersion, observations, scored, schemaValid, published

vars == <<datasetVersion, observations, scored, schemaValid, published>>

Init ==
    /\ datasetVersion = FrozenVersion
    /\ observations = [case \in Cases |-> 0]
    /\ scored = [case \in Cases |-> FALSE]
    /\ schemaValid = [case \in Cases |-> FALSE]
    /\ published = FALSE

Record(case) ==
    /\ ~published
    /\ case \in Cases
    /\ ~scored[case]
    /\ observations[case] < 2
    /\ observations' = [observations EXCEPT ![case] = @ + 1]
    /\ UNCHANGED <<datasetVersion, scored, schemaValid, published>>

Score(case) ==
    /\ ~published
    /\ case \in Cases
    /\ ~scored[case]
    /\ scored' = [scored EXCEPT ![case] = TRUE]
    /\ schemaValid' = [schemaValid EXCEPT ![case] = observations[case] = 1]
    /\ UNCHANGED <<datasetVersion, observations, published>>

Publish ==
    /\ ~published
    /\ \A case \in Cases : scored[case]
    /\ published' = TRUE
    /\ UNCHANGED <<datasetVersion, observations, scored, schemaValid>>

Next ==
    \/ \E case \in Cases : Record(case)
    \/ \E case \in Cases : Score(case)
    \/ Publish

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ datasetVersion \in Nat
    /\ observations \in [Cases -> 0..2]
    /\ scored \in [Cases -> BOOLEAN]
    /\ schemaValid \in [Cases -> BOOLEAN]
    /\ published \in BOOLEAN

GroundTruthIsFrozen == datasetVersion = FrozenVersion

OnlyExactlyOneObservationCanBeValid ==
    \A case \in Cases :
        scored[case] => (schemaValid[case] <=> observations[case] = 1)

PublishedReportIsComplete ==
    published => \A case \in Cases : scored[case]

PublishedEvidenceIsFrozen ==
    published => datasetVersion = FrozenVersion

=============================================================================
