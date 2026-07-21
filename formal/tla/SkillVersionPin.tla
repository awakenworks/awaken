-------------------------- MODULE SkillVersionPin --------------------------
EXTENDS Integers, FiniteSets, TLC

CONSTANT MaxVersion

VARIABLE latest, last, visible, stored, pinned, loaded, workspaceOk, hashOk

vars == <<latest, last, visible, stored, pinned, loaded, workspaceOk, hashOk>>

SetMax(S) == CHOOSE value \in S : \A candidate \in S : candidate <= value

Init == /\ latest = 1 /\ last = 1
        /\ visible = {1} /\ stored = {1}
        /\ pinned = 0 /\ loaded = 0
        /\ workspaceOk = TRUE /\ hashOk = TRUE

Publish ==
    /\ last < MaxVersion
    /\ last' = last + 1 /\ latest' = last + 1
    /\ visible' = visible \cup {last + 1}
    /\ stored' = stored \cup {last + 1}
    /\ UNCHANGED <<pinned, loaded, workspaceOk, hashOk>>

Resolve ==
    /\ pinned = 0
    /\ pinned' = latest
    /\ UNCHANGED <<latest, last, visible, stored, loaded, workspaceOk, hashOk>>

Retire ==
    /\ Cardinality(visible) > 1
    /\ \E version \in visible :
        /\ visible' = visible \ {version}
        /\ latest' = IF version = latest THEN SetMax(visible \ {version}) ELSE latest
    /\ UNCHANGED <<last, stored, pinned, loaded, workspaceOk, hashOk>>

Restart ==
    /\ loaded' = 0
    /\ UNCHANGED <<latest, last, visible, stored, pinned, workspaceOk, hashOk>>

LoadPinned ==
    /\ pinned # 0 /\ pinned \in stored /\ workspaceOk /\ hashOk
    /\ loaded' = pinned
    /\ UNCHANGED <<latest, last, visible, stored, pinned, workspaceOk, hashOk>>

RejectWorkspace ==
    /\ workspaceOk
    /\ workspaceOk' = FALSE /\ loaded' = 0
    /\ UNCHANGED <<latest, last, visible, stored, pinned, hashOk>>

RejectHash ==
    /\ hashOk
    /\ hashOk' = FALSE /\ loaded' = 0
    /\ UNCHANGED <<latest, last, visible, stored, pinned, workspaceOk>>

Next == Publish \/ Resolve \/ Retire \/ Restart \/ LoadPinned \/
        RejectWorkspace \/ RejectHash

TypeOK == /\ latest \in 1..MaxVersion /\ last \in 1..MaxVersion
          /\ visible \subseteq 1..MaxVersion /\ stored \subseteq 1..MaxVersion
          /\ pinned \in 0..MaxVersion /\ loaded \in 0..MaxVersion
          /\ workspaceOk \in BOOLEAN /\ hashOk \in BOOLEAN
VisibleVersionsRemainStored == visible \subseteq stored
AssignedVersionsNeverDisappear == stored = 1..last
PinnedVersionSurvivesRetirement == pinned = 0 \/ pinned \in stored
LoadedVersionEqualsPin == loaded = 0 \/ loaded = pinned
InvalidScopeOrHashCannotRemainLoaded == (~workspaceOk \/ ~hashOk) => loaded = 0

Spec == Init /\ [][Next]_vars
=============================================================================
