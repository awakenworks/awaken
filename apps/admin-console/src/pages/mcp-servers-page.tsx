import { type ReactNode, useEffect, useMemo, useState } from "react";
import {
  type McpRestartPolicy,
  type McpServerRecord,
  type McpServerSpec,
  configApi,
} from "@/lib/config-api";
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
  const [servers, setServers] = useState<McpServerRecord[]>([]);
  const [draft, setDraft] = useState<McpServerRecord | null>(null);
  const [argsDraft, setArgsDraft] = useState("");
  const [configDraft, setConfigDraft] = useState("{}");
  const [envDraft, setEnvDraft] = useState("{}");
  const [envMode, setEnvMode] = useState<EnvMode>("replace");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const isEditingExisting = useMemo(
    () => (draft ? servers.some((server) => server.id === draft.id) : false),
    [draft, servers],
  );

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const response = await configApi.list<McpServerRecord>("mcp-servers");
        if (!cancelled) {
          setServers(response.items);
          setError(null);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
          setServers([]);
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
    setDraft({
      ...EMPTY_SERVER,
      restart_policy: { ...DEFAULT_RESTART_POLICY },
    });
    setArgsDraft("");
    setConfigDraft("{}");
    setEnvDraft("{}");
    setEnvMode("replace");
  }

  function startEdit(server: McpServerRecord) {
    setDraft({
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

  async function handleSave() {
    if (!draft) {
      return;
    }

    try {
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

      setSaving(true);
      if (isEditingExisting) {
        const updated = await configApi.update<McpServerSpec, McpServerRecord>(
          "mcp-servers",
          draft.id,
          payload,
        );
        setServers((current) =>
          current.map((server) => (server.id === updated.id ? updated : server)),
        );
      } else {
        const created = await configApi.create<McpServerSpec, McpServerRecord>(
          "mcp-servers",
          payload,
        );
        setServers((current) =>
          [...current.filter((server) => server.id !== created.id), created].sort(
            (left, right) => left.id.localeCompare(right.id),
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
    if (!confirm(`Delete MCP server "${id}"?`)) {
      return;
    }

    try {
      await configApi.delete("mcp-servers", id);
      setServers((current) => current.filter((server) => server.id !== id));
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

      {error ? (
        <div className="mb-4 rounded-2xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700">
          {error}
        </div>
      ) : null}

      {draft ? (
        <section className="mb-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <h3 className="text-lg font-semibold text-slate-950">
            {isEditingExisting ? "Edit MCP server" : "Create MCP server"}
          </h3>

          <div className="mt-4 grid gap-4 md:grid-cols-2">
            <Field label="Server ID">
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

            <Field label="Transport">
              <select
                value={draft.transport}
                onChange={(event) =>
                  setDraft((current) =>
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

            {draft.transport === "stdio" ? (
              <>
                <Field label="Command">
                  <input
                    value={String(draft.command ?? "")}
                    onChange={(event) =>
                      setDraft((current) =>
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
                  value={String(draft.url ?? "")}
                  onChange={(event) =>
                    setDraft((current) =>
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
                value={Number(draft.timeout_secs ?? 30)}
                onChange={(event) =>
                  setDraft((current) =>
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
                    {isEditingExisting && draft.has_env
                      ? `Existing keys: ${(draft.env_keys ?? []).join(", ") || "stored"}`
                      : "Provide a JSON object of environment variables."}
                  </p>
                </div>
                {isEditingExisting ? (
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
                  checked={Boolean(draft.restart_policy?.enabled)}
                  onChange={(event) =>
                    setDraft((current) =>
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
                  value={String(draft.restart_policy?.max_attempts ?? "")}
                  onChange={(event) =>
                    setDraft((current) =>
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
                  value={Number(draft.restart_policy?.delay_ms ?? 1000)}
                  onChange={(event) =>
                    setDraft((current) =>
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
                  value={Number(draft.restart_policy?.backoff_multiplier ?? 2)}
                  onChange={(event) =>
                    setDraft((current) =>
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
                  value={Number(draft.restart_policy?.max_delay_ms ?? 30000)}
                  onChange={(event) =>
                    setDraft((current) =>
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
            Loading MCP servers...
          </div>
        ) : servers.length === 0 ? (
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
              {servers.map((server) => (
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
                        onClick={() => void handleDelete(server.id)}
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
