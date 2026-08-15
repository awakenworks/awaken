---------------- MODULE ExecutableProjectionRefresh ----------------
EXTENDS Naturals

\* Active-active Coordinator requests refresh the Agent projection and then
\* the Environment projection before entering a Runtime-admitting handler.
\* Each durable registrar owns a per-domain mutex. Replay is applied to a clone;
\* only a successful clone install may advance the cursor. The separate Install
\* and Advance actions retain the short production linearization window where
\* readers may see a newer projection while the cursor still names its prefix.
CONSTANTS Requests, AgentDomain, EnvironmentDomain, NoRequest, MaxSequence

Domains == {AgentDomain, EnvironmentDomain}

KernelAssumptions ==
    /\ Requests # {}
    /\ AgentDomain # EnvironmentDomain
    /\ NoRequest \notin Requests
    /\ MaxSequence \in Nat \ {0}
    /\ 0 \in 0..MaxSequence

Phases == {
    "idle",
    "agent_waiting", "agent_refreshing", "agent_advancing",
    "environment_waiting", "environment_refreshing", "environment_advancing",
    "admitted", "rejected"
}

AgentLockPhases == {"agent_refreshing", "agent_advancing"}
EnvironmentLockPhases == {"environment_refreshing", "environment_advancing"}
EnvironmentPhases == {
    "environment_waiting", "environment_refreshing", "environment_advancing",
    "admitted"
}
TerminalPhases == {"admitted", "rejected"}

VARIABLES authority, installed, cursor, observed, phase, refreshed, lockOwner

vars == <<authority, installed, cursor, observed, phase, refreshed, lockOwner>>

Init ==
    /\ authority = [domain \in Domains |-> 0]
    /\ installed = [domain \in Domains |-> 0]
    /\ cursor = [domain \in Domains |-> 0]
    /\ observed = [request \in Requests |-> [domain \in Domains |-> 0]]
    /\ phase = [request \in Requests |-> "idle"]
    /\ refreshed = [request \in Requests |-> {}]
    /\ lockOwner = [domain \in Domains |-> NoRequest]

Publish(domain) ==
    /\ domain \in Domains
    /\ authority[domain] < MaxSequence
    /\ authority' = [authority EXCEPT ![domain] = @ + 1]
    /\ UNCHANGED <<installed, cursor, observed, phase, refreshed, lockOwner>>

Start(request) ==
    /\ request \in Requests
    /\ phase[request] = "idle"
    /\ phase' = [phase EXCEPT ![request] = "agent_waiting"]
    /\ refreshed' = [refreshed EXCEPT ![request] = {}]
    /\ UNCHANGED <<authority, installed, cursor, observed, lockOwner>>

AcquireAgent(request) ==
    /\ request \in Requests
    /\ phase[request] = "agent_waiting"
    /\ lockOwner[AgentDomain] = NoRequest
    /\ observed' = [observed EXCEPT
                       ![request][AgentDomain] = authority[AgentDomain]]
    /\ phase' = [phase EXCEPT ![request] = "agent_refreshing"]
    /\ lockOwner' = [lockOwner EXCEPT ![AgentDomain] = request]
    /\ UNCHANGED <<authority, installed, cursor, refreshed>>

\* Replaying into a clone may fail before this action. Installing the clone is
\* the first externally visible mutation and is immediately followed by the
\* infallible cursor advance while the same registrar mutex remains held.
InstallAgent(request) ==
    /\ request \in Requests
    /\ phase[request] = "agent_refreshing"
    /\ lockOwner[AgentDomain] = request
    /\ installed' = [installed EXCEPT
                        ![AgentDomain] = observed[request][AgentDomain]]
    /\ phase' = [phase EXCEPT ![request] = "agent_advancing"]
    /\ UNCHANGED <<authority, cursor, observed, refreshed, lockOwner>>

AdvanceAgent(request) ==
    /\ request \in Requests
    /\ phase[request] = "agent_advancing"
    /\ lockOwner[AgentDomain] = request
    /\ cursor' = [cursor EXCEPT
                     ![AgentDomain] = observed[request][AgentDomain]]
    /\ refreshed' = [refreshed EXCEPT
                        ![request] = @ \cup {AgentDomain}]
    /\ phase' = [phase EXCEPT ![request] = "environment_waiting"]
    /\ lockOwner' = [lockOwner EXCEPT ![AgentDomain] = NoRequest]
    /\ UNCHANGED <<authority, installed, observed>>

FailAgent(request) ==
    /\ request \in Requests
    /\ phase[request] = "agent_refreshing"
    /\ lockOwner[AgentDomain] = request
    /\ phase' = [phase EXCEPT ![request] = "rejected"]
    /\ lockOwner' = [lockOwner EXCEPT ![AgentDomain] = NoRequest]
    /\ UNCHANGED <<authority, installed, cursor, observed, refreshed>>

AcquireEnvironment(request) ==
    /\ request \in Requests
    /\ phase[request] = "environment_waiting"
    /\ AgentDomain \in refreshed[request]
    /\ lockOwner[EnvironmentDomain] = NoRequest
    /\ observed' = [observed EXCEPT
                       ![request][EnvironmentDomain] = authority[EnvironmentDomain]]
    /\ phase' = [phase EXCEPT ![request] = "environment_refreshing"]
    /\ lockOwner' = [lockOwner EXCEPT ![EnvironmentDomain] = request]
    /\ UNCHANGED <<authority, installed, cursor, refreshed>>

InstallEnvironment(request) ==
    /\ request \in Requests
    /\ phase[request] = "environment_refreshing"
    /\ lockOwner[EnvironmentDomain] = request
    /\ installed' = [installed EXCEPT
                        ![EnvironmentDomain] = observed[request][EnvironmentDomain]]
    /\ phase' = [phase EXCEPT ![request] = "environment_advancing"]
    /\ UNCHANGED <<authority, cursor, observed, refreshed, lockOwner>>

AdvanceEnvironment(request) ==
    /\ request \in Requests
    /\ phase[request] = "environment_advancing"
    /\ lockOwner[EnvironmentDomain] = request
    /\ cursor' = [cursor EXCEPT
                     ![EnvironmentDomain] = observed[request][EnvironmentDomain]]
    /\ refreshed' = [refreshed EXCEPT
                        ![request] = @ \cup {EnvironmentDomain}]
    /\ phase' = [phase EXCEPT ![request] = "admitted"]
    /\ lockOwner' = [lockOwner EXCEPT ![EnvironmentDomain] = NoRequest]
    /\ UNCHANGED <<authority, installed, observed>>

FailEnvironment(request) ==
    /\ request \in Requests
    /\ phase[request] = "environment_refreshing"
    /\ lockOwner[EnvironmentDomain] = request
    /\ phase' = [phase EXCEPT ![request] = "rejected"]
    /\ lockOwner' = [lockOwner EXCEPT ![EnvironmentDomain] = NoRequest]
    /\ UNCHANGED <<authority, installed, cursor, observed, refreshed>>

Reset(request) ==
    /\ request \in Requests
    /\ phase[request] \in TerminalPhases
    /\ phase' = [phase EXCEPT ![request] = "idle"]
    /\ refreshed' = [refreshed EXCEPT ![request] = {}]
    /\ UNCHANGED <<authority, installed, cursor, observed, lockOwner>>

Next ==
    \/ \E domain \in Domains: Publish(domain)
    \/ \E request \in Requests: Start(request)
    \/ \E request \in Requests: AcquireAgent(request)
    \/ \E request \in Requests: InstallAgent(request)
    \/ \E request \in Requests: AdvanceAgent(request)
    \/ \E request \in Requests: FailAgent(request)
    \/ \E request \in Requests: AcquireEnvironment(request)
    \/ \E request \in Requests: InstallEnvironment(request)
    \/ \E request \in Requests: AdvanceEnvironment(request)
    \/ \E request \in Requests: FailEnvironment(request)
    \/ \E request \in Requests: Reset(request)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ authority \in [Domains -> 0..MaxSequence]
    /\ installed \in [Domains -> 0..MaxSequence]
    /\ cursor \in [Domains -> 0..MaxSequence]
    /\ observed \in [Requests -> [Domains -> 0..MaxSequence]]
    /\ phase \in [Requests -> Phases]
    /\ refreshed \in [Requests -> SUBSET Domains]
    /\ lockOwner \in [Domains -> Requests \cup {NoRequest}]

\* The production install happens before the release-store cursor advance.
\* Therefore the cursor may lag a newly installed clone but cannot lead it.
CursorNeverAheadOfInstall ==
    \A domain \in Domains: cursor[domain] <= installed[domain]

InstallNeverAheadOfAuthority ==
    \A domain \in Domains: installed[domain] <= authority[domain]

RefreshTargetValid ==
    /\ \A request \in Requests:
         phase[request] \in AgentLockPhases =>
           /\ cursor[AgentDomain] <= observed[request][AgentDomain]
           /\ observed[request][AgentDomain] <= authority[AgentDomain]
    /\ \A request \in Requests:
         phase[request] \in EnvironmentLockPhases =>
           /\ cursor[EnvironmentDomain] <= observed[request][EnvironmentDomain]
           /\ observed[request][EnvironmentDomain] <= authority[EnvironmentDomain]

\* The install action fixes the clone at the request's captured high-water.
\* This inductive bridge justifies the following cursor advance.
InstalledTargetCoherent ==
    /\ \A request \in Requests:
         phase[request] = "agent_advancing" =>
           installed[AgentDomain] = observed[request][AgentDomain]
    /\ \A request \in Requests:
         phase[request] = "environment_advancing" =>
           installed[EnvironmentDomain] = observed[request][EnvironmentDomain]

RefreshOrder ==
    /\ \A request \in Requests:
         EnvironmentDomain \in refreshed[request] =>
           AgentDomain \in refreshed[request]
    /\ \A request \in Requests:
         phase[request] \in EnvironmentPhases =>
           AgentDomain \in refreshed[request]
    /\ \A request \in Requests:
         phase[request] = "admitted" =>
           /\ AgentDomain \in refreshed[request]
           /\ EnvironmentDomain \in refreshed[request]
    /\ \A request \in Requests:
         phase[request] \in
           {"idle", "agent_waiting", "agent_refreshing", "agent_advancing"} =>
             refreshed[request] = {}
    /\ \A request \in Requests:
         phase[request] \in
           {"environment_waiting", "environment_refreshing",
            "environment_advancing"} =>
           EnvironmentDomain \notin refreshed[request]

RefreshedTargetsCovered ==
    /\ \A request \in Requests:
         AgentDomain \in refreshed[request] =>
           observed[request][AgentDomain] <= cursor[AgentDomain]
    /\ \A request \in Requests:
         EnvironmentDomain \in refreshed[request] =>
           observed[request][EnvironmentDomain] <= cursor[EnvironmentDomain]

AdmissionCovered ==
    \A request \in Requests:
      phase[request] = "admitted" =>
        /\ observed[request][AgentDomain] <= cursor[AgentDomain]
        /\ observed[request][EnvironmentDomain] <= cursor[EnvironmentDomain]

MutexCoherent ==
    /\ \A request \in Requests:
         (phase[request] \in AgentLockPhases) \equiv
           (lockOwner[AgentDomain] = request)
    /\ \A request \in Requests:
         (phase[request] \in EnvironmentLockPhases) \equiv
           (lockOwner[EnvironmentDomain] = request)

Safety ==
    /\ TypeOK
    /\ CursorNeverAheadOfInstall
    /\ InstallNeverAheadOfAuthority
    /\ RefreshTargetValid
    /\ InstalledTargetCoherent
    /\ RefreshOrder
    /\ RefreshedTargetsCovered
    /\ AdmissionCovered
    /\ MutexCoherent

======================================================================
