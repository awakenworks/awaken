---------------------- MODULE ManagedProjectionPublish ----------------------
EXTENDS Naturals, TLC

CONSTANTS Writers, Components, MaxSourceVersion, MaxCacheRevision, NoWriter

VARIABLES source, cacheRevision, resultFence, checkpointFence,
          writerPhase, observedCacheRevision, candidateFence,
          candidateChanged,
          previousResultFence, lastWriter, lastOutcome,
          lastCacheBefore, lastCacheAfter, lastBroadcast, lastCandidateChanged

vars == <<source, cacheRevision, resultFence, checkpointFence,
          writerPhase, observedCacheRevision, candidateFence,
          candidateChanged,
          previousResultFence, lastWriter, lastOutcome,
          lastCacheBefore, lastCacheAfter, lastBroadcast, lastCandidateChanged>>

ZeroFence == [component \in Components |-> 0]

Dominates(left, right) ==
    \A component \in Components: left[component] >= right[component]

Init ==
    /\ source = ZeroFence
    /\ cacheRevision = 0
    /\ resultFence = ZeroFence
    /\ checkpointFence = ZeroFence
    /\ writerPhase = [writer \in Writers |-> "idle"]
    /\ observedCacheRevision = [writer \in Writers |-> 0]
    /\ candidateFence = [writer \in Writers |-> ZeroFence]
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
    /\ source[component] < MaxSourceVersion
    /\ source' = [source EXCEPT ![component] = @ + 1]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = NoWriter
    /\ lastOutcome' = "source_advanced"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ lastBroadcast' = FALSE
    /\ lastCandidateChanged' = FALSE
    /\ UNCHANGED <<cacheRevision, resultFence, checkpointFence,
                    writerPhase, observedCacheRevision, candidateFence,
                    candidateChanged>>

BeginRefresh(writer, auxiliaryChanged) ==
    /\ writer \in Writers
    /\ auxiliaryChanged \in BOOLEAN
    /\ writerPhase[writer] = "idle"
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "built"]
    /\ observedCacheRevision' =
         [observedCacheRevision EXCEPT ![writer] = cacheRevision]
    /\ candidateFence' = [candidateFence EXCEPT ![writer] = source]
    /\ candidateChanged' =
         [candidateChanged EXCEPT ![writer] = auxiliaryChanged \/ source # resultFence]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastOutcome' = "built"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ lastBroadcast' = FALSE
    /\ lastCandidateChanged' = FALSE
    /\ UNCHANGED <<source, cacheRevision, resultFence, checkpointFence>>

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
    /\ UNCHANGED <<source, observedCacheRevision, candidateFence, candidateChanged>>

RefreshFailure(writer) ==
    /\ writer \in Writers
    /\ writerPhase[writer] = "built"
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "idle"]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastOutcome' = "failed"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ lastBroadcast' = FALSE
    /\ lastCandidateChanged' = FALSE
    /\ UNCHANGED <<source, cacheRevision, resultFence, checkpointFence,
                    observedCacheRevision, candidateFence, candidateChanged>>

Next ==
    \/ \E component \in Components: SourceAdvance(component)
    \/ \E writer \in Writers, auxiliaryChanged \in BOOLEAN:
         BeginRefresh(writer, auxiliaryChanged)
    \/ \E writer \in Writers: Publish(writer)
    \/ \E writer \in Writers: RefreshFailure(writer)

TypeOK ==
    /\ source \in [Components -> 0..MaxSourceVersion]
    /\ cacheRevision \in 0..MaxCacheRevision
    /\ resultFence \in [Components -> 0..MaxSourceVersion]
    /\ checkpointFence \in [Components -> 0..MaxSourceVersion]
    /\ writerPhase \in [Writers -> {"idle", "built"}]
    /\ observedCacheRevision \in [Writers -> 0..MaxCacheRevision]
    /\ candidateFence \in [Writers -> [Components -> 0..MaxSourceVersion]]
    /\ candidateChanged \in [Writers -> BOOLEAN]
    /\ previousResultFence \in [Components -> 0..MaxSourceVersion]
    /\ lastWriter \in Writers \cup {NoWriter}
    /\ lastOutcome \in {"none", "source_advanced", "built", "applied",
                         "stale_cache", "source_regression", "already_current", "failed"}
    /\ lastCacheBefore \in 0..MaxCacheRevision
    /\ lastCacheAfter \in 0..MaxCacheRevision
    /\ lastBroadcast \in BOOLEAN
    /\ lastCandidateChanged \in BOOLEAN

ResultAndCheckpointArePaired == resultFence = checkpointFence
ProjectionNeverInventsSource == Dominates(source, resultFence)
VisibleProjectionNeverRegresses == Dominates(resultFence, previousResultFence)
PublishIsAtomicCAS ==
    IF lastOutcome = "applied"
       THEN lastCacheAfter = lastCacheBefore + 1
       ELSE lastCacheAfter = lastCacheBefore
FailureAndRejectionStutter ==
    lastOutcome \in {"failed", "stale_cache", "source_regression", "already_current"}
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

Safety ==
    /\ TypeOK
    /\ ResultAndCheckpointArePaired
    /\ ProjectionNeverInventsSource
    /\ VisibleProjectionNeverRegresses
    /\ PublishIsAtomicCAS
    /\ FailureAndRejectionStutter
    /\ ChangedCandidateIsRequiredForPublish
    /\ AlreadyCurrentIsARead
    /\ BroadcastIffPublished

Spec == Init /\ [][Next]_vars
=============================================================================
