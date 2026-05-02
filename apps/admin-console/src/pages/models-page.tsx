import { useEffect, useMemo } from "react";
import {
  type ModelBindingSpec,
  type ProviderRecord,
  configApi,
} from "@/lib/config-api";
import { useCrudPage } from "@/lib/use-crud-page";
import { Field } from "@/components/form-components";
import {
  ListSearchBar,
  PageSizeSelect,
  Pagination,
  SortableHeader,
  type SortableColumn,
} from "@/components/list-controls";
import {
  compareString,
  filterBySearch,
  paginate,
  sortItems,
  toggleSort,
  type SortConfig,
} from "@/lib/list-view";
import { useListUrlState } from "@/lib/list-url-state";

const EMPTY_MODEL: ModelBindingSpec = {
  id: "",
  provider_id: "",
  upstream_model: "",
};

const auxiliaryLoaders = () =>
  configApi
    .list<ProviderRecord>("providers")
    .then((response) => response.items.map((provider) => provider.id));

type ModelSortKey = "id" | "provider_id" | "upstream_model";

const SORT_CONFIG: SortConfig<ModelBindingSpec, ModelSortKey> = {
  id: (a, b) => compareString(a.id, b.id),
  provider_id: (a, b) => compareString(a.provider_id, b.provider_id),
  upstream_model: (a, b) => compareString(a.upstream_model, b.upstream_model),
};

const COLUMNS: SortableColumn<ModelSortKey>[] = [
  { key: "id", label: "ID" },
  { key: "provider_id", label: "Provider" },
  { key: "upstream_model", label: "Upstream Model" },
  { key: null, label: "Actions" },
];

const LIST_OPTIONS = {
  validSortKeys: ["id", "provider_id", "upstream_model"] as const,
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
          <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
            Runtime Catalog
          </p>
          <h2 className="mt-2 text-3xl font-semibold text-slate-950">Models</h2>
        </div>
        <button
          type="button"
          onClick={() => crud.startNew({ ...EMPTY_MODEL })}
          className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800"
        >
          New Model
        </button>
      </div>

      {crud.draft ? (
        <section className="mb-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <h3 className="text-lg font-semibold text-slate-950">
            {crud.isEditingExisting ? "Edit model" : "Create model"}
          </h3>
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
                className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500 disabled:bg-slate-100 disabled:text-slate-500"
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
                className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
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
                className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </Field>
          </div>

          <div className="mt-5 flex gap-3">
            <button
              type="button"
              onClick={() => void crud.handleSave()}
              disabled={crud.saving}
              className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {crud.saving ? "Saving..." : "Save"}
            </button>
            <button
              type="button"
              onClick={crud.cancelEdit}
              className="rounded-xl border border-slate-300 px-4 py-2 text-sm font-medium text-slate-700 transition hover:bg-slate-50"
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

      <div className="overflow-hidden rounded-2xl border border-slate-200 bg-white shadow-sm">
        {crud.loading ? (
          <div className="px-5 py-6 text-sm text-slate-500">Loading models...</div>
        ) : crud.items.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No managed models yet.
          </div>
        ) : view.items.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No models match the current filter.
          </div>
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
                {view.items.map((model) => (
                  <tr
                    key={model.id}
                    className="border-t border-slate-200 text-sm text-slate-700"
                  >
                    <td className="px-5 py-4 font-mono text-slate-950">{model.id}</td>
                    <td className="px-5 py-4">{model.provider_id}</td>
                    <td className="px-5 py-4 text-slate-500">
                      {model.upstream_model}
                    </td>
                    <td className="px-5 py-4">
                      <div className="flex gap-4">
                        <button
                          type="button"
                          onClick={() => crud.startEdit(model)}
                          className="font-medium text-slate-700 transition hover:text-slate-950"
                        >
                          Edit
                        </button>
                        <button
                          type="button"
                          onClick={() => void crud.handleDelete(model.id)}
                          className="font-medium text-rose-600 transition hover:text-rose-700"
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
