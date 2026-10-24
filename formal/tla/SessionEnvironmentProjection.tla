-------------------- MODULE SessionEnvironmentProjection --------------------
EXTENDS TLC, Naturals

\* Process-local Session Environment projection boundary. Durable effect ids
\* order logical adoption operations; the binding plus immutable generation
\* identify the physical owner. A recovery reader may capture effect A, lose
\* the lifecycle guard, and arrive after effect B has already been published.
CONSTANTS Effects, Generations, Bindings

VARIABLES phase, localEffect, localGeneration, localBinding,
          capturedEffect, capturedGeneration, capturedBinding,
          retirementFence, lastOutcome, lastBeforeEffect, lastAfterEffect,
          lastCapturedGeneration, lastCapturedBinding

VARIABLES retirementCause, lastRecovery
recoveryVars == <<retirementCause, lastRecovery>>

ownerVars == <<phase, localEffect, localGeneration, localBinding,
          capturedEffect, capturedGeneration, capturedBinding,
          retirementFence, lastOutcome, lastBeforeEffect, lastAfterEffect,
          lastCapturedGeneration, lastCapturedBinding, recoveryVars>>

VARIABLE convergence
VARIABLE lastSynchronization
VARIABLE terminalOwner
projectionVars == <<ownerVars, lastSynchronization, convergence>>
vars == <<projectionVars, terminalOwner>>

NoValue == "none"
RetirementCauses == {"Revocation", "Discard", "Terminal", "Checkpoint"}


\* The second configuration composes this owner boundary across independent
\* Coordinator/Worker projections. One ordered root is still the only truth.
Processes == {"Coordinator", "Worker"}
SnapshotA == [revision |-> 1, binding |-> "A"]
SnapshotB == [revision |-> 2, binding |-> "B"]
ConvergenceInitial ==
    [root |-> SnapshotA, closed |-> FALSE,
     cached |-> [p \in Processes |-> SnapshotA],
     physical |-> [p \in Processes |-> IF p = "Worker" THEN "A" ELSE "none"],
     inbox |-> [p \in Processes |-> {SnapshotA}],
     last |-> [before |-> SnapshotA, after |-> SnapshotA, incoming |-> SnapshotA,
               physical |-> "none", admitted |-> TRUE]]

ProjectionInit ==
    /\ convergence = ConvergenceInitial
    /\ lastSynchronization = [observed |-> TRUE, closed |-> FALSE,
                                allowed |-> FALSE, admitted |-> TRUE, after |-> "Resident"]
    /\ phase = "Resident"
    /\ localEffect \in Effects
    /\ localGeneration \in Generations
    /\ localBinding \in Bindings
    /\ capturedEffect = NoValue
    /\ capturedGeneration = NoValue
    /\ capturedBinding = NoValue
    /\ retirementFence = NoValue
    /\ lastOutcome = NoValue
    /\ lastBeforeEffect = NoValue
    /\ lastAfterEffect = NoValue
    /\ lastCapturedGeneration = NoValue
    /\ lastCapturedBinding = NoValue
    /\ retirementCause = NoValue
    /\ lastRecovery = [exact |-> FALSE, closed |-> FALSE, allowed |-> FALSE,
                        recoverable |-> TRUE, cleared |-> FALSE,
                        cause |-> NoValue,
                        before |-> "Resident", after |-> "Resident"]

Init == ProjectionInit /\ terminalOwner = 0

Capture(effect, generation, binding) ==
    /\ UNCHANGED recoveryVars
    /\ effect \in Effects
    /\ generation \in Generations
    /\ binding \in Bindings
    /\ capturedEffect' = effect
    /\ capturedGeneration' = generation
    /\ capturedBinding' = binding
    /\ UNCHANGED <<phase, localEffect, localGeneration, localBinding,
                    retirementFence, lastOutcome, lastBeforeEffect,
                    lastAfterEffect, lastCapturedGeneration,
                    lastCapturedBinding>>

\* The canonical local adoption path may publish a newer logical effect for
\* the same physical Environment while a captured recovery read is waiting.
PublishNewEffect(effect) ==
    /\ UNCHANGED recoveryVars
    /\ phase = "Resident"
    /\ effect \in Effects
    /\ effect # localEffect
    /\ localEffect' = effect
    /\ UNCHANGED <<phase, localGeneration, localBinding,
                    capturedEffect, capturedGeneration, capturedBinding,
                    retirementFence, lastOutcome, lastBeforeEffect,
                    lastAfterEffect, lastCapturedGeneration,
                    lastCapturedBinding>>

BeginRetiring(cause) ==
    /\ cause \in RetirementCauses
    /\ retirementCause' = cause
    /\ UNCHANGED lastRecovery
    /\ phase = "Resident"
    /\ phase' = "Retiring"
    /\ retirementFence' = localEffect
    /\ UNCHANGED <<localEffect, localGeneration, localBinding,
                    capturedEffect, capturedGeneration, capturedBinding,
                    lastOutcome, lastBeforeEffect, lastAfterEffect,
                    lastCapturedGeneration, lastCapturedBinding>>

\* Installation is observational for Resident/Retiring owners. Exact effects
\* replay; stale effects on the same physical owner are absorbed; a different
\* binding or generation is rejected. None may relabel the local owner.
InstallCaptured ==
    /\ phase # "Vacant"
    /\ UNCHANGED recoveryVars
    /\ capturedEffect \in Effects
    /\ lastBeforeEffect' = localEffect
    /\ lastAfterEffect' = localEffect
    /\ lastCapturedGeneration' = capturedGeneration
    /\ lastCapturedBinding' = capturedBinding
    /\ lastOutcome' =
         IF capturedGeneration # localGeneration \/ capturedBinding # localBinding
         THEN "rejected"
         ELSE IF capturedEffect = localEffect THEN "exact" ELSE "stale"
    /\ capturedEffect' = NoValue
    /\ capturedGeneration' = NoValue
    /\ capturedBinding' = NoValue
    /\ UNCHANGED <<phase, localEffect, localGeneration, localBinding,
                    retirementFence>>

\* Cause/effect rules: exact captured owner + closed physical observation +
\* rebuild policy may retire Resident, Discard or Revocation, never Terminal or
\* Checkpoint. !exact abstracts every cause/identity/binding/Arc ABA mismatch.
\* Provider observation truth and Rust identity comparison remain TCB: this
\* finite transition model does not claim whole implementation refinement.
RecoverClosed(exact, closed, allowed) ==
    /\ phase # "Vacant"
    /\ LET recoverable == phase = "Resident" \/
                            retirementCause \in {"Revocation", "Discard"}
           admit == exact /\ closed /\ allowed /\ recoverable
       IN /\ phase' = IF admit THEN "Vacant" ELSE phase
          /\ lastRecovery' = [exact |-> exact, closed |-> closed,
                               allowed |-> allowed, recoverable |-> recoverable,
                               cause |-> retirementCause,
                               cleared |-> admit, before |-> phase, after |-> phase']
    /\ UNCHANGED <<localEffect, localGeneration, localBinding,
                    capturedEffect, capturedGeneration, capturedBinding,
                    retirementFence, retirementCause, lastOutcome,
                    lastBeforeEffect, lastAfterEffect, lastCapturedGeneration,
                    lastCapturedBinding>>

\* Caller boundary for an already-resident Worker. Physical loss is an
\* independent provider input, never inferred from the local Arc. This action
\* abstracts one observation plus policy decision, NOT atomic provider IO or
\* perpetual liveness after that observation. Reuse the owner transition above.
\* Decision rules S1 Ready => retain; S2 closed+continuity => reject/retain;
\* S3 closed+rebuild => clear the exact owner before replacement publication.
\* The Rust driver test crosses Runtime cached/evicted with S1-S3 and removes
\* the actual Local root; K3d separately checks replacement UID/PVC/file bytes.
SynchronizeResident(closed, allowed) ==
    /\ phase = "Resident"
    /\ LET observed == TRUE
       IN /\ IF observed THEN RecoverClosed(TRUE, closed, allowed)
                           ELSE UNCHANGED ownerVars
          /\ lastSynchronization' = [observed |-> observed, closed |-> closed,
                                       allowed |-> allowed,
                                       admitted |-> ~observed \/ ~closed \/ allowed,
                                       after |-> phase']

OwnerNext ==
    \/ \E effect \in Effects, generation \in Generations, binding \in Bindings:
           Capture(effect, generation, binding)
    \/ \E effect \in Effects: PublishNewEffect(effect)
    \/ \E cause \in RetirementCauses: BeginRetiring(cause)
    \/ InstallCaptured
    \/ \E exact, closed, allowed \in BOOLEAN: RecoverClosed(exact, closed, allowed)


Next ==
    /\ UNCHANGED <<convergence, terminalOwner>>
    /\ ( (OwnerNext /\ UNCHANGED lastSynchronization)
        \/ \E closed, allowed \in BOOLEAN: SynchronizeResident(closed, allowed))

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in {"Resident", "Retiring", "Vacant"}
    /\ retirementCause \in RetirementCauses \cup {NoValue}
    /\ localEffect \in Effects
    /\ localGeneration \in Generations
    /\ localBinding \in Bindings
    /\ capturedEffect \in Effects \cup {NoValue}
    /\ capturedGeneration \in Generations \cup {NoValue}
    /\ capturedBinding \in Bindings \cup {NoValue}
    /\ retirementFence \in Effects \cup {NoValue}
    /\ lastOutcome \in {NoValue, "exact", "stale", "rejected"}
    /\ lastBeforeEffect \in Effects \cup {NoValue}
    /\ lastAfterEffect \in Effects \cup {NoValue}
    /\ lastCapturedGeneration \in Generations \cup {NoValue}
    /\ lastCapturedBinding \in Bindings \cup {NoValue}

StaleProjectionNeverRollsBackLocalEffect ==
    lastOutcome = "stale" => lastAfterEffect = lastBeforeEffect

RejectedProjectionNeverRelabelsLocalEffect ==
    lastOutcome = "rejected" => lastAfterEffect = lastBeforeEffect

ExactProjectionIsAlsoAnIdentityStutter ==
    lastOutcome = "exact" => lastAfterEffect = lastBeforeEffect

AbsorptionRequiresExactPhysicalOwner ==
    lastOutcome \in {"exact", "stale"} =>
        lastCapturedGeneration = localGeneration /\
        lastCapturedBinding = localBinding

RetiringKeepsExactEffectFence ==
    phase = "Retiring" => localEffect = retirementFence

ProjectionNeverRewritesTheRetirementFence ==
    lastOutcome \in {"exact", "stale", "rejected"} /\ phase = "Retiring" =>
        localEffect = retirementFence

ClosedRecoveryIsExactAndComplete ==
      /\ lastRecovery.recoverable = (lastRecovery.before = "Resident" \/
                                      lastRecovery.cause \in {"Revocation", "Discard"})
      /\ lastRecovery.cleared = (lastRecovery.exact /\ lastRecovery.closed /\
                                 lastRecovery.allowed /\ lastRecovery.recoverable)
      /\ (lastRecovery.cleared => lastRecovery.after = "Vacant")
      /\ (~lastRecovery.cleared => lastRecovery.after = lastRecovery.before)

ResidentSynchronizationObservesPhysicalOwner ==
    /\ lastSynchronization.observed
    /\ lastSynchronization.admitted =
         (~lastSynchronization.closed \/ lastSynchronization.allowed)
    /\ lastSynchronization.after =
         IF lastSynchronization.closed /\ lastSynchronization.allowed
         THEN "Vacant" ELSE "Resident"

Safety ==
    /\ ResidentSynchronizationObservesPhysicalOwner
    /\ TypeOK
    /\ StaleProjectionNeverRollsBackLocalEffect
    /\ RejectedProjectionNeverRelabelsLocalEffect
    /\ ExactProjectionIsAlsoAnIdentityStutter
    /\ AbsorptionRequiresExactPhysicalOwner
    /\ RetiringKeepsExactEffectFence
    /\ ProjectionNeverRewritesTheRetirementFence
    /\ ClosedRecoveryIsExactAndComplete

\* Same predicate as the production root-receipt ordering kernel. Binding here
\* represents the equal aggregate-coordinate digest, with contents erased.
RootProjectionAdmitted(before, incoming) ==
    incoming.revision > before.revision \/ incoming = before
PhysicalProjectionCompatible(physical, incoming) ==
    physical = "none" \/ physical = incoming.binding

ConvergenceCapture(p) ==
    /\ UNCHANGED <<ownerVars, lastSynchronization, terminalOwner>>
    /\ convergence.root \notin convergence.inbox[p]
    /\ convergence' = [convergence EXCEPT !.inbox[p] = @ \cup {convergence.root}]
ConvergenceLose ==
    /\ UNCHANGED <<ownerVars, lastSynchronization, terminalOwner>>
    /\ ~convergence.closed
    /\ convergence' = [convergence EXCEPT !.closed = TRUE]
ConvergencePublish ==
    /\ UNCHANGED <<ownerVars, lastSynchronization, terminalOwner>>
    /\ convergence.closed /\ convergence.root = SnapshotA
    /\ convergence' = [convergence EXCEPT !.root = SnapshotB]
ConvergenceClear ==
    /\ UNCHANGED <<ownerVars, lastSynchronization, terminalOwner>>
    /\ convergence.closed /\ convergence.physical["Worker"] = "A"
    /\ convergence' = [convergence EXCEPT !.physical["Worker"] = "none"]
ConvergenceAdopt ==
    /\ UNCHANGED <<ownerVars, lastSynchronization, terminalOwner>>
    /\ convergence.cached["Worker"] = SnapshotB
    /\ convergence.physical["Worker"] = "none"
    /\ convergence' = [convergence EXCEPT !.physical["Worker"] = "B"]
ConvergenceDeliver(p, incoming) ==
    /\ UNCHANGED <<ownerVars, lastSynchronization, terminalOwner>>
    /\ incoming \in convergence.inbox[p]
    /\ LET before == convergence.cached[p]
           physical == convergence.physical[p]
           admitted == RootProjectionAdmitted(before, incoming) /\
                       PhysicalProjectionCompatible(physical, incoming)
           after == IF admitted THEN incoming ELSE before
       IN convergence' = [convergence EXCEPT
            !.cached[p] = after,
            !.last = [before |-> before, after |-> after, incoming |-> incoming,
                      physical |-> physical, admitted |-> admitted]]
ConvergenceLatest(p) == ConvergenceDeliver(p, convergence.root)
ConvergenceNext ==
    /\ UNCHANGED <<ownerVars, lastSynchronization, terminalOwner>>
    /\ (ConvergenceLose \/ ConvergencePublish \/ ConvergenceClear \/ ConvergenceAdopt
        \/ (\E p \in Processes: ConvergenceCapture(p))
        \/ \E p \in Processes, incoming \in {SnapshotA, SnapshotB}: ConvergenceDeliver(p, incoming))
ConvergenceSpec == Init /\ [][ConvergenceNext]_vars
    /\ WF_vars(ConvergenceClear) /\ WF_vars(ConvergenceAdopt)
    /\ \A p \in Processes: WF_vars(ConvergenceCapture(p)) /\ WF_vars(ConvergenceLatest(p))
ProjectionConvergenceSafety ==
    /\ convergence.last.after.revision >= convergence.last.before.revision
    /\ (~convergence.last.admitted => convergence.last.after = convergence.last.before)
    /\ (convergence.last.admitted =>
          PhysicalProjectionCompatible(convergence.last.physical, convergence.last.incoming))
ProjectionEventuallyConverges ==
    (convergence.root = SnapshotB) ~>
      (\A p \in Processes: convergence.cached[p] = SnapshotB) /\
      convergence.physical["Worker"] = "B"

\* Executed executor observations use this SAME admission predicate. The
\* producer reads before/after from the slot and records the actual Result.
\* No statement about provider atomicity or all Rust interleavings is implied.
ProjectionObservation(before, incoming, after, admitted) ==
    /\ admitted = (~before.exists \/
          RootProjectionAdmitted([revision |-> before.revision, binding |-> before.fingerprint],
                                 [revision |-> incoming.revision, binding |-> incoming.fingerprint]))
    /\ IF admitted
       THEN /\ after.exists
            /\ after.revision = incoming.revision
            /\ after.fingerprint = incoming.fingerprint
            /\ after.binding = incoming.binding
       ELSE after = before
\* Terminal composition of the SAME local owner boundary. One exact physical
\* target, two callers, one lease replacement, and one cancellation per caller.
\* Root-commit knowledge is an input: terminal authority must never invent it.
\* Resource preparation, its durable acceptance, physical removal and aggregate
\* acknowledgement are separate actions; cancellation may split any pair.
\* This models Host ordering, not truth of Provider observations, physical CAS,
\* credential transport, or Rust refinement. Unknown resources can block forever.
TerminalCallers == {1, 2}
TerminalOrigins == {"uncommitted", "lost", "committed"}
TerminalParticipants == {"Memory", "Files", "Repository"}
TerminalCurrent == terminalOwner.caller # 0 /\
                   terminalOwner.captured = terminalOwner.epoch
TerminalInit ==
    /\ ProjectionInit
    /\ \E origin \in TerminalOrigins:
      terminalOwner = [origin |-> origin, identity |-> IF origin = "committed" THEN 1 ELSE 0,
        rootBound |-> origin # "uncommitted", wrapper |-> 1, phase |-> "Candidate",
        epoch |-> 1, captured |-> 0, caller |-> 0, cancelled |-> {},
        participants |-> {}, prepared |-> FALSE, durable |-> FALSE,
        removed |-> FALSE, acknowledged |-> FALSE, published |-> FALSE,
        retried |-> FALSE, cancelledAfterRemoval |-> FALSE, badEffect |-> FALSE]
TerminalAcquire(c) ==
    /\ c \in TerminalCallers /\ terminalOwner.caller = 0
    /\ ~terminalOwner.acknowledged
    /\ terminalOwner' = [terminalOwner EXCEPT !.caller = c, !.captured = terminalOwner.epoch]
TerminalRelease ==
    /\ terminalOwner.caller # 0
    /\ terminalOwner' = [terminalOwner EXCEPT !.caller = 0, !.captured = 0]
TerminalCancel ==
    /\ terminalOwner.caller # 0 /\ terminalOwner.caller \notin terminalOwner.cancelled
    /\ terminalOwner' = [terminalOwner EXCEPT
         !.cancelledAfterRemoval = @ \/ terminalOwner.removed,
         !.cancelled = @ \cup {terminalOwner.caller}, !.caller = 0, !.captured = 0]
TerminalReplace ==
    /\ terminalOwner.epoch = 1 /\ ~terminalOwner.acknowledged
    /\ terminalOwner' = [terminalOwner EXCEPT !.epoch = 2]
TerminalRetire ==
    /\ TerminalCurrent /\ terminalOwner.phase = "Candidate"
    /\ terminalOwner' = [terminalOwner EXCEPT !.phase = "Retiring"]
TerminalAuxiliaryRetry ==
    /\ TerminalCurrent /\ terminalOwner.phase = "Retiring"
    /\ ~terminalOwner.removed
    /\ terminalOwner' = [terminalOwner EXCEPT !.retried = TRUE, !.wrapper = 1]
TerminalParticipant(p) ==
    /\ TerminalCurrent /\ terminalOwner.phase = "Retiring"
    /\ p \in TerminalParticipants \ terminalOwner.participants
    /\ terminalOwner' = [terminalOwner EXCEPT !.participants = @ \cup {p}]
TerminalPrepare ==
    /\ TerminalCurrent /\ terminalOwner.phase = "Retiring"
    /\ terminalOwner.participants = TerminalParticipants
    /\ terminalOwner' = [terminalOwner EXCEPT !.prepared = TRUE]
TerminalCommitPreparation ==
    /\ TerminalCurrent /\ terminalOwner.prepared
    /\ terminalOwner' = [terminalOwner EXCEPT !.durable = TRUE]
TerminalRemove(exact) ==
    /\ TerminalCurrent /\ exact /\ terminalOwner.durable
    /\ terminalOwner.phase = "Retiring"
    /\ terminalOwner' = [terminalOwner EXCEPT !.removed = TRUE,
         !.badEffect = (~TerminalCurrent \/ ~exact)]
TerminalAcknowledge ==
    /\ TerminalCurrent /\ terminalOwner.removed
    /\ terminalOwner' = [terminalOwner EXCEPT !.acknowledged = TRUE,
         !.phase = "Vacant", !.caller = 0, !.captured = 0]
TerminalNext ==
    \/ \E c \in TerminalCallers: TerminalAcquire(c)
    \/ TerminalRelease \/ TerminalCancel \/ TerminalReplace \/ TerminalRetire
    \/ TerminalAuxiliaryRetry \/ TerminalPrepare \/ TerminalCommitPreparation
    \/ \E p \in TerminalParticipants: TerminalParticipant(p)
    \/ \E exact \in BOOLEAN: TerminalRemove(exact)
    \/ TerminalAcknowledge
TerminalSpec == TerminalInit /\ [][TerminalNext /\ UNCHANGED projectionVars]_vars
TerminalIdentityUnchanged ==
    /\ terminalOwner.identity = IF terminalOwner.origin = "committed" THEN 1 ELSE 0
    /\ terminalOwner.rootBound = (terminalOwner.origin # "uncommitted")
    /\ ~terminalOwner.published
TerminalRetainsExactOwner ==
    /\ terminalOwner.wrapper = 1
    /\ (terminalOwner.phase = "Vacant") = terminalOwner.acknowledged
TerminalDisposalRequiresParticipants ==
    terminalOwner.removed => (terminalOwner.durable /\ terminalOwner.prepared /\
                               terminalOwner.participants = TerminalParticipants)
TerminalEffectsAreCurrentAndExact == ~terminalOwner.badEffect
TerminalOwnerSafety ==
    /\ terminalOwner.origin \in TerminalOrigins
    /\ terminalOwner.phase \in {"Candidate", "Retiring", "Vacant"}
    /\ terminalOwner.epoch \in 1..2 /\ terminalOwner.caller \in 0..2
    /\ terminalOwner.captured \in 0..2
    /\ terminalOwner.cancelled \subseteq TerminalCallers
    /\ terminalOwner.participants \subseteq TerminalParticipants
    /\ TerminalIdentityUnchanged /\ TerminalRetainsExactOwner
    /\ TerminalDisposalRequiresParticipants /\ TerminalEffectsAreCurrentAndExact
NoUncommittedTerminalCompletion ==
    ~(terminalOwner.origin = "uncommitted" /\ terminalOwner.acknowledged)
NoLostResponseTerminalCompletion ==
    ~(terminalOwner.origin = "lost" /\ terminalOwner.acknowledged)
NoCancelledAuxiliaryTakeoverCompletion ==
    ~(terminalOwner.epoch = 2 /\ terminalOwner.cancelled # {} /\
      terminalOwner.cancelledAfterRemoval /\ terminalOwner.retried /\ terminalOwner.acknowledged)
=============================================================================
