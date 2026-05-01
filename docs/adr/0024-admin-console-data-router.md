# ADR-0024: Admin Console Data Router Migration

- **Status**: Accepted
- **Date**: 2026-05-02
- **Depends on**: ADR-0023

## Context

PR #154 introduced `apps/admin-console/src/components/unsaved-changes-guard.tsx`
to intercept navigation away from the agent editor when the form has
unsaved changes. The guard uses `useBlocker` (react-router v7), which only
operates under a *data router* context; mounting the same application under
the legacy `<BrowserRouter>` causes react-router to throw `useBlocker must
be used within a data router` at runtime, taking the entire
`AgentEditorPage` down with it.

Until that PR the entrypoint in `apps/admin-console/src/main.tsx` rendered
`<BrowserRouter><App /></BrowserRouter>`, a pattern that predates the v7
data-router API. Unit tests, type checking, and `npm run build` all passed
because the invariant is thrown at runtime, not at compile time. The
regression was first observed when the new Playwright suite that exercises
the unsaved-changes guard timed out waiting for `getByLabel('Agent ID')`;
the editor never rendered because the hook crashed during component mount.

## Decisions

### D1: Bootstrap the runtime entrypoint with `createBrowserRouter` + `RouterProvider`

`apps/admin-console/src/main.tsx` no longer uses `<BrowserRouter>`. Instead,
it imports a `router` constant from `apps/admin-console/src/app.tsx` and
renders `<RouterProvider router={router} />`. The `router` is constructed once
at module load:

```ts
export const router = createBrowserRouter(createRoutesFromElements(appRoutes()));
```

The legacy `App` export in `apps/admin-console/src/app.tsx` is kept intact:
it renders `<Routes>{appRoutes()}</Routes>` and is available to callers that
control their own router context (e.g., embed the component inside a larger
application that already provides a data router).

### D2: Export `appRoutes()` so tests can build an equivalent memory router

The route tree is extracted into a named export `appRoutes()` in
`apps/admin-console/src/app.tsx`. The smoke-test suite builds a
`createMemoryRouter` from the same elements:

```ts
createMemoryRouter(createRoutesFromElements(appRoutes()), { initialEntries: [path] })
```

This ensures tests exercise exactly the route tree that runs in production,
including any loaders or actions attached to individual routes, without
spawning a real browser.

### D3: Provider order — `ToastProvider > ConfirmDialogProvider > AuthProvider > RouterProvider`

Providers that supply UI context (`ToastProvider`, `ConfirmDialogProvider`)
and the authentication context (`AuthProvider`) are placed *outside*
`RouterProvider`. This means route components and any data-router loaders
or actions can call hooks from those providers without additional wrapper
layers. `RouterProvider` is the innermost shell so that all navigation state
is contained within an already-initialised application context.

### D4: Test policy — every new top-level route requires a smoke test

The smoke-test suite at `apps/admin-console/src/app.smoke.test.tsx`
(jsdom environment, Vitest) is the canonical CI guardrail against future
v7-only-hook regressions. Every new top-level route added to `appRoutes()`
must have at least one smoke-test case that mounts it under a
`createMemoryRouter` and asserts the route renders without throwing. This
prevents a silent rollback to `<BrowserRouter>`-only semantics — and any
analogous "unit tests pass but runtime crashes" failure mode — from going
undetected.

## Alternatives Considered

**Manual navigation blocking via `history.block` or a custom `usePrompt`.**
This avoids the data-router requirement but re-implements browser navigation
interception by hand, including the `beforeunload` edge case that
`useBlocker` already handles. Rejected because it trades a one-line
invariant error for an ongoing maintenance surface.

**`unstable_usePrompt` from react-router.**
Available under `<BrowserRouter>` in some react-router versions, but the API
is explicitly marked unstable and is not guaranteed to survive minor version
bumps. Rejected in favour of the stable `useBlocker` API introduced in
react-router v6.8 and carried into v7.

## Consequences

- All stable react-router v7 data-router APIs (`useBlocker`, `useNavigation`,
  route-level `loader`/`action`) are available to every route component
  without additional wrapper changes.
- The `<App>` export retains the legacy `<Routes>`-based rendering for
  callers that embed the console inside a host application; this export
  continues to work as long as the host provides any router context that
  satisfies the `<Routes>` contract.
- Developers accustomed to the `<BrowserRouter>` + `<Routes>` pattern will
  encounter `createBrowserRouter` as the entry point;
  `apps/admin-console/src/app.smoke.test.tsx` demonstrates the
  `createMemoryRouter` equivalent and serves as a worked example.
