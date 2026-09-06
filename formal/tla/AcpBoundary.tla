------------------------------ MODULE AcpBoundary ------------------------------
EXTENDS Naturals, FiniteSets

VARIABLES waiters, exitCode, termSent, killSent, cleanupExpired, observing, ack
VARIABLES handshake, permissionCall, projectedOption, promptCount, finalCount
processVars == <<waiters, exitCode, termSent, killSent, cleanupExpired, observing, ack>>
protocolVars == <<handshake, permissionCall, projectedOption, promptCount, finalCount>>
vars == <<processVars, handshake, permissionCall, projectedOption, promptCount, finalCount>>

Init ==
  /\ waiters = {} /\ exitCode = 0
  /\ termSent = FALSE /\ killSent = FALSE /\ cleanupExpired = FALSE
  /\ observing = TRUE /\ ack = FALSE
  /\ handshake = FALSE
  /\ permissionCall = FALSE
  /\ projectedOption = "None"
  /\ promptCount = 0
  /\ finalCount = 0

CompleteHandshake ==
  /\ UNCHANGED processVars
  /\ ~handshake
  /\ handshake' = TRUE
  /\ UNCHANGED <<permissionCall, projectedOption, promptCount, finalCount>>

BeginPermission ==
  /\ UNCHANGED processVars
  /\ handshake
  /\ ~permissionCall
  /\ permissionCall' = TRUE
  /\ promptCount' = promptCount + 1
  /\ UNCHANGED <<handshake, projectedOption, finalCount>>

ProjectAllow ==
  /\ UNCHANGED processVars
  /\ permissionCall
  /\ finalCount = 0
  /\ projectedOption' = "Allow"
  /\ finalCount' = 1
  /\ UNCHANGED <<handshake, permissionCall, promptCount>>

ProjectReject ==
  /\ UNCHANGED processVars
  /\ permissionCall
  /\ finalCount = 0
  /\ projectedOption' = "Reject"
  /\ finalCount' = 1
  /\ UNCHANGED <<handshake, permissionCall, promptCount>>


\* Two independently cancellable observers share one process-owned completion.
\* Exit is an external observation. TERM/KILL requests and lease timeouts are
\* never that observation; no provider terminal API is assumed by this model.
BeginWait(w) ==
  /\ w \in {1, 2} /\ w \notin waiters /\ exitCode = 0
  /\ waiters' = waiters \cup {w}
  /\ UNCHANGED <<protocolVars, exitCode, termSent, killSent, cleanupExpired, observing, ack>>
DropWait(w) ==
  /\ w \in waiters
  /\ waiters' = waiters \ {w}
  /\ UNCHANGED <<protocolVars, exitCode, termSent, killSent, cleanupExpired, observing, ack>>
SendTerm ==
  /\ ~termSent /\ exitCode = 0 /\ termSent' = TRUE
  /\ UNCHANGED <<protocolVars, waiters, exitCode, killSent, cleanupExpired, observing, ack>>
SendKill ==
  /\ termSent /\ ~killSent /\ exitCode = 0 /\ killSent' = TRUE
  /\ UNCHANGED <<protocolVars, waiters, exitCode, termSent, cleanupExpired, observing, ack>>
CleanupTimeout ==
  /\ termSent /\ ~cleanupExpired /\ cleanupExpired' = TRUE
  /\ UNCHANGED <<protocolVars, waiters, exitCode, termSent, killSent, observing, ack>>
ObserveExit(code) ==
  /\ code \in {7, 137} /\ exitCode = 0
  /\ exitCode' = code
  /\ UNCHANGED <<protocolVars, waiters, termSent, killSent, cleanupExpired, observing, ack>>
Acknowledge ==
  /\ exitCode # 0 /\ observing /\ ~ack /\ ack' = TRUE
  /\ UNCHANGED <<protocolVars, waiters, exitCode, termSent, killSent, cleanupExpired, observing>>

Next == CompleteHandshake \/ BeginPermission \/ ProjectAllow \/ ProjectReject
        \/ (\E w \in {1, 2}: BeginWait(w) \/ DropWait(w))
        \/ SendTerm \/ SendKill \/ CleanupTimeout
        \/ (\E code \in {7, 137}: ObserveExit(code)) \/ Acknowledge
Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ WF_vars(Acknowledge)
ActualExitEventuallyAcknowledged == exitCode # 0 ~> ack
NoFabricatedQuiescence == ack => exitCode # 0
CleanupObserverRetained == observing

TypeOK ==
  /\ waiters \subseteq {1, 2} /\ exitCode \in {0, 7, 137}
  /\ termSent \in BOOLEAN /\ killSent \in BOOLEAN
  /\ cleanupExpired \in BOOLEAN /\ observing \in BOOLEAN /\ ack \in BOOLEAN
  /\ handshake \in BOOLEAN
  /\ permissionCall \in BOOLEAN
  /\ projectedOption \in {"None", "Allow", "Reject"}
  /\ promptCount \in 0..1
  /\ finalCount \in 0..1

HandshakeBeforePrompt == promptCount > 0 => handshake
ProjectionRequiresCall == projectedOption # "None" => permissionCall
AtMostOneFinal == finalCount <= 1
Safety == NoFabricatedQuiescence /\ CleanupObserverRetained /\ TypeOK /\ HandshakeBeforePrompt /\ ProjectionRequiresCall /\ AtMostOneFinal
=============================================================================
