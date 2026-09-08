----------------------- MODULE K8sProcessObservation -----------------------
EXTENDS Naturals, FiniteSets

\* Provider-only observation model. No Dispatch, claim or recovery state machine
\* is duplicated here. A receipt covers local containers, never remote effects.
CONSTANTS MaxGeneration, Containers
Generations == 1..MaxGeneration
VARIABLES current, present, started, exited, bound, failed, observation
vars == <<current, present, started, exited, bound, failed, observation>>
Domain(g) == {<<g, c>> : c \in Containers}
CompleteExit(g) == Domain(g) \subseteq exited

Init ==
    /\ current = 1
    /\ present = {1}
    /\ started = {}
    /\ exited = {}
    /\ bound = {}
    /\ failed = {}
    /\ observation = 0

\* Never-restart is a provider premise, not inferred from an empty status list.
Start(g, c) ==
    /\ g \in present
    /\ <<g, c>> \notin started
    /\ started' = started \cup {<<g, c>>}
    /\ UNCHANGED <<current, present, exited, bound, failed, observation>>

Bind ==
    /\ current \in present
    /\ Domain(current) \subseteq started
    /\ Domain(current) \intersect exited = {}
    /\ bound' = bound \cup {current}
    /\ UNCHANGED <<current, present, started, exited, failed, observation>>

Crash(g, c) ==
    /\ <<g, c>> \in started \ exited
    /\ exited' = exited \cup {<<g, c>>}
    /\ UNCHANGED <<current, present, started, bound, failed, observation>>

\* A controller may report Failed while the node's actual processes still run.
ControllerFailure(g) ==
    /\ g \in present
    /\ failed' = failed \cup {g}
    /\ UNCHANGED <<current, present, started, exited, bound, observation>>

Delete(g) ==
    /\ g \in present
    /\ present' = present \ {g}
    /\ observation' = 0
    /\ UNCHANGED <<current, started, exited, bound, failed>>

Replace ==
    /\ current \notin present
    /\ current < MaxGeneration
    /\ current' = current + 1
    /\ present' = present \cup {current + 1}
    /\ observation' = 0
    /\ UNCHANGED <<started, exited, bound, failed>>

\* Query a historical binding against the current name. Missing, replaced,
\* phase-only and partially stopped observations fail closed.
Observe(g) ==
    /\ g \in bound
    /\ observation' = IF /\ g = current
                           /\ current \in present
                           /\ CompleteExit(current)
                       THEN g ELSE 0
    /\ UNCHANGED <<current, present, started, exited, bound, failed>>

Next ==
    \/ \E g \in Generations, c \in Containers: Start(g, c) \/ Crash(g, c)
    \/ Bind
    \/ \E g \in Generations: ControllerFailure(g) \/ Delete(g) \/ Observe(g)
    \/ Replace

TypeOK ==
    /\ current \in Generations
    /\ present \subseteq Generations
    /\ started \subseteq (Generations \X Containers)
    /\ exited \subseteq started
    /\ bound \subseteq Generations
    /\ failed \subseteq Generations
    /\ observation \in Generations \cup {0}

ExactCompleteObservation == observation # 0 =>
    /\ observation = current
    /\ observation \in present \intersect bound
    /\ CompleteExit(observation)

\* Negative witness used by the mutation runner to reject vacuous proofs.
NoObservedExit == observation = 0
Safety == TypeOK /\ ExactCompleteObservation
Spec == Init /\ [][Next]_vars
=============================================================================
