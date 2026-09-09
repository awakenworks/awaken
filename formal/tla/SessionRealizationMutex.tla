------------------------- MODULE SessionRealizationMutex -------------------------
EXTENDS Naturals, FiniteSets, Sequences

CONSTANTS DriverA, DriverB, NoDriver, Sessions, MaxCompleted, MaxConcurrent, MaxRenewals,
          CheckPublicationRecovery
ASSUME /\ DriverA # DriverB
       /\ NoDriver \notin {DriverA, DriverB}
       /\ Sessions # {}
       /\ IsFiniteSet(Sessions)
       /\ MaxCompleted \in Nat \ {0}
       /\ MaxConcurrent \in Nat \ {0}
       /\ MaxConcurrent <= Cardinality(Sessions)
       /\ MaxRenewals \in Nat
       /\ CheckPublicationRecovery \in BOOLEAN

Drivers == {DriverA, DriverB}
VARIABLES owner, phase, completed, remainingRenewals, leaseVisible, receiptCurrent,
          publication
vars == <<owner, phase, completed, remainingRenewals, leaseVisible, receiptCurrent,
          publication>>

(* One bounded refinement of this same phase driver, not another realization
   owner. Stage may recover an old generation while its replacement activates.
   Names represent immutable generation identities; expiry equality remains
   the existing receiptCurrent abstraction. Fingerprints, CAS conflicts, real
   transport and arbitrary generations are checked elsewhere, not proved here. *)
Generations == {"old", "new", "keep"}
ActiveGenerations == {"new", "keep"}
EmptyPublication == [staged |-> {}, publish |-> {}, drain |-> {}, lost |-> FALSE]

ActiveSessions == {session \in Sessions : owner[session] # NoDriver}

Init == /\ owner = [session \in Sessions |-> NoDriver]
        /\ phase = [session \in Sessions |-> "Idle"]
        /\ completed = [session \in Sessions |-> 0]
        /\ remainingRenewals = [session \in Sessions |-> 0]
        /\ leaseVisible = [session \in Sessions |-> FALSE]
        /\ receiptCurrent = [session \in Sessions |-> FALSE]
        /\ publication = [session \in Sessions |-> EmptyPublication]

Acquire(session, d) ==
  /\ session \in Sessions
  /\ d \in Drivers
  /\ owner[session] = NoDriver
  /\ completed[session] < MaxCompleted
  /\ Cardinality(ActiveSessions) < MaxConcurrent
  /\ owner' = [owner EXCEPT ![session] = d]
  /\ phase' = [phase EXCEPT ![session] = "WaitingForControl"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = MaxRenewals]
  /\ publication' = [publication EXCEPT ![session] = EmptyPublication]
  /\ UNCHANGED <<completed, leaseVisible, receiptCurrent>>

ControlRespond(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ phase' = [phase EXCEPT ![session] = "PreparingResources"]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = TRUE]
  /\ \E recovering \in IF CheckPublicationRecovery THEN BOOLEAN ELSE {FALSE} :
       publication' = [publication EXCEPT ![session] =
         [staged |-> IF recovering THEN Generations ELSE {"new"},
          publish |-> {}, drain |-> {}, lost |-> FALSE]]
  /\ UNCHANGED <<owner, completed, remainingRenewals, receiptCurrent>>

ControlTimeout(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ UNCHANGED <<completed, leaseVisible, receiptCurrent, publication>>

(* Reads hold the existing driver lock but not renewal capacity. A temporary
   failure is stuttering; explicit loss/expiry cancels before lock-wait cleanup.
   Owner/epoch/expiry CAS arithmetic is proved by the contract kernels, not by
   this scheduling abstraction. *)
PrepareResources(session, d) ==
  /\ owner[session] = d
  /\ phase[session] = "PreparingResources"
  /\ leaseVisible[session]
  /\ phase' = [phase EXCEPT ![session] = "Driving"]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible, receiptCurrent, publication>>

LosePreparationAuthority(session, d) ==
  /\ owner[session] = d
  /\ phase[session] = "PreparingResources"
  /\ phase' = [phase EXCEPT ![session] = "CancellingResources"]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<owner, completed, remainingRenewals, receiptCurrent, publication>>

AbortPreparation(session, d) ==
  /\ owner[session] = d
  /\ phase[session] \in {"PreparingResources", "CancellingResources"}
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<completed, receiptCurrent, publication>>

Renew(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] \in {"PreparingResources", "Driving", "Publishing"}
  /\ leaseVisible[session]
  /\ MaxRenewals > 0
  (* Cyclic finite abstraction permits infinitely many successful renewals;
     it never assumes that heartbeats eventually stop. receiptCurrent records
     only equality versus an authorized older expiry, not epoch arithmetic. *)
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = IF @ = 0 THEN MaxRenewals ELSE @ - 1]
  /\ receiptCurrent' = [receiptCurrent EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<owner, phase, completed, leaseVisible, publication>>

PublicationCandidates(staged, active) == staged \cap active

PublicationObservation(staged, active, published, drained) ==
    /\ published = PublicationCandidates(staged, active)
    /\ published \cap drained = {}

Advance(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Driving"
  /\ phase' = [phase EXCEPT ![session] = "Publishing"]
  /\ receiptCurrent' = [receiptCurrent EXCEPT ![session] = TRUE]
  /\ \E lost \in IF CheckPublicationRecovery THEN BOOLEAN ELSE {FALSE} :
       publication' = [publication EXCEPT ![session] =
         [staged |-> @.staged,
           publish |-> PublicationCandidates(@.staged, ActiveGenerations),
          drain |-> {"old"}, lost |-> lost]]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible>>

(* Completed physical evidence remains valid under live same-generation
   authority. Renewal does not send this phase back to Driving. *)
Acknowledge(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Publishing"
  /\ ~publication[session].lost
  /\ phase' = [phase EXCEPT ![session] = "Committing"]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible, receiptCurrent, publication>>

(* Retrying activation after its committed response was lost must validate
   retained old receipts against Draining claims, without publishing them. *)
RetryActivation(session, d) ==
  /\ owner[session] = d
  /\ phase[session] = "Publishing"
  /\ publication[session].lost
  /\ publication[session].staged \subseteq
       (publication[session].publish \cup publication[session].drain)
  /\ publication' = [publication EXCEPT ![session].lost = FALSE]
  /\ UNCHANGED <<owner, phase, completed, remainingRenewals, leaseVisible, receiptCurrent>>

Release(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Committing"
  /\ completed[session] < MaxCompleted
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ completed' = [completed EXCEPT ![session] = @ + 1]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<receiptCurrent, publication>>

ControlSettles(session, d) ==
  ControlRespond(session, d) \/ ControlTimeout(session, d)

Next == \E session \in Sessions, d \in Drivers :
          Acquire(session, d)
          \/ ControlRespond(session, d)
          \/ ControlTimeout(session, d)
          \/ PrepareResources(session, d)
          \/ LosePreparationAuthority(session, d)
          \/ AbortPreparation(session, d)
          \/ Renew(session, d)
          \/ Advance(session, d)
          \/ Acknowledge(session, d)
          \/ RetryActivation(session, d)
          \/ Release(session, d)

Fairness ==
  /\ \A session \in Sessions, d \in Drivers : WF_vars(ControlSettles(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(PrepareResources(session, d) \/ AbortPreparation(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Advance(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Acknowledge(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(RetryActivation(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Release(session, d))

Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ owner \in [Sessions -> Drivers \cup {NoDriver}]
          /\ phase \in [Sessions -> {"Idle", "WaitingForControl", "PreparingResources", "CancellingResources", "Driving", "Publishing", "Committing"}]
          /\ completed \in [Sessions -> 0..MaxCompleted]
          /\ remainingRenewals \in [Sessions -> 0..MaxRenewals]
          /\ leaseVisible \in [Sessions -> BOOLEAN]
          /\ receiptCurrent \in [Sessions -> BOOLEAN]
          /\ publication \in [Sessions ->
                [staged : SUBSET Generations, publish : SUBSET Generations,
                 drain : SUBSET Generations, lost : BOOLEAN]]
OneDriverPerSession ==
  \A session \in Sessions :
    phase[session] = "Idle" <=> owner[session] = NoDriver
BoundedCrossSessionConcurrency ==
  Cardinality(ActiveSessions) <= MaxConcurrent
PublicationExcludesRetiredGenerations ==
  \A session \in Sessions :
    /\ publication[session].publish \subseteq ActiveGenerations
    /\ publication[session].publish \cap publication[session].drain = {}
    /\ (phase[session] \in {"Publishing", "Committing"} =>
          /\ publication[session].publish = (publication[session].staged \cap ActiveGenerations)
          /\ publication[session].drain = {"old"})
Safety == TypeOK /\ OneDriverPerSession /\ BoundedCrossSessionConcurrency
          /\ PublicationExcludesRetiredGenerations

ResourcesHaveRenewableAuthority ==
  \A session \in Sessions :
    (phase[session] \in {"PreparingResources", "Driving", "Publishing", "Committing"}) = leaseVisible[session]

LostActivationCanReplay ==
  \A session \in Sessions :
    phase[session] = "Publishing" /\ publication[session].lost =>
      ENABLED RetryActivation(session, owner[session])

NoRecoveredPublicationReached ==
  \A session \in Sessions :
    ~(phase[session] = "Publishing" /\ "old" \in publication[session].staged)
NoLostActivationReached ==
  \A session \in Sessions : ~publication[session].lost

(* A bounded Control request cannot retain scheduler capacity forever. Under
   weak process fairness, each acquired slot either times out or completes its
   phase driver and returns to Idle. *)
EveryAcquiredSlotEventuallyReleases ==
  \A session \in Sessions : [](owner[session] # NoDriver => <>(owner[session] = NoDriver))
(* Continuous observation refinement of the canonical driver. The coarse
   fair scheduler above remains the exhaustive interleaving model. This finite
   relation checks actual durable roots AND completed adapter calls, including
   process loss. It does not assert an unbounded refinement theorem for Rust. *)
EffectKeys(s) == {s.effects[i].key : i \in 1..Len(s.effects)}
RootView(s) == <<s.revision, s.created, s.updated, s.owner, s.incarnation,
                s.epoch, s.expiry, s.active, s.acked, s.known, s.rootrefs, s.pending, s.retiring>>
PhysicalView(s) == <<s.staged, s.published, s.drained>>
EffectRefs(s) == {s.effects[i] : i \in 1..Len(s.effects)}
PhysicalRefs(s) == <<s.stagefacts, s.publishfacts, s.drainfacts>>
ExpectedView(s) == <<s.action, s.expectstage, s.expectpub, s.expectdrain, s.planstage, s.planpub, s.plandrain>>
CurrentEffects(s) ==
    \A i \in 1..Len(s.effects) :
      /\ s.effects[i].session = s.session
      /\ s.effects[i].incarnation = s.incarnation
      /\ s.effects[i].epoch = s.epoch
      /\ s.effects[i].expiry <= s.expiry

RefAuthorized(asserted, current) ==
    /\ <<asserted.key, asserted.session, asserted.incarnation, asserted.epoch>> =
         <<current.key, current.session, current.incarnation, current.epoch>>
    /\ asserted.expiry <= current.expiry
RootAuthorizes(s, refs) ==
    \A ref \in refs : \E current \in s.rootrefs : RefAuthorized(ref, current)

ObservedSafety(s) ==
    /\ s.event \in {"begin-after", "activate-before", "activate-after", "ack-before", "ack-after",
                       "projection", "stage", "published", "complete"} => s.hasauthority
    /\ s.hasauthority =>
         /\ <<s.authorityowner, s.authorityincarnation, s.authorityepoch>> = <<s.owner, s.incarnation, s.epoch>>
         /\ s.authoritytime <= s.expiry
    /\ s.event = "projection" =>
         /\ CASE s.action = "stage" -> s.expectstage = s.pending \cup (s.active \ s.acked)
              [] s.action = "publish" -> s.expectpub = s.active \ s.acked /\ s.expectdrain = s.retiring
              [] OTHER -> s.pending = {} /\ s.retiring = {}
         /\ RootAuthorizes(s, s.planstage \cup s.planpub \cup s.plandrain)
         /\ \A ref \in s.planstage \cup s.planpub \cup s.plandrain : ref.session = s.session
    /\ s.event = "published" =>
         /\ s.reportedpub = s.planpub
         /\ s.reporteddrain = s.plandrain
    /\ s.acked \subseteq s.active
    /\ s.active \subseteq s.known
    /\ s.event = "stage" =>
         /\ CurrentEffects(s)
         /\ EffectKeys(s) = s.expectstage
         /\ EffectRefs(s) = s.planstage
         /\ EffectKeys(s) \subseteq s.known
    /\ s.event = "publish-one" =>
         /\ CurrentEffects(s)
         /\ Cardinality(EffectKeys(s)) = 1
         /\ EffectKeys(s) \subseteq s.expectpub \cap s.active \cap s.staged
         /\ EffectRefs(s) \subseteq s.planpub
         /\ \A ref \in EffectRefs(s) : \E staged \in s.stagefacts : RefAuthorized(staged, ref)
    /\ s.event = "drain-one" =>
         /\ Cardinality(EffectKeys(s)) = 1
         /\ EffectKeys(s) \subseteq s.expectdrain
         /\ EffectRefs(s) \subseteq s.plandrain
         /\ EffectKeys(s) \cap s.active = {}
         /\ \A i \in 1..Len(s.effects) : s.effects[i].session = s.session
    /\ s.event \in {"published", "ack-before", "ack-after"} =>
         /\ s.expectpub \subseteq s.published
         /\ s.expectdrain \subseteq s.drained
         /\ s.planpub \subseteq s.publishfacts
         /\ s.plandrain \subseteq s.drainfacts
         /\ PublicationObservation(s.staged, s.active, s.published, s.drained)
    /\ s.event \in {"ack-after", "complete"} =>
         /\ s.active \subseteq s.acked
         /\ s.active \subseteq s.published

ObservedOrder(s, t) ==
    CASE t.event = "begin-before" -> s.event \in {"crash", "complete"}
      [] t.event = "begin-after" -> s.event = "begin-before"
      [] t.event = "projection" -> s.event \in {"begin-after", "activate-after"}
      [] t.event = "stage" -> s.event = "projection" /\ s.action = "stage"
      [] t.event = "activate-before" -> s.event = "stage"
      [] t.event = "activate-after" -> s.event = "activate-before"
      [] t.event = "publish-one" ->
           s.event \in {"projection", "publish-one"} /\ s.action = "publish"
      [] t.event = "drain-one" ->
           /\ s.event \in {"projection", "publish-one", "drain-one"}
           /\ s.action = "publish" /\ s.expectpub \subseteq s.published
      [] t.event = "published" ->
           s.event \in {"projection", "publish-one", "drain-one"} /\ s.action = "publish"
      [] t.event = "ack-before" -> s.event = "published"
      [] t.event = "ack-after" -> s.event = "ack-before"
      [] t.event = "complete" ->
           s.event = "ack-after" \/ (s.event = "projection" /\ s.action = "complete")
      [] t.event = "crash" -> s.event \in {"stage", "publish-one", "drain-one", "published", "ack-after"}
      [] OTHER -> FALSE

ObservedNext(s, t) ==
    /\ t.sequence = s.sequence + 1
    /\ t.session = s.session
    /\ t.created = s.created
    /\ t.updated >= s.updated
    /\ t.revision >= s.revision
    /\ ObservedOrder(s, t)
    /\ t.event \notin {"begin-before", "begin-after", "activate-after", "ack-after"}
         => RootView(t) = RootView(s)
    /\ t.event \notin {"projection", "crash"} => ExpectedView(t) = ExpectedView(s)
    /\ CASE t.event = "crash" -> PhysicalView(t) = <<{}, {}, {}>>
         [] t.event = "stage" -> PhysicalView(t) = <<s.staged \cup EffectKeys(t), s.published, s.drained>>
         [] t.event = "publish-one" -> PhysicalView(t) = <<s.staged, s.published \cup EffectKeys(t), s.drained>>
         [] t.event = "drain-one" -> PhysicalView(t) = <<s.staged, s.published, s.drained \cup EffectKeys(t)>>
         [] OTHER -> PhysicalView(t) = PhysicalView(s)
    /\ CASE t.event = "crash" -> PhysicalRefs(t) = <<{}, {}, {}>>
         [] t.event = "stage" -> PhysicalRefs(t) = <<s.stagefacts \cup EffectRefs(t), s.publishfacts, s.drainfacts>>
         [] t.event = "publish-one" -> PhysicalRefs(t) = <<s.stagefacts, s.publishfacts \cup EffectRefs(t), s.drainfacts>>
         [] t.event = "drain-one" -> PhysicalRefs(t) = <<s.stagefacts, s.publishfacts, s.drainfacts \cup EffectRefs(t)>>
         [] OTHER -> PhysicalRefs(t) = PhysicalRefs(s)
    /\ t.event = "publish-one" => EffectKeys(t) \cap s.published = {}
    /\ t.event = "drain-one" => EffectKeys(t) \cap s.drained = {}
    /\ t.event = "ack-after" =>
         /\ t.active = s.active
         /\ <<t.owner, t.incarnation, t.epoch>> = <<s.owner, s.incarnation, s.epoch>>

RealizationTraceIsRefinement(trace, cut, count) ==
    /\ Len(trace) > 2
    /\ trace[1].event = "begin-before" /\ trace[1].sequence = 0
    /\ trace[Len(trace)].event = "complete"
    /\ Cardinality(trace[Len(trace)].active) = count
    /\ Cardinality({i \in 1..Len(trace) : trace[i].event = "crash"}) = 1
    /\ \A i \in 1..Len(trace) : ObservedSafety(trace[i])
    /\ \A i \in 2..Len(trace) :
         /\ ObservedNext(trace[i-1], trace[i])
         /\ trace[i].event = "crash" => trace[i-1].event = cut

(* The phase-driver ActiveSessions/ControlTimeout model above deliberately
   abstracts driver ownership. It is NOT a refinement mapping for the renewal
   adapter: that adapter releases a request permit before reacquiring completion
   capacity, and its exact Session mutex may remain held independently.
   These observations check the actual adapter's separate resources at controlled
   poll/drop cuts, using the same model authority without a second controller.
   Tokio internals and arbitrary schedules are outside this finite relation. *)
SchedulerSnapshot(s, capacity) ==
    /\ s.available \in 0..capacity /\ s.reserved \in 0..capacity
    /\ s.request \in {"none", "queued", "held", "done"}
    /\ s.completion \in {"none", "queued", "held", "done"}
    /\ s.active \in BOOLEAN
    /\ s.active = (s.request = "held")
    /\ s.available + s.reserved + (IF s.active THEN 1 ELSE 0)
         + (IF s.completion = "held" THEN 1 ELSE 0) = capacity
    /\ s.completion # "none" => s.request = "done"

SchedulerCut(s) == <<s.available, s.reserved, s.request, s.completion>>
SchedulerObservationStep(s, t, capacity) ==
    /\ t.sequence = s.sequence + 1
    /\ t.scheduler = s.scheduler
    /\ CASE t.event = "queue-request" ->
          /\ s.request = "none"
          /\ SchedulerCut(t) = <<s.available, s.reserved, "queued", "none">>
        [] t.event = "wait-queued" ->
          /\ s.request = "queued" /\ s.available = 0
          /\ SchedulerCut(t) = SchedulerCut(s)
        [] t.event = "admit-request" ->
          /\ s.request = "queued" /\ s.reserved > 0
          /\ SchedulerCut(t) = <<s.available, s.reserved - 1, "held", "none">>
        [] t.event \in {"return", "deadline", "cancel-request"} ->
          /\ s.request = "held"
          /\ SchedulerCut(t) = <<s.available + 1, s.reserved, "done", "none">>
        [] t.event = "cancel-queued-request" ->
          /\ s.request = "queued"
          /\ SchedulerCut(t) = <<s.available, s.reserved, "done", "none">>
        [] t.event = "queue-completion" ->
          /\ s.request = "done" /\ s.completion = "none" /\ s.available > 0
          /\ SchedulerCut(t) = <<s.available - 1, s.reserved + 1, "done", "queued">>
        [] t.event = "admit-completion" ->
          /\ s.completion = "queued" /\ s.reserved > 0
          /\ SchedulerCut(t) = <<s.available, s.reserved - 1, "done", "held">>
        [] t.event = "cancel-queued-completion" ->
          /\ s.completion = "queued"
          /\ SchedulerCut(t) = <<s.available, s.reserved, "done", "done">>
        [] t.event \in {"finish", "cancel-held-completion"} ->
          /\ s.completion = "held"
          /\ SchedulerCut(t) = <<s.available + 1, s.reserved, "done", "done">>
        [] t.event = "final" ->
          /\ s.request = "done" /\ s.completion \in {"none", "done"}
          /\ SchedulerCut(t) = <<capacity, 0, "done", "done">>
        [] OTHER -> FALSE

ControlSchedulerObservation(trace, capacity) ==
    /\ capacity = 8 /\ Len(trace) >= 5
    /\ trace[1].event = "initial" /\ trace[1].sequence = 0
    /\ SchedulerCut(trace[1]) = <<0, capacity, "none", "none">>
    /\ trace[Len(trace)].event = "final"
    /\ \A i \in 1..Len(trace) : SchedulerSnapshot(trace[i], capacity)
    /\ \A i \in 2..Len(trace) : SchedulerObservationStep(trace[i-1], trace[i], capacity)

HeldRealizationMutexObservation(o) ==
    /\ o.capacity = 8 /\ o.peak = o.capacity
    /\ o.entered = 2 * o.capacity
    /\ Len(o.before) = o.capacity /\ Len(o.after) = o.capacity
    /\ \A i \in 1..o.capacity :
         /\ o.before[i].locked /\ o.after[i].locked
         /\ <<o.before[i].session, o.before[i].identity, o.before[i].lease>> =
              <<o.after[i].session, o.after[i].identity, o.after[i].lease>>
         /\ o.after[i].closed
    /\ Cardinality({o.before[i].session : i \in 1..o.capacity}) = o.capacity
    /\ Cardinality({o.before[i].identity : i \in 1..o.capacity}) = o.capacity

ReconciliationCapacityObservation(o) ==
    CASE o.kind = "adapter" -> ControlSchedulerObservation(o.trace, o.capacity)
      [] o.kind = "mutex" -> HeldRealizationMutexObservation(o)
      [] OTHER -> FALSE
=============================================================================
