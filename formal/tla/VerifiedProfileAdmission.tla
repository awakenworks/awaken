--------------------- MODULE VerifiedProfileAdmission ---------------------
EXTENDS Integers, TLC

CONSTANT Dependencies
VARIABLE snapshot, decision
vars == <<snapshot, decision>>

\* Point-in-time abstraction of validate_tcb_evidence AFTER artifact/proof
\* qualification. Target version/config values are normalized to 0; 1 means
\* mismatch, not a real version or hash. complete abstracts exact, unique,
\* nonempty IDs and closed shapes; hashes abstracts syntactic validity only.
\* Bounds: two dependencies, match/mismatch, clock/observation -1..3, max age 1.
\* Trusted caller authenticates a frozen snapshot. No Python refinement,
\* signature authentication, continuous revocation or unconditional liveness
\* is claimed. The decision never authorizes future changed runtime snapshots.
Snapshots == [complete : BOOLEAN,
              versions : [Dependencies -> 0..1],
              configurations : [Dependencies -> 0..1],
              contracts : [Dependencies -> BOOLEAN],
              hashes : BOOLEAN,
              now : -1..3,
              observed : [Dependencies -> -1..3]]

Init == /\ snapshot \in Snapshots
        /\ decision = "Pending"

\* Cause/effect rules: R1 all guards -> Accept, including age = 1.
\* R2 any identity/contract/hash/version/config mismatch -> Reject.
\* R3 negative/future/expired time -> Reject; one valid dependency cannot
\* mask another invalid dependency. Decision is terminal for this snapshot.
Admissible == /\ snapshot.complete
              /\ snapshot.hashes
              /\ \A d \in Dependencies : snapshot.contracts[d]
              /\ \A d \in Dependencies : snapshot.versions[d] = 0
              /\ \A d \in Dependencies : snapshot.configurations[d] = 0
              /\ snapshot.now >= 0
              /\ \A d \in Dependencies :
                    /\ snapshot.observed[d] >= 0
                    /\ snapshot.observed[d] <= snapshot.now
                    /\ snapshot.now - snapshot.observed[d] <= 1

Decide == /\ decision = "Pending"
          /\ decision' = IF Admissible THEN "Accepted" ELSE "Rejected"
          /\ UNCHANGED snapshot
Next == Decide \/ UNCHANGED vars

TypeOK == /\ snapshot \in Snapshots
          /\ decision \in {"Pending", "Accepted", "Rejected"}
AcceptedHasExactFingerprint == decision = "Accepted" =>
    \A d \in Dependencies :
        /\ snapshot.versions[d] = 0
        /\ snapshot.configurations[d] = 0
AcceptedHasFreshEvidence == decision = "Accepted" =>
    /\ snapshot.now >= 0
    /\ \A d \in Dependencies :
        /\ 0 <= snapshot.observed[d]
        /\ snapshot.observed[d] <= snapshot.now
        /\ snapshot.now <= snapshot.observed[d] + 1
AcceptedHasCompleteContracts == decision = "Accepted" =>
    /\ snapshot.complete /\ snapshot.hashes
    /\ \A d \in Dependencies : snapshot.contracts[d]
ValidSnapshotIsNotRejected == Admissible => decision # "Rejected"

Spec == Init /\ [][Next]_vars
=============================================================================
