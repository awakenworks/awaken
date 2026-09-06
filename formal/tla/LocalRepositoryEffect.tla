---------------------- MODULE LocalRepositoryEffect ----------------------
EXTENDS LocalProcessLifecycle

\* Repository transport composes the SAME launch and kernel boundary, not a
\* second Git/CAS model. Causes: reply valid/invalid, complete tree live/exited,
\* observed status success/failure, caller waiting/cancelled, Worker crash.
\* Rules: R1 only valid reply AND exact exit AND successful status can return;
\* R2 stdout or leader exit alone cannot; R3 cancellation cannot resume return.
\* One effect, one reply, one status observation. Pipe bytes, Git remote CAS,
\* actual OS evidence and Rust refinement are outside this bounded abstraction.
VARIABLES repositoryReply, repositoryStatus, repositoryCaller
repositoryVars == <<repositoryReply, repositoryStatus, repositoryCaller>>
allRepositoryVars == <<allBoundaryVars, renewalVars, repositoryVars>>

RepositoryInit == Init
  /\ repositoryReply = "none" /\ repositoryStatus = "none"
  /\ repositoryCaller = "waiting"

RepositoryReply(valid) ==
  /\ workloadStarted /\ leaderAlive /\ repositoryReply = "none"
  /\ repositoryReply' = IF valid THEN "valid" ELSE "invalid"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, repositoryStatus, repositoryCaller>>

RepositoryWait(ok) ==
  /\ workloadStarted /\ initState = "exited" /\ repositoryStatus = "none"
  /\ repositoryStatus' = IF ok THEN "success" ELSE "failure"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, repositoryReply, repositoryCaller>>

RepositoryReturn ==
  /\ repositoryCaller = "waiting" /\ phase = "owned"
  /\ repositoryReply = "valid" /\ repositoryStatus = "success"
  /\ initState = "exited"
  /\ repositoryCaller' = "returned"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, repositoryReply, repositoryStatus>>

RepositoryCancel ==
  /\ repositoryCaller = "waiting" /\ repositoryCaller' = "cancelled"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, repositoryReply, repositoryStatus>>

RepositoryNext ==
  (BoundaryNext /\ UNCHANGED <<renewalVars, repositoryVars>>)
  \/ (\E valid \in BOOLEAN : RepositoryReply(valid))
  \/ (\E ok \in BOOLEAN : RepositoryWait(ok))
  \/ RepositoryReturn \/ RepositoryCancel
RepositorySpec == RepositoryInit /\ [][RepositoryNext]_allRepositoryVars

RepositoryTypeOK == BoundaryTypeOK
  /\ repositoryReply \in {"none", "valid", "invalid"}
  /\ repositoryStatus \in {"none", "success", "failure"}
  /\ repositoryCaller \in {"waiting", "cancelled", "returned"}
RepositoryReturnRequiresExit == repositoryCaller = "returned" => initState = "exited"
RepositoryReturnRequiresValidReply == repositoryCaller = "returned" => repositoryReply = "valid"
RepositoryReturnRequiresSuccess == repositoryCaller = "returned" => repositoryStatus = "success"
RepositoryCancellationIsTerminal ==
  [][repositoryCaller = "cancelled" => repositoryCaller' = "cancelled"]_allRepositoryVars
RepositoryCrashCannotReturn ==
  [][(repositoryCaller # "returned" /\ repositoryCaller' = "returned")
     => phase = "owned"]_allRepositoryVars
\* Witnesses reject vacuity: success, early reply with live descendants, and
\* cancellation followed by exact teardown must each be reachable.
NoRepositoryReturnReached == repositoryCaller # "returned"
NoEarlyRepositoryReplyReached ==
  ~(repositoryReply = "valid" /\ descendantAlive /\ initState = "live")
NoCancelledRepositoryExitReached ==
  ~(repositoryCaller = "cancelled" /\ workloadStarted /\ initState = "exited")
=============================================================================
