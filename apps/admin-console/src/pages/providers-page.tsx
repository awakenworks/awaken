import { useCallback, useMemo, useState } from "react";
import {
  configApi,
  type ProviderRecord,
  type ProviderSpec,
} from "@/lib/config-api";
import { useCrudPage } from "@/lib/use-crud-page";
import { Field, ModeButton } from "@/components/form-components";

const FALLBACK_ADAPTERS = [
  "anthropic",
  "openai",
  "openai_resp",
  "deepseek",
  "gemini",
  "ollama",
  "cohere",
  "together",
  "fireworks",
  "groq",
  "xai",
  "zai",
  "bigmodel",
  "aliyun",
  "mimo",
  "nebius",
];

type ApiKeyMode = "preserve" | "replace" | "clear";

const EMPTY_PROVIDER: ProviderRecord = {
  id: "",
  adapter: "anthropic",
  timeout_secs: 300,
};

export function ProvidersPage() {
  const [apiKeyMode, setApiKeyMode] = useState<ApiKeyMode>("replace");
  const [apiKeyDraft, setApiKeyDraft] = useState("");

  const prepareSave = useCallback(
    (draft: ProviderRecord): ProviderSpec => {
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

      return payload;
    },
    [apiKeyMode, apiKeyDraft],
  );

  const crud = useCrudPage<ProviderRecord, ProviderSpec>({
    namespace: "providers",
    entityLabel: "provider",
    prepareSave,
    auxiliaryLoaders: () =>
      configApi
        .capabilities()
        .then((caps) => [caps.supported_adapters ?? FALLBACK_ADAPTERS]),
  });

  const serverAdapters = crud.auxiliaryData[0] as string[] | undefined;

  const adapterOptions = useMemo(() => {
    const options = new Set(serverAdapters ?? FALLBACK_ADAPTERS);
    if (crud.draft?.adapter) {
      options.add(crud.draft.adapter);
    }
    return Array.from(options).sort((left, right) => left.localeCompare(right));
  }, [crud.draft?.adapter, serverAdapters]);

  function startCreate() {
    crud.startNew({ ...EMPTY_PROVIDER });
    setApiKeyMode("replace");
    setApiKeyDraft("");
  }

  function startEdit(provider: ProviderRecord) {
    crud.startEdit(provider);
    setApiKeyMode("preserve");
    setApiKeyDraft("");
  }

  async function handleSave() {
    await crud.handleSave();
    setApiKeyDraft("");
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

      {crud.error ? (
        <div className="mb-4 rounded-2xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700">
          {crud.error}
        </div>
      ) : null}

      {crud.draft ? (
        <section className="mb-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <h3 className="text-lg font-semibold text-slate-950">
            {crud.isEditingExisting ? "Edit provider" : "Create provider"}
          </h3>

          <div className="mt-4 grid gap-4 md:grid-cols-2">
            <Field label="Provider ID">
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

            <Field label="Adapter">
              <select
                value={crud.draft.adapter}
                onChange={(event) =>
                  crud.setDraft((current) =>
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
                value={String(crud.draft.base_url ?? "")}
                onChange={(event) =>
                  crud.setDraft((current) =>
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
                value={Number(crud.draft.timeout_secs ?? 300)}
                onChange={(event) =>
                  crud.setDraft((current) =>
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
                  {crud.isEditingExisting
                    ? crud.draft.has_api_key
                      ? "A key is currently stored. Keep it, replace it, or clear it."
                      : "No stored key. Requests will fall back to the adapter environment variable."
                    : "Optional. Leave empty to use the adapter environment variable."}
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                {crud.isEditingExisting ? (
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
          <div className="px-5 py-6 text-sm text-slate-500">
            Loading providers...
          </div>
        ) : crud.items.length === 0 ? (
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
              {crud.items.map((provider) => (
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
                        onClick={() => void crud.handleDelete(provider.id)}
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
