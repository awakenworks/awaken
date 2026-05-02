import { useMemo } from "react";
import { useSearchParams } from "react-router";
import {
  DEFAULT_PAGE_SIZE,
  PAGE_SIZE_OPTIONS,
  type PageSize,
  type SortState,
} from "./list-view";
import {
  DEFAULT_SKILLS_FILTER,
  type ContextFilter,
  type InvocableFilter,
  type SkillsFilterState,
} from "./skills-filter";
import {
  DEFAULT_FIXTURE_FILTER,
  type FixtureFilterState,
  type FixtureStatusFilter,
} from "./eval-reports-filter";
import { type AuditAction } from "./audit-log";

// ---------------------------------------------------------------------------
// Generic list state (search + sort + pagination)
// ---------------------------------------------------------------------------

export interface ListState<TKey extends string> {
  search: string;
  sort: SortState<TKey> | null;
  pageSize: PageSize;
  page: number;
}

export interface ReadListStateOptions<TKey extends string> {
  /// Allowed sort keys (rejected if URL has unknown key)
  validSortKeys: readonly TKey[];
  /// Default sort (used when URL omits or value is invalid)
  defaultSort?: SortState<TKey> | null;
  /// Default page size
  defaultPageSize?: PageSize;
}

export function readListState<TKey extends string>(
  params: URLSearchParams,
  options: ReadListStateOptions<TKey>,
): ListState<TKey> {
  const defaultSort = options.defaultSort ?? null;
  const defaultPageSize = options.defaultPageSize ?? DEFAULT_PAGE_SIZE;

  // search
  const search = params.get("q") ?? "";

  // sort
  const sortKeyRaw = params.get("sort");
  const dirRaw = params.get("dir");
  let sort: SortState<TKey> | null = defaultSort;
  if (sortKeyRaw !== null) {
    const key = sortKeyRaw as TKey;
    if (options.validSortKeys.includes(key)) {
      const direction = dirRaw === "desc" ? "desc" : "asc";
      sort = { key, direction };
    }
    // unknown key → fall back to default (no-op, already set above)
  }

  // pageSize
  const sizeRaw = params.get("size");
  let pageSize: PageSize = defaultPageSize;
  if (sizeRaw !== null) {
    const parsed = Number(sizeRaw);
    const matched = PAGE_SIZE_OPTIONS.find((s) => s === parsed);
    if (matched !== undefined) {
      pageSize = matched;
    }
    // invalid → keep default
  }

  // page (1-based)
  const pageRaw = params.get("page");
  let page = 1;
  if (pageRaw !== null) {
    const parsed = Number(pageRaw);
    if (Number.isInteger(parsed) && parsed >= 1) {
      page = parsed;
    }
    // invalid → clamp to 1
  }

  return { search, sort, pageSize, page };
}

export function writeListState<TKey extends string>(
  params: URLSearchParams,
  state: ListState<TKey>,
  options: ReadListStateOptions<TKey>,
): URLSearchParams {
  const defaultSort = options.defaultSort ?? null;
  const defaultPageSize = options.defaultPageSize ?? DEFAULT_PAGE_SIZE;
  const next = new URLSearchParams(params.toString());

  // search: omit when empty
  if (state.search.length > 0) {
    next.set("q", state.search);
  } else {
    next.delete("q");
  }

  // sort: omit when matches default
  const sortMatchesDefault =
    defaultSort === null
      ? state.sort === null
      : state.sort !== null &&
        state.sort.key === defaultSort.key &&
        state.sort.direction === defaultSort.direction;

  if (state.sort !== null && !sortMatchesDefault) {
    next.set("sort", state.sort.key);
    if (state.sort.direction === "desc") {
      next.set("dir", "desc");
    } else {
      next.delete("dir");
    }
  } else if (state.sort === null) {
    next.delete("sort");
    next.delete("dir");
  } else {
    // sort matches default — omit
    next.delete("sort");
    next.delete("dir");
  }

  // pageSize: omit when default
  if (state.pageSize !== defaultPageSize) {
    next.set("size", String(state.pageSize));
  } else {
    next.delete("size");
  }

  // page: omit when 1
  if (state.page > 1) {
    next.set("page", String(state.page));
  } else {
    next.delete("page");
  }

  return next;
}

// ---------------------------------------------------------------------------
// Skills filter URL helpers
// ---------------------------------------------------------------------------

const VALID_INVOCABLE: readonly InvocableFilter[] = [
  "any",
  "user",
  "model",
  "internal",
];
const VALID_CONTEXT: readonly ContextFilter[] = ["any", "inline", "fork"];

export function readSkillsFilter(params: URLSearchParams): SkillsFilterState {
  const search = params.get("q") ?? DEFAULT_SKILLS_FILTER.search;

  const invocableRaw = params.get("caller");
  const invocable: InvocableFilter =
    invocableRaw !== null &&
    VALID_INVOCABLE.includes(invocableRaw as InvocableFilter)
      ? (invocableRaw as InvocableFilter)
      : DEFAULT_SKILLS_FILTER.invocable;

  const contextRaw = params.get("ctx");
  const context: ContextFilter =
    contextRaw !== null && VALID_CONTEXT.includes(contextRaw as ContextFilter)
      ? (contextRaw as ContextFilter)
      : DEFAULT_SKILLS_FILTER.context;

  return { search, invocable, context };
}

export function writeSkillsFilter(
  params: URLSearchParams,
  state: SkillsFilterState,
): URLSearchParams {
  const next = new URLSearchParams(params.toString());

  if (state.search.length > 0) {
    next.set("q", state.search);
  } else {
    next.delete("q");
  }

  if (state.invocable !== DEFAULT_SKILLS_FILTER.invocable) {
    next.set("caller", state.invocable);
  } else {
    next.delete("caller");
  }

  if (state.context !== DEFAULT_SKILLS_FILTER.context) {
    next.set("ctx", state.context);
  } else {
    next.delete("ctx");
  }

  return next;
}

// ---------------------------------------------------------------------------
// Fixture (eval reports) filter URL helpers
// ---------------------------------------------------------------------------

const VALID_STATUS: readonly FixtureStatusFilter[] = [
  "all",
  "passed",
  "failed",
  "regressions",
  "fixed",
];

export function readFixtureFilter(
  params: URLSearchParams,
): FixtureFilterState {
  const search = params.get("q") ?? DEFAULT_FIXTURE_FILTER.search;

  const statusRaw = params.get("status");
  const status: FixtureStatusFilter =
    statusRaw !== null &&
    VALID_STATUS.includes(statusRaw as FixtureStatusFilter)
      ? (statusRaw as FixtureStatusFilter)
      : DEFAULT_FIXTURE_FILTER.status;

  return { search, status };
}

export function writeFixtureFilter(
  params: URLSearchParams,
  state: FixtureFilterState,
): URLSearchParams {
  const next = new URLSearchParams(params.toString());

  if (state.search.length > 0) {
    next.set("q", state.search);
  } else {
    next.delete("q");
  }

  if (state.status !== DEFAULT_FIXTURE_FILTER.status) {
    next.set("status", state.status);
  } else {
    next.delete("status");
  }

  return next;
}

// ---------------------------------------------------------------------------
// React hooks — URL glue
// ---------------------------------------------------------------------------

export interface ListUrlState<TKey extends string> extends ListState<TKey> {
  apply: (patch: Partial<ListState<TKey>>) => void;
}

export function useListUrlState<TKey extends string>(
  options: ReadListStateOptions<TKey>,
): ListUrlState<TKey> {
  const [searchParams, setSearchParams] = useSearchParams();

  const { search, sort, pageSize, page } = useMemo(
    () => readListState(searchParams, options),
    // options is a module-level constant on every call site; including it
    // would break memoisation if callers passed an inline object.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [searchParams],
  );

  function apply(patch: Partial<ListState<TKey>>) {
    setSearchParams(
      (prev) => {
        const current = readListState(prev, options);
        return writeListState(prev, { ...current, ...patch }, options);
      },
      { replace: true },
    );
  }

  return { search, sort, pageSize, page, apply };
}

export interface SkillsFilterUrlState extends SkillsFilterState {
  apply: (patch: Partial<SkillsFilterState>) => void;
}

export function useSkillsFilterUrlState(): SkillsFilterUrlState {
  const [searchParams, setSearchParams] = useSearchParams();

  const filter = useMemo(
    () => readSkillsFilter(searchParams),
    [searchParams],
  );

  function apply(patch: Partial<SkillsFilterState>) {
    setSearchParams(
      (prev) => writeSkillsFilter(prev, { ...readSkillsFilter(prev), ...patch }),
      { replace: true },
    );
  }

  return { ...filter, apply };
}

export interface FixtureFilterUrlState extends FixtureFilterState {
  apply: (patch: Partial<FixtureFilterState>) => void;
}

export function useFixtureFilterUrlState(): FixtureFilterUrlState {
  const [searchParams, setSearchParams] = useSearchParams();

  const filter = useMemo(
    () => readFixtureFilter(searchParams),
    [searchParams],
  );

  function apply(patch: Partial<FixtureFilterState>) {
    setSearchParams(
      (prev) =>
        writeFixtureFilter(prev, { ...readFixtureFilter(prev), ...patch }),
      { replace: true },
    );
  }

  return { ...filter, apply };
}

// ---------------------------------------------------------------------------
// Audit log filter URL helpers
// ---------------------------------------------------------------------------

export interface AuditFilterState {
  since: string;
  until: string;
  action: AuditAction | "";
  resource: string;
  actor: string;
}

export const DEFAULT_AUDIT_FILTER: AuditFilterState = {
  since: "",
  until: "",
  action: "",
  resource: "",
  actor: "",
};

const VALID_AUDIT_ACTIONS: readonly AuditAction[] = [
  "create",
  "update",
  "delete",
  "restart",
  "publish",
];

export function readAuditFilter(params: URLSearchParams): AuditFilterState {
  const since = params.get("since") ?? DEFAULT_AUDIT_FILTER.since;
  const until = params.get("until") ?? DEFAULT_AUDIT_FILTER.until;
  const resource = params.get("resource") ?? DEFAULT_AUDIT_FILTER.resource;
  const actor = params.get("actor") ?? DEFAULT_AUDIT_FILTER.actor;

  const actionRaw = params.get("action");
  const action: AuditAction | "" =
    actionRaw !== null && VALID_AUDIT_ACTIONS.includes(actionRaw as AuditAction)
      ? (actionRaw as AuditAction)
      : DEFAULT_AUDIT_FILTER.action;

  return { since, until, action, resource, actor };
}

export function writeAuditFilter(
  params: URLSearchParams,
  state: AuditFilterState,
): URLSearchParams {
  const next = new URLSearchParams(params.toString());

  if (state.since) {
    next.set("since", state.since);
  } else {
    next.delete("since");
  }

  if (state.until) {
    next.set("until", state.until);
  } else {
    next.delete("until");
  }

  if (state.action) {
    next.set("action", state.action);
  } else {
    next.delete("action");
  }

  if (state.resource) {
    next.set("resource", state.resource);
  } else {
    next.delete("resource");
  }

  if (state.actor) {
    next.set("actor", state.actor);
  } else {
    next.delete("actor");
  }

  return next;
}

export interface AuditFilterUrlState extends AuditFilterState {
  apply: (patch: Partial<AuditFilterState>) => void;
}

export function useAuditFilterUrlState(): AuditFilterUrlState {
  const [searchParams, setSearchParams] = useSearchParams();

  const filter = useMemo(() => readAuditFilter(searchParams), [searchParams]);

  function apply(patch: Partial<AuditFilterState>) {
    setSearchParams(
      (prev) => writeAuditFilter(prev, { ...readAuditFilter(prev), ...patch }),
      { replace: true },
    );
  }

  return { ...filter, apply };
}
