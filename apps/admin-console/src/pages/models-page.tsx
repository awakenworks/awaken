import { type ReactNode, useEffect, useMemo, useState } from "react";
import { type ModelSpec, type ProviderRecord, configApi } from "@/lib/config-api";

const EMPTY_MODEL: ModelSpec = {
  id: "",
  provider: "",
  model: "",
};

export function ModelsPage() {
  const [models, setModels] = useState<ModelSpec[]>([]);
  const [providerIds, setProviderIds] = useState<string[]>([]);
  const [draft, setDraft] = useState<ModelSpec | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const providerOptions = useMemo(() => {
    const options = new Set(providerIds);
    if (draft?.provider) {
      options.add(draft.provider);
    }
    return Array.from(options).sort((left, right) => left.localeCompare(right));
  }, [draft?.provider, providerIds]);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const [modelResponse, providerResponse] = await Promise.all([
          configApi.list<ModelSpec>("models"),
          configApi.list<ProviderRecord>("providers"),
        ]);
        if (!cancelled) {
          setModels(modelResponse.items);
          setProviderIds(providerResponse.items.map((provider) => provider.id));
          setError(null);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
          setModels([]);
          setProviderIds([]);
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    }

    void load();

    return () => {
      cancelled = true;
    };
  }, []);

  async function handleSave() {
    if (!draft) {
      return;
    }

    setSaving(true);
    try {
      const exists = models.some((model) => model.id === draft.id);
      if (exists) {
        const updated = await configApi.update<ModelSpec>("models", draft.id, draft);
        setModels((current) =>
          current.map((model) => (model.id === updated.id ? updated : model)),
        );
      } else {
        const created = await configApi.create<ModelSpec>("models", draft);
        setModels((current) =>
          [...current.filter((model) => model.id !== created.id), created].sort((left, right) =>
            left.id.localeCompare(right.id),
          ),
        );
      }
      setDraft(null);
      setError(null);
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : String(saveError));
    } finally {
      setSaving(false);
    }
  }

  async function handleDelete(id: string) {
    if (!confirm(`Delete model "${id}"?`)) {
      return;
    }

    try {
      await configApi.delete("models", id);
      setModels((current) => current.filter((model) => model.id !== id));
      setError(null);
    } catch (deleteError) {
      setError(
        deleteError instanceof Error ? deleteError.message : String(deleteError),
      );
    }
  }

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
          onClick={() => setDraft({ ...EMPTY_MODEL })}
          className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800"
        >
          New Model
        </button>
      </div>

      {error ? (
        <div className="mb-4 rounded-2xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700">
          {error}
        </div>
      ) : null}

      {draft ? (
        <section className="mb-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <h3 className="text-lg font-semibold text-slate-950">
            {models.some((model) => model.id === draft.id) ? "Edit model" : "Create model"}
          </h3>
          <div className="mt-4 grid gap-4 md:grid-cols-3">
            <Field label="Model ID">
              <input
                value={draft.id}
                onChange={(event) =>
                  setDraft((current) =>
                    current ? { ...current, id: event.target.value } : current,
                  )
                }
                className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </Field>
            <Field label="Provider ID">
              <select
                value={draft.provider}
                onChange={(event) =>
                  setDraft((current) =>
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
                value={draft.model}
                onChange={(event) =>
                  setDraft((current) =>
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
              onClick={() => void handleSave()}
              disabled={saving}
              className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {saving ? "Saving..." : "Save"}
            </button>
            <button
              type="button"
              onClick={() => setDraft(null)}
              className="rounded-xl border border-slate-300 px-4 py-2 text-sm font-medium text-slate-700 transition hover:bg-slate-50"
            >
              Cancel
            </button>
          </div>
        </section>
      ) : null}

      <div className="overflow-hidden rounded-2xl border border-slate-200 bg-white shadow-sm">
        {loading ? (
          <div className="px-5 py-6 text-sm text-slate-500">Loading models...</div>
        ) : models.length === 0 ? (
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
              {models.map((model) => (
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
                        onClick={() => setDraft({ ...model })}
                        className="font-medium text-slate-700 transition hover:text-slate-950"
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        onClick={() => void handleDelete(model.id)}
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

function Field({
  label,
  children,
}: {
  label: string;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <span className="mb-1.5 block text-sm font-medium text-slate-600">{label}</span>
      {children}
    </label>
  );
}
