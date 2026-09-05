-------------------- MODULE DreamCleanupBoundary --------------------
EXTENDS TLC

\* Composition boundary between the Coordinator-owned Dream process and the
\* Session-owned terminal cleanup receipt. Dream never infers cleanup from an
\* archive fence or Worker dispatch: only the exact Session Completed receipt
\* allows the durable cleanup flag, and therefore the public terminal, to clear.

VARIABLES dreamStatus, sessionCleanup, cleanupPending

vars == <<dreamStatus, sessionCleanup, cleanupPending>>

Init ==
    /\ dreamStatus = "Running"
    /\ sessionCleanup = "NotRequested"
    /\ cleanupPending = FALSE

Finish(status) ==
    /\ dreamStatus = "Running"
    /\ status \in {"Completed", "Failed"}
    /\ dreamStatus' = status
    /\ sessionCleanup' = "Requested"
    /\ cleanupPending' = TRUE

Cancel ==
    /\ dreamStatus = "Running"
    /\ dreamStatus' = "Canceled"
    /\ sessionCleanup' = "Requested"
    /\ cleanupPending' = TRUE

SessionCleanupCompletes ==
    /\ sessionCleanup = "Requested"
    /\ sessionCleanup' = "Completed"
    /\ UNCHANGED <<dreamStatus, cleanupPending>>

CleanupRetryBeforeReceipt ==
    /\ cleanupPending
    /\ sessionCleanup # "Completed"
    /\ UNCHANGED vars

CleanupCommitsAfterReceipt ==
    /\ cleanupPending
    /\ sessionCleanup = "Completed"
    /\ cleanupPending' = FALSE
    /\ UNCHANGED <<dreamStatus, sessionCleanup>>

Next ==
    \/ \E status \in {"Completed", "Failed"}: Finish(status)
    \/ Cancel
    \/ SessionCleanupCompletes
    \/ CleanupRetryBeforeReceipt
    \/ CleanupCommitsAfterReceipt

Spec == Init /\ [][Next]_vars

PublicStatus ==
    IF cleanupPending /\ dreamStatus \in {"Completed", "Failed"}
    THEN "Running"
    ELSE dreamStatus

TypeOK ==
    /\ dreamStatus \in {"Running", "Completed", "Failed", "Canceled"}
    /\ sessionCleanup \in {"NotRequested", "Requested", "Completed"}
    /\ cleanupPending \in BOOLEAN

PublicTerminalRequiresEveryReceipt ==
    PublicStatus \in {"Completed", "Failed"} =>
        ~cleanupPending /\ sessionCleanup = "Completed"

UnsettledSessionKeepsDreamCleanupPending ==
    dreamStatus \in {"Completed", "Failed", "Canceled"} /\ sessionCleanup # "Completed" =>
        cleanupPending

CanceledCleanupSettlesOnlyAfterReceipt ==
    dreamStatus = "Canceled" /\ ~cleanupPending => sessionCleanup = "Completed"

Safety ==
    /\ TypeOK
    /\ PublicTerminalRequiresEveryReceipt
    /\ UnsettledSessionKeepsDreamCleanupPending
    /\ CanceledCleanupSettlesOnlyAfterReceipt

=============================================================================
