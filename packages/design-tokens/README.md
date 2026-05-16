## Awaken Design Tokens (`@awaken/design-tokens`)

W3C Design Tokens Community Group (DTCG) compliant token source. Built with
Style Dictionary v4 into `packages/design-tokens/dist/css/`, consumed via the
package `exports` field by `awaken-admin-console`, `@awaken/www`, and any
future Awaken surface.

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
`os` → `aw`). A vitest under `tokens.parity.test.ts` diffs the shared subtrees on every CI
run; if teams or Awaken edits one of them and forgets to push the change to
the other, CI fails.

The Awaken-specific files are *forks* — same DTCG schema, different
brand values. Awaken uses warm-leaning slate neutrals + indigo accent
(per the design memo's "Lucid Control" stance). Teams uses Vercel-style
monochrome (black accent, hex neutrals).

### Build

From the repo root:

```sh
pnpm tokens:validate   # structural DTCG check
pnpm tokens:build      # one-shot, also runs implicitly via consumer predev/prebuild
pnpm tokens:dev        # watch mode (re-emits on JSON edit)
```

Outputs (consumed via package `exports`):

```
dist/css/
├── tokens.css           :root + [data-theme="light"]
├── tokens-dark.css      [data-theme="dark"]
├── tokens-auto-dark.css @media (prefers-color-scheme: dark)
└── tokens.json          DTCG round-trip (Figma / Storybook)
```

Consumers import via bare specifiers, e.g.

```css
@import "@awaken/design-tokens/css/tokens.css";
@import "@awaken/design-tokens/css/tokens-dark.css";
@import "@awaken/design-tokens/css/tokens-auto-dark.css";
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

Today: teams and Awaken are independent monorepos with no shared package
spanning both. Cross-repo coordination has a cost; we paid it once (this
fork) and let CI catch drift on the small set of values that genuinely
should match. Within Awaken the tokens are now a true shared workspace
package (`@awaken/design-tokens`) so every internal surface stays aligned
by construction.
