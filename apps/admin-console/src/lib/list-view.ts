export type SortDirection = "asc" | "desc";

export interface SortState<TKey extends string> {
  key: TKey;
  direction: SortDirection;
}

export type Comparator<T> = (a: T, b: T) => number;

export type SortConfig<T, TKey extends string> = Record<TKey, Comparator<T>>;

export type SearchSelector<T> = (item: T) => Iterable<string | null | undefined>;

export const PAGE_SIZE_OPTIONS = [10, 20, 50, 100] as const;
export type PageSize = (typeof PAGE_SIZE_OPTIONS)[number];

export const DEFAULT_PAGE_SIZE: PageSize = 20;

/// Case-insensitive substring filter. Empty queries pass through unchanged.
/// Whitespace is collapsed and split into AND-tokens, so "foo bar" matches
/// items whose searchable strings contain both "foo" and "bar".
export function filterBySearch<T>(
  items: T[],
  query: string,
  selector: SearchSelector<T>,
): T[] {
  const trimmed = query.trim();
  if (trimmed.length === 0) return items;
  const tokens = trimmed
    .toLowerCase()
    .split(/\s+/)
    .filter((token) => token.length > 0);
  if (tokens.length === 0) return items;

  return items.filter((item) => {
    const haystack = Array.from(selector(item))
      .filter((value): value is string => typeof value === "string")
      .map((value) => value.toLowerCase())
      .join("  ");
    return tokens.every((token) => haystack.includes(token));
  });
}

export function sortItems<T, TKey extends string>(
  items: T[],
  state: SortState<TKey> | null,
  config: SortConfig<T, TKey>,
): T[] {
  if (!state) return items;
  const comparator = config[state.key];
  if (!comparator) return items;
  const direction = state.direction === "asc" ? 1 : -1;
  const copy = items.slice();
  copy.sort((a, b) => comparator(a, b) * direction);
  return copy;
}

export function toggleSort<TKey extends string>(
  current: SortState<TKey> | null,
  key: TKey,
): SortState<TKey> | null {
  if (!current || current.key !== key) {
    return { key, direction: "asc" };
  }
  if (current.direction === "asc") {
    return { key, direction: "desc" };
  }
  return null;
}

export interface PaginationState {
  page: number;
  pageSize: PageSize;
  totalItems: number;
}

export interface PageView<T> {
  items: T[];
  page: number;
  pageCount: number;
  pageSize: PageSize;
  totalItems: number;
  startIndex: number;
  endIndex: number;
}

export function paginate<T>(items: T[], state: PaginationState): PageView<T> {
  const totalItems = items.length;
  const pageCount = Math.max(1, Math.ceil(totalItems / state.pageSize));
  const safePage = clamp(state.page, 1, pageCount);
  const startIndex = (safePage - 1) * state.pageSize;
  const endIndex = Math.min(totalItems, startIndex + state.pageSize);
  return {
    items: items.slice(startIndex, endIndex),
    page: safePage,
    pageCount,
    pageSize: state.pageSize,
    totalItems,
    startIndex,
    endIndex,
  };
}

function clamp(value: number, min: number, max: number): number {
  if (Number.isNaN(value)) return min;
  if (value < min) return min;
  if (value > max) return max;
  return value;
}

export function compareString(a: string | undefined, b: string | undefined): number {
  return (a ?? "").localeCompare(b ?? "");
}

export function compareNumber(a: number | undefined, b: number | undefined): number {
  const left = typeof a === "number" ? a : 0;
  const right = typeof b === "number" ? b : 0;
  return left - right;
}

export function compareBoolean(
  a: boolean | undefined,
  b: boolean | undefined,
): number {
  return Number(Boolean(a)) - Number(Boolean(b));
}
