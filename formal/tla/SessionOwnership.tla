------------------------ MODULE SessionOwnership ------------------------
EXTENDS SessionRootKernel

\* One arbitrarily selected Session identity; other identities stutter here.
\* This wrapper observes the canonical root, never a second write protocol.
\* readCompleted is an observation pulse, not persisted Session state. It makes
\* a read returning an unchanged/wrong empty tuple testable instead of stutter.
VARIABLES readSnapshot, readCompleted

NoRow == [present |-> FALSE, owner |-> NoOwner, generation |-> 0]
RootRow == IF kExistence = "Live"
           THEN [present |-> TRUE, owner |-> kOwner, generation |-> kRevision]
           ELSE NoRow
OwnershipVars == <<kVars, readSnapshot, readCompleted>>
OwnershipInit == /\ Init
                 /\ readSnapshot = NoRow
                 /\ readCompleted = FALSE
Read == /\ readSnapshot' = RootRow
        /\ readCompleted' = ~readCompleted
        /\ UNCHANGED kVars
OwnershipNext == (Next /\ UNCHANGED <<readSnapshot, readCompleted>>) \/ Read
OwnershipSpec == OwnershipInit /\ [][OwnershipNext]_OwnershipVars

RowIsOwned(row) ==
    /\ row \in [present : BOOLEAN, owner : Owners \cup {NoOwner},
                generation : 0..MaxRevision]
    /\ (row.present => row.owner \in Owners)
    /\ (row.present <=> row.generation > 0)
VisibleRowAlwaysHasOwner == RowIsOwned(RootRow)
ReadSnapshotIsCoherent == RowIsOwned(readSnapshot)
OwnershipSafety == /\ Safety
                   /\ readCompleted \in BOOLEAN
                   /\ VisibleRowAlwaysHasOwner
                   /\ ReadSnapshotIsCoherent

\* Historical snapshots may survive later writes/deletion. Each new observation
\* must capture owner and revision from the same current root.
ReadCapturesOneRootFact ==
    [][readCompleted' # readCompleted => readSnapshot' = RootRow]_OwnershipVars
OwnerNeverRebinds == [][kExistence # "Absent" => kOwner' = kOwner]_OwnershipVars
NoOwnedReadReached == ~readSnapshot.present
NoHistoricalReadAfterTombstone ==
    ~(kExistence = "Tombstoned" /\ readSnapshot.present)
NoConflictingReadReached ==
    ~(kLastCreateOutcome = "Conflict" /\ readSnapshot.present)
=============================================================================
