---------------------- MODULE WorkspacePathProjection ----------------------
EXTENDS Naturals, TLC

CONSTANTS Providers, FidelitousProviders, Tools, NoProvider, HostPath

VARIABLES provider, admitted, bound, observations, explicitLocalGrant

vars == <<provider, admitted, bound, observations, explicitLocalGrant>>

LogicalWorkspace == "logical:/workspace"
NoObservation == "none"

Init ==
    /\ provider = NoProvider
    /\ admitted = FALSE
    /\ bound = FALSE
    /\ observations = [tool \in Tools |-> NoObservation]
    /\ explicitLocalGrant = FALSE

GrantLocalCapability ==
    /\ ~explicitLocalGrant
    /\ explicitLocalGrant' = TRUE
    /\ UNCHANGED <<provider, admitted, bound, observations>>

Admit(p) ==
    /\ ~admitted
    /\ p \in FidelitousProviders
    /\ provider' = p
    /\ admitted' = TRUE
    /\ UNCHANGED <<bound, observations, explicitLocalGrant>>

Bind ==
    /\ admitted
    /\ ~bound
    /\ bound' = TRUE
    /\ UNCHANGED <<provider, admitted, observations, explicitLocalGrant>>

Invoke(tool) ==
    /\ bound
    /\ tool \in Tools
    /\ observations' = [observations EXCEPT ![tool] = LogicalWorkspace]
    /\ UNCHANGED <<provider, admitted, bound, explicitLocalGrant>>

Next ==
    \/ GrantLocalCapability
    \/ \E p \in Providers: Admit(p)
    \/ Bind
    \/ \E tool \in Tools: Invoke(tool)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ provider \in Providers \cup {NoProvider}
    /\ admitted \in BOOLEAN
    /\ bound \in BOOLEAN
    /\ observations \in [Tools -> {NoObservation, LogicalWorkspace, HostPath}]
    /\ explicitLocalGrant \in BOOLEAN

AdmissionRequiresPathFidelity == admitted => provider \in FidelitousProviders
BindingRequiresAdmission == bound => admitted
EveryToolUsesOneLogicalRoot ==
    \A tool \in Tools: observations[tool] \in {NoObservation, LogicalWorkspace}
HostPathNeverCrossesToolBoundary ==
    \A tool \in Tools: observations[tool] # HostPath
LocalGrantDoesNotChangeProjection ==
    explicitLocalGrant => observations \in [Tools -> {NoObservation, LogicalWorkspace}]

Safety ==
    /\ TypeOK
    /\ AdmissionRequiresPathFidelity
    /\ BindingRequiresAdmission
    /\ EveryToolUsesOneLogicalRoot
    /\ HostPathNeverCrossesToolBoundary
    /\ LocalGrantDoesNotChangeProjection
=============================================================================
