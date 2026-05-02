import { useEffect, useMemo } from "react";
import { Link } from "react-router";
import {
  type ModelBindingSpec,
  type ProviderRecord,
  configApi,
} from "@/lib/config-api";
import { adminRoutes } from "@/lib/routes";
import { useCrudPage } from "@/lib/use-crud-page";
import { Field } from "@/components/form-components";
import { EmptyState } from "@/components/ui/empty-state";
import { SkeletonRows } from "@/components/ui/skeleton";
import {
  ListSearchBar,
  PageSizeSelect,
  Pagination,
  SortableHeader,
  type SortableColumn,
} from "@/components/list-controls";
import {
  compareNumber,
  compareString,
  filterBySearch,
  paginate,
  sortItems,
  toggleSort,
  type SortConfig,
} from "@/lib/list-view";
import { useListUrlState } from "@/lib/list-url-state";
import { formatRelativeTime } from "@/lib/format-time";

const EMPTY_MODEL: ModelBindingSpec = {
  id: "",
  provider_id: "",
  upstream_model: "",
};

const auxiliaryLoaders = () =>
  configApi
    .list<ProviderRecord>("providers")
    .then((response) => response.items.map((provider) => provider.id));

type ModelSortKey = "id" | "provider_id" | "upstream_model" | "updated_at";

const SORT_CONFIG: SortConfig<ModelBindingSpec, ModelSortKey> = {
  id: (a, b) => compareString(a.id, b.id),
  provider_id: (a, b) => compareString(a.provider_id, b.provider_id),
  upstream_model: (a, b) => compareString(a.upstream_model, b.upstream_model),
  updated_at: (a, b) => compareNumber(a.updated_at ?? 0, b.updated_at ?? 0),
};

const COLUMNS: SortableColumn<ModelSortKey>[] = [
  { key: "id", label: "ID" },
  { key: "provider_id", label: "Provider" },
  { key: "upstream_model", label: "Upstream Model" },
  { key: "updated_at", label: "Last modified" },
  { key: null, label: "Actions" },
];

const LIST_OPTIONS = {
  validSortKeys: ["id", "provider_id", "upstream_model", "updated_at"] as const,
  defaultSort: { key: "id" as ModelSortKey, direction: "asc" as const },
} as const;

export function ModelsPage() {
  const crud = useCrudPage<ModelBindingSpec>({
    namespace: "models",
    entityLabel: "model",
    auxiliaryLoaders,
  });

  const { search, sort, pageSize, page, apply: applyListState } = useListUrlState<ModelSortKey>(LIST_OPTIONS);

  const providerIds = crud.auxiliaryData as string[];
  const providerOptions = useMemo(() => {
    const options = new Set(providerIds);
    if (crud.draft?.provider_id) {
      options.add(crud.draft.provider_id);
    }
    return Array.from(options).sort((left, right) => left.localeCompare(right));
  }, [crud.draft?.provider_id, providerIds]);

  const filtered = useMemo(
    () =>
      filterBySearch(crud.items, search, (model) => [
        model.id,
        model.provider_id,
        model.upstream_model,
      ]),
    [crud.items, search],
  );

  const sorted = useMemo(
    () => sortItems(filtered, sort, SORT_CONFIG),
    [filtered, sort],
  );

  const view = useMemo(
    () =>
      paginate(sorted, {
        page,
        pageSize,
        totalItems: sorted.length,
      }),
    [sorted, page, pageSize],
  );

  useEffect(() => {
    if (view.page !== page) applyListState({ page: view.page });
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [view.page, page]);

  return (
    <div className="mx-auto max-w-5xl p-6 md:p-8">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div>
          <p className="text-sm font-medium uppercase tracking-[0.2em] text-fg-soft">
            Runtime Catalog
          </p>
          <h2 className="mt-2 text-3xl font-semibold text-fg-strong">Models</h2>
        </div>
        <button
          type="button"
          onClick={() => crud.startNew({ ...EMPTY_MODEL })}
          className="rounded-xl bg-fg-strong px-4 py-2 text-sm font-medium text-white transition hover:bg-fg"
        >
          New Model
        </button>
      </div>

      {crud.draft ? (
        <section className="mb-6 rounded-2xl border border-line bg-surface p-5 shadow-sm">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">
              {crud.isEditingExisting ? "Edit model" : "Create model"}
            </h3>
            {crud.isEditingExisting && crud.draft.id && (
              <Link
                to={adminRoutes.auditLogForResource(`models/${crud.draft.id}`)}
                className="text-sm font-medium text-fg-soft transition hover:text-fg"
              >
                History
              </Link>
            )}
          </div>
          <div className="mt-4 grid gap-4 md:grid-cols-3">
            <Field label="Model ID">
              <input
                value={crud.draft.id}
                disabled={crud.isEditingExisting}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current ? { ...current, id: event.target.value } : current,
                  )
                }
                className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong disabled:bg-muted disabled:text-fg-soft"
              />
            </Field>
            <Field label="Provider ID">
              <select
                value={crud.draft.provider_id}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? { ...current, provider_id: event.target.value }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              >
                <option value="">Select a provider</option>
                {providerOptions.map((providerId) => (
                  <option key={providerId} value={providerId}>
                    {providerId}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Upstream Model">
              <input
                value={crud.draft.upstream_model}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? { ...current, upstream_model: event.target.value }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              />
            </Field>
          </div>

          <div className="mt-5 flex gap-3">
            <button
              type="button"
              onClick={() => void crud.handleSave()}
              disabled={crud.saving}
              className="rounded-xl bg-fg-strong px-4 py-2 text-sm font-medium text-white transition hover:bg-fg disabled:cursor-not-allowed disabled:opacity-60"
            >
              {crud.saving ? "Saving..." : "Save"}
            </button>
            <button
              type="button"
              onClick={crud.cancelEdit}
              className="rounded-xl border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft"
            >
              Cancel
            </button>
          </div>
        </section>
      ) : null}

      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
        <ListSearchBar
          value={search}
          onChange={(next) => applyListState({ search: next, page: 1 })}
          placeholder="Search by id, provider, upstream…"
        />
        <PageSizeSelect
          value={pageSize}
          onChange={(next) => applyListState({ pageSize: next, page: 1 })}
        />
      </div>

      <div className="overflow-x-auto rounded-md border border-line bg-surface shadow-card">
        {!crud.loading && crud.items.length === 0 ? (
          <EmptyState
            title="No managed models yet"
            description="Bind a provider to an upstream model name to make it usable by agents."
            actions={
              <button
                type="button"
                onClick={() => crud.startNew({ ...EMPTY_MODEL })}
                className="inline-flex h-9 items-center rounded-md bg-fg-strong px-4 text-sm font-medium text-bg transition-colors hover:bg-fg"
              >
                + New Model
              </button>
            }
          />
        ) : (
          <>
            <table className="min-w-full">
              <SortableHeader
                columns={COLUMNS}
                sort={sort}
                onSort={(key) =>
                  applyListState({ sort: toggleSort(sort, key), page: 1 })
                }
              />
              <tbody>
                {crud.loading && <SkeletonRows rows={4} cols={COLUMNS.length} />}
                {!crud.loading && view.items.length === 0 && (
                  <tr>
                    <td colSpan={COLUMNS.length} className="px-5 py-8 text-center text-sm text-fg-soft">
                      No models match the current filter.
                    </td>
                  </tr>
                )}
                {!crud.loading && view.items.map((model) => (
                  <tr
                    key={model.id}
                    className="border-t border-line text-sm text-fg"
                  >
                    <td className="px-5 py-4 font-mono text-fg-strong">{model.id}</td>
                    <td className="px-5 py-4">{model.provider_id}</td>
                    <td className="px-5 py-4 text-fg-soft">
                      {model.upstream_model}
                    </td>
                    <td className="px-5 py-4 text-fg-soft">
                      {formatRelativeTime(model.updated_at)}
                    </td>
                    <td className="px-5 py-4">
                      <div className="flex gap-4">
                        <button
                          type="button"
                          onClick={() => crud.startEdit(model)}
                          className="font-medium text-fg transition hover:text-fg-strong"
                        >
                          Edit
                        </button>
                        <button
                          type="button"
                          onClick={() => void crud.handleDelete(model.id)}
                          className="font-medium text-tone-error transition hover:text-tone-error"
                        >
                          Delete
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {view.pageCount > 1 || view.totalItems > pageSize ? (
              <Pagination
                page={view.page}
                pageCount={view.pageCount}
                startIndex={view.startIndex}
                endIndex={view.endIndex}
                totalItems={view.totalItems}
                onPageChange={(p) => applyListState({ page: p })}
              />
            ) : null}
          </>
        )}
      </div>
    </div>
  );
}
