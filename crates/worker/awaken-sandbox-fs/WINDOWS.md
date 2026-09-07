# Windows filesystem contract

The Windows adapter uses `fs_at` for a native directory-handle-relative
`NtCreateFile` open of each validated leaf. A second `cap-std` open supplies
Windows sharing control. The two opened identities must agree before any
content read, overwrite, or deletion. Every ancestor handle is retained without
delete sharing; regular-file access also excludes concurrent writers. All
reparse points (including junctions) are rejected during traversal. Exact tree
cleanup may delete the reparse point's own handle, never its target.

`winapi-util` obtains the volume serial number, file index, and hard-link count
from the opened handle. Secret shredding rejects multiple links and overwrites
that same verified handle. Missing relative parents or files are idempotent,
but the root must still exist with the expected identity. Permission errors,
reparse points, and identity changes remain errors.

File stages are flushed before publication. `atomicwrites` calls
`MoveFileExW` with `MOVEFILE_WRITE_THROUGH`, and only replacement adds
`MOVEFILE_REPLACE_EXISTING`. There is no destination unlink or existence-based
no-replace emulation. A failed replacement leaves the old marker intact.
Directory publication conflicts leave both the destination and private stage
intact. The caller continues to own stage lifetime, parent-directory mutation
authority and replacement serialization, just as on Unix.

`fs_at::FileExt::delete_by_handle` deletes the inspected object while its
identity is retained, including during recursive cleanup. The adapter does not
introduce unsafe Rust into this workspace.

These primitives are not an OS sandbox: Windows Local execution still runs as
the host user. Windows ACLs, resource policy, and provider lifecycle ownership
remain the caller's responsibility. The tests cover process interruption, not
hardware power-loss guarantees or physical erasure on SSDs.

## Native regression checks

```text
cargo test -p awaken-sandbox-fs --locked
cargo clippy -p awaken-sandbox-fs --all-targets --locked -- -D warnings
cargo test -p awaken-sandbox-local --lib --locked windows_disposal
```

The Windows tests cover failed and interrupted marker replacement, junction
traversal, directory replacement attempts while handles are retained,
hard-linked credentials, an identity swap between opens, absent credentials,
and a competing directory publisher at the rename boundary. Provider tests
exercise disposal completion and retry after a rejected alias is removed.
