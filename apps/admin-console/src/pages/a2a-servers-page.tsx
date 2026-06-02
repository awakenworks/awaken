import { useCallback, useMemo, useState } from "react";
import { Link } from "react-router";
import {
  type A2aAgentCard,
  type A2aServerRecord,
  type A2aServerSpec,
} from "@/lib/config-api";
import { Field } from "@/components/form-components";
import { EmptyState } from "@/components/ui/empty-state";
import { SecretField, SecretStatusPill } from "@/components/ui/secret-field";
import { useCrudPage } from "@/lib/use-crud-page";
import { parseJsonObject, stringifyJsonObject } from "@/lib/config-form-helpers";
import { adminRoutes } from "@/lib/routes";
import { useA2aStatusQuery } from "@/lib/query/hooks/a2a";

type AuthMode = "preserve" | "replace" | "clear";

const EMPTY_SERVER: A2aServerRecord = {
  id: "",
  base_url: "",
  timeout_ms: 300_000,
  options: {},
};

export function A2aServersPage() {
  const [optionsDraft, setOptionsDraft] = useState("{}");
  const [authMode, setAuthMode] = useState<AuthMode>("replace");
  const [tokenDraft, setTokenDraft] = useState("");
  const [errors, setErrors] = useState<Partial<Record<"id" | "base_url", string>>>({});

  const prepareSave = useCallback(
    (draft: A2aServerRecord): A2aServerSpec => {
      const payload: A2aServerSpec = {
        id: draft.id.trim(),
        base_url: draft.base_url.trim(),
        target: draft.target?.trim() || undefined,
        timeout_ms: Number(draft.timeout_ms) || 300_000,
        options: parseJsonObject<Record<string, unknown>>(optionsDraft, "Options JSON"),
      };
      if (authMode === "replace") {
        const token = tokenDraft.trim();
        if (token) payload.auth = { type: "bearer", token };
      } else if (authMode === "clear") {
        payload.auth = null;
      }
      return payload;
    },
    [authMode, optionsDraft, tokenDraft],
  );

  const crud = useCrudPage<A2aServerRecord, A2aServerSpec>({
    namespace: "a2a-servers",
    entityLabel: "A2A server",
    prepareSave,
  });

  const sorted = useMemo(
    () => [...crud.items].sort((left, right) => left.id.localeCompare(right.id)),
    [crud.items],
  );

  function startCreate() {
    crud.startNew({ ...EMPTY_SERVER, options: {} });
    setOptionsDraft("{}");
    setAuthMode("replace");
    setTokenDraft("");
    setErrors({});
  }

  function startEdit(server: A2aServerRecord) {
    crud.startEdit({
      ...server,
      target: server.target ?? "",
      options: { ...(server.options ?? {}) },
    });
    setOptionsDraft(stringifyJsonObject(server.options));
    setAuthMode("preserve");
    setTokenDraft("");
    setErrors({});
  }

  function validateDraft(draft: A2aServerRecord): typeof errors {
    const next: typeof errors = {};
    if (!draft.id.trim()) next.id = "Required";
    if (!draft.base_url.trim()) next.base_url = "Required";
    return next;
  }

  async function handleSave() {
    if (!crud.draft) return;
    const next = validateDraft(crud.draft);
    setErrors(next);
    if (Object.keys(next).length > 0) return;
    await crud.handleSave();
  }

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
      <div className="mb-4 flex items-end justify-between gap-4">
        <div className="flex items-baseline gap-3">
          <h1 className="text-[22px] font-bold tracking-title-em text-fg-strong">A2A Servers</h1>
          <span aria-hidden className="font-mono text-sm text-fg-faint">
            {crud.items.length}
          </span>
        </div>
        <button
          type="button"
          onClick={startCreate}
          className="inline-flex h-9 items-center rounded-sm bg-accent px-3 text-sm font-medium text-accent-text transition hover:opacity-90"
        >
          New A2A server
        </button>
      </div>

      {crud.error ? (
        <div className="mb-4 rounded-sm border border-tone-error/30 bg-tone-error/10 px-4 py-3 text-sm text-tone-error">
          {crud.error}
        </div>
      ) : null}

      {crud.draft ? (
        <section className="mb-6 rounded-sm border border-line bg-surface p-5 shadow-sm">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">
              {crud.isEditingExisting ? "Edit A2A server" : "Create A2A server"}
            </h3>
            {crud.isEditingExisting && crud.draft.id ? (
              <Link
                to={adminRoutes.auditLogForResource(`a2a-servers/${crud.draft.id}`)}
                className="text-sm font-medium text-fg-soft transition hover:text-fg"
              >
                History
              </Link>
            ) : null}
          </div>

          <div className="mt-4 grid gap-4 md:grid-cols-2">
            <Field label="Server ID" required error={errors.id}>
              <input
                value={crud.draft.id}
                disabled={crud.isEditingExisting}
                aria-invalid={Boolean(errors.id)}
                onChange={(event) => {
                  const value = event.target.value;
                  crud.setDraft((current) => (current ? { ...current, id: value } : current));
                  if (errors.id) setErrors((current) => ({ ...current, id: undefined }));
                }}
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg disabled:bg-muted disabled:text-fg-soft aria-[invalid=true]:border-tone-error"
              />
            </Field>
            <Field label="Base URL" required error={errors.base_url}>
              <input
                value={crud.draft.base_url}
                aria-invalid={Boolean(errors.base_url)}
                onChange={(event) => {
                  const value = event.target.value;
                  crud.setDraft((current) =>
                    current ? { ...current, base_url: value } : current,
                  );
                  if (errors.base_url) {
                    setErrors((current) => ({ ...current, base_url: undefined }));
                  }
                }}
                placeholder="https://agents.example.com"
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg aria-[invalid=true]:border-tone-error"
              />
            </Field>
            <Field label="Timeout (ms)">
              <input
                type="number"
                min={1}
                value={Number(crud.draft.timeout_ms ?? 300_000)}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? { ...current, timeout_ms: Number(event.target.value) || 300_000 }
                      : current,
                  )
                }
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              />
            </Field>
            <Field label="Optional target">
              <input
                value={String(crud.draft.target ?? "")}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current ? { ...current, target: event.target.value } : current,
                  )
                }
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              />
            </Field>
          </div>

          <div className="mt-5 grid gap-4 lg:grid-cols-2">
            <Field label="Options JSON">
              <textarea
                value={optionsDraft}
                onChange={(event) => setOptionsDraft(event.target.value)}
                rows={8}
                className="w-full rounded-sm border border-line-strong px-3 py-2 font-mono text-sm text-fg outline-none transition focus:border-fg"
              />
            </Field>

            <SecretField
              mode={authMode === "preserve" ? "keep" : authMode}
              onModeChange={(next) => setAuthMode(next === "keep" ? "preserve" : next)}
              currentlyHasValue={Boolean(crud.isEditingExisting && crud.draft.has_auth)}
              statusPill={
                crud.draft.has_auth ? (
                  <SecretStatusPill state={authMode === "clear" ? "will-clear" : "stored"} />
                ) : (
                  <SecretStatusPill state="no-value" />
                )
              }
              labels={{
                title: `A2A bearer token${crud.draft.id ? ` - ${crud.draft.id}` : ""}`,
                description: crud.isEditingExisting
                  ? "Leave existing auth untouched, replace the bearer token, or clear it."
                  : "Optional bearer token used when fetching the A2A agent card and running remote agents.",
                replaceLabel: "Replace token",
                clearLabel: "Clear token",
                keepBody: (
                  <>
                    <strong>Existing token is preserved.</strong>{" "}
                    <span>Save will not touch stored auth.</span>
                  </>
                ),
                clearBody: (
                  <>
                    <strong>The stored token will be removed on save.</strong>{" "}
                    <span>Future discovery requests will be unauthenticated.</span>
                  </>
                ),
              }}
            >
              <input
                type="password"
                value={tokenDraft}
                onChange={(event) => setTokenDraft(event.target.value)}
                className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg outline-none transition-colors focus:border-link"
              />
            </SecretField>
          </div>

          <div className="mt-5 flex gap-3">
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
              onClick={crud.cancelEdit}
              className="rounded-sm border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft"
            >
              Cancel
            </button>
          </div>
        </section>
      ) : null}

      <div className="space-y-3">
        {!crud.loading && sorted.length === 0 ? (
          <EmptyState
            title="No A2A servers configured"
            description="Connect an A2A server to discover remote A2A agents automatically."
            actions={
              <button
                type="button"
                onClick={startCreate}
                className="inline-flex h-9 items-center rounded-sm bg-accent px-4 text-sm font-medium text-accent-text transition-colors hover:opacity-90"
              >
                New A2A server
              </button>
            }
          />
        ) : null}

        {crud.loading ? (
          <div className="rounded-sm border border-line bg-surface p-5 text-sm text-fg-soft">
            Loading A2A servers...
          </div>
        ) : (
          sorted.map((server) => (
            <A2aServerRow
              key={server.id}
              server={server}
              onEdit={() => startEdit(server)}
              onDelete={() => void crud.handleDelete(server.id)}
            />
          ))
        )}
      </div>
    </div>
  );
}

function A2aServerRow({
  server,
  onEdit,
  onDelete,
}: {
  server: A2aServerRecord;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const statusQuery = useA2aStatusQuery(server.id);
  const status = statusQuery.data;
  const card = status?.card ?? null;
  const skills = card?.skills ?? [];
  const interfaces = card?.supportedInterfaces ?? [];

  return (
    <section className="rounded-sm border border-line bg-surface shadow-card">
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-5 py-4">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="font-mono text-base font-semibold text-fg-strong">{server.id}</h2>
            <span
              className={[
                "inline-block h-2 w-2 rounded-pill",
                statusQuery.isPending
                  ? "bg-fg-faint"
                  : status?.connected
                    ? "bg-state-done"
                    : "bg-state-blocked",
              ].join(" ")}
              title={
                statusQuery.isPending
                  ? "Loading"
                  : status?.connected
                    ? "Connected"
                    : status?.last_error || "Disconnected"
              }
            />
          </div>
          <div className="mt-1 break-all text-xs text-fg-soft">{server.base_url}</div>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => void statusQuery.refetch()}
            className="rounded-sm border border-line-strong px-3 py-1.5 text-xs font-medium text-fg transition hover:bg-soft"
          >
            Refresh card
          </button>
          <button
            type="button"
            onClick={onEdit}
            className="rounded-sm border border-line-strong px-3 py-1.5 text-xs font-medium text-fg transition hover:bg-soft"
          >
            Edit
          </button>
          <button
            type="button"
            onClick={onDelete}
            className="rounded-sm border border-tone-error/40 px-3 py-1.5 text-xs font-medium text-tone-error transition hover:bg-tone-error/10"
          >
            Delete
          </button>
        </div>
      </div>

      <div className="grid gap-4 p-5 lg:grid-cols-[minmax(0,1fr),20rem]">
        <div>
          {statusQuery.isError ? (
            <StatusError message={String(statusQuery.error)} />
          ) : status && !status.connected ? (
            <StatusError message={status.last_error || "A2A card unavailable"} />
          ) : card ? (
            <AgentCardSummary card={card} />
          ) : (
            <div className="text-sm text-fg-soft">Loading agent card...</div>
          )}
        </div>
        <div className="space-y-3">
          <Metric label="Interfaces" value={interfaces.length} />
          <Metric label="Skills" value={skills.length} />
          <Metric label="Auth" value={server.has_auth ? "Bearer" : "None"} />
        </div>
      </div>
    </section>
  );
}

function AgentCardSummary({ card }: { card: A2aAgentCard }) {
  const interfaces = card.supportedInterfaces ?? [];
  const skills = card.skills ?? [];
  return (
    <div>
      <div className="flex flex-wrap items-baseline gap-2">
        <h3 className="text-lg font-semibold text-fg-strong">{card.name}</h3>
        <span className="rounded bg-soft px-1.5 font-mono text-[10px] text-fg-soft">
          {card.version}
        </span>
      </div>
      <p className="mt-1 text-sm text-fg-soft">{card.description}</p>

      <div className="mt-4 grid gap-4 md:grid-cols-2">
        <InventoryList
          title="A2A interfaces"
          empty="No supported interfaces advertised"
          items={interfaces.map((entry) => ({
            key: `${entry.protocolBinding}:${entry.url}`,
            title: `${entry.protocolBinding} ${entry.protocolVersion}`,
            body: entry.agentId ? `${entry.url} · ${entry.agentId}` : entry.url,
          }))}
        />
        <InventoryList
          title="A2A skills"
          empty="No skills advertised"
          items={skills.map((skill) => ({
            key: skill.id,
            title: skill.name,
            body: skill.description ?? skill.id,
          }))}
        />
      </div>
    </div>
  );
}

function InventoryList({
  title,
  empty,
  items,
}: {
  title: string;
  empty: string;
  items: Array<{ key: string; title: string; body: string }>;
}) {
  return (
    <div>
      <h4 className="text-xs font-semibold uppercase tracking-eyebrow text-fg-soft">{title}</h4>
      {items.length === 0 ? (
        <div className="mt-2 text-sm text-fg-faint">{empty}</div>
      ) : (
        <ul className="mt-2 space-y-2">
          {items.map((item) => (
            <li key={item.key} className="rounded-sm border border-line bg-soft px-3 py-2">
              <div className="text-sm font-medium text-fg">{item.title}</div>
              <div className="mt-0.5 break-all text-xs text-fg-soft">{item.body}</div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Metric({ label, value }: { label: string; value: string | number }) {
  return (
    <div className="rounded-sm border border-line bg-soft px-3 py-2">
      <div className="text-[10px] font-semibold uppercase tracking-eyebrow text-fg-faint">
        {label}
      </div>
      <div className="mt-1 text-sm font-semibold text-fg-strong">{value}</div>
    </div>
  );
}

function StatusError({ message }: { message: string }) {
  return (
    <div className="rounded-sm border border-tone-error/30 bg-tone-error/10 px-3 py-2 text-sm text-tone-error">
      {message}
    </div>
  );
}
