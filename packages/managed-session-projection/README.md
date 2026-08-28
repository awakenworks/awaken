# Managed Session projection helpers

`@awaken/managed-session-projection` contains the product-neutral browser rules
shared by Awaken reference products:

- merge committed Session events by event ID, allowing only monotonic
  `processed_at` enrichment and rejecting immutable conflicts;
- fold lifecycle, tool-reply, and accepted-but-unprocessed inbound facts into
  one browser Runtime projection and conservatively join it with the aggregate
  Session status for input admission;
- keep `event_start` and `event_delta` data in a volatile live preview;
- clear that preview when the corresponding committed event or a terminal fact
  arrives;
- prevent volatile stream envelopes from entering durable product projections.

The current official SDK selected by `@awaken/managed-sdk-oracle` remains the
wire-type and generated event-catalog authority. This package introduces no SDK
version pin or hand-maintained event list. It does not interpret product tools,
open SSE connections, persist events, or own durable Session state. Its phases
are a product-neutral browser projection of official facts; Session/ThreadCommit
remain the execution authority. Pilot and Harness keep domain behavior and
application orchestration in their own layers.

The package is private while Awaken `1.0.0-dev` is consumed from a sibling
checkout. Reference products use a `file:` dependency on this directory. A
published release should replace that development link with the matching
versioned package; it should not vendor another copy of these reducers.

Run its focused gates from the Awaken repository root:

```sh
pnpm --filter @awaken/managed-session-projection test
pnpm --filter @awaken/managed-session-projection typecheck
```
