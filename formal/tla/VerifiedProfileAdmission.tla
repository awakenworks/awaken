--------------------- MODULE VerifiedProfileAdmission ---------------------
EXTENDS Integers, TLC

CONSTANT Dependencies
VARIABLE snapshot, release, decision
vars == <<snapshot, release, decision>>

\* Point-in-time abstraction of release consumption followed by TCB admission.
\* Target version/config values are normalized to 0; 1 means
\* mismatch, not a real version or hash. complete abstracts exact, unique,
\* nonempty IDs and closed shapes; hashes abstracts syntactic validity only.
\* Bounds: two dependencies, match/mismatch, clock/observation -1..3, max age 1;
\* four Boolean release identity/receipt projections and two image-path bits.
\* imageMatches projects exact binary/manifest bytes and OCI labels; this does
\* not prove Docker/signatures or substitute a second generated manifest.
\* Release consumers require
\* exact artifact, requested profile, completed strict execution, and source /
\* ledger / proof identities. This models the release flags enabled, not local
\* diagnostic validation. Authenticated CI transport/signatures remain TCB.
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

Releases == [artifactMatches : BOOLEAN, profileMatches : BOOLEAN,
             proofComplete : BOOLEAN, sourceMatches : BOOLEAN,
             imageRequired : BOOLEAN, imageMatches : BOOLEAN]

Init == /\ snapshot \in Snapshots
        /\ release \in Releases
        /\ decision = "ReleasePending"

ReleaseAdmissible == /\ release.artifactMatches
                     /\ release.profileMatches
                     /\ release.proofComplete
                     /\ release.sourceMatches

ValidateRelease == /\ decision = "ReleasePending"
                   /\ decision' = IF ReleaseAdmissible THEN "ReleaseAccepted" ELSE "ReleaseRejected"
                   /\ UNCHANGED <<snapshot, release>>

\* I1 canonical archive path skips image validation, not release validation.
\* I2 either image build/reuse path needs the same exact payload comparison.
\* I3 mismatch/missing payload rejects before any TCB/profile admission.
ValidatePayload == /\ decision = "ReleaseAccepted"
                   /\ decision' = IF ~release.imageRequired \/ release.imageMatches
                                   THEN "PayloadAccepted" ELSE "PayloadRejected"
                   /\ UNCHANGED <<snapshot, release>>

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

Decide == /\ decision = "PayloadAccepted"
          /\ decision' = IF Admissible THEN "Accepted" ELSE "Rejected"
          /\ UNCHANGED <<snapshot, release>>
Next == ValidateRelease \/ ValidatePayload \/ Decide \/ UNCHANGED vars

TypeOK == /\ snapshot \in Snapshots
          /\ release \in Releases
          /\ decision \in {"ReleasePending", "ReleaseAccepted", "ReleaseRejected", "PayloadAccepted", "PayloadRejected", "Accepted", "Rejected"}
ReleaseConsumptionHasExactExecutedEvidence == decision \in {"ReleaseAccepted", "PayloadAccepted", "PayloadRejected", "Accepted", "Rejected"} =>
    /\ release.artifactMatches /\ release.profileMatches
    /\ release.proofComplete /\ release.sourceMatches
ImageConsumptionHasExactPayload == decision \in {"PayloadAccepted", "Accepted", "Rejected"} =>
    ~release.imageRequired \/ release.imageMatches
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
