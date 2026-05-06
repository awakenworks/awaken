import { useEffect, useState } from "react";
import { useParams, Link } from "react-router";
import { useTranslation } from "react-i18next";
import {
  type ToolSpec,
  type RecordMeta,
  configApi,
  deriveSourceState,
} from "@/lib/config-api";
import { adminRoutes } from "@/lib/routes";

const SOFT_WARN_LEN = 400;

export function ToolEditorPage() {
  const { t } = useTranslation();
  const { id = "" } = useParams();

  const [builtin, setBuiltin] = useState<ToolSpec | null>(null);
  const [meta, setMeta] = useState<RecordMeta | null>(null);
  const [draft, setDraft] = useState<string>("");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    let mounted = true;
    Promise.all([configApi.getTool(id), configApi.getMeta("tools", id)]).then(
      ([spec, m]) => {
        if (!mounted) return;
        setBuiltin(spec);
        setMeta(m);
        setDraft(spec.description);
      },
    );
    return () => {
      mounted = false;
    };
  }, [id]);

  if (!builtin || !meta) return <p className="p-6 text-fg-soft">Loading…</p>;

  const dirty = draft !== builtin.description;
  const overLength = draft.length >= SOFT_WARN_LEN;

  async function onSave() {
    if (!dirty) return;
    setSaving(true);
    try {
      const next = await configApi.patchToolOverrides(id, { description: draft });
      const nextMeta = await configApi.getMeta("tools", id);
      setBuiltin(next);
      setMeta(nextMeta);
    } finally {
      setSaving(false);
    }
  }

  async function onRevert() {
    setSaving(true);
    try {
      const next = await configApi.clearToolOverrides(id);
      const nextMeta = await configApi.getMeta("tools", id);
      setBuiltin(next);
      setDraft(next.description);
      setMeta(nextMeta);
    } finally {
      setSaving(false);
    }
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
          <p className="mt-2 whitespace-pre-wrap text-sm text-fg-soft">
            {builtin.description}
          </p>
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
          <p className="mt-1 text-[11px] text-fg-faint">
            {draft.length} chars
          </p>
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
