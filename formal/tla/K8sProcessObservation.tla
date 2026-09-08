----------------------- MODULE K8sProcessObservation -----------------------
EXTENDS Naturals, FiniteSets

\* Provider-only observation model. No Dispatch, claim or recovery state machine
\* is duplicated here. A receipt covers local containers, never remote effects.
CONSTANTS MaxGeneration, Containers
Generations == 1..MaxGeneration
VARIABLES current, present, started, exited, authenticated, bound, failed, observation
VARIABLES retained, protectedBindings, stopRequests
stopState == <<retained, protectedBindings, stopRequests>>
vars == <<current, present, started, exited, authenticated, bound, failed, observation, stopState>>
Domain(g) == {<<g, c>> : c \in Containers}
CompleteExit(g) == Domain(g) \subseteq exited

Init ==
    /\ current = 1
    /\ present = {1}
    /\ started = {}
    /\ exited = {}
    /\ authenticated = 0
    /\ bound = {}
    /\ failed = {}
    /\ observation = 0
    /\ retained = {}
    /\ protectedBindings = {}
    /\ stopRequests = {}

\* Never-restart is a provider premise, not inferred from an empty status list.
Start(g, c) ==
    /\ g \in present
    /\ <<g, c>> \notin started
    /\ started' = started \cup {<<g, c>>}
    /\ UNCHANGED <<current, present, exited, authenticated, bound, failed, observation, stopState>>

\* TokenReview and the subsequent Pod read are separate observations. A name
\* replacement may occur between them; authentication is not a cached mapping.
Authenticate ==
    /\ current \in present
    /\ authenticated' = current
    /\ UNCHANGED <<current, present, started, exited, bound, failed, observation, stopState>>

Bind ==
    /\ current \in present
    /\ authenticated = current
    /\ Domain(current) \subseteq started
    /\ Domain(current) \intersect exited = {}
    /\ bound' = bound \cup {<<authenticated, current>>}
    \* This bounded model retains one original frozen binding per generation.
    \* A later finalizer cannot upgrade that value; Dispatch separately prevents
    \* replacing an admitted opaque binding with a newly constructed one.
    /\ protectedBindings' = IF /\ <<current, current>> \notin bound
                               /\ current \in retained
                            THEN protectedBindings \cup {current}
                            ELSE protectedBindings
    /\ UNCHANGED <<current, present, started, exited, authenticated, failed, observation, retained, stopRequests>>

Crash(g, c) ==
    /\ <<g, c>> \in started \ exited
    /\ exited' = exited \cup {<<g, c>>}
    /\ UNCHANGED <<current, present, started, authenticated, bound, failed, observation, stopState>>

\* A controller may report Failed while the node's actual processes still run.
ControllerFailure(g) ==
    /\ g \in present
    /\ failed' = failed \cup {g}
    /\ UNCHANGED <<current, present, started, exited, authenticated, bound, observation, stopState>>

Delete(g) ==
    /\ g \in present
    /\ present' = present \ {g}
    /\ observation' = 0
    /\ UNCHANGED <<current, started, exited, authenticated, bound, failed, stopState>>

Replace ==
    /\ current \notin present
    /\ current < MaxGeneration
    /\ current' = current + 1
    /\ present' = present \cup {current + 1}
    /\ observation' = 0
    /\ UNCHANGED <<started, exited, authenticated, bound, failed, stopState>>

\* Query a historical binding against the current name. Missing, replaced,
\* phase-only and partially stopped observations fail closed.
Observe(g) ==
    /\ <<g, g>> \in bound
    /\ observation' = IF /\ g = current
                           /\ current \in present
                           /\ CompleteExit(current)
                       THEN g ELSE 0
    /\ UNCHANGED <<current, present, started, exited, authenticated, bound, failed, stopState>>

\* Trusted deployment installs retention; external loss remains possible and
\* is not interpreted as proof. The stopper itself never releases this history.
Retention(g) ==
    /\ g \in present
    /\ retained' \in {retained \cup {g}, retained \ {g}}
    /\ UNCHANGED <<current, present, started, exited, authenticated, bound, failed,
                    observation, protectedBindings, stopRequests>>

\* UID/resourceVersion conditional DELETE linearizes at the API. A lost reply
\* permits this exact retry, not an unconditional name-based write or receipt.
RequestStop(g) ==
    /\ <<g, g>> \in bound
    /\ g \in protectedBindings
    /\ current = g
    /\ current \in present
    /\ current \in retained
    /\ ~CompleteExit(current)
    /\ stopRequests' = stopRequests \cup
         {<<g, current, g \in protectedBindings, current \in retained>>}
    /\ UNCHANGED <<current, present, started, exited, authenticated, bound, failed,
                    observation, retained, protectedBindings>>

Next ==
    \/ \E g \in Generations, c \in Containers: Start(g, c) \/ Crash(g, c)
    \/ Authenticate \/ Bind
    \/ \E g \in Generations: ControllerFailure(g) \/ Delete(g) \/ Observe(g)
                              \/ Retention(g) \/ RequestStop(g)
    \/ Replace

TypeOK ==
    /\ current \in Generations
    /\ present \subseteq Generations
    /\ started \subseteq (Generations \X Containers)
    /\ exited \subseteq started
    /\ authenticated \in Generations \cup {0}
    /\ bound \subseteq ((Generations \cup {0}) \X Generations)
    /\ failed \subseteq Generations
    /\ observation \in Generations \cup {0}
    /\ retained \subseteq Generations
    /\ protectedBindings \subseteq Generations
    /\ stopRequests \subseteq (Generations \X Generations \X BOOLEAN \X BOOLEAN)

ExactCompleteObservation == observation # 0 =>
    /\ observation = current
    /\ observation \in present
    /\ <<observation, observation>> \in bound
    /\ CompleteExit(observation)

\* Negative witness used by the mutation runner to reject vacuous proofs.
NoObservedExit == observation = 0
BindingMatchesAuthenticatedPod == \A pair \in bound : pair[1] = pair[2]
NoReplacementBinding == <<2, 2>> \notin bound
\* A fresh successor must also be able to produce its own exact exit evidence;
\* merely allowing its binding would not exclude permanently denied observation.
NoReplacementExit == observation # 2
StopRequestsAreExactAndRetained == \A request \in stopRequests :
    /\ request[1] = request[2]
    /\ request[3] /\ request[4]
NoLiveStopRequest == \A request \in stopRequests : CompleteExit(request[1])
NoReplacementStop == \A request \in stopRequests : request[1] # 2
Safety == TypeOK /\ ExactCompleteObservation /\ BindingMatchesAuthenticatedPod
          /\ StopRequestsAreExactAndRetained
Spec == Init /\ [][Next]_vars
=============================================================================
