# Vendored Awaken UI distributable

This directory contains the generated `dist` output from
`awakenworks/awaken-ui@ac82353cd323ee9ea816d93ca6c47544141564d3` (`@awaken/ui` 0.2.0).

It is vendored because that source package's `prepack` boundary check resolves file URLs as
filesystem paths and cannot install on Windows. The distributable was produced with `pnpm build`;
source maps are omitted. Replace this package with a published or Windows-portable upstream package
when one is available.

Cause graph: a clean install plus a Git-hosted UI dependency triggers upstream `prepack`; on Windows
its invalid path prevents `dist` from being produced. A pinned, already-built local package removes
that platform-dependent effect while keeping the exact upstream revision auditable.

Decision table: clean/existing install × Windows/non-Windows always resolves this local package; an
unknown upstream revision is never selected implicitly.
