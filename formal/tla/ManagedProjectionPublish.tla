---------------------- MODULE ManagedProjectionPublish ----------------------
EXTENDS Naturals, TLC

CONSTANTS Writers, Components, OptionalComponents, Absent,
          MaxSourceVersion, MaxCacheRevision, NoWriter

VARIABLES source, cacheRevision, resultFence, checkpointFence,
          writerPhase, observedCacheRevision, candidateFence,
          firstSource, candidateChanged,
          previousResultFence, lastWriter, lastOutcome,
          lastCacheBefore, lastCacheAfter, lastBroadcast, lastCandidateChanged

vars == <<source, cacheRevision, resultFence, checkpointFence,
          writerPhase, observedCacheRevision, candidateFence,
          firstSource, candidateChanged,
          previousResultFence, lastWriter, lastOutcome,
          lastCacheBefore, lastCacheAfter, lastBroadcast, lastCandidateChanged>>

\* Absent is a model-only sentinel outside the numeric version domain.
Fences == {f \in [Components -> (0..MaxSourceVersion) \cup {Absent}]:
    \A component \in Components \ OptionalComponents: f[component] # Absent}
ZeroFence == [component \in Components |->
    IF component \in OptionalComponents THEN Absent ELSE 0]

Dominates(left, right) ==
    \A component \in Components:
      IF right[component] = Absent THEN TRUE
      ELSE IF left[component] = Absent THEN FALSE
      ELSE left[component] >= right[component]

ProjectionObservationDecision(priorComplete, refreshComplete) ==
    IF refreshComplete THEN "fresh"
    ELSE IF priorComplete THEN "prior"
    ELSE "retry"

Init ==
    /\ source = ZeroFence
    /\ cacheRevision = 0
    /\ resultFence = ZeroFence
    /\ checkpointFence = ZeroFence
    /\ writerPhase = [writer \in Writers |-> "idle"]
    /\ observedCacheRevision = [writer \in Writers |-> 0]
    /\ candidateFence = [writer \in Writers |-> ZeroFence]
    /\ firstSource = [writer \in Writers |-> ZeroFence]
    /\ candidateChanged = [writer \in Writers |-> FALSE]
    /\ previousResultFence = ZeroFence
    /\ lastWriter = NoWriter
    /\ lastOutcome = "none"
    /\ lastCacheBefore = 0
    /\ lastCacheAfter = 0
    /\ lastBroadcast = FALSE
    /\ lastCandidateChanged = FALSE

SourceAdvance(component) ==
    /\ component \in Components
    /\ IF source[component] = Absent THEN TRUE
       ELSE source[component] < MaxSourceVersion
    /\ source' = [source EXCEPT ![component] = IF @ = Absent THEN 0 ELSE @ + 1]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = NoWriter
    /\ lastOutcome' = "source_advanced"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ lastBroadcast' = FALSE
    /\ lastCandidateChanged' = FALSE
    /\ UNCHANGED <<cacheRevision, resultFence, checkpointFence,
                    writerPhase, observedCacheRevision, candidateFence,
                    firstSource, candidateChanged>>

BeginRefresh(writer, auxiliaryChanged) ==
    /\ writer \in Writers
    /\ auxiliaryChanged \in BOOLEAN
    /\ writerPhase[writer] = "idle"
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "reading"]
    /\ observedCacheRevision' =
         [observedCacheRevision EXCEPT ![writer] = cacheRevision]
    /\ firstSource' = [firstSource EXCEPT ![writer] = source]
    /\ candidateChanged' =
         [candidateChanged EXCEPT ![writer] = auxiliaryChanged \/ source # resultFence]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastOutcome' = "first_read"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ lastBroadcast' = FALSE
    /\ lastCandidateChanged' = FALSE
    /\ UNCHANGED <<source, cacheRevision, resultFence, checkpointFence,
                    candidateFence>>

FinishRead(writer) ==
    /\ writer \in Writers
    /\ writerPhase[writer] = "reading"
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ lastBroadcast' = FALSE
    /\ lastCandidateChanged' = FALSE
    /\ IF source = firstSource[writer]
       THEN /\ writerPhase' = [writerPhase EXCEPT ![writer] = "built"]
            /\ candidateFence' = [candidateFence EXCEPT ![writer] = source]
            /\ lastOutcome' = "built"
       ELSE /\ writerPhase' = [writerPhase EXCEPT ![writer] = "idle"]
            /\ UNCHANGED candidateFence
            /\ lastOutcome' = "mixed_source"
    /\ UNCHANGED <<source, cacheRevision, resultFence, checkpointFence,
                    observedCacheRevision, firstSource, candidateChanged>>

Publish(writer) ==
    /\ writer \in Writers
    /\ writerPhase[writer] = "built"
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastCacheBefore' = cacheRevision
    /\ lastCandidateChanged' = candidateChanged[writer]
    /\ IF observedCacheRevision[writer] # cacheRevision
       THEN /\ UNCHANGED <<cacheRevision, resultFence, checkpointFence>>
            /\ lastOutcome' = "stale_cache"
            /\ lastCacheAfter' = cacheRevision
            /\ lastBroadcast' = FALSE
       ELSE IF ~Dominates(candidateFence[writer], checkpointFence)
       THEN /\ UNCHANGED <<cacheRevision, resultFence, checkpointFence>>
            /\ lastOutcome' = "source_regression"
            /\ lastCacheAfter' = cacheRevision
            /\ lastBroadcast' = FALSE
       ELSE IF candidateFence[writer] = checkpointFence /\ ~candidateChanged[writer]
       THEN /\ UNCHANGED <<cacheRevision, resultFence, checkpointFence>>
            /\ lastOutcome' = "already_current"
            /\ lastCacheAfter' = cacheRevision
            /\ lastBroadcast' = FALSE
       ELSE /\ cacheRevision < MaxCacheRevision
            /\ cacheRevision' = cacheRevision + 1
            /\ resultFence' = candidateFence[writer]
            /\ checkpointFence' = candidateFence[writer]
            /\ lastOutcome' = "applied"
            /\ lastCacheAfter' = cacheRevision + 1
            /\ lastBroadcast' = TRUE
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "idle"]
    /\ UNCHANGED <<source, observedCacheRevision, candidateFence,
                    firstSource, candidateChanged>>

RefreshFailure(writer) ==
    /\ writer \in Writers
    \* Read errors and private-reduction errors share the same failure owner.
    \* Frame the CURRENT result/cache, not the writer's earlier observed base:
    \* another writer may have published successfully while this one awaited IO.
    /\ writerPhase[writer] \in {"reading", "built"}
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "idle"]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastOutcome' = "failed"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ lastBroadcast' = FALSE
    /\ lastCandidateChanged' = FALSE
    /\ UNCHANGED <<source, cacheRevision, resultFence, checkpointFence,
                    observedCacheRevision, candidateFence, firstSource,
                    candidateChanged>>

Next ==
    \/ \E component \in Components: SourceAdvance(component)
    \/ \E writer \in Writers, auxiliaryChanged \in BOOLEAN:
         BeginRefresh(writer, auxiliaryChanged)
    \/ \E writer \in Writers: FinishRead(writer)
    \/ \E writer \in Writers: Publish(writer)
    \/ \E writer \in Writers: RefreshFailure(writer)

TypeOK ==
    /\ Absent > MaxSourceVersion
    /\ OptionalComponents \subseteq Components
    /\ source \in Fences
    /\ cacheRevision \in 0..MaxCacheRevision
    /\ resultFence \in Fences
    /\ checkpointFence \in Fences
    /\ writerPhase \in [Writers -> {"idle", "reading", "built"}]
    /\ observedCacheRevision \in [Writers -> 0..MaxCacheRevision]
    /\ candidateFence \in [Writers -> Fences]
    /\ firstSource \in [Writers -> Fences]
    /\ candidateChanged \in [Writers -> BOOLEAN]
    /\ previousResultFence \in Fences
    /\ lastWriter \in Writers \cup {NoWriter}
    /\ lastOutcome \in {"none", "source_advanced", "first_read", "built", "mixed_source", "applied",
                         "stale_cache", "source_regression", "already_current", "failed"}
    /\ lastCacheBefore \in 0..MaxCacheRevision
    /\ lastCacheAfter \in 0..MaxCacheRevision
    /\ lastBroadcast \in BOOLEAN
    /\ lastCandidateChanged \in BOOLEAN

ResultAndCheckpointArePaired == resultFence = checkpointFence
ProjectionNeverInventsSource == Dominates(source, resultFence)
VisibleProjectionNeverRegresses == Dominates(resultFence, previousResultFence)
CacheReceiptMatchesVisible == cacheRevision = lastCacheAfter
PublishIsAtomicCAS ==
    IF lastOutcome = "applied"
       THEN lastCacheAfter = lastCacheBefore + 1
       ELSE lastCacheAfter = lastCacheBefore
FailureAndRejectionStutter ==
    lastOutcome \in {"failed", "mixed_source", "stale_cache", "source_regression", "already_current"}
      => resultFence = previousResultFence
ChangedCandidateIsRequiredForPublish ==
    lastOutcome = "applied"
      => /\ Dominates(resultFence, previousResultFence)
         /\ (lastCandidateChanged \/ resultFence # previousResultFence)
AlreadyCurrentIsARead ==
    lastOutcome = "already_current"
      => /\ lastCacheAfter = lastCacheBefore
         /\ resultFence = previousResultFence
BroadcastIffPublished == lastBroadcast = (lastOutcome = "applied")

PriorProjectionFallbackIsExact ==
    lastOutcome = "none" \/
      \A priorComplete \in BOOLEAN, refreshComplete \in BOOLEAN:
        /\ (ProjectionObservationDecision(priorComplete, refreshComplete) = "fresh")
             <=> refreshComplete
        /\ (ProjectionObservationDecision(priorComplete, refreshComplete) = "prior")
             <=> (~refreshComplete /\ priorComplete)
        /\ (ProjectionObservationDecision(priorComplete, refreshComplete) = "retry")
             <=> (~refreshComplete /\ ~priorComplete)

MixedSourceNeverBuilds ==
    lastOutcome = "mixed_source"
      => writerPhase[lastWriter] = "idle"

\* The complete R1-R4 decision table must hold even for a stable lagging
\* candidate. Normal concurrent reads alone may never exercise regression;
\* DecisionInit supplies the actual publisher's finite source-input boundary.
DecisionIsExact ==
    lastOutcome \in {"stale_cache", "source_regression", "already_current", "applied"}
    => lastOutcome =
       IF observedCacheRevision[lastWriter] # lastCacheBefore THEN "stale_cache"
       ELSE IF ~Dominates(candidateFence[lastWriter], previousResultFence)
            THEN "source_regression"
       ELSE IF candidateFence[lastWriter] = previousResultFence /\ ~lastCandidateChanged
            THEN "already_current"
       ELSE "applied"

DecisionInit ==
    \E current \in Fences, candidate \in Fences, changed \in BOOLEAN,
       cacheMatches \in BOOLEAN:
    /\ source = [component \in Components |-> MaxSourceVersion]
    /\ cacheRevision = 0
    /\ resultFence = current /\ checkpointFence = current
    /\ writerPhase = [writer \in Writers |-> "built"]
    /\ observedCacheRevision = [writer \in Writers |-> IF cacheMatches THEN 0 ELSE 1]
    /\ candidateFence = [writer \in Writers |-> candidate]
    /\ firstSource = candidateFence
    /\ candidateChanged = [writer \in Writers |-> changed]
    /\ previousResultFence = current
    /\ lastWriter = NoWriter /\ lastOutcome = "none"
    /\ lastCacheBefore = 0 /\ lastCacheAfter = 0
    /\ lastBroadcast = FALSE /\ lastCandidateChanged = FALSE
DecisionNext == \E writer \in Writers: Publish(writer)

NoApply == lastOutcome # "applied"
NoRetry == lastOutcome # "stale_cache"
NoRegressionRejection == lastOutcome # "source_regression"
NoCompletedRead == lastOutcome # "already_current"
NoZeroChildAddition == ~(lastOutcome = "applied" /\
    \E child \in OptionalComponents:
        previousResultFence[child] = Absent /\ resultFence[child] = 0)
NoZeroChildRemovalRejection == ~(lastOutcome = "source_regression" /\
    \E child \in OptionalComponents:
        previousResultFence[child] = 0 /\ candidateFence[lastWriter][child] = Absent)

\* Negative reachability properties used by the failure qualification table.
\* Each must fail on an actual transition, excluding a vacuous disabled phase.
NoReadingFailure == [][~(\E writer \in Writers:
    writerPhase[writer] = "reading" /\ RefreshFailure(writer))]_vars
NoBuiltFailure == [][~(\E writer \in Writers:
    writerPhase[writer] = "built" /\ RefreshFailure(writer))]_vars
NoFailureAfterCompetingPublish ==
    IF lastWriter \in Writers
       THEN ~(lastOutcome = "failed" /\
              observedCacheRevision[lastWriter] < cacheRevision)
       ELSE TRUE

Safety ==
    /\ TypeOK
    /\ DecisionIsExact
    /\ ResultAndCheckpointArePaired
    /\ ProjectionNeverInventsSource
    /\ VisibleProjectionNeverRegresses
    /\ CacheReceiptMatchesVisible
    /\ PublishIsAtomicCAS
    /\ FailureAndRejectionStutter
    /\ ChangedCandidateIsRequiredForPublish
    /\ AlreadyCurrentIsARead
    /\ BroadcastIffPublished
    /\ PriorProjectionFallbackIsExact
    /\ MixedSourceNeverBuilds

Spec == Init /\ [][Next]_vars
=============================================================================
