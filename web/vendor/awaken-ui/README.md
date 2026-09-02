# Vendored Awaken UI distributable

This directory is a composite, generated `@awaken/ui` distributable. It keeps
the historical vendored baseline and the Awaken console overlays in
`data/grid`, `forms/field`, and `styles/components-core.css`; it is not a
byte-for-byte mirror of one upstream tree. The local package version is
`0.3.0-awaken.2`.

The `SuiteSwitcher` navigation export and its generated runtime/type modules
come from the successful `pnpm build` of
`AwakenWorks/awaken-ui@b669c96446e1b1b0f9c3057a4e598765c75ae42e`.

The upstream Windows path defect is fixed in this distributable. It remains vendored
until the same immutable package is available from the internal registry, so clean Release builds
do not depend on SSH credentials or mutable Git state. It was produced with `pnpm build`; source
maps are omitted. The root pnpm workspace links this directory as the sole local
`@awaken/ui` package, so an existing install reads the same audited files instead
of retaining a second copied dependency snapshot.

Cause graph: a clean install plus a Git-hosted dependency requires network and repository credentials
before its `prepack` can produce `dist`. A pinned, already-built local package removes those effects
while keeping the exact upstream revision auditable.

Decision table: clean/existing install × Windows/non-Windows always resolves this local package; an
unknown upstream revision is never selected implicitly. Every relative runtime and type export must
resolve inside this audited distributable before the console build is accepted.
