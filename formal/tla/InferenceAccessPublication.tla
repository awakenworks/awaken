------------------- MODULE InferenceAccessPublication -------------------
EXTENDS Naturals, TLC

CONSTANT MaxGeneration

VARIABLE authoredAccess, snapshotAccess, dispatchAccess, runtimeAccess,
         credentialGeneration, snapshotCredentialGeneration,
         dispatchCredentialGeneration, runtimeCredentialGeneration,
         credentialAvailable, publicationCount, phase

vars == <<authoredAccess, snapshotAccess, dispatchAccess, runtimeAccess,
          credentialGeneration, snapshotCredentialGeneration,
          dispatchCredentialGeneration, runtimeCredentialGeneration,
          credentialAvailable, publicationCount, phase>>

Init == /\ authoredAccess = 1
        /\ snapshotAccess = 0
        /\ dispatchAccess = 0
        /\ runtimeAccess = 0
        /\ credentialGeneration = 1
        /\ snapshotCredentialGeneration = 0
        /\ dispatchCredentialGeneration = 0
        /\ runtimeCredentialGeneration = 0
        /\ credentialAvailable = TRUE
        /\ publicationCount = 0
        /\ phase = "draft"

Edit == /\ authoredAccess < MaxGeneration
        /\ authoredAccess' = authoredAccess + 1
        /\ UNCHANGED <<snapshotAccess, dispatchAccess, runtimeAccess,
                        credentialGeneration, snapshotCredentialGeneration,
                        dispatchCredentialGeneration, runtimeCredentialGeneration,
                        credentialAvailable, publicationCount, phase>>

RotateCredential == /\ phase \in {"draft", "published", "dispatched"}
                    /\ credentialGeneration < MaxGeneration
                    /\ credentialGeneration' = credentialGeneration + 1
                    /\ credentialAvailable' = TRUE
                    /\ UNCHANGED <<authoredAccess, snapshotAccess, dispatchAccess,
                                    runtimeAccess, snapshotCredentialGeneration,
                                    dispatchCredentialGeneration,
                                    runtimeCredentialGeneration, publicationCount,
                                    phase>>

RevokeCredential == /\ phase \in {"draft", "published", "dispatched"}
                    /\ credentialAvailable
                    /\ credentialAvailable' = FALSE
                    /\ UNCHANGED <<authoredAccess, snapshotAccess, dispatchAccess,
                                    runtimeAccess, credentialGeneration,
                                    snapshotCredentialGeneration,
                                    dispatchCredentialGeneration,
                                    runtimeCredentialGeneration, publicationCount,
                                    phase>>

Publish == /\ phase = "draft"
           /\ snapshotAccess' = authoredAccess
           /\ snapshotCredentialGeneration' = credentialGeneration
           /\ publicationCount' = publicationCount + 1
           /\ phase' = "published"
           /\ UNCHANGED <<authoredAccess, dispatchAccess, runtimeAccess,
                           credentialGeneration, dispatchCredentialGeneration,
                           runtimeCredentialGeneration, credentialAvailable>>

Dispatch == /\ phase = "published"
            /\ dispatchAccess' = snapshotAccess
            /\ dispatchCredentialGeneration' = snapshotCredentialGeneration
            /\ phase' = "dispatched"
            /\ UNCHANGED <<authoredAccess, snapshotAccess, runtimeAccess,
                            credentialGeneration, snapshotCredentialGeneration,
                            runtimeCredentialGeneration, credentialAvailable,
                            publicationCount>>

Materialize == /\ phase = "dispatched"
               /\ credentialAvailable
               /\ credentialGeneration = dispatchCredentialGeneration
               /\ runtimeAccess' = dispatchAccess
               /\ runtimeCredentialGeneration' = dispatchCredentialGeneration
               /\ phase' = "materialized"
               /\ UNCHANGED <<authoredAccess, snapshotAccess, dispatchAccess,
                               credentialGeneration, snapshotCredentialGeneration,
                               dispatchCredentialGeneration, credentialAvailable,
                               publicationCount>>

RejectUnavailable == /\ phase = "dispatched"
                     /\ \/ ~credentialAvailable
                        \/ credentialGeneration # dispatchCredentialGeneration
                     /\ phase' = "rejected"
                     /\ UNCHANGED <<authoredAccess, snapshotAccess,
                                     dispatchAccess, runtimeAccess,
                                     credentialGeneration,
                                     snapshotCredentialGeneration,
                                     dispatchCredentialGeneration,
                                     runtimeCredentialGeneration,
                                     credentialAvailable, publicationCount>>

Next == Edit \/ RotateCredential \/ RevokeCredential \/ Publish \/ Dispatch \/
        Materialize \/ RejectUnavailable

TypeOK == /\ authoredAccess \in 1..MaxGeneration
          /\ snapshotAccess \in 0..MaxGeneration
          /\ dispatchAccess \in 0..MaxGeneration
          /\ runtimeAccess \in 0..MaxGeneration
          /\ credentialGeneration \in 1..MaxGeneration
          /\ snapshotCredentialGeneration \in 0..MaxGeneration
          /\ dispatchCredentialGeneration \in 0..MaxGeneration
          /\ runtimeCredentialGeneration \in 0..MaxGeneration
          /\ credentialAvailable \in BOOLEAN
          /\ publicationCount \in 0..1
          /\ phase \in {"draft", "published", "dispatched",
                         "materialized", "rejected"}

AccessResolvedAtMostOnce == publicationCount <= 1
DispatchCopiesPublishedAccess == dispatchAccess = 0 \/ dispatchAccess = snapshotAccess
RuntimeNeverResolvesAccess == runtimeAccess = 0 \/ runtimeAccess = dispatchAccess
DispatchCopiesCredentialPin == dispatchCredentialGeneration = 0 \/
                               dispatchCredentialGeneration = snapshotCredentialGeneration
MaterializedAccessWasPublished == phase # "materialized" \/
                                  /\ snapshotAccess # 0
                                  /\ runtimeAccess = snapshotAccess
                                  /\ runtimeCredentialGeneration = snapshotCredentialGeneration
MaterializedCredentialWasCurrent == phase # "materialized" \/
                                    /\ credentialAvailable
                                    /\ runtimeCredentialGeneration = credentialGeneration

Spec == Init /\ [][Next]_vars
=============================================================================
