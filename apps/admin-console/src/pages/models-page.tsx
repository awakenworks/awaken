import { useMemo } from "react";
import { type ModelSpec, type ProviderRecord, configApi } from "@/lib/config-api";
import { useCrudPage } from "@/lib/use-crud-page";
import { Field } from "@/components/form-components";

const EMPTY_MODEL: ModelSpec = {
  id: "",
  provider: "",
  model: "",
};

const auxiliaryLoaders = () =>
  configApi
    .list<ProviderRecord>("providers")
    .then((response) => response.items.map((provider) => provider.id));

export function ModelsPage() {
  const crud = useCrudPage<ModelSpec>({
    namespace: "models",
    entityLabel: "model",
    auxiliaryLoaders,
  });

  const providerIds = crud.auxiliaryData as string[];
  const providerOptions = useMemo(() => {
    const options = new Set(providerIds);
    if (crud.draft?.provider) {
      options.add(crud.draft.provider);
    }
    return Array.from(options).sort((left, right) => left.localeCompare(right));
  }, [crud.draft?.provider, providerIds]);

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

      {crud.error ? (
        <div className="mb-4 rounded-2xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700">
          {crud.error}
        </div>
      ) : null}

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
                value={crud.draft.provider}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? { ...current, provider: event.target.value }
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
                value={crud.draft.model}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current ? { ...current, model: event.target.value } : current,
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

      <div className="overflow-hidden rounded-2xl border border-slate-200 bg-white shadow-sm">
        {crud.loading ? (
          <div className="px-5 py-6 text-sm text-slate-500">Loading models...</div>
        ) : crud.items.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No managed models yet.
          </div>
        ) : (
          <table className="min-w-full">
            <thead className="bg-slate-50 text-left text-xs uppercase tracking-wide text-slate-500">
              <tr>
                <th className="px-5 py-3">ID</th>
                <th className="px-5 py-3">Provider</th>
                <th className="px-5 py-3">Upstream Model</th>
                <th className="px-5 py-3">Actions</th>
              </tr>
            </thead>
            <tbody>
              {crud.items.map((model) => (
                <tr
                  key={model.id}
                  className="border-t border-slate-200 text-sm text-slate-700"
                >
                  <td className="px-5 py-4 font-mono text-slate-950">{model.id}</td>
                  <td className="px-5 py-4">{model.provider}</td>
                  <td className="px-5 py-4 text-slate-500">{model.model}</td>
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
        )}
      </div>
    </div>
  );
}
