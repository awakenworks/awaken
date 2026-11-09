# Portable Managed Session projection package

The projection package remains the single owner of committed-event merging and
browser Runtime projection. Portable packaging changes import and distribution
boundaries, not the execution reducers. Native Session and ThreadCommit facts
remain authoritative; an accepted tool reply is not a completed tool execution.

## Contracts and cases

| Contract | Validation |
| --- | --- |
| Processing observation | A correctly typed native receipt with `processed_at: null` stays resolving; official event kinds and payload fields remain closed |
| Shared execution semantics | Existing decision-table tests retain exact tool identities, immutable-event conflict refusal, monotonic processing enrichment and aggregate/event admission |
| One SDK authority | Artifact types use the exact current official SDK selected by the oracle; emitted event catalogs retain the oracle's canonical generated content |
| Closed production imports | Static, dynamic and declaration-only imports cannot retain workspace, sibling, oracle or Node I/O dependencies; generated provenance comments remain intact |
| Platform-independent bytes | CRLF and LF compiler and source inputs produce identical portable files and provenance; literal escapes and generated comments remain unchanged |
| Gzip envelope | Qualified Linux and Windows Node 22 artifacts use the RFC 1952 unknown OS marker; compressed payload, CRC, source files and strict admission integrity remain unchanged |
| Standalone distribution | An actual tarball installs offline into an external consumer, resolves inside that consumer, imports and executes without the source checkout or oracle package |
| Clean source entry | Package type checking first builds the selected oracle and exported declarations; the root check requires no preexisting `dist` |
| Public declarations | The external consumer runs strict TypeScript checking against the installed package, accepts a native null stamp and rejects an invented aggregate status |
| No publication side effect | The artifact is private, has no lifecycle scripts and contains no machine path or credentials; packing does not publish it |
| Owned cleanup | Output must be outside the checkout; temporary cleanup stays within its allocated temporary prefix |

`src/index.test.ts` exercises the projection contracts.
`test/portable-package.test.mjs` exercises the artifact boundary and the real
external consumer. Tests require the pinned package manager. The external
consumer first resolves the exact selected public SDK, TypeScript and Node type
dependencies using cache when available. It then removes only its own installed
modules and performs an offline reinstall with the resulting frozen lockfile.
A missing dependency or unavailable registry fails qualification; it never
becomes a skipped or successful result.

The `pack:portable` package script builds the oracle and projection, verifies the
JavaScript oracle catalog, and writes a versioned tarball to `--out-dir` outside
the checkout. Its production types use the selected SDK directly. The development
workspace continues using the oracle; no second SDK version selector is authored.
The artifact records a SHA-256 digest of canonical UTF-8 source inputs, compiler configuration, lockfile and packaged documentation, with no local paths. Portable text uses LF on every host.

These checks qualify package consumption and event projection. They do not
qualify native IAM authority, a product's tool renderer, business command
confirmation, or an external registry release. Products must retain the original
tool call and reply family, and obtain business outcomes from their business owner.

The package envelope uses [RFC 1952](https://www.rfc-editor.org/rfc/rfc1952.html)
section 2.3.1.2 default OS metadata. Unexpected header flags or timestamps are
refused rather than rewriting a header checksum or weakening integrity checks.
