// URL-persisted list state: search text, sort key/direction, and page live in the
// query string, so a filtered/sorted view is shareable and survives reload/back.

import { useCallback, useMemo } from "react";
import { useSearchParams } from "react-router";

export interface ListState {
  q: string;
  sort: string;
  dir: "asc" | "desc";
  page: number;
  setQ: (q: string) => void;
  setSort: (key: string) => void;
  setPage: (page: number) => void;
}

export function useListState(defaultSort = ""): ListState {
  const [params, setParams] = useSearchParams();
  const q = params.get("q") ?? "";
  const sort = params.get("sort") ?? defaultSort;
  const dir = params.get("dir") === "desc" ? "desc" : "asc";
  const page = Math.max(1, Number(params.get("page")) || 1);

  const patch = useCallback(
    (next: Record<string, string | null>) => {
      setParams(
        (prev) => {
          const p = new URLSearchParams(prev);
          for (const [k, v] of Object.entries(next)) {
            if (v === null || v === "") p.delete(k);
            else p.set(k, v);
          }
          return p;
        },
        { replace: true },
      );
    },
    [setParams],
  );

  return useMemo<ListState>(
    () => ({
      q,
      sort,
      dir,
      page,
      setQ: (v) => patch({ q: v, page: null }),
      // Toggling the active column flips direction; a new column starts ascending.
      setSort: (key) => patch({ sort: key, dir: key === sort && dir === "asc" ? "desc" : "asc" }),
      setPage: (n) => patch({ page: n <= 1 ? null : String(n) }),
    }),
    [q, sort, dir, page, patch],
  );
}
