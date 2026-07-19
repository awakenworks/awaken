------------------------- MODULE ConfigActivation -------------------------
EXTENDS Naturals, TLC

CONSTANT MaxGeneration

VARIABLE authorGeneration, publicationGeneration, installedGeneration,
         lastInstalledGeneration

vars == <<authorGeneration, publicationGeneration, installedGeneration,
          lastInstalledGeneration>>

Init == /\ authorGeneration = 0 /\ publicationGeneration = 0
        /\ installedGeneration = 0 /\ lastInstalledGeneration = 0

Edit == /\ authorGeneration < MaxGeneration
        /\ authorGeneration' = authorGeneration + 1
        /\ UNCHANGED <<publicationGeneration, installedGeneration, lastInstalledGeneration>>

Publish == /\ authorGeneration > 0
           /\ publicationGeneration' = authorGeneration
           /\ UNCHANGED <<authorGeneration, installedGeneration, lastInstalledGeneration>>

Install == /\ publicationGeneration = authorGeneration
           /\ publicationGeneration >= installedGeneration
           /\ installedGeneration' = publicationGeneration
           /\ lastInstalledGeneration' = installedGeneration
           /\ UNCHANGED <<authorGeneration, publicationGeneration>>

RejectStale == /\ publicationGeneration < authorGeneration
               /\ UNCHANGED vars

Next == Edit \/ Publish \/ Install \/ RejectStale

TypeOK == /\ authorGeneration \in 0..MaxGeneration
          /\ publicationGeneration \in 0..MaxGeneration
          /\ installedGeneration \in 0..MaxGeneration
          /\ lastInstalledGeneration \in 0..MaxGeneration
InstalledNeverExceedsAuthored == installedGeneration <= authorGeneration
ActivationGenerationNeverRollsBack == installedGeneration >= lastInstalledGeneration

Spec == Init /\ [][Next]_vars
=============================================================================
