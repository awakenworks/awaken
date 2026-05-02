import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router";
import {
  ConfigApiError,
  configApi,
  type ProviderRecord,
  type ProviderSpec,
} from "@/lib/config-api";
import { useToast } from "@/components/toast-provider";
import { useCrudPage } from "@/lib/use-crud-page";
import { Field } from "@/components/form-components";
import { EmptyState } from "@/components/ui/empty-state";
import { SecretField, SecretStatusPill } from "@/components/ui/secret-field";
import { SkeletonRows } from "@/components/ui/skeleton";
import { adminRoutes } from "@/lib/routes";
import {
  ListSearchBar,
  PageSizeSelect,
  Pagination,
  SortableHeader,
  type SortableColumn,
} from "@/components/list-controls";
import {
  compareBoolean,
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

type ProviderSortKey = "id" | "adapter" | "base_url" | "has_api_key" | "updated_at";

const SORT_CONFIG: SortConfig<ProviderRecord, ProviderSortKey> = {
  id: (a, b) => compareString(a.id, b.id),
  adapter: (a, b) => compareString(a.adapter, b.adapter),
  base_url: (a, b) => compareString(a.base_url, b.base_url),
  has_api_key: (a, b) => compareBoolean(a.has_api_key, b.has_api_key),
  updated_at: (a, b) => compareNumber(a.updated_at ?? 0, b.updated_at ?? 0),
};

const COLUMNS: SortableColumn<ProviderSortKey>[] = [
  { key: "id", label: "ID" },
  { key: "adapter", label: "Adapter" },
  { key: "base_url", label: "Base URL" },
  { key: "has_api_key", label: "API Key" },
  { key: "updated_at", label: "Last modified" },
  { key: null, label: "Actions" },
];

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

const LIST_OPTIONS = {
  validSortKeys: ["id", "adapter", "base_url", "has_api_key", "updated_at"] as const,
  defaultSort: { key: "id" as ProviderSortKey, direction: "asc" as const },
} as const;

interface TestStatus {
  ok: boolean;
  latency_ms: number;
  error?: string;
  testedAt: number;
}

export function ProvidersPage() {
  const [apiKeyMode, setApiKeyMode] = useState<ApiKeyMode>("replace");
  const [apiKeyDraft, setApiKeyDraft] = useState("");
  const [testing, setTesting] = useState(false);
  const [testStatus, setTestStatus] = useState<TestStatus | null>(null);
  const toast = useToast();
  const testingIdRef = useRef<string | null>(null);

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

  const { search, sort, pageSize, page, apply: applyListState } = useListUrlState<ProviderSortKey>(LIST_OPTIONS);

  const filtered = useMemo(
    () =>
      filterBySearch(crud.items, search, (provider) => [
        provider.id,
        provider.adapter,
        provider.base_url,
      ]),
    [crud.items, search],
  );
  const sorted = useMemo(
    () => sortItems(filtered, sort, SORT_CONFIG),
    [filtered, sort],
  );
  const view = useMemo(
    () => paginate(sorted, { page, pageSize, totalItems: sorted.length }),
    [sorted, page, pageSize],
  );

  useEffect(() => {
    if (view.page !== page) applyListState({ page: view.page });
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [view.page, page]);

  function startCreate() {
    crud.startNew({ ...EMPTY_PROVIDER });
    setApiKeyMode("replace");
    setApiKeyDraft("");
    setTestStatus(null);
  }

  function startEdit(provider: ProviderRecord) {
    crud.startEdit(provider);
    setApiKeyMode("preserve");
    setApiKeyDraft("");
    setTestStatus(null);
  }

  async function handleTestConnection() {
    if (!crud.draft || !crud.isEditingExisting) return;
    const id = crud.draft.id;
    testingIdRef.current = id;
    setTesting(true);
    try {
      const result = await configApi.testProvider(id);
      if (testingIdRef.current !== id) return;
      setTestStatus({ ...result, testedAt: Date.now() });
      if (result.ok) {
        toast.success(`Provider OK (${result.latency_ms}ms)`);
      } else {
        toast.error(result.error ?? "Provider test failed");
      }
    } catch (err) {
      if (testingIdRef.current !== id) return;
      const message =
        err instanceof ConfigApiError ? err.message : "Provider test failed";
      setTestStatus({ ok: false, latency_ms: 0, error: message, testedAt: Date.now() });
      toast.error(message);
    } finally {
      if (testingIdRef.current === id) setTesting(false);
    }
  }

  async function handleSave() {
    await crud.handleSave();
    setApiKeyDraft("");
  }

  const [rowTestingId, setRowTestingId] = useState<string | null>(null);

  async function handleRowTest(providerId: string) {
    setRowTestingId(providerId);
    try {
      const result = await configApi.testProvider(providerId);
      if (result.ok) {
        toast.success(`${providerId} OK · ${result.latency_ms}ms`);
      } else {
        toast.error(`${providerId}: ${result.error ?? "test failed"}`);
      }
    } catch (err) {
      toast.error(
        `${providerId}: ${err instanceof Error ? err.message : "test failed"}`,
      );
    } finally {
      setRowTestingId((current) => (current === providerId ? null : current));
    }
  }

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div>
          <p className="text-sm font-medium uppercase tracking-[0.2em] text-fg-soft">
            Runtime Catalog
          </p>
          <h2 className="mt-2 text-3xl font-semibold text-fg-strong">Providers</h2>
        </div>
        <button
          type="button"
          onClick={startCreate}
          className="rounded-xl bg-fg-strong px-4 py-2 text-sm font-medium text-white transition hover:bg-fg"
        >
          New Provider
        </button>
      </div>

      {crud.draft ? (
        <section className="mb-6 rounded-2xl border border-line bg-surface p-5 shadow-sm">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">
              {crud.isEditingExisting ? "Edit provider" : "Create provider"}
            </h3>
            {crud.isEditingExisting && crud.draft.id && (
              <Link
                to={adminRoutes.auditLogForResource(`providers/${crud.draft.id}`)}
                className="text-sm font-medium text-fg-soft transition hover:text-fg"
              >
                History
              </Link>
            )}
          </div>

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
                className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong disabled:bg-muted disabled:text-fg-soft"
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
                className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
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
                className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
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
                className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              />
            </Field>
          </div>

          <div className="mt-5">
            <SecretField
              mode={apiKeyMode === "preserve" ? "keep" : apiKeyMode}
              onModeChange={(next) =>
                setApiKeyMode(next === "keep" ? "preserve" : next)
              }
              currentlyHasValue={Boolean(crud.isEditingExisting && crud.draft.has_api_key)}
              statusPill={
                crud.draft.has_api_key ? (
                  <SecretStatusPill
                    state={apiKeyMode === "clear" ? "will-clear" : "stored"}
                  />
                ) : crud.isEditingExisting ? (
                  <SecretStatusPill state="no-value" />
                ) : (
                  <SecretStatusPill
                    state={apiKeyDraft.trim().length > 0 ? "will-set" : "no-value"}
                  />
                )
              }
              labels={{
                title: `API key${crud.draft.id ? ` — ${crud.draft.id}` : ""}`,
                description: crud.isEditingExisting
                  ? "Default mode is Keep — the existing key never goes over the wire while you edit other fields."
                  : "Optional. Leave empty to fall back to the adapter's environment variable.",
                replaceLabel: "Set new key",
                clearLabel: "Clear key",
                keepBody: (
                  <>
                    <strong>Existing key is preserved.</strong>{" "}
                    <span>
                      Save will not modify the secret; other fields update normally.
                    </span>
                  </>
                ),
                clearBody: (
                  <>
                    <strong>Credential will be removed on save.</strong>{" "}
                    <span>
                      Subsequent calls fall back to the adapter's host environment variable, or fail if none is present.
                    </span>
                  </>
                ),
              }}
              hint={
                <>
                  Stored encrypted at rest. Submitting saves the new value and rotates the runtime client; in-flight requests continue with the prior key.
                </>
              }
            >
              <input
                type="password"
                autoComplete="off"
                value={apiKeyDraft}
                onChange={(event) => setApiKeyDraft(event.target.value)}
                placeholder={crud.isEditingExisting ? "sk-…" : "Optional API key"}
                className="w-full rounded-md border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg-strong outline-none transition-colors focus:border-link"
              />
            </SecretField>
          </div>

          <div className="mt-5 flex flex-wrap items-center gap-3">
            <button
              type="button"
              onClick={() => void handleSave()}
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
            {crud.isEditingExisting ? (
              <button
                type="button"
                onClick={() => void handleTestConnection()}
                disabled={testing}
                className="rounded-xl border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
              >
                {testing ? "Testing..." : "Test connection"}
              </button>
            ) : null}
          </div>

          {testStatus !== null ? (
            <div
              className={`mt-3 flex items-center gap-2 rounded-xl border px-4 py-2 text-sm ${
                testStatus.ok
                  ? "border-tone-success/30 bg-tone-success/10 text-tone-success"
                  : "border-tone-error/30 bg-tone-error/10 text-tone-error"
              }`}
            >
              <span className="font-medium">
                {testStatus.ok
                  ? `OK — ${testStatus.latency_ms}ms`
                  : `Failed${testStatus.error ? `: ${testStatus.error}` : ""}`}
              </span>
              <span className="ml-auto text-xs opacity-60">
                {new Date(testStatus.testedAt).toLocaleTimeString()}
              </span>
            </div>
          ) : null}
        </section>
      ) : null}

      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
        <ListSearchBar
          value={search}
          onChange={(next) => applyListState({ search: next, page: 1 })}
          placeholder="Search by id, adapter, base url…"
        />
        <PageSizeSelect
          value={pageSize}
          onChange={(next) => applyListState({ pageSize: next, page: 1 })}
        />
      </div>

      <div className="overflow-x-auto rounded-md border border-line bg-surface shadow-card">
        {!crud.loading && crud.items.length === 0 ? (
          <EmptyState
            title="No managed providers yet"
            description="A provider connects Awaken to an LLM API. Configure the adapter and credentials, then bind models on top."
            actions={
              <button
                type="button"
                onClick={startCreate}
                className="inline-flex h-9 items-center rounded-md bg-fg-strong px-4 text-sm font-medium text-bg transition-colors hover:bg-fg"
              >
                + New Provider
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
                      No providers match the current filter.
                    </td>
                  </tr>
                )}
                {!crud.loading && view.items.map((provider) => (
                  <tr
                    key={provider.id}
                    className="border-t border-line text-sm text-fg"
                  >
                    <td className="px-5 py-4 font-mono text-fg-strong">{provider.id}</td>
                    <td className="px-5 py-4">{provider.adapter}</td>
                    <td className="px-5 py-4 text-fg-soft">
                      {provider.base_url ?? "Default"}
                    </td>
                    <td className="px-5 py-4 text-fg-soft">
                      {provider.has_api_key ? "Stored" : "Environment / none"}
                    </td>
                    <td className="px-5 py-4 text-fg-soft">
                      {formatRelativeTime(provider.updated_at)}
                    </td>
                    <td className="px-5 py-4">
                      <div className="flex gap-4">
                        <button
                          type="button"
                          onClick={() => void handleRowTest(provider.id)}
                          disabled={rowTestingId === provider.id}
                          className="font-medium text-link transition-colors hover:text-link-hover disabled:cursor-not-allowed disabled:opacity-60"
                        >
                          {rowTestingId === provider.id ? "Testing…" : "Test"}
                        </button>
                        <button
                          type="button"
                          onClick={() => startEdit(provider)}
                          className="font-medium text-fg transition-colors hover:text-fg-strong"
                        >
                          Edit
                        </button>
                        <button
                          type="button"
                          onClick={() => void crud.handleDelete(provider.id)}
                          className="font-medium text-tone-error transition-colors hover:text-tone-error"
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
