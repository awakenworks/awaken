----------------------- MODULE SessionNetworkPolicy -----------------------
EXTENDS TLC
CONSTANTS Hosts, Blank

\* Canonical host identities abstract Rust trim/ASCII-case folding. Blank is
\* the empty result of trimming and is not a host. This is a set abstraction,
\* not a proof of Unicode normalization, DNS, sandbox enforcement or Rust
\* collection refinement. None is {}, unrestricted projects to Hosts.
Canonical(hosts) == hosts \ {Blank}
Meet(left, right) == Canonical(left) \intersect Canonical(right)

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
