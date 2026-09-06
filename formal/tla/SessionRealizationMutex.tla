------------------------- MODULE SessionRealizationMutex -------------------------
EXTENDS Naturals, FiniteSets, Sequences

CONSTANTS DriverA, DriverB, NoDriver, Sessions, MaxCompleted, MaxConcurrent, MaxRenewals
ASSUME /\ DriverA # DriverB
       /\ NoDriver \notin {DriverA, DriverB}
       /\ Sessions # {}
       /\ IsFiniteSet(Sessions)
       /\ MaxCompleted \in Nat \ {0}
       /\ MaxConcurrent \in Nat \ {0}
       /\ MaxConcurrent <= Cardinality(Sessions)
       /\ MaxRenewals \in Nat

Drivers == {DriverA, DriverB}
VARIABLES owner, phase, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet
vars == <<owner, phase, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet>>

ActiveSessions == {session \in Sessions : owner[session] # NoDriver}

Init == /\ owner = [session \in Sessions |-> NoDriver]
        /\ phase = [session \in Sessions |-> "Idle"]
        /\ completed = [session \in Sessions |-> 0]
        /\ remainingRenewals = [session \in Sessions |-> 0]
        /\ leaseVisible = [session \in Sessions |-> FALSE]
        /\ receiptCurrent = [session \in Sessions |-> FALSE]
        /\ publishSet = [session \in Sessions |-> {}]
        /\ drainSet = [session \in Sessions |-> {}]

Acquire(session, d) ==
  /\ session \in Sessions
  /\ d \in Drivers
  /\ owner[session] = NoDriver
  /\ completed[session] < MaxCompleted
  /\ Cardinality(ActiveSessions) < MaxConcurrent
  /\ owner' = [owner EXCEPT ![session] = d]
  /\ phase' = [phase EXCEPT ![session] = "WaitingForControl"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = MaxRenewals]
  /\ UNCHANGED <<completed, leaseVisible, receiptCurrent, publishSet, drainSet>>

ControlRespond(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ phase' = [phase EXCEPT ![session] = "PreparingResources"]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = TRUE]
  /\ UNCHANGED <<owner, completed, remainingRenewals, receiptCurrent, publishSet, drainSet>>

ControlTimeout(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ UNCHANGED <<completed, leaseVisible, receiptCurrent, publishSet, drainSet>>

(* Reads hold the existing driver lock but not renewal capacity. A temporary
   failure is stuttering; explicit loss/expiry cancels before lock-wait cleanup.
   Owner/epoch/expiry CAS arithmetic is proved by the contract kernels, not by
   this scheduling abstraction. *)
PrepareResources(session, d) ==
  /\ owner[session] = d
  /\ phase[session] = "PreparingResources"
  /\ leaseVisible[session]
  /\ phase' = [phase EXCEPT ![session] = "Driving"]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet>>

LosePreparationAuthority(session, d) ==
  /\ owner[session] = d
  /\ phase[session] = "PreparingResources"
  /\ phase' = [phase EXCEPT ![session] = "CancellingResources"]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<owner, completed, remainingRenewals, receiptCurrent, publishSet, drainSet>>

AbortPreparation(session, d) ==
  /\ owner[session] = d
  /\ phase[session] \in {"PreparingResources", "CancellingResources"}
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<completed, receiptCurrent, publishSet, drainSet>>

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
  /\ UNCHANGED <<owner, phase, completed, leaseVisible, publishSet, drainSet>>

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
  \* The regression witness: Stage returned both old and new generations.
  \* Activate durably retires old; Publish partitions those exact receipts.
  /\ publishSet' = [publishSet EXCEPT ![session] = PublicationCandidates({"old", "new"}, {"new"})]
  /\ drainSet' = [drainSet EXCEPT ![session] = {"old"}]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible>>

(* Completed physical evidence remains valid under live same-generation
   authority. Renewal does not send this phase back to Driving. *)
Acknowledge(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Publishing"
  /\ phase' = [phase EXCEPT ![session] = "Committing"]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet>>

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
  /\ UNCHANGED <<receiptCurrent, publishSet, drainSet>>

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
          \/ Release(session, d)

Fairness ==
  /\ \A session \in Sessions, d \in Drivers : WF_vars(ControlSettles(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(PrepareResources(session, d) \/ AbortPreparation(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Advance(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Acknowledge(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Release(session, d))

Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ owner \in [Sessions -> Drivers \cup {NoDriver}]
          /\ phase \in [Sessions -> {"Idle", "WaitingForControl", "PreparingResources", "CancellingResources", "Driving", "Publishing", "Committing"}]
          /\ completed \in [Sessions -> 0..MaxCompleted]
          /\ remainingRenewals \in [Sessions -> 0..MaxRenewals]
          /\ leaseVisible \in [Sessions -> BOOLEAN]
          /\ receiptCurrent \in [Sessions -> BOOLEAN]
OneDriverPerSession ==
  \A session \in Sessions :
    phase[session] = "Idle" <=> owner[session] = NoDriver
BoundedCrossSessionConcurrency ==
  Cardinality(ActiveSessions) <= MaxConcurrent
PublicationExcludesRetiredGenerations ==
  \A session \in Sessions :
    /\ publishSet[session] \subseteq {"new"}
    /\ publishSet[session] \cap drainSet[session] = {}
    /\ (phase[session] \in {"Publishing", "Committing"} => publishSet[session] = {"new"})
Safety == TypeOK /\ OneDriverPerSession /\ BoundedCrossSessionConcurrency
          /\ PublicationExcludesRetiredGenerations

ResourcesHaveRenewableAuthority ==
  \A session \in Sessions :
    (phase[session] \in {"PreparingResources", "Driving", "Publishing", "Committing"}) = leaseVisible[session]

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
=============================================================================
