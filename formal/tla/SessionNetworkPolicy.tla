----------------------- MODULE SessionNetworkPolicy -----------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC
CONSTANTS Hosts, Blank

\* Canonical host identities abstract Rust trim/ASCII-case folding. Blank is
\* the empty result of trimming and is not a host. This is a set abstraction,
\* not a proof of Unicode normalization, DNS, sandbox enforcement or Rust
\* collection refinement. None is {}, unrestricted projects to Hosts.
Canonical(hosts) == hosts \ {Blank}
Meet(left, right) == Canonical(left) \intersect Canonical(right)

\* Sequence projection vocabulary for the same canonical meet. A selection
\* witness identifies the retained source position of every output element;
\* it specifies stable filtering, not Rust iterator/library refinement.
SequenceHosts(sequence) == {sequence[i] : i \in DOMAIN sequence}
SelectedSequence(source, output, selection) ==
    /\ selection \in [DOMAIN output -> DOMAIN source]
    /\ \A i \in DOMAIN output: output[i] = source[selection[i]]
ExactSelection(source, output, selection, allowed) ==
    \A i \in DOMAIN source:
        (source[i] \in allowed) <=> (\E j \in DOMAIN output: selection[j] = i)
IncreasingSelection(output, selection) ==
    \A i, j \in DOMAIN output: i < j => selection[i] < selection[j]
InjectiveSelection(output, selection) ==
    \A i, j \in DOMAIN output: selection[i] = selection[j] => i = j
SequenceOrderedBy(sequence, order) ==
    \A i, j \in DOMAIN sequence: i < j => <<sequence[i], sequence[j]>> \in order
SequenceUnique(sequence) ==
    \A i, j \in DOMAIN sequence: sequence[i] = sequence[j] => i = j

VARIABLE left, right, result
vars == <<left, right, result>>
RawHostSets == SUBSET (Hosts \cup {Blank})

\* N7 cause/effect: both operands contain Blank but no real common host ->
\* empty deny result, never an allowlist containing an empty hostname.
\* All raw-set pairs are initial states; subsequent restrictions only narrow
\* the previous result, including repeated and reordered restriction inputs.
Init == /\ left \in RawHostSets /\ right \in RawHostSets
        /\ result = Meet(left, right)
Restrict == /\ left' = result
            /\ right' \in RawHostSets
            /\ result' = Meet(left', right')
Next == Restrict \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars

TypeOK == /\ left \in RawHostSets /\ right \in RawHostSets
          /\ result \subseteq Hosts
NoBlankHostIsGranted == Blank \notin result
NeitherOperandIsWidened == /\ result \subseteq left /\ result \subseteq right
ExactCanonicalMeet == result = Meet(left, right)
=============================================================================
