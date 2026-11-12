# File Lifecycle

This document owns logical File creation, byte resolution, deletion replay and
reclamation. It is split from the [Resources lifecycle design](resources-memory-files-skills.md),
which retains the [component catalog](resources-memory-files-skills.md#resource-input-component-catalog)
and the common Session resource lifecycle.

### Configure

The Files API delegates to `FileApplication`; artifacts first pass `ArtifactPublication::verify`
before quota, immutable blob or catalog effects. The application writes the File ownership
reference through the lifecycle repository. `FileStore` computes the BLAKE3
`blob_id`; equal bytes may share that blob while distinct uploads retain distinct
logical `FileId` values. Knowing either id is never authority.

Public multipart upload accepts an optional standard `Idempotency-Key` header.
The key is scoped by Workspace and binds the complete logical effect: immutable
bytes, filename, MIME type, download policy, expiry and scope. An exact replay
returns the original `FileId`; reuse with any changed effect returns `409` and
never creates a second logical File. Omitting the header preserves the Files API
rule that every upload is an independent logical File. Relative
`expires_in_seconds` is deliberately incompatible with an idempotency key because
its wall-clock-derived expiry is not a stable replay fingerprint; clients needing
replayable uploads omit relative expiry. The key remains internal catalog data and
is not projected by list or metadata responses.

Artifact content deduplication and publishing-Run provenance have different
owners. The existing File key deduplicates immutable path/content publication;
`ArtifactCapture.origin` and the capture revision retain the publishing Run's
reference. The harvester binds that reference from its existing Run fence
before accepting a capture receipt. The HTTP boundary rechecks the same
reference against its admitted fence. A peer cannot erase or replace it, and
an unclaimed capture never invents one. No second artifact identity or history
store is derived from provider bytes, which carry no Run authority.

### Bind and resolve

Agent and Session bindings carry the logical `FileId`. Resolution checks the
Workspace ownership edge, current deletion state, mount path, and referenced blob
existence. No File config or version lookup exists.

### Activate and use

The Worker asks `FileContentSource` for the logical File under its current claim.
The Resources adapter resolves `FileCatalog` to `blob_id`, reads `FileStore`, and
both sides validate the immutable digest. The Sandbox provider materializes a
disposable Session working copy;
the copy may be edited, but it has no write-back path to the immutable FileStore
object. Text conversion is forbidden. Edited bytes become a new File or Artifact
only through an explicit publish; the original blob is never overwritten.

The object adapter commits with the provider's atomic create operation. Its S3
adapter recognizes official OSS S3 HTTPS endpoints and configures the existing
SDK's virtual-host addressing and forbid-overwrite conditional-copy support.
Any deployment whose SDK configuration disables conditional PUT must configure
conditional copy instead: upload a uniquely named object inside the allocation's
`.staging` prefix, copy it to the content key only if absent, then delete the
temporary object. The conditional copy is the sole commit point. Replays verify
the existing content; corruption is never overwritten. Copy or cleanup failures
remain indeterminate and retryable. Temporary objects are not Files and are
excluded from File listing; an interrupted process can leave uncommitted staging
objects for provider retention cleanup.

### Release and reclaim

Session release deletes only the sandbox copy. Logical File deletion revokes a
Workspace ownership edge. `FileApplication::delete` retains access to the scoped
tombstone for command replay: it idempotently persists the same purge intent,
marks the logical File deleted, then removes its exact ownership reference.
If reference removal fails after the tombstone commits, another DELETE resumes
that prefix; it cannot use the ordinary active-only read and skip cleanup.
Ordinary get/bytes still deny tombstones, and replay retains the missing-File
response after cleanup.

`ResourcesApplication::purge_preparation` exposes that same File application to
the existing `ResourceReclaimer`. After claiming a durable intent and before
checking guards or acquiring the physical fence, it resumes logical cleanup.
The reference index identifies candidate owners in the intent's Workspace;
the File application re-derives its original intent from each retained File and
requires the complete immutable command identity before replaying `delete`.
It never parses opaque ids or treats a generic blob purge as permission to delete
every File sharing those bytes. A preparation error stays retryable in the same
intent, with no physical effects. The original startup and five-second service
loop both install this preparation; no second queue or scanner exists.

An intent-insert failure leaves no durable command and still needs another
authorized DELETE. Once the intent commits, restart recovery can finish either
the missing tombstone or its reference removal without user redelivery.
Physical blob GC is allowed only when all are false:

```text
Workspace ownership edges
Agent bindings
active/effective Session references
Artifact references
retention or legal hold
```

A shared blob survives removal of one Workspace's ownership edge.

Session File item create/delete is an item-shaped adapter over the canonical
complete-manifest command. The adapter reads the Session root once, derives the
new complete desired manifest, and submits that read revision as the exact root
precondition. It never retries by rebasing on a concurrent winner. Therefore one
racing item mutation commits, the loser returns HTTP `409`, and the winner's
File set cannot be silently overwritten.
