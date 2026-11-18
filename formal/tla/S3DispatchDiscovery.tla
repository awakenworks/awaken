------------------------- MODULE S3DispatchDiscovery -------------------------
EXTENDS Naturals, FiniteSets

Actors == {1, 2, 3}
None == 0
MaxGeneration == 4
VARIABLES generation, live, catalog, base, observed
vars == <<generation, live, catalog, base, observed>>

Init ==
    /\ generation = 0
    /\ live = None
    /\ catalog = {}
    /\ base = [actor \in Actors |-> None]
    /\ observed = {}

Prepare(actor) ==
    /\ actor \notin catalog
    /\ base[actor] = None
    /\ catalog' = catalog \cup {actor}
    /\ base' = [base EXCEPT ![actor] = generation + 1]
    /\ UNCHANGED <<generation, live, observed>>

Publish(actor) ==
    /\ actor \in catalog
    /\ base[actor] = generation + 1
    /\ generation < MaxGeneration
    /\ generation' = generation + 1
    /\ live' = actor
    /\ UNCHANGED <<catalog, base, observed>>

Advance ==
    /\ generation < MaxGeneration
    /\ generation' = generation + 1
    /\ UNCHANGED <<live, catalog, base, observed>>

Settle ==
    /\ live # None
    /\ generation < MaxGeneration
    /\ generation' = generation + 1
    /\ live' = None
    /\ UNCHANGED <<catalog, base, observed>>

\* A preparer can crash at any time. Fencing consumes its original version
\* before a sweeper deletes the only marker for a possible publication.
Sweep(actor) ==
    /\ actor \in catalog
    /\ actor # live
    /\ base[actor] # generation + 1
    /\ catalog' = catalog \ {actor}
    /\ UNCHANGED <<generation, live, base, observed>>

Discover ==
    /\ live \in catalog
    /\ live \notin observed
    /\ observed' = observed \cup {live}
    /\ UNCHANGED <<generation, live, catalog, base>>

Next ==
    \/ Advance
    \/ Settle
    \/ Discover
    \/ \E actor \in Actors : Prepare(actor) \/ Publish(actor) \/ Sweep(actor)

Spec == Init /\ [][Next]_vars /\ WF_vars(Discover)
PublishedCandidateExists == live = None \/ live \in catalog
PreparedCandidateCannotBeClaimed ==
    live = None \/ base[live] < generation + 1
EventuallyDiscovered ==
    \A actor \in Actors : (live = actor) ~> (actor \in observed \/ live # actor)

=============================================================================
