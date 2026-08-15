------------- MODULE ExecutableProjectionRefreshProof -------------
EXTENDS ExecutableProjectionRefresh, TLAPS

LEMMA InitTypeOK == KernelAssumptions /\ Init => TypeOK
<1>1 ASSUME KernelAssumptions, Init
     PROVE TypeOK
  <2>1 authority \in [Domains -> 0..MaxSequence] /\
        installed \in [Domains -> 0..MaxSequence] /\
        cursor \in [Domains -> 0..MaxSequence]
    BY <1>1 DEF KernelAssumptions, Init, Domains
  <2>2 observed \in [Requests -> [Domains -> 0..MaxSequence]]
    BY <1>1 DEF KernelAssumptions, Init, Domains
  <2>3 phase \in [Requests -> Phases]
    BY <1>1 DEF Init, Phases
  <2>4 refreshed \in [Requests -> SUBSET Domains]
    BY <1>1 DEF Init
  <2>5 lockOwner \in [Domains -> Requests \cup {NoRequest}]
    BY <1>1 DEF Init
  <2>6 QED
    BY <2>1, <2>2, <2>3, <2>4, <2>5 DEF TypeOK
<1>2 QED
  BY <1>1

LEMMA InitStructuralSafety ==
    KernelAssumptions /\ Init =>
      /\ CursorNeverAheadOfInstall
      /\ InstallNeverAheadOfAuthority
      /\ RefreshTargetValid
      /\ InstalledTargetCoherent
      /\ RefreshOrder
      /\ RefreshedTargetsCovered
      /\ AdmissionCovered
      /\ MutexCoherent
BY DEF KernelAssumptions, Init,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent, AgentLockPhases,
   EnvironmentLockPhases, EnvironmentPhases, Domains

LEMMA InitSafety == KernelAssumptions /\ Init => Safety
BY InitTypeOK, InitStructuralSafety DEF Safety

LEMMA PublishSafety ==
    \A domain \in Domains:
      KernelAssumptions /\ Safety /\ Publish(domain) => Safety'
BY Z3T(30) DEF KernelAssumptions, Publish, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases,
   Phases, Domains

LEMMA StartSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ Start(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, Start, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AcquireAgentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AcquireAgent(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, AcquireAgent, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA InstallAgentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ InstallAgent(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, InstallAgent, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceAgentCombinedSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, AdvanceAgent, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder,
   RefreshedTargetsCovered, AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceAgentTypeOK ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        TypeOK'
BY Z3T(30) DEF KernelAssumptions, AdvanceAgent, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceAgentCursorSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        CursorNeverAheadOfInstall'
BY AdvanceAgentCombinedSafety DEF Safety

LEMMA AdvanceAgentAuthoritySafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        InstallNeverAheadOfAuthority'
BY Z3T(30) DEF AdvanceAgent, Safety, InstallNeverAheadOfAuthority

LEMMA AdvanceAgentTargetSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        RefreshTargetValid'
BY AdvanceAgentCombinedSafety DEF Safety

LEMMA AdvanceAgentInstalledTargetSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        InstalledTargetCoherent'
BY AdvanceAgentCombinedSafety DEF Safety

LEMMA AdvanceAgentOrderSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        RefreshOrder'
BY AdvanceAgentCombinedSafety DEF Safety

LEMMA AdvanceAgentMutexSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        MutexCoherent'
BY AdvanceAgentCombinedSafety DEF Safety

LEMMA AdvanceAgentCoverageSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) =>
        /\ RefreshedTargetsCovered'
        /\ AdmissionCovered'
BY Z3T(30) DEF KernelAssumptions, AdvanceAgent, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceAgentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceAgent(request) => Safety'
BY AdvanceAgentTypeOK, AdvanceAgentCursorSafety, AdvanceAgentAuthoritySafety,
   AdvanceAgentTargetSafety, AdvanceAgentInstalledTargetSafety,
   AdvanceAgentOrderSafety, AdvanceAgentMutexSafety,
   AdvanceAgentCoverageSafety DEF Safety

LEMMA FailAgentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ FailAgent(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, FailAgent, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AcquireEnvironmentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AcquireEnvironment(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, AcquireEnvironment, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA InstallEnvironmentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ InstallEnvironment(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, InstallEnvironment, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceEnvironmentCombinedSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, AdvanceEnvironment, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder,
   RefreshedTargetsCovered, AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceEnvironmentTypeOK ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        TypeOK'
BY Z3T(30) DEF KernelAssumptions, AdvanceEnvironment, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceEnvironmentCursorSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        CursorNeverAheadOfInstall'
BY AdvanceEnvironmentCombinedSafety DEF Safety

LEMMA AdvanceEnvironmentAuthoritySafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        InstallNeverAheadOfAuthority'
BY Z3T(30) DEF AdvanceEnvironment, Safety, InstallNeverAheadOfAuthority

LEMMA AdvanceEnvironmentTargetSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        RefreshTargetValid'
BY AdvanceEnvironmentCombinedSafety DEF Safety

LEMMA AdvanceEnvironmentInstalledTargetSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        InstalledTargetCoherent'
BY AdvanceEnvironmentCombinedSafety DEF Safety

LEMMA AdvanceEnvironmentOrderSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        RefreshOrder'
BY AdvanceEnvironmentCombinedSafety DEF Safety

LEMMA AdvanceEnvironmentMutexSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        MutexCoherent'
BY AdvanceEnvironmentCombinedSafety DEF Safety

LEMMA AdvanceEnvironmentCoverageSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) =>
        /\ RefreshedTargetsCovered'
        /\ AdmissionCovered'
BY Z3T(30) DEF KernelAssumptions, AdvanceEnvironment, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA AdvanceEnvironmentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ AdvanceEnvironment(request) => Safety'
BY AdvanceEnvironmentTypeOK, AdvanceEnvironmentCursorSafety,
   AdvanceEnvironmentAuthoritySafety, AdvanceEnvironmentTargetSafety,
   AdvanceEnvironmentInstalledTargetSafety, AdvanceEnvironmentOrderSafety,
   AdvanceEnvironmentMutexSafety,
   AdvanceEnvironmentCoverageSafety DEF Safety

LEMMA FailEnvironmentSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ FailEnvironment(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, FailEnvironment, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, Phases, Domains

LEMMA ResetSafety ==
    \A request \in Requests:
      KernelAssumptions /\ Safety /\ Reset(request) => Safety'
BY Z3T(30) DEF KernelAssumptions, Reset, Safety, TypeOK,
   CursorNeverAheadOfInstall, InstallNeverAheadOfAuthority,
   RefreshTargetValid, InstalledTargetCoherent, RefreshOrder, RefreshedTargetsCovered,
   AdmissionCovered, MutexCoherent,
   AgentLockPhases, EnvironmentLockPhases, EnvironmentPhases, TerminalPhases,
   Phases, Domains

LEMMA NextSafety == KernelAssumptions /\ Safety /\ Next => Safety'
BY PublishSafety, StartSafety, AcquireAgentSafety, InstallAgentSafety,
   AdvanceAgentSafety, FailAgentSafety, AcquireEnvironmentSafety,
   InstallEnvironmentSafety, AdvanceEnvironmentSafety, FailEnvironmentSafety,
   ResetSafety DEF Next

LEMMA SafetyStutter == Safety /\ UNCHANGED vars => Safety'
BY Z3T(30) DEF Safety, TypeOK, CursorNeverAheadOfInstall,
   InstallNeverAheadOfAuthority, RefreshTargetValid, InstalledTargetCoherent, RefreshOrder,
   RefreshedTargetsCovered, AdmissionCovered, MutexCoherent, AgentLockPhases,
   EnvironmentLockPhases, EnvironmentPhases, Phases, Domains, vars

InductiveSafety == KernelAssumptions /\ Safety

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_vars => InductiveSafety'
BY NextSafety, SafetyStutter
   DEF InductiveSafety, KernelAssumptions, vars

THEOREM ExecutableProjectionRefreshSafety ==
    KernelAssumptions /\ Spec => []Safety
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

THEOREM AdmissionRequiresBothSuccessfulRefreshes ==
    \A request \in Requests:
      Safety /\ phase[request] = "admitted" =>
        /\ AgentDomain \in refreshed[request]
        /\ EnvironmentDomain \in refreshed[request]
        /\ observed[request][AgentDomain] <= cursor[AgentDomain]
        /\ observed[request][EnvironmentDomain] <= cursor[EnvironmentDomain]
BY Z3T(30) DEF Safety, RefreshOrder, AdmissionCovered

THEOREM RefreshFailureDoesNotPublishProjection ==
    \A request \in Requests:
      FailAgent(request) \/ FailEnvironment(request) =>
        UNCHANGED <<installed, cursor, refreshed>>
BY DEF FailAgent, FailEnvironment

======================================================================
