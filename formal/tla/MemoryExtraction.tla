-------------------------- MODULE MemoryExtraction --------------------------
EXTENDS Naturals

CONSTANT MaxAttempts, StableRun

VARIABLE phase, claim, stage, attempts, runStarts, observations, mutations,
         receipt, runIdentity, terminalRunStarts

vars == <<phase, claim, stage, attempts, runStarts, observations, mutations,
          receipt, runIdentity, terminalRunStarts>>

Terminal == {"completed", "execution_indeterminate", "terminal_failed"}
Recoverable == {"pending", "claimed", "extracted", "stored"}

Init ==
    /\ phase = "pending"
    /\ claim = FALSE
    /\ stage = "idle"
    /\ attempts = 0
    /\ runStarts = 0
    /\ observations = 0
    /\ mutations = FALSE
    /\ receipt = FALSE
    /\ runIdentity = StableRun
    /\ terminalRunStarts = 0

Claim ==
    /\ phase \in {"pending", "extracted", "stored"}
    /\ ~claim
    /\ attempts < MaxAttempts
    /\ phase' = IF phase = "pending" THEN "claimed" ELSE phase
    /\ claim' = TRUE
    /\ attempts' = attempts + 1
    /\ UNCHANGED <<stage, runStarts, observations, mutations, receipt,
                    runIdentity, terminalRunStarts>>

StartExtraction ==
    /\ phase = "claimed"
    /\ claim
    /\ stage = "idle"
    /\ runStarts = 0
    /\ stage' = "executing"
    /\ runStarts' = runStarts + 1
    /\ observations' = observations + 1
    /\ UNCHANGED <<phase, claim, attempts, mutations, receipt,
                    runIdentity, terminalRunStarts>>

ObserveStableExtraction ==
    /\ phase = "claimed"
    /\ claim
    /\ stage = "idle"
    /\ runStarts = 1
    /\ stage' = "observing"
    /\ observations' = observations + 1
    /\ UNCHANGED <<phase, claim, attempts, runStarts, mutations, receipt,
                    runIdentity, terminalRunStarts>>

RecoverExtraction ==
    /\ phase = "claimed"
    /\ ~claim
    /\ stage = "unobserved"
    /\ attempts < MaxAttempts
    /\ claim' = TRUE
    /\ stage' = "observing"
    /\ attempts' = attempts + 1
    /\ observations' = observations + 1
    /\ UNCHANGED <<phase, runStarts, mutations, receipt, runIdentity,
                    terminalRunStarts>>

ReturnMutations ==
    /\ stage \in {"executing", "observing"}
    /\ phase = "claimed"
    /\ phase' = "extracted"
    /\ stage' = "idle"
    /\ mutations' = TRUE
    /\ UNCHANGED <<claim, attempts, runStarts, observations, receipt,
                    runIdentity, terminalRunStarts>>

ReturnRetryable ==
    /\ stage \in {"executing", "observing"}
    /\ phase = "claimed"
    /\ phase' = "pending"
    /\ claim' = FALSE
    /\ stage' = "idle"
    /\ UNCHANGED <<attempts, runStarts, observations, mutations, receipt,
                    runIdentity, terminalRunStarts>>

ReturnIndeterminate ==
    /\ stage \in {"executing", "observing"}
    /\ phase = "claimed"
    /\ phase' = "execution_indeterminate"
    /\ claim' = FALSE
    /\ stage' = "idle"
    /\ terminalRunStarts' = runStarts
    /\ UNCHANGED <<attempts, runStarts, observations, mutations, receipt,
                    runIdentity>>

ReturnPermanent ==
    /\ stage \in {"executing", "observing"}
    /\ phase = "claimed"
    /\ phase' = "terminal_failed"
    /\ claim' = FALSE
    /\ stage' = "idle"
    /\ terminalRunStarts' = runStarts
    /\ UNCHANGED <<attempts, runStarts, observations, mutations, receipt,
                    runIdentity>>

CrashBeforeExtraction ==
    /\ phase = "claimed"
    /\ claim
    /\ stage = "idle"
    /\ claim' = FALSE
    /\ phase' = "pending"
    /\ UNCHANGED <<stage, attempts, runStarts, observations, mutations, receipt,
                    runIdentity, terminalRunStarts>>

CrashDuringExtraction ==
    /\ phase = "claimed"
    /\ claim
    /\ stage \in {"executing", "observing"}
    /\ phase' = phase
    /\ claim' = FALSE
    /\ stage' = "unobserved"
    /\ UNCHANGED <<attempts, runStarts, observations, mutations, receipt,
                    runIdentity, terminalRunStarts>>

LoseDurableStageClaim ==
    /\ phase \in {"extracted", "stored"}
    /\ claim
    /\ stage = "idle"
    /\ claim' = FALSE
    /\ UNCHANGED <<phase, stage, attempts, runStarts, observations, mutations,
                    receipt, runIdentity, terminalRunStarts>>

ApplyMutations ==
    /\ phase = "extracted"
    /\ claim
    /\ mutations
    /\ phase' = "stored"
    /\ receipt' = TRUE
    /\ UNCHANGED <<claim, stage, attempts, runStarts, observations, mutations,
                    runIdentity, terminalRunStarts>>

Complete ==
    /\ phase = "stored"
    /\ claim
    /\ receipt
    /\ phase' = "completed"
    /\ claim' = FALSE
    /\ terminalRunStarts' = runStarts
    /\ UNCHANGED <<stage, attempts, runStarts, observations, mutations, receipt,
                    runIdentity>>

ExhaustRetryBudget ==
    /\ phase = "pending"
    /\ ~claim
    /\ stage = "idle"
    /\ attempts = MaxAttempts
    /\ phase' = "terminal_failed"
    /\ terminalRunStarts' = runStarts
    /\ UNCHANGED <<claim, stage, attempts, runStarts, observations, mutations,
                    receipt, runIdentity>>

Next == Claim \/ StartExtraction \/ ObserveStableExtraction \/ RecoverExtraction
        \/ ReturnMutations \/ ReturnRetryable
        \/ ReturnIndeterminate \/ ReturnPermanent \/ CrashBeforeExtraction
        \/ CrashDuringExtraction \/ LoseDurableStageClaim \/ ApplyMutations \/ Complete
        \/ ExhaustRetryBudget

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ phase \in Recoverable \cup Terminal
    /\ claim \in BOOLEAN
    /\ stage \in {"idle", "executing", "observing", "unobserved"}
    /\ attempts \in 0..MaxAttempts
    /\ runStarts \in 0..1
    /\ observations \in 0..MaxAttempts
    /\ mutations \in BOOLEAN
    /\ receipt \in BOOLEAN
    /\ terminalRunStarts \in 0..1

StableAuxiliaryRun == runIdentity = StableRun
TerminalHasNoClaim == phase \in Terminal => ~claim
ReceiptRequiresMutations == receipt => mutations
CompletionRequiresReceipt == phase = "completed" => receipt
UnknownHasNoFabricatedWork ==
    phase = "execution_indeterminate" => ~mutations /\ ~receipt
UnknownCannotInvokeAgain ==
    phase = "execution_indeterminate" => runStarts = terminalRunStarts
SingleStableRunStart == runStarts <= 1
NoIndeterminateReached == phase # "execution_indeterminate"
NoCompletedReached == phase # "completed"

=============================================================================
