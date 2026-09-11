----------------------- MODULE K8sProcessObservation -----------------------
EXTENDS Naturals, FiniteSets

\* Provider-only observation model. No Dispatch, claim or recovery state machine
\* is duplicated here. A receipt covers local containers, never remote effects.
CONSTANTS MaxGeneration, Containers
Generations == 1..MaxGeneration
VARIABLES current, present, started, exited, authenticated, authenticatedIncarnation, bound, failed, observation
VARIABLES retained, protectedBindings, stopRequests, closedAdmission, stopObservation
VARIABLES durableExitReceipts, releasedEvidence
stopState == <<retained, protectedBindings, stopRequests, closedAdmission, stopObservation,
               durableExitReceipts, releasedEvidence>>
vars == <<current, present, started, exited, authenticated, authenticatedIncarnation, bound, failed, observation, stopState>>
Domain(g) == {<<g, c>> : c \in Containers}
CompleteExit(g) == Domain(g) \subseteq exited
StopRequested(g) == \E request \in stopRequests: request[1] = g
ExactExit(g) == /\ g = current
                /\ current \in present
                /\ CompleteExit(current)

Init ==
    /\ current = 1
    /\ present = {1}
    /\ started = {}
    /\ exited = {}
    /\ authenticated = 0
    /\ authenticatedIncarnation = 0
    /\ bound = {}
    /\ failed = {}
    /\ observation = 0
    /\ retained = {}
    /\ protectedBindings = {}
    /\ stopRequests = {}
    /\ closedAdmission = {}
    /\ stopObservation = 0
    /\ durableExitReceipts = {}
    /\ releasedEvidence = {}

\* Never-restart is a provider premise, not inferred from an empty status list.
Start(g, c) ==
    /\ g \in present
    /\ <<g, c>> \notin started
    /\ started' = started \cup {<<g, c>>}
    /\ UNCHANGED <<current, present, exited, authenticated, authenticatedIncarnation, bound, failed, observation, stopState>>

\* TokenReview and the subsequent Pod read are separate observations. A name
\* replacement may occur between them; authentication is not a cached mapping.
\* A Pod UID fixes the registration incarnation; Registry still owns its
\* generation and absorbing closure (checked by its kernel and HTTP traces).
\* Generations here enumerate Pod UIDs, not Registry lease generations.
Authenticate(incarnation) ==
    /\ current \in present
    /\ incarnation = current
    /\ authenticated' = current
    /\ authenticatedIncarnation' = incarnation
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
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation, failed, observation, retained, stopRequests, closedAdmission, stopObservation, durableExitReceipts, releasedEvidence>>

Crash(g, c) ==
    /\ <<g, c>> \in started \ exited
    /\ exited' = exited \cup {<<g, c>>}
    /\ UNCHANGED <<current, present, started, authenticated, authenticatedIncarnation, bound, failed, observation, stopState>>

\* A controller may report Failed while the node's actual processes still run.
ControllerFailure(g) ==
    /\ g \in present
    /\ failed' = failed \cup {g}
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation, bound, observation, stopState>>

Delete(g) ==
    /\ g \in present
    /\ present' = present \ {g}
    /\ observation' = 0
    /\ stopObservation' = 0
    /\ UNCHANGED <<current, started, exited, authenticated, authenticatedIncarnation, bound, failed,
                    retained, protectedBindings, stopRequests, closedAdmission, durableExitReceipts, releasedEvidence>>

Replace ==
    /\ current \notin present
    /\ current < MaxGeneration
    /\ current' = current + 1
    /\ present' = present \cup {current + 1}
    /\ observation' = 0
    /\ stopObservation' = 0
    /\ UNCHANGED <<started, exited, authenticated, authenticatedIncarnation, bound, failed,
                    retained, protectedBindings, stopRequests, closedAdmission, durableExitReceipts, releasedEvidence>>

\* Query a historical binding against the current name. Missing, replaced,
\* phase-only and partially stopped observations fail closed.
Observe(g) ==
    /\ <<g, g>> \in bound
    /\ observation' = IF ExactExit(g) THEN g ELSE 0
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation, bound, failed, stopState>>

\* The stopper consumes the same exact exit facts, but its returned observation
\* additionally requires the Pod's accepted deletion intent. This is an output
\* of the existing provider port, not a second recovery authority or store.
ObserveStopped(g) ==
    /\ <<g, g>> \in bound
    /\ stopObservation' = IF /\ ExactExit(g)
                               /\ g \in protectedBindings
                               /\ g \in retained
                               /\ g \in closedAdmission
                               /\ StopRequested(g)
                           THEN g ELSE 0
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation, bound, failed,
                    observation, retained, protectedBindings, stopRequests, closedAdmission, durableExitReceipts, releasedEvidence>>

\* Trusted deployment installs retention; external loss remains possible and
\* is not interpreted as proof. Authorized release is modeled separately below.
Retention(g) ==
    /\ g \in present
    /\ g \notin releasedEvidence
    /\ retained' \in {retained \cup {g}, retained \ {g}}
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation, bound, failed,
                    observation, protectedBindings, stopRequests, closedAdmission, stopObservation, durableExitReceipts, releasedEvidence>>

\* The application commits the subject-bound provider observation into the
\* existing Registry history before any finalizer update. This is a Worker-level
\* fact, not Dispatch retirement or complete remote-attempt evidence.
CommitExitReceipt(g) ==
    /\ stopObservation = g
    /\ durableExitReceipts' = durableExitReceipts \cup {g}
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation,
                    bound, failed, observation, retained, protectedBindings, stopRequests,
                    closedAdmission, stopObservation, releasedEvidence>>

\* Release is exact and idempotent. It may complete after absence or replacement,
\* but only a durable old-generation receipt authorizes removing its retention.
ReleaseEvidence(g) ==
    /\ g \in durableExitReceipts
    /\ releasedEvidence' = releasedEvidence \cup {g}
    /\ retained' = retained \ {g}
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation,
                    bound, failed, observation, protectedBindings, stopRequests, closedAdmission,
                    stopObservation, durableExitReceipts>>

\* Interface fact supplied by exact Registry expiry, not a duplicate Registry
\* state machine. Its atomic renewal/replacement rules have their own production
\* kernel/Kani/Loom tests. Closure is absorbing for this authenticated Pod UID.
CloseAdmission(g) ==
    /\ <<g, g>> \in bound
    /\ closedAdmission' = closedAdmission \cup {g}
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation, bound, failed,
                    observation, retained, protectedBindings, stopRequests, stopObservation, durableExitReceipts, releasedEvidence>>

\* UID/resourceVersion conditional DELETE linearizes at the API. A lost reply
\* permits this exact retry, not an unconditional name-based write or receipt.
RequestStop(g) ==
    /\ <<g, g>> \in bound
    /\ g \in closedAdmission
    /\ g \in protectedBindings
    /\ current = g
    /\ current \in present
    /\ current \in retained
    /\ ~StopRequested(g)
    /\ stopRequests' = stopRequests \cup
         {<<g, current, g \in protectedBindings, current \in retained, g \in closedAdmission, CompleteExit(current)>>}
    /\ UNCHANGED <<current, present, started, exited, authenticated, authenticatedIncarnation, bound, failed,
                    observation, retained, protectedBindings, closedAdmission, stopObservation, durableExitReceipts, releasedEvidence>>

Next ==
    \/ \E g \in Generations, c \in Containers: Start(g, c) \/ Crash(g, c)
    \/ (\E incarnation \in Generations: Authenticate(incarnation)) \/ Bind
    \/ \E g \in Generations: ControllerFailure(g) \/ Delete(g) \/ Observe(g)
                              \/ Retention(g) \/ RequestStop(g) \/ CloseAdmission(g) \/ ObserveStopped(g)
                              \/ CommitExitReceipt(g) \/ ReleaseEvidence(g)
    \/ Replace

TypeOK ==
    /\ current \in Generations
    /\ present \subseteq Generations
    /\ started \subseteq (Generations \X Containers)
    /\ exited \subseteq started
    /\ authenticated \in Generations \cup {0}
    /\ authenticatedIncarnation \in Generations \cup {0}
    /\ bound \subseteq ((Generations \cup {0}) \X Generations)
    /\ failed \subseteq Generations
    /\ observation \in Generations \cup {0}
    /\ stopObservation \in Generations \cup {0}
    /\ retained \subseteq Generations
    /\ protectedBindings \subseteq Generations
    /\ closedAdmission \subseteq Generations
    /\ stopRequests \subseteq (Generations \X Generations \X BOOLEAN \X BOOLEAN \X BOOLEAN \X BOOLEAN)
    /\ durableExitReceipts \subseteq Generations
    /\ releasedEvidence \subseteq Generations

ExactCompleteObservation == observation # 0 =>
    /\ observation = current
    /\ observation \in present
    /\ <<observation, observation>> \in bound
    /\ CompleteExit(observation)

StopObservationRequiresClosureAndExit == stopObservation # 0 =>
    /\ stopObservation = current
    /\ stopObservation \in present
    /\ <<stopObservation, stopObservation>> \in bound
    /\ CompleteExit(stopObservation)
    /\ StopRequested(stopObservation)
    /\ stopObservation \in closedAdmission

DurableExitRequiresExactClosedObservation == \A g \in durableExitReceipts :
    /\ <<g, g>> \in bound
    /\ g \in closedAdmission
    /\ CompleteExit(g)

EvidenceReleaseRequiresDurableExit ==
    /\ releasedEvidence \subseteq durableExitReceipts
    /\ releasedEvidence \intersect retained = {}

\* Negative witness used by the mutation runner to reject vacuous proofs.
NoObservedExit == observation = 0
AuthenticatedIncarnationIsPod == authenticatedIncarnation = authenticated
BindingMatchesAuthenticatedPod == \A pair \in bound : pair[1] = pair[2]
NoReplacementBinding == <<2, 2>> \notin bound
\* A fresh successor must also be able to produce its own exact exit evidence;
\* merely allowing its binding would not exclude permanently denied observation.
NoReplacementExit == observation # 2
StopRequestsAreExactAndRetained == \A request \in stopRequests :
    /\ request[1] = request[2]
    /\ request[3] /\ request[4] /\ request[5]
NoLiveStopRequest == \A request \in stopRequests : CompleteExit(request[1])
NoReplacementStop == \A request \in stopRequests : request[1] # 2
\* The sixth coordinate records exit at DELETE acceptance, not after it. This
\* witness excludes a stopper that permanently rejects an already-exited Pod.
NoPostExitStopReceipt == stopObservation = 0 \/
    ~\E request \in stopRequests: request[1] = stopObservation /\ request[6]
Safety == TypeOK /\ ExactCompleteObservation /\ BindingMatchesAuthenticatedPod
          /\ StopRequestsAreExactAndRetained /\ AuthenticatedIncarnationIsPod
          /\ StopObservationRequiresClosureAndExit
          /\ DurableExitRequiresExactClosedObservation
          /\ EvidenceReleaseRequiresDurableExit
Spec == Init /\ [][Next]_vars
=============================================================================
