------------------------- MODULE S3ClaimRouteRetention -------------------------
EXTENDS Naturals, FiniteSets

\* One UUIDv7 request has a fixed expiry. Two Coordinators can propose
\* different Sessions while the sweeper advances the cutoff and deletes its
\* route. In-flight creates and validations can outlive their first check.
Sessions == {1, 2}
NoSession == 0
Expiry == 1
VARIABLES cutoff, clock, route, opened, readRoute, accepted
vars == <<cutoff, clock, route, opened, readRoute, accepted>>

Init ==
    /\ cutoff = 0
    /\ clock = 0
    /\ route = NoSession
    /\ opened = {}
    /\ readRoute = [session \in Sessions |-> NoSession]
    /\ accepted = {}

Begin(session) ==
    /\ clock < Expiry /\ cutoff < Expiry
    /\ session \notin opened
    /\ opened' = opened \cup {session}
    /\ UNCHANGED <<cutoff, clock, route, readRoute, accepted>>

Open(session) ==
    /\ session \in opened
    /\ route' = IF route = NoSession THEN session ELSE route
    /\ readRoute' = [readRoute EXCEPT ![session] = route']
    /\ opened' = opened \ {session}
    /\ UNCHANGED <<cutoff, clock, accepted>>

Validate(session) ==
    /\ readRoute[session] # NoSession
    /\ clock < Expiry /\ cutoff < Expiry
    /\ accepted' = accepted \cup {readRoute[session]}
    /\ UNCHANGED <<cutoff, clock, route, opened, readRoute>>

SetClock ==
    /\ clock' \in {0, Expiry}
    /\ UNCHANGED <<cutoff, route, opened, readRoute, accepted>>

AdvanceCutoff ==
    /\ clock > cutoff
    /\ cutoff' = clock
    /\ UNCHANGED <<clock, route, opened, readRoute, accepted>>

Collect ==
    /\ cutoff >= Expiry
    /\ route' = NoSession
    /\ UNCHANGED <<cutoff, clock, opened, readRoute, accepted>>

Next ==
    \/ SetClock
    \/ AdvanceCutoff
    \/ Collect
    \/ \E session \in Sessions : Begin(session) \/ Open(session) \/ Validate(session)

Spec == Init /\ [][Next]_vars
AtMostOneAcceptedSession == Cardinality(accepted) <= 1
RetirementIsPermanent == [][cutoff' >= cutoff]_vars
RetiredRouteNeverAuthorizes == [][cutoff >= Expiry => accepted' = accepted]_vars

=============================================================================
