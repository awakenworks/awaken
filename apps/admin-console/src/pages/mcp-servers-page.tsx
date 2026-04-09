import { useCallback, useState } from "react";
import {
  type McpRestartPolicy,
  type McpServerRecord,
  type McpServerSpec,
} from "@/lib/config-api";
import { useCrudPage } from "@/lib/use-crud-page";
import { Field, ModeButton } from "@/components/form-components";
import {
  parseJsonObject,
  parseLineList,
  parseStringRecord,
  stringifyJsonObject,
  stringifyLineList,
} from "@/lib/config-form-helpers";

type EnvMode = "preserve" | "replace" | "clear";

const DEFAULT_RESTART_POLICY: McpRestartPolicy = {
  enabled: false,
  delay_ms: 1000,
  backoff_multiplier: 2,
  max_delay_ms: 30000,
};

const EMPTY_SERVER: McpServerRecord = {
  id: "",
  transport: "stdio",
  command: "",
  args: [],
  timeout_secs: 30,
  config: {},
  restart_policy: { ...DEFAULT_RESTART_POLICY },
};

export function McpServersPage() {
  const [argsDraft, setArgsDraft] = useState("");
  const [configDraft, setConfigDraft] = useState("{}");
  const [envDraft, setEnvDraft] = useState("{}");
  const [envMode, setEnvMode] = useState<EnvMode>("replace");

  const prepareSave = useCallback(
    (draft: McpServerRecord): McpServerSpec => {
      const payload: McpServerSpec = {
        ...draft,
        command: draft.transport === "stdio" ? String(draft.command ?? "") : undefined,
        url: draft.transport === "http" ? String(draft.url ?? "") : undefined,
        args: draft.transport === "stdio" ? parseLineList(argsDraft) : [],
        config: parseJsonObject<Record<string, unknown>>(configDraft, "Config JSON"),
        timeout_secs: Number(draft.timeout_secs) || 30,
        restart_policy: {
          ...DEFAULT_RESTART_POLICY,
          ...(draft.restart_policy ?? {}),
        },
      };

      if (envMode === "replace") {
        payload.env = parseStringRecord(envDraft, "Environment JSON");
      } else if (envMode === "clear") {
        payload.env = {};
      }

      return payload;
    },
    [argsDraft, configDraft, envMode, envDraft],
  );

  const crud = useCrudPage<McpServerRecord, McpServerSpec>({
    namespace: "mcp-servers",
    entityLabel: "MCP server",
    prepareSave,
  });

  function startCreate() {
    crud.startNew({
      ...EMPTY_SERVER,
      restart_policy: { ...DEFAULT_RESTART_POLICY },
    });
    setArgsDraft("");
    setConfigDraft("{}");
    setEnvDraft("{}");
    setEnvMode("replace");
  }

  function startEdit(server: McpServerRecord) {
    crud.startEdit({
      ...server,
      command: String(server.command ?? ""),
      url: String(server.url ?? ""),
      args: [...(server.args ?? [])],
      config: { ...(server.config ?? {}) },
      restart_policy: {
        ...DEFAULT_RESTART_POLICY,
        ...(server.restart_policy ?? {}),
      },
    });
    setArgsDraft(stringifyLineList(server.args));
    setConfigDraft(stringifyJsonObject(server.config));
    setEnvDraft("{}");
    setEnvMode("preserve");
  }

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div>
          <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
            Runtime Catalog
          </p>
          <h2 className="mt-2 text-3xl font-semibold text-slate-950">
            MCP Servers
          </h2>
        </div>
        <button
          type="button"
          onClick={startCreate}
          className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800"
        >
          New MCP Server
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
            {crud.isEditingExisting ? "Edit MCP server" : "Create MCP server"}
          </h3>

          <div className="mt-4 grid gap-4 md:grid-cols-2">
            <Field label="Server ID">
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

            <Field label="Transport">
              <select
                value={crud.draft.transport}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? {
                          ...current,
                          transport: event.target.value as "stdio" | "http",
                        }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              >
                <option value="stdio">stdio</option>
                <option value="http">http</option>
              </select>
            </Field>

            {crud.draft.transport === "stdio" ? (
              <>
                <Field label="Command">
                  <input
                    value={String(crud.draft.command ?? "")}
                    onChange={(event) =>
                      crud.setDraft((current) =>
                        current
                          ? { ...current, command: event.target.value }
                          : current,
                      )
                    }
                    className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                  />
                </Field>
                <Field label="Arguments (one per line)">
                  <textarea
                    value={argsDraft}
                    onChange={(event) => setArgsDraft(event.target.value)}
                    rows={5}
                    className="w-full rounded-xl border border-slate-300 px-3 py-2 font-mono text-sm text-slate-900 outline-none transition focus:border-slate-500"
                  />
                </Field>
              </>
            ) : (
              <Field label="URL">
                <input
                  value={String(crud.draft.url ?? "")}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current ? { ...current, url: event.target.value } : current,
                    )
                  }
                  className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
            )}

            <Field label="Timeout (seconds)">
              <input
                type="number"
                min={1}
                value={Number(crud.draft.timeout_secs ?? 30)}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? {
                          ...current,
                          timeout_secs: Number(event.target.value) || 30,
                        }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </Field>
          </div>

          <div className="mt-5 grid gap-4 lg:grid-cols-2">
            <Field label="Config JSON">
              <textarea
                value={configDraft}
                onChange={(event) => setConfigDraft(event.target.value)}
                rows={8}
                className="w-full rounded-xl border border-slate-300 px-3 py-2 font-mono text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </Field>

            <div className="rounded-xl border border-slate-200 bg-slate-50 p-4">
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div>
                  <h4 className="text-sm font-semibold text-slate-900">
                    Environment JSON
                  </h4>
                  <p className="mt-1 text-sm text-slate-500">
                    {crud.isEditingExisting && crud.draft.has_env
                      ? `Existing keys: ${(crud.draft.env_keys ?? []).join(", ") || "stored"}`
                      : "Provide a JSON object of environment variables."}
                  </p>
                </div>
                {crud.isEditingExisting ? (
                  <div className="flex flex-wrap gap-2">
                    <ModeButton
                      active={envMode === "preserve"}
                      onClick={() => setEnvMode("preserve")}
                      label="Keep current"
                    />
                    <ModeButton
                      active={envMode === "replace"}
                      onClick={() => setEnvMode("replace")}
                      label="Replace"
                    />
                    <ModeButton
                      active={envMode === "clear"}
                      onClick={() => setEnvMode("clear")}
                      label="Clear"
                    />
                  </div>
                ) : null}
              </div>

              {envMode === "replace" ? (
                <textarea
                  value={envDraft}
                  onChange={(event) => setEnvDraft(event.target.value)}
                  rows={8}
                  className="mt-4 w-full rounded-xl border border-slate-300 px-3 py-2 font-mono text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              ) : (
                <div className="mt-4 rounded-xl border border-dashed border-slate-300 px-3 py-4 text-sm text-slate-500">
                  {envMode === "clear"
                    ? "Saving will remove all stored environment variables."
                    : "Saving will preserve the current environment variables."}
                </div>
              )}
            </div>
          </div>

          <section className="mt-5 rounded-xl border border-slate-200 bg-slate-50 p-4">
            <div className="flex items-center justify-between gap-4">
              <div>
                <h4 className="text-sm font-semibold text-slate-900">Restart Policy</h4>
                <p className="mt-1 text-sm text-slate-500">
                  Controls automatic reconnection when the server becomes unavailable.
                </p>
              </div>
              <label className="flex items-center gap-2 text-sm font-medium text-slate-700">
                <input
                  type="checkbox"
                  checked={Boolean(crud.draft.restart_policy?.enabled)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              enabled: event.target.checked,
                            },
                          }
                        : current,
                    )
                  }
                />
                Enabled
              </label>
            </div>

            <div className="mt-4 grid gap-4 md:grid-cols-2 xl:grid-cols-4">
              <Field label="Max attempts">
                <input
                  type="number"
                  min={0}
                  value={String(crud.draft.restart_policy?.max_attempts ?? "")}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              max_attempts:
                                event.target.value === ""
                                  ? undefined
                                  : Number(event.target.value),
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
              <Field label="Delay (ms)">
                <input
                  type="number"
                  min={0}
                  value={Number(crud.draft.restart_policy?.delay_ms ?? 1000)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              delay_ms: Number(event.target.value) || 0,
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
              <Field label="Backoff multiplier">
                <input
                  type="number"
                  min={1}
                  step="0.1"
                  value={Number(crud.draft.restart_policy?.backoff_multiplier ?? 2)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              backoff_multiplier:
                                Number(event.target.value) || 1,
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
              <Field label="Max delay (ms)">
                <input
                  type="number"
                  min={0}
                  value={Number(crud.draft.restart_policy?.max_delay_ms ?? 30000)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              max_delay_ms: Number(event.target.value) || 0,
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
            </div>
          </section>

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
          <div className="px-5 py-6 text-sm text-slate-500">
            Loading MCP servers...
          </div>
        ) : crud.items.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No managed MCP servers yet.
          </div>
        ) : (
          <table className="min-w-full">
            <thead className="bg-slate-50 text-left text-xs uppercase tracking-wide text-slate-500">
              <tr>
                <th className="px-5 py-3">ID</th>
                <th className="px-5 py-3">Transport</th>
                <th className="px-5 py-3">Endpoint</th>
                <th className="px-5 py-3">Environment</th>
                <th className="px-5 py-3">Actions</th>
              </tr>
            </thead>
            <tbody>
              {crud.items.map((server) => (
                <tr
                  key={server.id}
                  className="border-t border-slate-200 text-sm text-slate-700"
                >
                  <td className="px-5 py-4 font-mono text-slate-950">{server.id}</td>
                  <td className="px-5 py-4">{server.transport}</td>
                  <td className="px-5 py-4 text-slate-500">
                    {server.transport === "stdio"
                      ? [server.command, ...(server.args ?? [])].filter(Boolean).join(" ")
                      : server.url ?? "Unconfigured"}
                  </td>
                  <td className="px-5 py-4 text-slate-500">
                    {server.has_env
                      ? (server.env_keys ?? []).join(", ") || "Stored"
                      : "None"}
                  </td>
                  <td className="px-5 py-4">
                    <div className="flex gap-4">
                      <button
                        type="button"
                        onClick={() => startEdit(server)}
                        className="font-medium text-slate-700 transition hover:text-slate-950"
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        onClick={() => void crud.handleDelete(server.id)}
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
