---------------------- MODULE ManagedProjectionPublish ----------------------
EXTENDS Naturals, TLC

CONSTANTS Writers, Components, MaxSourceVersion, MaxCacheRevision, NoWriter

VARIABLES source, cacheRevision, resultFence, checkpointFence,
          writerPhase, observedCacheRevision, candidateFence,
          previousResultFence, lastWriter, lastOutcome,
          lastCacheBefore, lastCacheAfter

vars == <<source, cacheRevision, resultFence, checkpointFence,
          writerPhase, observedCacheRevision, candidateFence,
          previousResultFence, lastWriter, lastOutcome,
          lastCacheBefore, lastCacheAfter>>

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
    /\ previousResultFence = ZeroFence
    /\ lastWriter = NoWriter
    /\ lastOutcome = "none"
    /\ lastCacheBefore = 0
    /\ lastCacheAfter = 0

SourceAdvance(component) ==
    /\ component \in Components
    /\ source[component] < MaxSourceVersion
    /\ source' = [source EXCEPT ![component] = @ + 1]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = NoWriter
    /\ lastOutcome' = "source_advanced"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ UNCHANGED <<cacheRevision, resultFence, checkpointFence,
                    writerPhase, observedCacheRevision, candidateFence>>

BeginRefresh(writer) ==
    /\ writer \in Writers
    /\ writerPhase[writer] = "idle"
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "built"]
    /\ observedCacheRevision' =
         [observedCacheRevision EXCEPT ![writer] = cacheRevision]
    /\ candidateFence' = [candidateFence EXCEPT ![writer] = source]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastOutcome' = "built"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ UNCHANGED <<source, cacheRevision, resultFence, checkpointFence>>

Publish(writer) ==
    /\ writer \in Writers
    /\ writerPhase[writer] = "built"
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastCacheBefore' = cacheRevision
    /\ IF observedCacheRevision[writer] = cacheRevision
          /\ Dominates(candidateFence[writer], checkpointFence)
          /\ cacheRevision < MaxCacheRevision
       THEN /\ cacheRevision' = cacheRevision + 1
            /\ resultFence' = candidateFence[writer]
            /\ checkpointFence' = candidateFence[writer]
            /\ lastOutcome' = "applied"
            /\ lastCacheAfter' = cacheRevision + 1
       ELSE /\ UNCHANGED <<cacheRevision, resultFence, checkpointFence>>
            /\ lastOutcome' = "rejected"
            /\ lastCacheAfter' = cacheRevision
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "idle"]
    /\ UNCHANGED <<source, observedCacheRevision, candidateFence>>

RefreshFailure(writer) ==
    /\ writer \in Writers
    /\ writerPhase[writer] = "built"
    /\ writerPhase' = [writerPhase EXCEPT ![writer] = "idle"]
    /\ previousResultFence' = resultFence
    /\ lastWriter' = writer
    /\ lastOutcome' = "failed"
    /\ lastCacheBefore' = cacheRevision
    /\ lastCacheAfter' = cacheRevision
    /\ UNCHANGED <<source, cacheRevision, resultFence, checkpointFence,
                    observedCacheRevision, candidateFence>>

Next ==
    \/ \E component \in Components: SourceAdvance(component)
    \/ \E writer \in Writers: BeginRefresh(writer)
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
    /\ previousResultFence \in [Components -> 0..MaxSourceVersion]
    /\ lastWriter \in Writers \cup {NoWriter}
    /\ lastOutcome \in {"none", "source_advanced", "built", "applied", "rejected", "failed"}
    /\ lastCacheBefore \in 0..MaxCacheRevision
    /\ lastCacheAfter \in 0..MaxCacheRevision

ResultAndCheckpointArePaired == resultFence = checkpointFence
ProjectionNeverInventsSource == Dominates(source, resultFence)
VisibleProjectionNeverRegresses == Dominates(resultFence, previousResultFence)
PublishIsAtomicCAS ==
    IF lastOutcome = "applied"
       THEN lastCacheAfter = lastCacheBefore + 1
       ELSE lastCacheAfter = lastCacheBefore
FailureAndRejectionStutter ==
    lastOutcome \in {"failed", "rejected"} => resultFence = previousResultFence

Safety ==
    /\ TypeOK
    /\ ResultAndCheckpointArePaired
    /\ ProjectionNeverInventsSource
    /\ VisibleProjectionNeverRegresses
    /\ PublishIsAtomicCAS
    /\ FailureAndRejectionStutter

Spec == Init /\ [][Next]_vars
=============================================================================
