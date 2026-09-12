-------------------- MODULE SessionRealizationContinuation --------------------
EXTENDS SessionRealizationAuthority

(* Bounded request/publication composition, not another lease implementation.
   Reuse every authority transition, including Replace, from the canonical
   model. Two Runs share one authenticated Worker/Session. originRun is a ghost
   fact: which Run received the published generation; installedRun observes the
   mutable local claim. Production does not need a second originRun store.
   Transport authentication, database CAS, Worker death and external effects
   remain outside this abstraction. No implementation refinement is claimed.

   RC1 new Run + failed Control -> preserve old association, retry still fresh.
   RC2 successful fresh publication -> claim/lease association changes together.
   RC3 same Run + matching live response -> retain the same generation.
   RC4 close/replacement during response -> reject continuation, never reopen.
*)
Runs == {1, 2}
VARIABLES installedRun, originRun, pendingRun, asserted, badContinuation, continued
retryVars == <<installedRun, originRun, pendingRun, asserted, badContinuation, continued>>
allVars == <<vars, retryVars>>

RetryInit == Init
  /\ installedRun = 1 /\ originRun = 1 /\ pendingRun = 0
  /\ asserted = NoSnapshot /\ badContinuation = FALSE /\ continued = FALSE

SendResume(run) ==
  /\ pendingRun = 0 /\ pendingRun' = run
  /\ asserted' = IF projection = "Live" /\ installedRun = run THEN lease ELSE NoSnapshot
  /\ UNCHANGED <<installedRun, originRun, badContinuation, continued>>
  /\ UNCHANGED vars

FailResume ==
  /\ pendingRun \in Runs
  /\ pendingRun' = 0 /\ asserted' = NoSnapshot
  /\ UNCHANGED <<installedRun, originRun, badContinuation, continued>>
  /\ UNCHANGED vars

PublishFresh ==
  /\ pendingRun \in Runs /\ asserted = NoSnapshot
  /\ Replace
  /\ installedRun' = pendingRun /\ originRun' = pendingRun
  /\ pendingRun' = 0 /\ asserted' = NoSnapshot
  /\ UNCHANGED <<badContinuation, continued>>

PublishContinuation ==
  /\ pendingRun \in Runs /\ asserted # NoSnapshot
  /\ projection = "Live" /\ lease[1] = asserted[1]
  /\ badContinuation' = (badContinuation \/ originRun # pendingRun
                          \/ projection # "Live" \/ lease[1] # asserted[1])
  /\ installedRun' = pendingRun /\ continued' = TRUE
  /\ pendingRun' = 0 /\ asserted' = NoSnapshot
  /\ UNCHANGED originRun
  /\ UNCHANGED vars

RetryNext == (Next /\ UNCHANGED retryVars)
  \/ (\E run \in Runs : SendResume(run))
  \/ FailResume \/ PublishFresh \/ PublishContinuation
RetrySpec == RetryInit /\ [][RetryNext]_allVars /\ WF_allVars(FinishPeer /\ UNCHANGED retryVars)

RetryTypeOK == TypeOK /\ installedRun \in Runs /\ originRun \in Runs
  /\ pendingRun \in Runs \cup {0} /\ asserted \in Snapshots \cup {NoSnapshot}
  /\ badContinuation \in BOOLEAN /\ continued \in BOOLEAN
NoInvalidContinuation == ~badContinuation
NoContinuationReached == ~continued
=============================================================================
