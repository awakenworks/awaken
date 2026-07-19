--------------------------- MODULE RuntimeVocabulary ---------------------------
\* Shared closed vocabularies for every Runtime safety model. Component models
\* own their transitions and component-specific states, but must not redefine
\* the same domain phase under different local aliases.

CoreRunStates == {"Running", "Awaiting", "Ended"}

ToolCallStates == {
    "Requested",
    "Executing",
    "AwaitingToolPermission",
    "Completed",
    "Indeterminate"
}
TerminalToolCallStates == {"Completed", "Indeterminate"}
ToolDecisionStates == {"None", "Approved", "Denied", "ResultSupplied"}

DelegatedChildStates == {"Absent", "Running", "Awaiting", "Ended"}
DelegationLinkStatuses == {"Absent", "Open", "Completed", "CancelRequested"}

=============================================================================
