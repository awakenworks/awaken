------------------------------ MODULE AcpBoundary ------------------------------
EXTENDS Naturals

VARIABLES handshake, permissionCall, projectedOption, promptCount, finalCount
vars == <<handshake, permissionCall, projectedOption, promptCount, finalCount>>

Init ==
  /\ handshake = FALSE
  /\ permissionCall = FALSE
  /\ projectedOption = "None"
  /\ promptCount = 0
  /\ finalCount = 0

CompleteHandshake ==
  /\ ~handshake
  /\ handshake' = TRUE
  /\ UNCHANGED <<permissionCall, projectedOption, promptCount, finalCount>>

BeginPermission ==
  /\ handshake
  /\ ~permissionCall
  /\ permissionCall' = TRUE
  /\ promptCount' = promptCount + 1
  /\ UNCHANGED <<handshake, projectedOption, finalCount>>

ProjectAllow ==
  /\ permissionCall
  /\ finalCount = 0
  /\ projectedOption' = "Allow"
  /\ finalCount' = 1
  /\ UNCHANGED <<handshake, permissionCall, promptCount>>

ProjectReject ==
  /\ permissionCall
  /\ finalCount = 0
  /\ projectedOption' = "Reject"
  /\ finalCount' = 1
  /\ UNCHANGED <<handshake, permissionCall, promptCount>>

Next == CompleteHandshake \/ BeginPermission \/ ProjectAllow \/ ProjectReject
Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ handshake \in BOOLEAN
  /\ permissionCall \in BOOLEAN
  /\ projectedOption \in {"None", "Allow", "Reject"}
  /\ promptCount \in 0..1
  /\ finalCount \in 0..1

HandshakeBeforePrompt == promptCount > 0 => handshake
ProjectionRequiresCall == projectedOption # "None" => permissionCall
AtMostOneFinal == finalCount <= 1
Safety == TypeOK /\ HandshakeBeforePrompt /\ ProjectionRequiresCall /\ AtMostOneFinal
=============================================================================
