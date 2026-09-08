-------------------- MODULE SessionNetworkPolicyProof --------------------
EXTENDS SessionNetworkPolicy, TLAPS

\* Theorems quantify over arbitrary sets, independently of the finite TLC
\* configuration. They specify the canonical restriction algebra, not a
\* universal refinement theorem for the production Rust String/Vec code.
THEOREM CanonicalIsIdempotent ==
    \A hosts : Canonical(Canonical(hosts)) = Canonical(hosts)
BY Z3T(30) DEF Canonical

THEOREM MeetNeverGrantsBlank ==
    \A a, b : Blank \notin Meet(a, b)
BY Z3T(30) DEF Meet, Canonical

THEOREM MeetNeverWidensEitherOperand ==
    \A a, b : /\ Meet(a, b) \subseteq Canonical(a)
              /\ Meet(a, b) \subseteq Canonical(b)
BY Z3T(30) DEF Meet, Canonical

THEOREM MeetIsCommutative == \A a, b : Meet(a, b) = Meet(b, a)
BY Z3T(30) DEF Meet, Canonical

THEOREM MeetIsIdempotent == \A a : Meet(a, a) = Canonical(a)
BY Z3T(30) DEF Meet, Canonical

THEOREM MeetIsAssociative ==
    \A a, b, c : Meet(Meet(a, b), c) = Meet(a, Meet(b, c))
BY Z3T(30) DEF Meet, Canonical

\* Cause/effect rules for the ordered collection boundary: S1 an exact source
\* position witness preserves membership; S2 complete selection equals meet;
\* S3 increasing positions preserve order; S4 injective positions preserve
\* uniqueness. Theorems quantify over arbitrary functions/host sets, without
\* claiming that Rust Vec/filter/sort/trim has been universally refined.
THEOREM SelectionCannotIntroduceAHost ==
    \A source, output, selection:
        SelectedSequence(source, output, selection) =>
            SequenceHosts(output) \subseteq SequenceHosts(source)
BY Z3T(30) DEF SelectedSequence, SequenceHosts

THEOREM ExactSelectionHasExactIntersection ==
    \A source, output, selection, allowed:
        SelectedSequence(source, output, selection) /\
        ExactSelection(source, output, selection, allowed) =>
            SequenceHosts(output) = SequenceHosts(source) \intersect allowed
BY Z3T(30) DEF SelectedSequence, ExactSelection, SequenceHosts

THEOREM SelectionPreservesCanonicalHosts ==
    \A source, output, selection:
        SelectedSequence(source, output, selection) /\
        Canonical(SequenceHosts(source)) = SequenceHosts(source) =>
            Canonical(SequenceHosts(output)) = SequenceHosts(output)
BY SelectionCannotIntroduceAHost, Z3T(30) DEF Canonical

THEOREM ExactSelectionOfCanonicalHostsIsMeet ==
    \A source, output, selection, allowed:
        SelectedSequence(source, output, selection) /\
        ExactSelection(source, output, selection, allowed) /\
        Canonical(SequenceHosts(source)) = SequenceHosts(source) =>
            SequenceHosts(output) = Meet(SequenceHosts(source), allowed)
BY ExactSelectionHasExactIntersection, Z3T(30) DEF Meet, Canonical

THEOREM IncreasingSelectionPreservesOrder ==
    \A source, output, selection, order:
        SelectedSequence(source, output, selection) /\
        IncreasingSelection(output, selection) /\
        SequenceOrderedBy(source, order) => SequenceOrderedBy(output, order)
BY Z3T(30) DEF SelectedSequence, IncreasingSelection, SequenceOrderedBy

THEOREM InjectiveSelectionPreservesUniqueness ==
    \A source, output, selection:
        SelectedSequence(source, output, selection) /\
        InjectiveSelection(output, selection) /\ SequenceUnique(source) =>
            SequenceUnique(output)
BY Z3T(30) DEF SelectedSequence, InjectiveSelection, SequenceUnique

\* S2/S3/S4 necessity witnesses: relaxing completeness, order or injectivity
\* admits a concrete wrong result even when the remaining projection is exact.
THEOREM ReorderedSelectionWitness ==
    LET source == <<0, 1>>
        output == <<1, 0>>
        selection == [i \in 1..2 |-> IF i = 1 THEN 2 ELSE 1]
        order == {<<0, 1>>}
    IN /\ SelectedSequence(source, output, selection)
       /\ ExactSelection(source, output, selection, {0, 1})
       /\ SequenceOrderedBy(source, order)
       /\ ~IncreasingSelection(output, selection)
       /\ ~SequenceOrderedBy(output, order)
BY Z3T(30) DEF SelectedSequence, ExactSelection, IncreasingSelection, SequenceOrderedBy

THEOREM IncompleteSelectionWitness ==
    LET source == <<0, 1>>
        output == <<0>>
        selection == [i \in 1..1 |-> i]
    IN /\ SelectedSequence(source, output, selection)
       /\ IncreasingSelection(output, selection)
       /\ InjectiveSelection(output, selection)
       /\ ~ExactSelection(source, output, selection, {0, 1})
       /\ SequenceHosts(output) # SequenceHosts(source) \intersect {0, 1}
BY Z3T(30) DEF SelectedSequence, ExactSelection, IncreasingSelection,
    InjectiveSelection, SequenceHosts

THEOREM DuplicateSelectionWitness ==
    LET source == <<0>>
        output == <<0, 0>>
        selection == [i \in 1..2 |-> 1]
    IN /\ SelectedSequence(source, output, selection)
       /\ ExactSelection(source, output, selection, {0})
       /\ SequenceUnique(source)
       /\ ~InjectiveSelection(output, selection)
       /\ ~SequenceUnique(output)
BY Z3T(30) DEF SelectedSequence, ExactSelection, InjectiveSelection, SequenceUnique
=============================================================================
