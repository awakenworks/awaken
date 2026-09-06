---------------------- MODULE LocalProcessRefresh ----------------------
EXTENDS LocalProcessLifecycle

\* Provider error classification composes the SAME launch/kernel boundary.
\* One failed refresh, arbitrarily interleaved with authority loss, control
\* disconnect, helper/leader death, and exact init exit. Successful delivery
\* and frame/clock behavior remain owned by LocalProcessRenewal.cfg.
\* A truthful ProcessHandle poll is assumed; it may fail/return unknown even
\* after kernel exit. No exception kind, leader exit, or stale preflight poll
\* proves tree quiescence. This is bounded abstract checking, not Rust/kernel
\* refinement or production Worker/Session composition proof.
VARIABLES refreshState, refreshObservation
refreshVars == <<refreshState, refreshObservation>>
allRefreshVars == <<allBoundaryVars, renewalVars, refreshVars>>

RefreshInit == Init /\ refreshState = "idle" /\ refreshObservation = "none"

RefreshPreflight ==
  /\ refreshState = "idle" /\ workloadStarted /\ initState = "live"
  /\ refreshState' = "checked"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, refreshObservation>>

\* Failure kind is deliberately abstract: authorization denial, timeout and
\* broken channel all require the same fresh observation. It may occur before
\* or after exact exit; callback/transport failure alone cannot close a tree.
RefreshDeliveryFails ==
  /\ refreshState = "checked" /\ refreshState' = "failed"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, refreshObservation>>

\* Cause/effect rules R2-R3 live/unknown -> error; R4-R6 exact exit -> skip.
\* An unknown poll remains an error even when the kernel has actually exited.
RefreshReobserve(unknown) ==
  /\ refreshState = "failed"
  /\ LET observation == IF unknown THEN "unknown"
                        ELSE IF initState = "exited" THEN "exited" ELSE "live"
     IN /\ refreshObservation' = observation
        /\ refreshState' = IF observation = "exited" THEN "ignored" ELSE "error"
  /\ UNCHANGED <<allBoundaryVars, renewalVars>>

RefreshNext == (BoundaryNext /\ UNCHANGED <<renewalVars, refreshVars>>)
  \/ RefreshPreflight \/ RefreshDeliveryFails
  \/ (\E unknown \in BOOLEAN : RefreshReobserve(unknown))
RefreshSpec == RefreshInit /\ [][RefreshNext]_allRefreshVars

RefreshTypeOK == BoundaryTypeOK
  /\ refreshState \in {"idle", "checked", "failed", "ignored", "error"}
  /\ refreshObservation \in {"none", "live", "exited", "unknown"}
IgnoredRefreshRequiresExactExit == refreshState = "ignored"
  => initState = "exited" /\ refreshObservation = "exited"
ConfirmedExitSkipsRefreshFailure == refreshObservation = "exited"
  => refreshState = "ignored"
UnknownRefreshRemainsError == refreshObservation = "unknown" => refreshState = "error"
\* Positive witnesses: mutation runner requires both outcomes reachable.
NoRefreshSkipReached == refreshState # "ignored"
NoRefreshErrorReached == refreshState # "error"
=============================================================================
