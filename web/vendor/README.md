# Shared UI dependency artifact

`awaken-ui-0.4.0.tgz` is the complete built npm package from
`AwakenWorks/awaken-ui@77dd44fa6ba2da8cad596ddee2de09c0fb13867a`.

- Package: `@awaken/ui@0.4.0`
- SHA-256: `10bbdbc104210e66265a6b69e842a65d7baf7eeda2c3a579130aceca9e4602b6`
- Build: `pnpm pack --out /absolute/path/awaken-ui-0.4.0.tgz` after `pnpm check`
- Installation: the sole `@awaken/ui` dependency in the parent package manifest;
  the pnpm lockfile binds the archive integrity.

This is a local integration artifact, not a published registry release. It contains
no product adapter or hand-patched module. UI source and tests remain authoritative
in awaken-ui; regenerate the entire package from one committed tree when updating.
Agents, Workforce and Cloud use this same archive. The archive is a versioned
third-party dependency, not this application's build output or a copied source tree.
Standard pnpm packing removes the packageManager and prepack manifest entries.
