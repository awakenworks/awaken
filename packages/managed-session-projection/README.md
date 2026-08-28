# Managed Session projection helpers

`@awaken/managed-session-projection` contains the product-neutral browser rules
shared by Awaken reference products:

- merge committed Session events by event ID;
- keep `event_start` and `event_delta` data in a volatile live preview;
- clear that preview when the corresponding committed event or a terminal fact
  arrives;
- prevent volatile stream envelopes from entering durable product projections.

The current official SDK selected by `@awaken/managed-sdk-oracle` remains the
wire-type and generated event-catalog authority. This package introduces no SDK
version pin or hand-maintained event list. It does not interpret product tools,
derive product phases, open SSE connections, or own Session state. Pilot and
Harness keep those responsibilities in their own domain and application layers.

The package is private while Awaken `1.0.0-dev` is consumed from a sibling
checkout. Reference products use a `file:` dependency on this directory. A
published release should replace that development link with the matching
versioned package; it should not vendor another copy of these reducers.

Run its complete gate from the Awaken repository root:

```sh
pnpm check
```
