------------------------------ MODULE AcpBoundary ------------------------------
EXTENDS Naturals, FiniteSets, Sequences

VARIABLES waiters, exitCode, termSent, termFailed, killSent, cleanupExpired, observing, ack
VARIABLES handshake, permissionCall, projectedOption, promptCount, finalCount
processVars == <<waiters, exitCode, termSent, termFailed, killSent, cleanupExpired, observing, ack>>
protocolVars == <<handshake, permissionCall, projectedOption, promptCount, finalCount>>
VARIABLES hostLeft, peerLeft, peerChunks, uplink, downlink, queue, delivered, wireOutcome
wireVars == <<hostLeft, peerLeft, peerChunks, uplink, downlink, queue, delivered, wireOutcome>>
vars == <<processVars, protocolVars, wireVars>>

Init ==
  /\ waiters = {} /\ exitCode = 0
  /\ termSent = FALSE /\ termFailed = FALSE /\ killSent = FALSE /\ cleanupExpired = FALSE
  /\ observing = TRUE /\ ack = FALSE
  /\ handshake = FALSE
  /\ permissionCall = FALSE
  /\ projectedOption = "None"
  /\ promptCount = 0
  /\ finalCount = 0
  /\ peerChunks \in {2, 3} /\ peerLeft = peerChunks /\ hostLeft = 2
  /\ uplink = 0 /\ downlink = 0 /\ queue = <<>> /\ delivered = <<>>
  /\ wireOutcome = "Active"

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
  /\ UNCHANGED <<protocolVars, exitCode, termSent, termFailed, killSent, cleanupExpired, observing, ack>>
DropWait(w) ==
  /\ w \in waiters
  /\ waiters' = waiters \ {w}
  /\ UNCHANGED <<protocolVars, exitCode, termSent, termFailed, killSent, cleanupExpired, observing, ack>>
\* termSent records a returned request, including failure; it is not delivery proof.
SendTerm ==
  /\ ~termSent /\ exitCode = 0 /\ termSent' = TRUE
  /\ termFailed' \in BOOLEAN
  /\ UNCHANGED <<protocolVars, waiters, exitCode, killSent, cleanupExpired, observing, ack>>
SendKill ==
  /\ termSent /\ ~killSent /\ exitCode = 0 /\ killSent' = TRUE
  /\ UNCHANGED <<protocolVars, waiters, exitCode, termSent, termFailed, cleanupExpired, observing, ack>>
CleanupTimeout ==
  /\ termSent /\ ~cleanupExpired /\ cleanupExpired' = TRUE
  /\ UNCHANGED <<protocolVars, waiters, exitCode, termSent, termFailed, killSent, observing, ack>>
ObserveExit(code) ==
  /\ code \in {7, 137} /\ exitCode = 0
  /\ exitCode' = code
  /\ UNCHANGED <<protocolVars, waiters, termSent, termFailed, killSent, cleanupExpired, observing, ack>>
Acknowledge ==
  /\ exitCode # 0 /\ observing /\ ~ack /\ ack' = TRUE
  /\ UNCHANGED <<protocolVars, waiters, exitCode, termSent, termFailed, killSent, cleanupExpired, observing>>

LifecycleNext == CompleteHandshake \/ BeginPermission \/ ProjectAllow \/ ProjectReject
        \/ (\E w \in {1, 2}: BeginWait(w) \/ DropWait(w))
        \/ SendTerm \/ SendKill \/ CleanupTimeout
        \/ (\E code \in {7, 137}: ObserveExit(code)) \/ Acknowledge
\* A one-slot duplex transports two outbound chunks while its synchronous peer
\* writes two/three notifications before reading. The bounded read-ahead queue
\* drains during send; projection still consumes it only after send completes.
HostWrite ==
  /\ wireOutcome = "Active" /\ hostLeft > 0 /\ uplink = 0
  /\ hostLeft' = hostLeft - 1 /\ uplink' = 1
  /\ UNCHANGED <<peerLeft, peerChunks, downlink, queue, delivered, wireOutcome>>
PeerWrite ==
  /\ wireOutcome = "Active" /\ peerLeft > 0 /\ downlink = 0
  /\ downlink' = peerChunks - peerLeft + 1 /\ peerLeft' = peerLeft - 1
  /\ UNCHANGED <<hostLeft, peerChunks, uplink, queue, delivered, wireOutcome>>
HostDrain ==
  /\ wireOutcome = "Active" /\ downlink > 0
  /\ IF Len(queue) < 2
        THEN /\ queue' = Append(queue, downlink) /\ downlink' = 0
             /\ UNCHANGED wireOutcome
        ELSE /\ wireOutcome' = "Overflow" /\ UNCHANGED <<queue, downlink>>
  /\ UNCHANGED <<hostLeft, peerLeft, peerChunks, uplink, delivered>>
PeerRead ==
  /\ wireOutcome = "Active" /\ peerLeft = 0 /\ uplink = 1
  /\ uplink' = 0
  /\ UNCHANGED <<hostLeft, peerLeft, peerChunks, downlink, queue, delivered, wireOutcome>>
ConsumeQueued ==
  /\ wireOutcome = "Active" /\ hostLeft = 0 /\ Len(queue) > 0
  /\ delivered' = Append(delivered, Head(queue)) /\ queue' = Tail(queue)
  /\ UNCHANGED <<hostLeft, peerLeft, peerChunks, uplink, downlink, wireOutcome>>
FinishWire ==
  /\ wireOutcome = "Active" /\ hostLeft = 0 /\ peerLeft = 0
  /\ uplink = 0 /\ downlink = 0 /\ queue = <<>>
  /\ wireOutcome' = "Done"
  /\ UNCHANGED <<hostLeft, peerLeft, peerChunks, uplink, downlink, queue, delivered>>
CancelWire ==
  /\ wireOutcome = "Active" /\ wireOutcome' = "Cancelled"
  /\ UNCHANGED <<hostLeft, peerLeft, peerChunks, uplink, downlink, queue, delivered>>
WireDataNext == HostWrite \/ PeerWrite \/ HostDrain \/ PeerRead \/ ConsumeQueued \/ FinishWire
WireNext == (WireDataNext \/ CancelWire) /\ UNCHANGED <<processVars, protocolVars>>
Next == (LifecycleNext /\ UNCHANGED wireVars) \/ WireNext
Spec == Init /\ [][Next]_vars
FairSpec == Spec /\ WF_vars(Acknowledge /\ UNCHANGED wireVars)
            /\ WF_vars(HostWrite /\ UNCHANGED <<processVars, protocolVars>>)
            /\ WF_vars(PeerWrite /\ UNCHANGED <<processVars, protocolVars>>)
            /\ WF_vars(HostDrain /\ UNCHANGED <<processVars, protocolVars>>)
            /\ WF_vars(PeerRead /\ UNCHANGED <<processVars, protocolVars>>)
            /\ WF_vars(ConsumeQueued /\ UNCHANGED <<processVars, protocolVars>>)
            /\ WF_vars(FinishWire /\ UNCHANGED <<processVars, protocolVars>>)
WireEventuallySettles == <>(wireOutcome # "Active")
WireCanAdvance == wireOutcome = "Active" => ENABLED WireDataNext
WireSafety ==
  /\ hostLeft \in 0..2 /\ peerChunks \in {2, 3} /\ peerLeft \in 0..peerChunks
  /\ uplink \in 0..1 /\ downlink \in 0..peerChunks
  /\ wireOutcome \in {"Active", "Cancelled", "Overflow", "Done"}
  /\ Len(queue) <= 2 /\ Len(delivered) <= peerChunks
  /\ delivered = SubSeq(<<1, 2, 3>>, 1, Len(delivered))
  /\ queue = SubSeq(<<1, 2, 3>>, Len(delivered) + 1, Len(delivered) + Len(queue))
  /\ (wireOutcome = "Done" => Len(delivered) = peerChunks)
  /\ WireCanAdvance
ActualExitEventuallyAcknowledged == exitCode # 0 ~> ack
NoFabricatedQuiescence == ack => exitCode # 0
CleanupObserverRetained == observing
\* Request failure does not prove exit or revoke the same-handle KILL stage.
TermFailureAllowsEscalation ==
  (termSent /\ termFailed /\ ~killSent /\ ~cleanupExpired /\ exitCode = 0)
    => ENABLED SendKill
NoFailedTermEscalationReached == ~(termFailed /\ killSent)

TypeOK ==
  /\ waiters \subseteq {1, 2} /\ exitCode \in {0, 7, 137}
  /\ termSent \in BOOLEAN /\ termFailed \in BOOLEAN /\ killSent \in BOOLEAN
  /\ cleanupExpired \in BOOLEAN /\ observing \in BOOLEAN /\ ack \in BOOLEAN
  /\ handshake \in BOOLEAN
  /\ permissionCall \in BOOLEAN
  /\ projectedOption \in {"None", "Allow", "Reject"}
  /\ promptCount \in 0..1
  /\ finalCount \in 0..1

HandshakeBeforePrompt == promptCount > 0 => handshake
ProjectionRequiresCall == projectedOption # "None" => permissionCall
AtMostOneFinal == finalCount <= 1
Safety == TermFailureAllowsEscalation /\ WireSafety /\ NoFabricatedQuiescence /\ CleanupObserverRetained /\ TypeOK /\ HandshakeBeforePrompt /\ ProjectionRequiresCall /\ AtMostOneFinal
=============================================================================
