--------------------------- MODULE S3Reachability ----------------------------
EXTENDS Naturals, FiniteSets

CONSTANTS MaxPublications, NoRef
Roles == {"raw", "snapshot", "fork", "nested", "effective", "live", "archive", "pin"}

Body(g) == [kind |-> "body", generation |-> g, body |-> g]
View(g, body) == [kind |-> "view", generation |-> g, body |-> body.generation]
Blob(g) == [kind |-> "blob", generation |-> g, body |-> g]
Frame(g) == [kind |-> "frame", generation |-> g, body |-> g]

VARIABLES head, objects, writer, collector

vars == <<head, objects, writer, collector>>
IdleWriter == [phase |-> "idle", generation |-> 0, version |-> 0]
IdleCollector == [phase |-> "idle", cutoff |-> 0, kept |-> {}]

Init ==
    /\ head = [version |-> 0, gc |-> 0, roots |-> [role \in Roles |->
         CASE role = "raw" -> Body(0)
           [] role = "live" -> Frame(0)
           [] OTHER -> NoRef]]
    /\ objects = {Body(0), Frame(0)}
    /\ writer = IdleWriter
    /\ collector = IdleCollector

Direct(roots) == {roots[r] : r \in Roles} \ {NoRef}
Reachable(roots) ==
    Direct(roots) \union {Body(v.body) : v \in {o \in Direct(roots) : o.kind = "view"}}

\* Repacking changes the physical key of the same logical transcript body.
\* Frozen views keep the original key. View and stream transformations below
\* describe successful head publications, with their referenced bytes staged.
Repack ==
    /\ head.roots["raw"] /= Body(head.gc)
    /\ objects' = objects \union {Body(head.gc)}
    /\ head' = [head EXCEPT !.version = @ + 1, !.roots["raw"] = Body(head.gc)]
    /\ UNCHANGED <<writer, collector>>

Freeze ==
    LET view == View(head.gc, head.roots["raw"])
    IN /\ head.roots["snapshot"] = NoRef
       /\ objects' = objects \union {view}
       /\ head' = [head EXCEPT !.version = @ + 1, !.roots["snapshot"] = view]
       /\ UNCHANGED <<writer, collector>>

CopyView(from, to) ==
    /\ head.roots[from] /= NoRef
    /\ head.roots[to] = NoRef
    /\ head' = [head EXCEPT !.version = @ + 1, !.roots[to] = head.roots[from]]
    /\ UNCHANGED <<objects, writer, collector>>

ReleaseSnapshot ==
    /\ head.roots["snapshot"] /= NoRef
    /\ head' = [head EXCEPT !.version = @ + 1, !.roots["snapshot"] = NoRef]
    /\ UNCHANGED <<objects, writer, collector>>

ArchiveStream ==
    /\ head.roots["live"] /= NoRef
    /\ head' = [head EXCEPT !.version = @ + 1, !.roots["archive"] = head.roots["live"],
                           !.roots["live"] = NoRef]
    /\ UNCHANGED <<objects, writer, collector>>

\* Upload may finish after a seal. Pin admission then reads a fresh head and
\* permits an old generation only when the same blob is already pinned.
BeginUpload ==
    /\ writer.phase = "idle"
    /\ writer' = [IdleWriter EXCEPT !.phase = "uploading", !.generation = head.gc]
    /\ UNCHANGED <<head, objects, collector>>

StoreUpload ==
    /\ writer.phase = "uploading"
    /\ objects' = objects \union {Blob(writer.generation)}
    /\ writer' = [writer EXCEPT !.phase = "staged"]
    /\ UNCHANGED <<head, collector>>

PreparePin ==
    /\ writer.phase = "staged"
    /\ Blob(writer.generation) \in objects
    /\ writer.generation = head.gc \/ head.roots["pin"] = Blob(writer.generation)
    /\ writer' = [writer EXCEPT !.phase = "prepared", !.version = head.version]
    /\ UNCHANGED <<head, objects, collector>>

PublishPin ==
    /\ writer.phase = "prepared"
    /\ writer.version = head.version
    /\ head' = [head EXCEPT !.version = @ + 1, !.roots["pin"] = Blob(writer.generation)]
    /\ writer' = IdleWriter
    /\ UNCHANGED <<objects, collector>>

Abandon ==
    /\ writer.phase /= "idle"
    /\ writer' = IdleWriter
    /\ UNCHANGED <<head, objects, collector>>

Seal ==
    /\ collector.phase = "idle"
    /\ head.gc < 2
    /\ collector' = [IdleCollector EXCEPT !.phase = "sealed", !.cutoff = head.gc]
    /\ head' = [head EXCEPT !.version = @ + 1, !.gc = @ + 1]
    /\ UNCHANGED <<objects, writer>>

Mark ==
    /\ collector.phase = "sealed"
    /\ collector' = [collector EXCEPT !.phase = "marked", !.kept = Reachable(head.roots)]
    /\ UNCHANGED <<head, objects, writer>>

Delete(o) ==
    /\ collector.phase = "marked"
    /\ o.generation <= collector.cutoff
    /\ o \notin collector.kept
    /\ objects' = objects \ {o}
    /\ UNCHANGED <<head, writer, collector>>

Finish ==
    /\ collector.phase = "marked"
    /\ collector' = IdleCollector
    /\ UNCHANGED <<head, objects, writer>>

Next ==
    \/ /\ head.version < MaxPublications
       /\ (Repack \/ Freeze \/ ReleaseSnapshot \/ ArchiveStream \/ PublishPin \/ Seal
           \/ CopyView("snapshot", "fork") \/ CopyView("fork", "nested")
           \/ CopyView("snapshot", "effective"))
    \/ BeginUpload \/ StoreUpload \/ PreparePin \/ Abandon \/ Mark \/ Finish
    \/ \E o \in objects : Delete(o)

Spec == Init /\ [][Next]_vars

LiveReferencesExist == Reachable(head.roots) \subseteq objects
SealedReferencesStayMarked ==
    collector.phase = "marked" =>
        {o \in Reachable(head.roots) : o.generation <= collector.cutoff}
            \subseteq collector.kept
NestedForkIsFrozen ==
    head.roots["nested"] /= NoRef => head.roots["nested"] = head.roots["fork"]

NoSealDuringPreparedPin ==
    ~(writer.phase = "prepared" /\ writer.generation < head.gc)
NoNestedForkAfterRelease ==
    ~(head.roots["nested"] /= NoRef /\ head.roots["snapshot"] = NoRef /\
      head.roots["nested"].body /= head.roots["raw"].generation)

(* Cold reads and pin outcomes captured around the real sweep barrier. *)
SealedCollectionRefinement(trace) ==
    /\ trace.old_generation = trace.cutoff
    /\ trace.fresh_generation = trace.cutoff + 1
    /\ trace.deleted > 0
    /\ trace.old_read_before
    /\ trace.sealed_pin_rejected
    /\ trace.fresh_pin_admitted
    /\ trace.old_read_after_failed
    /\ trace.fresh_read_after

=============================================================================
