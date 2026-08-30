# Vendored Awaken UI distributable

This directory contains the generated `dist` output from
`awakenworks/awaken-ui@170dd1d9d2309b5bef60611b7a6558ab8abc7905` (`@awaken/ui` 0.3.0).

The upstream Windows path defect is fixed in this revision. The distributable remains vendored
until the same immutable package is available from the internal registry, so clean Release builds
do not depend on SSH credentials or mutable Git state. It was produced with `pnpm build`; source
maps are omitted.

Cause graph: a clean install plus a Git-hosted dependency requires network and repository credentials
before its `prepack` can produce `dist`. A pinned, already-built local package removes those effects
while keeping the exact upstream revision auditable.

Decision table: clean/existing install × Windows/non-Windows always resolves this local package; an
unknown upstream revision is never selected implicitly.
