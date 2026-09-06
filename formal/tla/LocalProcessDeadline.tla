-------------------- MODULE LocalProcessDeadline --------------------
EXTENDS LocalProcessLifecycle

(* Compose the existing finite OS clock/renewal model with the relative-budget
   conversion BEFORE transport. One proof expires at boot time 3. Clock ticks
   can occur during authority lookup and after sampling its remaining lifetime.
   Physical GO, frame delivery and shutdown retain their existing owner model;
   this adds no alternative lifecycle or business lease FSM. Fractional time,
   overflow, OS clock truth and Rust refinement remain executable/TCB concerns. *)
VARIABLES budgetPhase, budgetAnchor, budgetRemaining, budgetCutoff, conversionTime
budgetVars == <<budgetPhase, budgetAnchor, budgetRemaining, budgetCutoff, conversionTime>>
allBudgetVars == <<allRenewalVars, budgetVars>>
BudgetInit == BoundaryInit /\ budgetPhase = "idle" /\ budgetAnchor = 0
  /\ budgetRemaining = 0 /\ budgetCutoff = 0 /\ conversionTime = 0

CaptureAnchor ==
  /\ budgetPhase = "idle" /\ budgetPhase' = "anchored"
  /\ budgetAnchor' = cutoffClock
  /\ UNCHANGED <<allRenewalVars, budgetRemaining, budgetCutoff, conversionTime>>

SampleBudget ==
  /\ budgetPhase = "anchored" /\ authorityLive /\ cutoffClock < 3
  /\ budgetPhase' = "sampled" /\ budgetRemaining' = 3 - cutoffClock
  /\ UNCHANGED <<allRenewalVars, budgetAnchor, budgetCutoff, conversionTime>>

ConvertBudget ==
  /\ budgetPhase = "sampled"
  /\ LET candidate == budgetAnchor + budgetRemaining
     IN /\ budgetCutoff' = candidate
        /\ budgetPhase' = IF candidate > cutoffClock THEN "accepted" ELSE "rejected"
  /\ conversionTime' = cutoffClock
  /\ UNCHANGED <<allRenewalVars, budgetAnchor, budgetRemaining>>

BudgetNext == (RenewalNext /\ UNCHANGED budgetVars)
  \/ CaptureAnchor \/ SampleBudget \/ ConvertBudget
BudgetSpec == BudgetInit /\ [][BudgetNext]_allBudgetVars
BudgetTypeOK == RenewalTypeOK
  /\ budgetPhase \in {"idle", "anchored", "sampled", "accepted", "rejected"}
  /\ budgetAnchor \in 0..4 /\ budgetRemaining \in 0..3
  /\ budgetCutoff \in 0..7 /\ conversionTime \in 0..4
ConversionNeverMintsAuthorityWaitTime ==
  budgetPhase = "accepted" => budgetCutoff <= 3
ExpiredConversionHasNoEffect ==
  budgetPhase = "accepted" => conversionTime < budgetCutoff
NoAcceptedConversionReached == budgetPhase # "accepted"
NoExpiredConversionReached == budgetPhase # "rejected"
=============================================================================
