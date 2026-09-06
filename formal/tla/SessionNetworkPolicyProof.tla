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
=============================================================================
