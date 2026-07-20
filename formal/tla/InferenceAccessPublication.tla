------------------- MODULE InferenceAccessPublication -------------------
EXTENDS Naturals, TLC

CONSTANT MaxGeneration

VARIABLE authoredAccess, snapshotAccess, dispatchAccess, runtimeAccess,
         publicationCount, phase

vars == <<authoredAccess, snapshotAccess, dispatchAccess, runtimeAccess,
          publicationCount, phase>>

Init == /\ authoredAccess = 1
        /\ snapshotAccess = 0
        /\ dispatchAccess = 0
        /\ runtimeAccess = 0
        /\ publicationCount = 0
        /\ phase = "draft"

Edit == /\ authoredAccess < MaxGeneration
        /\ authoredAccess' = authoredAccess + 1
        /\ UNCHANGED <<snapshotAccess, dispatchAccess, runtimeAccess,
                        publicationCount, phase>>

Publish == /\ phase = "draft"
           /\ snapshotAccess' = authoredAccess
           /\ publicationCount' = publicationCount + 1
           /\ phase' = "published"
           /\ UNCHANGED <<authoredAccess, dispatchAccess, runtimeAccess>>

Dispatch == /\ phase = "published"
            /\ dispatchAccess' = snapshotAccess
            /\ phase' = "dispatched"
            /\ UNCHANGED <<authoredAccess, snapshotAccess, runtimeAccess,
                            publicationCount>>

Materialize == /\ phase = "dispatched"
               /\ runtimeAccess' = dispatchAccess
               /\ phase' = "materialized"
               /\ UNCHANGED <<authoredAccess, snapshotAccess, dispatchAccess,
                               publicationCount>>

RejectUnavailable == /\ phase = "dispatched"
                     /\ phase' = "rejected"
                     /\ UNCHANGED <<authoredAccess, snapshotAccess,
                                     dispatchAccess, runtimeAccess,
                                     publicationCount>>

Next == Edit \/ Publish \/ Dispatch \/ Materialize \/ RejectUnavailable

TypeOK == /\ authoredAccess \in 1..MaxGeneration
          /\ snapshotAccess \in 0..MaxGeneration
          /\ dispatchAccess \in 0..MaxGeneration
          /\ runtimeAccess \in 0..MaxGeneration
          /\ publicationCount \in 0..1
          /\ phase \in {"draft", "published", "dispatched",
                         "materialized", "rejected"}

AccessResolvedAtMostOnce == publicationCount <= 1
DispatchCopiesPublishedAccess == dispatchAccess = 0 \/ dispatchAccess = snapshotAccess
RuntimeNeverResolvesAccess == runtimeAccess = 0 \/ runtimeAccess = dispatchAccess
MaterializedAccessWasPublished == phase # "materialized" \/
                                  /\ snapshotAccess # 0
                                  /\ runtimeAccess = snapshotAccess

Spec == Init /\ [][Next]_vars
=============================================================================
