------------------------ MODULE WebhookSubscription ------------------------
EXTENDS Naturals, FiniteSets, TLC

\* One subscription id, one owning Workspace, and one durable mutation intent.
\* Row and intent are the WebhookStore facts; materials model SecretStore refs.
VARIABLES kind, requestOwner, rowRef, materials, intent, phase, processUp

vars == <<kind, requestOwner, rowRef, materials, intent, phase, processUp>>

Kinds == {"create", "rotate", "delete"}
Refs == {"old", "new"}
Before(k) == IF k = "create" THEN "none" ELSE "old"
After(k) == IF k = "delete" THEN "none" ELSE "new"

\* A new id gets its owner from the edge-stamped Workspace. A foreign caller
\* exists only when the id already belongs to another Workspace.
Init == \E k \in Kinds, caller \in {"owner", "foreign"} :
    /\ (k = "create" => caller = "owner")
    /\ kind = k
    /\ requestOwner = caller
    /\ rowRef = Before(k)
    /\ materials = IF Before(k) = "none" THEN {} ELSE {"old"}
    /\ intent = FALSE
    /\ phase = "idle"
    /\ processUp = TRUE

\* The router's row-owner fence is checked before it begins a material intent.
Begin ==
    /\ processUp /\ phase = "idle" /\ requestOwner = "owner"
    /\ intent' = TRUE
    /\ phase' = "begun"
    /\ UNCHANGED <<kind, requestOwner, rowRef, materials, processUp>>

PutNewMaterial ==
    /\ processUp /\ phase = "begun" /\ After(kind) # "none"
    /\ materials' = materials \cup {After(kind)}
    /\ phase' = "material_written"
    /\ UNCHANGED <<kind, requestOwner, rowRef, intent, processUp>>

\* Apply is the WebhookStore's exact before/after mutation; a visible row is
\* never committed before its new signing material is present.
Apply ==
    /\ processUp /\ intent
    /\ phase \in {"begun", "material_written"}
    /\ (After(kind) = "none" \/ After(kind) \in materials)
    /\ rowRef = Before(kind)
    /\ rowRef' = After(kind)
    /\ phase' = "applied"
    /\ UNCHANGED <<kind, requestOwner, materials, intent, processUp>>

CleanupRetiredMaterial ==
    /\ processUp /\ intent /\ phase = "applied"
    /\ materials' = materials \ {Before(kind)}
    /\ phase' = "cleaned"
    /\ UNCHANGED <<kind, requestOwner, rowRef, intent, processUp>>

Complete ==
    /\ processUp /\ intent /\ phase = "cleaned"
    /\ intent' = FALSE
    /\ phase' = "done"
    /\ UNCHANGED <<kind, requestOwner, rowRef, materials, processUp>>

\* Recovery reads the committed row, not a process-local phase. If the row
\* still equals before, it rolls back new material; if it equals after, it
\* retains new material and retires old material. A third row is outside this
\* one-id model and is rejected by the production adapter's conflict branch.
RecoverBefore ==
    /\ processUp /\ intent /\ rowRef = Before(kind)
    /\ materials' = materials \ {After(kind)}
    /\ intent' = FALSE
    /\ phase' = "done"
    /\ UNCHANGED <<kind, requestOwner, rowRef, processUp>>

RecoverAfter ==
    /\ processUp /\ intent /\ rowRef = After(kind)
    /\ materials' = materials \ {Before(kind)}
    /\ intent' = FALSE
    /\ phase' = "done"
    /\ UNCHANGED <<kind, requestOwner, rowRef, processUp>>

Crash ==
    /\ processUp
    /\ processUp' = FALSE
    /\ UNCHANGED <<kind, requestOwner, rowRef, materials, intent, phase>>

Restart ==
    /\ ~processUp
    /\ processUp' = TRUE
    /\ UNCHANGED <<kind, requestOwner, rowRef, materials, intent, phase>>

Next == Begin \/ PutNewMaterial \/ Apply \/ CleanupRetiredMaterial \/
        Complete \/ RecoverBefore \/ RecoverAfter \/ Crash \/ Restart

TypeOK ==
    /\ kind \in Kinds
    /\ requestOwner \in {"owner", "foreign"}
    /\ rowRef \in Refs \cup {"none"}
    /\ materials \subseteq Refs
    /\ intent \in BOOLEAN /\ processUp \in BOOLEAN
    /\ phase \in {"idle", "begun", "material_written", "applied", "cleaned", "done"}

VisibleRowHasMaterial == rowRef = "none" \/ rowRef \in materials
UnpublishedMaterialHasIntent == \A ref \in materials : ref = rowRef \/ intent
ForeignWorkspaceCannotMutate ==
    requestOwner = "foreign" =>
        (rowRef = Before(kind) /\ ~intent /\ phase = "idle")

Spec == Init /\ [][Next]_vars
=============================================================================
