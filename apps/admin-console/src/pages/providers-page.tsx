import { type ReactNode, useEffect, useMemo, useState } from "react";
import {
  type ProviderRecord,
  type ProviderSpec,
  configApi,
} from "@/lib/config-api";

const KNOWN_ADAPTERS = [
  "anthropic",
  "openai",
  "openai_resp",
  "deepseek",
  "gemini",
  "ollama",
  "cohere",
  "scripted",
  "together",
  "fireworks",
];

type ApiKeyMode = "preserve" | "replace" | "clear";

const EMPTY_PROVIDER: ProviderRecord = {
  id: "",
  adapter: "anthropic",
  timeout_secs: 300,
};

export function ProvidersPage() {
  const [providers, setProviders] = useState<ProviderRecord[]>([]);
  const [draft, setDraft] = useState<ProviderRecord | null>(null);
  const [apiKeyMode, setApiKeyMode] = useState<ApiKeyMode>("replace");
  const [apiKeyDraft, setApiKeyDraft] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const isEditingExisting = useMemo(
    () => (draft ? providers.some((provider) => provider.id === draft.id) : false),
    [draft, providers],
  );
  const adapterOptions = useMemo(() => {
    const options = new Set(KNOWN_ADAPTERS);
    if (draft?.adapter) {
      options.add(draft.adapter);
    }
    return Array.from(options).sort((left, right) => left.localeCompare(right));
  }, [draft?.adapter]);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const response = await configApi.list<ProviderRecord>("providers");
        if (!cancelled) {
          setProviders(response.items);
          setError(null);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
          setProviders([]);
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

  function startCreate() {
    setDraft({ ...EMPTY_PROVIDER });
    setApiKeyMode("replace");
    setApiKeyDraft("");
  }

  function startEdit(provider: ProviderRecord) {
    setDraft({ ...provider });
    setApiKeyMode("preserve");
    setApiKeyDraft("");
  }

  async function handleSave() {
    if (!draft) {
      return;
    }

    const payload: ProviderSpec = {
      ...draft,
      timeout_secs: Number(draft.timeout_secs) || 300,
    };

    if (apiKeyMode === "replace") {
      if (apiKeyDraft.trim().length > 0) {
        payload.api_key = apiKeyDraft.trim();
      }
    } else if (apiKeyMode === "clear") {
      payload.api_key = "";
    }

    setSaving(true);
    try {
      if (isEditingExisting) {
        const updated = await configApi.update<ProviderSpec, ProviderRecord>(
          "providers",
          draft.id,
          payload,
        );
        setProviders((current) =>
          current.map((provider) => (provider.id === updated.id ? updated : provider)),
        );
      } else {
        const created = await configApi.create<ProviderSpec, ProviderRecord>(
          "providers",
          payload,
        );
        setProviders((current) =>
          [...current.filter((provider) => provider.id !== created.id), created].sort(
            (left, right) => left.id.localeCompare(right.id),
          ),
        );
      }

      setDraft(null);
      setApiKeyDraft("");
      setError(null);
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : String(saveError));
    } finally {
      setSaving(false);
    }
  }

  async function handleDelete(id: string) {
    if (!confirm(`Delete provider "${id}"?`)) {
      return;
    }

    try {
      await configApi.delete("providers", id);
      setProviders((current) => current.filter((provider) => provider.id !== id));
      setError(null);
    } catch (deleteError) {
      setError(
        deleteError instanceof Error ? deleteError.message : String(deleteError),
      );
    }
  }

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div>
          <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
            Runtime Catalog
          </p>
          <h2 className="mt-2 text-3xl font-semibold text-slate-950">Providers</h2>
        </div>
        <button
          type="button"
          onClick={startCreate}
          className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800"
        >
          New Provider
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
            {isEditingExisting ? "Edit provider" : "Create provider"}
          </h3>

          <div className="mt-4 grid gap-4 md:grid-cols-2">
            <Field label="Provider ID">
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

            <Field label="Adapter">
              <select
                value={draft.adapter}
                onChange={(event) =>
                  setDraft((current) =>
                    current ? { ...current, adapter: event.target.value } : current,
                  )
                }
                className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              >
                {adapterOptions.map((adapter) => (
                  <option key={adapter} value={adapter}>
                    {adapter}
                  </option>
                ))}
              </select>
            </Field>

            <Field label="Base URL">
              <input
                value={String(draft.base_url ?? "")}
                onChange={(event) =>
                  setDraft((current) =>
                    current
                      ? {
                          ...current,
                          base_url: event.target.value || undefined,
                        }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </Field>

            <Field label="Timeout (seconds)">
              <input
                type="number"
                min={1}
                value={Number(draft.timeout_secs ?? 300)}
                onChange={(event) =>
                  setDraft((current) =>
                    current
                      ? {
                          ...current,
                          timeout_secs: Number(event.target.value) || 300,
                        }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </Field>
          </div>

          <section className="mt-5 rounded-xl border border-slate-200 bg-slate-50 p-4">
            <div className="flex flex-wrap items-center justify-between gap-3">
              <div>
                <h4 className="text-sm font-semibold text-slate-900">API Key</h4>
                <p className="mt-1 text-sm text-slate-500">
                  {isEditingExisting
                    ? draft.has_api_key
                      ? "A key is currently stored. Keep it, replace it, or clear it."
                      : "No stored key. Requests will fall back to the adapter environment variable."
                    : "Optional. Leave empty to use the adapter environment variable."}
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                {isEditingExisting ? (
                  <>
                    <ModeButton
                      active={apiKeyMode === "preserve"}
                      onClick={() => setApiKeyMode("preserve")}
                      label="Keep current"
                    />
                    <ModeButton
                      active={apiKeyMode === "replace"}
                      onClick={() => setApiKeyMode("replace")}
                      label="Set new key"
                    />
                    <ModeButton
                      active={apiKeyMode === "clear"}
                      onClick={() => setApiKeyMode("clear")}
                      label="Clear key"
                    />
                  </>
                ) : null}
              </div>
            </div>

            {apiKeyMode === "replace" ? (
              <input
                type="password"
                value={apiKeyDraft}
                onChange={(event) => setApiKeyDraft(event.target.value)}
                className="mt-4 w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            ) : (
              <div className="mt-4 text-sm text-slate-500">
                {apiKeyMode === "clear"
                  ? "Saving will remove the stored API key."
                  : "Saving will preserve the current key state."}
              </div>
            )}
          </section>

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
          <div className="px-5 py-6 text-sm text-slate-500">
            Loading providers...
          </div>
        ) : providers.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No managed providers yet.
          </div>
        ) : (
          <table className="min-w-full">
            <thead className="bg-slate-50 text-left text-xs uppercase tracking-wide text-slate-500">
              <tr>
                <th className="px-5 py-3">ID</th>
                <th className="px-5 py-3">Adapter</th>
                <th className="px-5 py-3">Base URL</th>
                <th className="px-5 py-3">API Key</th>
                <th className="px-5 py-3">Actions</th>
              </tr>
            </thead>
            <tbody>
              {providers.map((provider) => (
                <tr
                  key={provider.id}
                  className="border-t border-slate-200 text-sm text-slate-700"
                >
                  <td className="px-5 py-4 font-mono text-slate-950">{provider.id}</td>
                  <td className="px-5 py-4">{provider.adapter}</td>
                  <td className="px-5 py-4 text-slate-500">
                    {provider.base_url ?? "Default"}
                  </td>
                  <td className="px-5 py-4 text-slate-500">
                    {provider.has_api_key ? "Stored" : "Environment / none"}
                  </td>
                  <td className="px-5 py-4">
                    <div className="flex gap-4">
                      <button
                        type="button"
                        onClick={() => startEdit(provider)}
                        className="font-medium text-slate-700 transition hover:text-slate-950"
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        onClick={() => void handleDelete(provider.id)}
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

function ModeButton({
  active,
  label,
  onClick,
}: {
  active: boolean;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={[
        "rounded-full px-3 py-1.5 text-xs font-medium transition",
        active
          ? "bg-slate-950 text-white"
          : "border border-slate-300 bg-white text-slate-700 hover:bg-slate-100",
      ].join(" ")}
    >
      {label}
    </button>
  );
}
