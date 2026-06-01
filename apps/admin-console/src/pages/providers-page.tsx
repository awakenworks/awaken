import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router";
import {
  capabilitiesApi,
  capabilitiesFromResult,
  ConfigApiError,
  providersApi,
  type ModelSpec,
  type ProviderRecord,
  type ProviderSpec,
  type ProviderTestResponse,
} from "@/lib/api";
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
import { ModelProviderSetupBanner } from "@/components/model-provider-setup-banner";

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

// Used only when /v1/capabilities is unreachable (offline dev). The runtime
// is the source of truth — `supported_adapters` from the API replaces this list
// once it loads. Mirrors `crates/awaken-server/src/services/config_runtime.rs`'s
// SUPPORTED_ADAPTERS enumeration.
const FALLBACK_ADAPTERS = [
  "anthropic",
  "openai",
  "openai_resp",
  "deepseek",
  "gemini",
  "ollama",
  "ollama_cloud",
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
  "vertex",
  "github_copilot",
];
const PROVIDER_AUXILIARY_QUERY_KEY = ["supported-adapters"] as const;

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
  network_tested: boolean;
  error?: string;
  testedAt: number;
}

type ProviderFieldErrors = Partial<Record<"id" | "adapter" | "saJson", string>>;

/** Discriminator for `adapter_options.credentials_kind`. */
type CredentialsKind = "bearer" | "service_account_json";

/** Adapters where Awaken can mint OAuth tokens from a service account. */
const SERVICE_ACCOUNT_ADAPTERS = new Set(["vertex"]);

function readCredentialsKind(options: Record<string, unknown> | undefined): CredentialsKind {
  const raw = options?.credentials_kind;
  return raw === "service_account_json" ? "service_account_json" : "bearer";
}

function providerTestSuccessLabel(result: ProviderTestResponse): string {
  return result.network_tested
    ? `Connection OK — ${result.latency_ms}ms`
    : `Config OK — ${result.latency_ms}ms`;
}

function providerTestToastLabel(result: ProviderTestResponse): string {
  return result.network_tested
    ? `Provider connection OK (${result.latency_ms}ms)`
    : `Provider config OK (${result.latency_ms}ms)`;
}

export function ProvidersPage() {
  const { t } = useTranslation();
  const [apiKeyMode, setApiKeyMode] = useState<ApiKeyMode>("replace");
  const [apiKeyDraft, setApiKeyDraft] = useState("");
  const [saJsonDraft, setSaJsonDraft] = useState("");
  const [testing, setTesting] = useState(false);
  const [testStatus, setTestStatus] = useState<TestStatus | null>(null);
  const [errors, setErrors] = useState<ProviderFieldErrors>({});
  const toast = useToast();
  const testingIdRef = useRef<string | null>(null);

  const prepareSave = useCallback(
    (draft: ProviderRecord): ProviderSpec => {
      const payload: ProviderSpec = {
        ...draft,
        timeout_secs: Number(draft.timeout_secs) || 300,
      };

      const kind = readCredentialsKind(
        draft.adapter_options as Record<string, unknown> | undefined,
      );

      if (kind === "service_account_json") {
        // SA JSON path: api_key carries the JSON content. The Replace/Keep
        // semantics still apply (admin may want to rotate the JSON without
        // re-pasting on every edit).
        if (apiKeyMode === "replace" && saJsonDraft.trim().length > 0) {
          payload.api_key = saJsonDraft.trim();
        } else if (apiKeyMode === "clear") {
          payload.api_key = "";
        }
      } else if (apiKeyMode === "replace") {
        if (apiKeyDraft.trim().length > 0) {
          payload.api_key = apiKeyDraft.trim();
        }
      } else if (apiKeyMode === "clear") {
        payload.api_key = "";
      }

      return payload;
    },
    [apiKeyMode, apiKeyDraft, saJsonDraft],
  );

  const crud = useCrudPage<ProviderRecord, ProviderSpec>({
    namespace: "providers",
    entityLabel: "provider",
    prepareSave,
    auxiliaryQueryKey: PROVIDER_AUXILIARY_QUERY_KEY,
    auxiliaryLoaders: () =>
      capabilitiesApi.capabilities().then((result) => {
        const caps = capabilitiesFromResult(result);
        if (!caps) return [FALLBACK_ADAPTERS, []];
        return [caps.supported_adapters ?? FALLBACK_ADAPTERS, caps.models ?? []];
      }),
  });

  const serverAdapters = crud.auxiliaryData[0] as string[] | undefined;
  const serverModels = crud.auxiliaryData[1] as ModelSpec[] | undefined;

  // Derived from the draft so it stays in sync when admin switches adapters
  // or pastes credentials_kind directly into adapter_options.
  const credentialsKind: CredentialsKind = crud.draft
    ? readCredentialsKind(crud.draft.adapter_options as Record<string, unknown> | undefined)
    : "bearer";
  const adapterSupportsServiceAccount =
    crud.draft != null && SERVICE_ACCOUNT_ADAPTERS.has(crud.draft.adapter);

  const adapterOptions = useMemo(() => {
    const options = new Set(serverAdapters ?? FALLBACK_ADAPTERS);
    if (crud.draft?.adapter) {
      options.add(crud.draft.adapter);
    }
    return Array.from(options).sort((left, right) => left.localeCompare(right));
  }, [crud.draft?.adapter, serverAdapters]);

  const {
    search,
    sort,
    pageSize,
    page,
    apply: applyListState,
  } = useListUrlState<ProviderSortKey>(LIST_OPTIONS);

  const filtered = useMemo(
    () =>
      filterBySearch(crud.items, search, (provider) => [
        provider.id,
        provider.adapter,
        provider.base_url,
      ]),
    [crud.items, search],
  );
  const sorted = useMemo(() => sortItems(filtered, sort, SORT_CONFIG), [filtered, sort]);
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
    setSaJsonDraft("");
    setTestStatus(null);
    setErrors({});
  }

  function startEdit(provider: ProviderRecord) {
    crud.startEdit(provider);
    setApiKeyMode("preserve");
    setApiKeyDraft("");
    setSaJsonDraft("");
    setTestStatus(null);
    setErrors({});
  }

  function cancelEdit() {
    crud.cancelEdit();
    setErrors({});
  }

  function validate(draft: ProviderRecord): ProviderFieldErrors {
    const next: ProviderFieldErrors = {};
    if (!draft.id.trim()) next.id = t("validation.required");
    if (!draft.adapter.trim()) next.adapter = t("validation.required");
    return next;
  }

  async function handleTestConnection() {
    if (!crud.draft || !crud.isEditingExisting) return;
    const id = crud.draft.id;
    testingIdRef.current = id;
    setTesting(true);
    try {
      const result = await providersApi.testProvider(id);
      if (testingIdRef.current !== id) return;
      setTestStatus({ ...result, testedAt: Date.now() });
      if (result.ok) {
        toast.success(providerTestToastLabel(result));
      } else {
        toast.error(result.error ?? "Provider test failed");
      }
    } catch (err) {
      if (testingIdRef.current !== id) return;
      const message = err instanceof ConfigApiError ? err.message : "Provider test failed";
      setTestStatus({
        ok: false,
        latency_ms: 0,
        network_tested: false,
        error: message,
        testedAt: Date.now(),
      });
      toast.error(message);
    } finally {
      if (testingIdRef.current === id) setTesting(false);
    }
  }

  async function handleSave() {
    if (!crud.draft) return;
    const next = validate(crud.draft);
    // Extra: SA JSON shape check (cheap; backend will validate authoritatively)
    const kind = readCredentialsKind(
      crud.draft.adapter_options as Record<string, unknown> | undefined,
    );
    if (
      kind === "service_account_json" &&
      apiKeyMode === "replace" &&
      saJsonDraft.trim().length > 0
    ) {
      try {
        const parsed = JSON.parse(saJsonDraft.trim()) as Record<string, unknown>;
        if (typeof parsed.client_email !== "string" || typeof parsed.private_key !== "string") {
          next.saJson = "JSON must include client_email and private_key";
        }
      } catch {
        next.saJson = "Not valid JSON — paste the full file content from GCP IAM";
      }
    }
    setErrors(next);
    if (Object.keys(next).length > 0) return;
    await crud.handleSave();
    setApiKeyDraft("");
    setSaJsonDraft("");
  }

  const [rowTestingId, setRowTestingId] = useState<string | null>(null);

  async function handleRowTest(providerId: string) {
    setRowTestingId(providerId);
    try {
      const result = await providersApi.testProvider(providerId);
      if (result.ok) {
        toast.success(
          result.network_tested
            ? `${providerId} connection OK · ${result.latency_ms}ms`
            : `${providerId} config OK · ${result.latency_ms}ms`,
        );
      } else {
        toast.error(`${providerId}: ${result.error ?? "test failed"}`);
      }
    } catch (err) {
      toast.error(`${providerId}: ${err instanceof Error ? err.message : "test failed"}`);
    } finally {
      setRowTestingId((current) => (current === providerId ? null : current));
    }
  }

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
      <div className="mb-4 flex items-end justify-between gap-4">
        <div className="flex items-baseline gap-3">
          <h1 className="text-[22px] font-bold tracking-title-em text-fg-strong">
            {t("providers.title")}
          </h1>
          <span aria-hidden className="font-mono text-sm text-fg-faint">
            {crud.items.length}
          </span>
        </div>
        <button
          type="button"
          onClick={startCreate}
          className="inline-flex h-9 items-center rounded-sm bg-accent px-3 text-sm font-medium text-accent-text transition hover:opacity-90"
        >
          {t("providers.new")}
        </button>
      </div>

      <ModelProviderSetupBanner
        providerCount={crud.items.length}
        modelCount={serverModels?.length ?? 0}
        onCreateProvider={startCreate}
      />

      {crud.draft ? (
        <section className="mb-6 rounded-sm border border-line bg-surface p-5 shadow-sm">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">
              {crud.isEditingExisting
                ? t("providers.formTitle.edit")
                : t("providers.formTitle.create")}
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
            <Field label={t("providers.fields.providerId")} required error={errors.id}>
              <input
                value={crud.draft.id}
                disabled={crud.isEditingExisting}
                aria-invalid={Boolean(errors.id)}
                onChange={(event) => {
                  const value = event.target.value;
                  crud.setDraft((current) => (current ? { ...current, id: value } : current));
                  if (errors.id) setErrors((e) => ({ ...e, id: undefined }));
                }}
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg disabled:bg-muted disabled:text-fg-soft aria-[invalid=true]:border-tone-error"
              />
            </Field>

            <Field label={t("providers.fields.adapter")} required error={errors.adapter}>
              <select
                value={crud.draft.adapter}
                onChange={(event) => {
                  const nextAdapter = event.target.value;
                  // Switching away from a service-account-capable adapter
                  // resets credentials_kind so the form doesn't end up in
                  // a state the backend will reject (e.g. service_account_json
                  // on openai). Reverting to bearer is the safe default.
                  crud.setDraft((current) => {
                    if (!current) return current;
                    const opts = { ...(current.adapter_options ?? {}) };
                    if (!SERVICE_ACCOUNT_ADAPTERS.has(nextAdapter)) {
                      delete opts.credentials_kind;
                    }
                    return {
                      ...current,
                      adapter: nextAdapter,
                      adapter_options: Object.keys(opts).length > 0 ? opts : undefined,
                    };
                  });
                }}
                className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              >
                {adapterOptions.map((adapter) => (
                  <option key={adapter} value={adapter}>
                    {adapter}
                  </option>
                ))}
              </select>
            </Field>

            {adapterSupportsServiceAccount && (
              <Field label={t("providers.fields.credentialsKind")}>
                <select
                  value={credentialsKind}
                  onChange={(event) => {
                    const nextKind = event.target.value as CredentialsKind;
                    crud.setDraft((current) => {
                      if (!current) return current;
                      const opts = { ...(current.adapter_options ?? {}) };
                      if (nextKind === "bearer") {
                        delete opts.credentials_kind;
                      } else {
                        opts.credentials_kind = nextKind;
                      }
                      return {
                        ...current,
                        adapter_options: Object.keys(opts).length > 0 ? opts : undefined,
                      };
                    });
                    // Cross-mode pivot: clear the other field's draft so
                    // pasting a bearer doesn't leak into the SA JSON
                    // payload (or vice versa).
                    setApiKeyDraft("");
                    setSaJsonDraft("");
                    setApiKeyMode("replace");
                  }}
                  className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
                >
                  <option value="bearer">{t("providers.credentialsKind.bearer")}</option>
                  <option value="service_account_json">
                    {t("providers.credentialsKind.serviceAccountJson")}
                  </option>
                </select>
              </Field>
            )}

            <Field label={t("providers.fields.baseUrl")}>
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
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              />
            </Field>

            <Field label={t("providers.fields.timeout")}>
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
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              />
            </Field>
          </div>

          <div className="mt-5">
            <SecretField
              mode={apiKeyMode === "preserve" ? "keep" : apiKeyMode}
              onModeChange={(next) => setApiKeyMode(next === "keep" ? "preserve" : next)}
              currentlyHasValue={Boolean(crud.isEditingExisting && crud.draft.has_api_key)}
              statusPill={
                crud.draft.has_api_key ? (
                  <SecretStatusPill state={apiKeyMode === "clear" ? "will-clear" : "stored"} />
                ) : crud.isEditingExisting ? (
                  <SecretStatusPill state="no-value" />
                ) : (
                  <SecretStatusPill
                    state={
                      (credentialsKind === "service_account_json"
                        ? saJsonDraft
                        : apiKeyDraft
                      ).trim().length > 0
                        ? "will-set"
                        : "no-value"
                    }
                  />
                )
              }
              labels={{
                title:
                  credentialsKind === "service_account_json"
                    ? `${t("providers.fields.saJson")}${crud.draft.id ? ` — ${crud.draft.id}` : ""}`
                    : `API key${crud.draft.id ? ` — ${crud.draft.id}` : ""}`,
                description:
                  credentialsKind === "service_account_json"
                    ? t("providers.credentialsKind.serviceAccountJsonHint")
                    : crud.isEditingExisting
                      ? "Default mode is Keep — the existing key never goes over the wire while you edit other fields."
                      : "Optional. Leave empty to fall back to the adapter's environment variable.",
                replaceLabel: "Set new key",
                clearLabel: "Clear key",
                keepBody: (
                  <>
                    <strong>Existing credential is preserved.</strong>{" "}
                    <span>Save will not modify the secret; other fields update normally.</span>
                  </>
                ),
                clearBody: (
                  <>
                    <strong>Credential will be removed on save.</strong>{" "}
                    <span>
                      Subsequent calls fall back to the adapter's host environment variable, or fail
                      if none is present.
                    </span>
                  </>
                ),
              }}
              hint={
                credentialsKind === "service_account_json" ? (
                  <>{t("providers.fields.saJsonHint")}</>
                ) : (
                  <>
                    Redacted in the UI and audit payloads. Submitting saves the new value and
                    rotates the runtime client; in-flight requests continue with the prior key.
                  </>
                )
              }
            >
              {credentialsKind === "service_account_json" ? (
                <div>
                  <textarea
                    value={saJsonDraft}
                    onChange={(event) => setSaJsonDraft(event.target.value)}
                    rows={8}
                    placeholder={t("providers.fields.saJsonPh")}
                    aria-invalid={Boolean(errors.saJson)}
                    className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-xs text-fg outline-none transition-colors focus:border-link aria-[invalid=true]:border-tone-error"
                  />
                  {errors.saJson && (
                    <span role="alert" className="mt-1 block text-xs text-tone-error">
                      {errors.saJson}
                    </span>
                  )}
                </div>
              ) : (
                <input
                  type="password"
                  autoComplete="off"
                  value={apiKeyDraft}
                  onChange={(event) => setApiKeyDraft(event.target.value)}
                  placeholder={crud.isEditingExisting ? "sk-…" : "Optional API key"}
                  className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg outline-none transition-colors focus:border-link"
                />
              )}
            </SecretField>
          </div>

          <div className="mt-5 flex flex-wrap items-center gap-3">
            <button
              type="button"
              onClick={() => void handleSave()}
              disabled={crud.saving}
              className="rounded-sm bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {crud.saving ? "Saving..." : "Save"}
            </button>
            <button
              type="button"
              onClick={cancelEdit}
              className="rounded-sm border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft"
            >
              Cancel
            </button>
            {crud.isEditingExisting ? (
              <button
                type="button"
                onClick={() => void handleTestConnection()}
                disabled={testing}
                className="rounded-sm border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
              >
                {testing ? "Testing..." : "Test connection"}
              </button>
            ) : null}
          </div>

          {testStatus !== null ? (
            <div
              className={`mt-3 flex items-center gap-2 rounded-sm border px-4 py-2 text-sm ${
                testStatus.ok
                  ? "border-tone-success/30 bg-tone-success/10 text-tone-success"
                  : "border-tone-error/30 bg-tone-error/10 text-tone-error"
              }`}
            >
              <span className="font-medium">
                {testStatus.ok
                  ? providerTestSuccessLabel(testStatus)
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
          placeholder={t("providers.searchPh")}
        />
        <PageSizeSelect
          value={pageSize}
          onChange={(next) => applyListState({ pageSize: next, page: 1 })}
        />
      </div>

      <div className="overflow-x-auto rounded-sm border border-line bg-surface shadow-card">
        {!crud.loading && crud.items.length === 0 ? (
          <EmptyState
            title={t("providers.empty.title")}
            description={t("providers.empty.desc")}
            actions={
              <button
                type="button"
                onClick={startCreate}
                className="inline-flex h-9 items-center rounded-sm bg-accent px-4 text-sm font-medium text-accent-text transition-colors hover:opacity-90"
              >
                {t("providers.new")}
              </button>
            }
          />
        ) : (
          <>
            <table className="min-w-full">
              <SortableHeader
                columns={COLUMNS}
                sort={sort}
                onSort={(key) => applyListState({ sort: toggleSort(sort, key), page: 1 })}
              />
              <tbody>
                {crud.loading && <SkeletonRows rows={4} cols={COLUMNS.length} />}
                {!crud.loading && view.items.length === 0 && (
                  <tr>
                    <td
                      colSpan={COLUMNS.length}
                      className="px-5 py-8 text-center text-sm text-fg-soft"
                    >
                      No providers match the current filter.
                    </td>
                  </tr>
                )}
                {!crud.loading &&
                  view.items.map((provider) => (
                    <tr key={provider.id} className="border-t border-line text-sm text-fg">
                      <td className="px-5 py-4 font-mono text-fg-strong">{provider.id}</td>
                      <td className="px-5 py-4">{provider.adapter}</td>
                      <td className="px-5 py-4 text-fg-soft">{provider.base_url ?? "Default"}</td>
                      <td className="px-5 py-4 text-fg-soft">
                        {provider.has_api_key ? "Stored" : "Environment / none"}
                      </td>
                      <td className="px-5 py-4 text-fg-soft">
                        {formatRelativeTime(provider.updated_at)}
                      </td>
                      <td className="px-5 py-4">
                        <div className="flex gap-4 text-sm">
                          <button
                            type="button"
                            onClick={() => void handleRowTest(provider.id)}
                            disabled={rowTestingId === provider.id}
                            className="font-medium text-fg-soft transition-colors hover:text-fg-strong disabled:cursor-not-allowed disabled:opacity-60"
                          >
                            {rowTestingId === provider.id ? "Testing…" : "Test"}
                          </button>
                          <button
                            type="button"
                            onClick={() => startEdit(provider)}
                            className="font-medium text-fg-soft transition-colors hover:text-fg-strong"
                          >
                            Edit
                          </button>
                          <button
                            type="button"
                            onClick={() => void crud.handleDelete(provider.id)}
                            className="font-medium text-tone-error/80 transition-colors hover:text-tone-error"
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
