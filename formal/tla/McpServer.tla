------------------------------- MODULE McpServer -------------------------------
EXTENDS Naturals, TLC

VARIABLES phase, kind, admitted, cancelled, hostCalls, responses, results,
          progress, progressAtFinal

vars == <<phase, kind, admitted, cancelled, hostCalls, responses, results,
          progress, progressAtFinal>>

Init ==
    /\ phase = "idle"
    /\ kind = "none"
    /\ admitted = FALSE
    /\ cancelled = FALSE
    /\ hostCalls = 0
    /\ responses = 0
    /\ results = 0
    /\ progress = 0
    /\ progressAtFinal = 0

StartRequest ==
    /\ phase = "idle"
    /\ \E supported, valid \in BOOLEAN:
        /\ kind' = "request"
        /\ admitted' = supported /\ valid
        /\ cancelled' = FALSE
        /\ IF supported /\ valid
              THEN /\ phase' = "active"
                   /\ hostCalls' = 1
                   /\ responses' = 0
              ELSE /\ phase' = "final"
                   /\ hostCalls' = 0
                   /\ responses' = 1
        /\ results' = 0
        /\ UNCHANGED progress
        /\ progressAtFinal' = progress

StartNotification ==
    /\ phase = "idle"
    /\ phase' = "final"
    /\ kind' = "notification"
    /\ responses' = 0
    /\ results' = 0
    /\ progressAtFinal' = progress
    /\ UNCHANGED <<admitted, cancelled, hostCalls, progress>>

EmitProgress ==
    /\ phase = "active"
    /\ ~cancelled
    /\ progress < 3
    /\ progress' = progress + 1
    /\ UNCHANGED <<phase, kind, admitted, cancelled, hostCalls, responses,
                    results, progressAtFinal>>

Complete ==
    /\ phase = "active"
    /\ phase' = "final"
    /\ responses' = responses + 1
    /\ results' = results + 1
    /\ progressAtFinal' = progress
    /\ UNCHANGED <<kind, admitted, cancelled, hostCalls, progress>>

Cancel ==
    /\ phase = "active"
    /\ phase' = "final"
    /\ cancelled' = TRUE
    /\ responses' = responses + 1
    /\ results' = results
    /\ progressAtFinal' = progress
    /\ UNCHANGED <<kind, admitted, hostCalls, progress>>

Next == StartRequest \/ StartNotification \/ EmitProgress \/ Complete \/ Cancel
Spec == Init /\ [][Next]_vars

AtMostOneFinal == responses <= 1
NotificationHasNoResponse == kind = "notification" => responses = 0
FinalStopsProgress == phase = "final" => progress = progressAtFinal
RejectedSkipsHost == ~admitted => hostCalls = 0
CancelledHasNoResult == cancelled => results = 0
IdleHasNoHost == phase = "idle" => hostCalls = 0

Safety == AtMostOneFinal /\ NotificationHasNoResponse /\ FinalStopsProgress /\
          RejectedSkipsHost /\ CancelledHasNoResult /\ IdleHasNoHost

=============================================================================
