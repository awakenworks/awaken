## Awaken Admin Console — Design Tokens

W3C Design Tokens Community Group (DTCG) compliant token source. Built with
Style Dictionary v4 into `apps/admin-console/src/styles/generated/`.

### Layers

```
tokens/
├── primitives/       Raw values: scale, palette, type, motion, shadow.
│   ├── colors.json       Awaken-specific (warm slate + indigo + violet)
│   ├── motion.json       ⇆ teams (parity)
│   ├── phase.json        ⇆ teams (parity, indirection-only)
│   ├── radius.json       ⇆ teams (parity)
│   ├── shadows.json      Awaken-specific (OKLCH-alpha shadows)
│   ├── spacing.json      ⇆ teams (parity)
│   └── typography.json   ⇆ teams (parity)
└── semantic/         Theme-aware roles bound to primitives:
    ├── colors-light.json   Awaken-specific
    ├── colors-dark.json    Awaken-specific
    ├── phase-chrome.json   ⇆ teams (parity)
    └── sizing.json         ⇆ teams (parity)
```

The `⇆ teams (parity)` files are byte-for-byte identical to
`~/Codes/teams/web/design-tokens/` (after renaming the top-level namespace
`os` → `aw`). A vitest under `src/__tests__/tokens.parity.test.ts`
diffs the shared subtrees on every CI run; if teams or admin-console
edits one of them and forgets to push the change to the other, CI fails.

The Awaken-specific files are *forks* — same DTCG schema, different
brand values. Awaken uses warm-leaning slate neutrals + indigo accent
(per the design memo's "Lucid Control" stance). Teams uses Vercel-style
monochrome (black accent, hex neutrals).

### Build

```sh
pnpm tokens:validate   # structural DTCG check
pnpm tokens:build      # one-shot, runs as part of pnpm build / pnpm dev
pnpm tokens:dev        # watch mode (re-emits on JSON edit)
```

Outputs (consumed by `src/globals.css`):

```
src/styles/generated/
├── tokens.css           :root + [data-theme="light"]
├── tokens-dark.css      [data-theme="dark"]
├── tokens-auto-dark.css @media (prefers-color-scheme: dark)
└── tokens.json          DTCG round-trip (Figma / Storybook)
```

### Conventions

- Token name == JSON path joined with `-`, prefixed `aw-`. `aw.bg.elevated`
  → `--aw-bg-elevated`.
- Add new tokens by editing JSON. Do not write new `--aw-*` declarations
  by hand in CSS files; they'll drift from the generated source.
- Fork policy: if a token is reasonable for both products, add it to a
  parity file (motion / radius / spacing / typography / phase / sizing /
  phase-chrome) and ping teams. If it's brand-specific, add it to one of
  the Awaken-fork files.

### Why fork instead of depend on teams or share a package?

Today: teams and admin-console are independent monorepos with no shared
package. Cross-repo coordination has a cost; we paid it once (this fork)
and let CI catch drift on the small set of values that genuinely should
match. When a third consumer appears or token-sync becomes a recurring
pain, we'll lift to a shared package (see `docs/superpowers/specs/`
for design memos).
