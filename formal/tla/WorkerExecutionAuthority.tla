-------------------- MODULE WorkerExecutionAuthority --------------------
EXTENDS SessionRealizationAuthority, Integers

(* Reuse the exact Session slot/cleanup kernel. Add only the registry receipt
   projection now shared by heartbeat and Session installation. One incarnation,
   one outstanding response, boot clock 0..3, conservative proof window 2.
   Clock may advance without a heartbeat tick (suspend/scheduling delay).
   Response sampling chooses current boot time or -1 (before every request)
   to exercise checked elapsed subtraction; -1 is an invalid observation,
   not a claim that real BOOTTIME regresses or that arbitrary rollback is
   detectable. One outstanding request excludes response reordering.
   Root checks and Session mutation are one synchronous callback action; no
   async owner lock is held there. This is not a model of physical GO, OS cutoff,
   arbitrary callback duration, clock correctness, or Rust refinement. *)
VARIABLES rootPhase, bootNow, deadline, heartbeatRequest, closedEver,
          badReceipt, badAdmission, admissions
rootVars == <<rootPhase, bootNow, deadline, heartbeatRequest, closedEver,
              badReceipt, badAdmission, admissions>>
allVars == <<vars, rootVars>>
RootCurrent == rootPhase = "Live" /\ bootNow < deadline
RootInit == Init /\ rootPhase = "Starting" /\ bootNow = 0 /\ deadline = 0
  /\ heartbeatRequest = -1 /\ closedEver = FALSE
  /\ badReceipt = FALSE /\ badAdmission = FALSE /\ admissions = 0

RequestHeartbeat ==
  /\ rootPhase # "Closed" /\ heartbeatRequest = -1
  /\ heartbeatRequest' = bootNow
  /\ UNCHANGED <<vars, rootPhase, bootNow, deadline, closedEver,
                  badReceipt, badAdmission, admissions>>

ApplyHeartbeatAt(observed) ==
  /\ heartbeatRequest # -1
  /\ LET accepted == (rootPhase = "Starting" \/ RootCurrent)
                      /\ observed >= heartbeatRequest
                      /\ observed < heartbeatRequest + 2
     IN /\ rootPhase' = IF accepted THEN "Live" ELSE "Closed"
        /\ deadline' = IF accepted THEN heartbeatRequest + 2 ELSE deadline
        /\ closedEver' = (closedEver \/ ~accepted)
        /\ badReceipt' = (badReceipt \/ (accepted /\
             (rootPhase = "Closed" \/ (rootPhase = "Live" /\ bootNow >= deadline)
              \/ observed < heartbeatRequest
              \/ observed >= heartbeatRequest + 2)))
  /\ heartbeatRequest' = -1
  /\ UNCHANGED <<vars, bootNow, badAdmission, admissions>>

ApplyHeartbeat == \E observed \in {bootNow, -1} : ApplyHeartbeatAt(observed)

CloseRoot ==
  /\ rootPhase # "Closed" /\ rootPhase' = "Closed" /\ closedEver' = TRUE
  /\ UNCHANGED <<vars, bootNow, deadline, heartbeatRequest,
                  badReceipt, badAdmission, admissions>>

Tick ==
  /\ bootNow < 3 /\ bootNow' = bootNow + 1
  /\ UNCHANGED <<vars, rootPhase, deadline, heartbeatRequest, closedEver,
                  badReceipt, badAdmission, admissions>>

(* A fresh Control claim may arrive after close or expiry. Root guard and slot
   publication linearize together; no copied preflight authorization suffices.
   Replace reuses the exact current Session generation's installation kernel.
   FreshAdmission covers a previously absent Session, with a bounded count. *)
InstallReplacement ==
  /\ RootCurrent /\ Replace
  /\ badAdmission' = (badAdmission \/ ~RootCurrent)
  /\ UNCHANGED <<rootPhase, bootNow, deadline, heartbeatRequest, closedEver,
                  badReceipt, admissions>>

FreshAdmission ==
  /\ RootCurrent /\ admissions < 2 /\ admissions' = admissions + 1
  /\ badAdmission' = (badAdmission \/ ~RootCurrent)
  /\ UNCHANGED <<vars, rootPhase, bootNow, deadline, heartbeatRequest,
                  closedEver, badReceipt>>

RenewProjection ==
  /\ RootCurrent /\ ApplyRenewal
  /\ badAdmission' = (badAdmission \/ ~RootCurrent)
  /\ UNCHANGED <<rootPhase, bootNow, deadline, heartbeatRequest, closedEver,
                  badReceipt, admissions>>

(* Root close precedes cohort selection; cleanup still uses captured owners.
   Pure Session cleanup and observation never require a live execution grant. *)
RootSessionNext ==
  (SendRenewal \/ ReleaseOwner \/ RecreateClosedSlot \/ FinishPeer
   \/ (rootPhase = "Closed" /\ CloseWorker)
   \/ (\E c \in Callers : BeginRevoke(c) \/ CancelCleanup(c) \/ FinishCleanup(c)))
  /\ UNCHANGED rootVars

RootNext == RequestHeartbeat \/ ApplyHeartbeat \/ CloseRoot \/ Tick
  \/ InstallReplacement \/ FreshAdmission \/ RenewProjection \/ RootSessionNext
RootSpec == RootInit /\ [][RootNext]_allVars
RootTypeOK == TypeOK /\ rootPhase \in {"Starting", "Live", "Closed"}
  /\ bootNow \in 0..3 /\ deadline \in 0..5 /\ heartbeatRequest \in {-1, 0, 1, 2, 3}
  /\ closedEver \in BOOLEAN /\ badReceipt \in BOOLEAN /\ badAdmission \in BOOLEAN
  /\ admissions \in 0..2
ClosedRootNeverReopens == closedEver => rootPhase = "Closed"
ReceiptNeverRevivesExpiredProof == ~badReceipt
SessionInstallationRequiresCurrentRoot == ~badAdmission
NoAdmissionReached == admissions = 0
NoRenewedRootReached == deadline < 3
NoRegressionResponse == [][~ApplyHeartbeatAt(-1)]_allVars
=============================================================================
