import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useParams, Link } from "react-router";
import { useTranslation } from "react-i18next";
import { configResourceApi, deriveSourceState, toolsApi, type ToolSpec } from "@/lib/api";
import { useConfigMetaQuery, useConfigRecordQuery } from "@/lib/query/hooks/config";
import { qk } from "@/lib/query/keys";
import { adminRoutes } from "@/lib/routes";

const SOFT_WARN_LEN = 400;

export function ToolEditorPage() {
  const { t } = useTranslation();
  const { id = "" } = useParams();
  const queryClient = useQueryClient();

  const [draft, setDraft] = useState<string>("");
  const toolQuery = useConfigRecordQuery<ToolSpec>("tools", id);
  const metaQuery = useConfigMetaQuery("tools", id);

  useEffect(() => {
    if (toolQuery.data) {
      setDraft(toolQuery.data.description);
    }
  }, [toolQuery.data]);

  const patchMutation = useMutation({
    mutationFn: async (description: string) => {
      const next = await toolsApi.patchToolOverrides(id, { description });
      const nextMeta = await configResourceApi.getMeta("tools", id);
      return { next, nextMeta };
    },
    onSuccess: ({ next, nextMeta }) => {
      queryClient.setQueryData(qk.config.get("tools", id), next);
      queryClient.setQueryData(qk.config.meta("tools", id), nextMeta);
      void queryClient.invalidateQueries({ queryKey: qk.config.list("tools") });
      void queryClient.invalidateQueries({ queryKey: qk.config.listMeta("tools") });
    },
  });

  const clearMutation = useMutation({
    mutationFn: async () => {
      const next = await toolsApi.clearToolOverrides(id);
      const nextMeta = await configResourceApi.getMeta("tools", id);
      return { next, nextMeta };
    },
    onSuccess: ({ next, nextMeta }) => {
      queryClient.setQueryData(qk.config.get("tools", id), next);
      queryClient.setQueryData(qk.config.meta("tools", id), nextMeta);
      void queryClient.invalidateQueries({ queryKey: qk.config.list("tools") });
      void queryClient.invalidateQueries({ queryKey: qk.config.listMeta("tools") });
    },
  });

  const builtin = toolQuery.data ?? null;
  const meta = metaQuery.data ?? null;
  const saving = patchMutation.isPending || clearMutation.isPending;

  if (!builtin || !meta) return <p className="p-6 text-fg-soft">Loading…</p>;

  const dirty = draft !== builtin.description;
  const overLength = draft.length >= SOFT_WARN_LEN;

  async function onSave() {
    if (!dirty) return;
    await patchMutation.mutateAsync(draft);
  }

  async function onRevert() {
    await clearMutation.mutateAsync();
  }

  return (
    <section className="flex flex-col gap-4 p-6">
      <header className="flex items-center gap-3">
        <Link to={adminRoutes.tools} className="text-fg-soft underline">
          ← {t("tools.list.title", { defaultValue: "Tools" })}
        </Link>
        <h1 className="text-xl font-semibold">{builtin.name}</h1>
        <span className="text-fg-faint text-sm">{builtin.id}</span>
      </header>

      <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
        <div className="border border-line rounded p-3">
          <h2 className="text-xs font-medium uppercase text-fg-soft">
            {t("tools.editor.builtin", { defaultValue: "Built-in" })}
          </h2>
          <p className="mt-2 whitespace-pre-wrap text-sm text-fg-soft">{builtin.description}</p>
        </div>
        <div className="border border-line rounded p-3">
          <h2 className="text-xs font-medium uppercase text-fg-soft">
            {t("tools.editor.userOverride", { defaultValue: "User override" })}
          </h2>
          <textarea
            className="mt-2 w-full min-h-[140px] rounded border border-line p-2 font-mono text-sm"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            disabled={saving}
          />
          <p className="mt-1 text-[11px] text-fg-faint">{draft.length} chars</p>
          {overLength && (
            <p className="mt-1 text-[11px] text-state-progress">
              {t("tools.editor.lengthWarning", {
                defaultValue:
                  "Long descriptions dilute model attention. Consider moving rules into the agent's system prompt.",
              })}
            </p>
          )}
        </div>
      </div>

      <footer className="flex gap-2">
        <button
          type="button"
          className="rounded bg-fg-strong px-3 py-1.5 text-sm text-canvas disabled:opacity-50"
          onClick={onSave}
          disabled={!dirty || saving}
        >
          {t("common.save", { defaultValue: "Save" })}
        </button>
        <button
          type="button"
          className="rounded border border-line px-3 py-1.5 text-sm disabled:opacity-50"
          onClick={onRevert}
          disabled={saving || deriveSourceState(meta) !== "customized"}
        >
          {t("tools.editor.revert", { defaultValue: "Revert to default" })}
        </button>
      </footer>
    </section>
  );
}
