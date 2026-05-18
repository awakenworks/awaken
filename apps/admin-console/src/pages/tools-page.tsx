import { useMemo, useState } from "react";
import { Link } from "react-router";
import { useTranslation } from "react-i18next";
import { type RecordMeta, type ToolSpec, deriveSourceState } from "@/lib/api";
import { useConfigListQuery, useConfigMetaListQuery } from "@/lib/query/hooks/config";
import { adminRoutes } from "@/lib/routes";

const EMPTY_TOOLS: ToolSpec[] = [];
const EMPTY_META_ITEMS: NonNullable<ReturnType<typeof useConfigMetaListQuery>["data"]> = [];

export function ToolsPage() {
  const { t } = useTranslation();
  const [filterOverridden, setFilterOverridden] = useState(false);
  const toolsQuery = useConfigListQuery<ToolSpec>("tools");
  const metaQuery = useConfigMetaListQuery("tools");

  const items = toolsQuery.data?.items ?? EMPTY_TOOLS;
  const metaItems = metaQuery.data ?? EMPTY_META_ITEMS;
  const metaById = useMemo(() => {
    const map = new Map<string, RecordMeta>();
    for (const meta of metaItems) {
      map.set(meta.id, meta.meta);
    }
    return map;
  }, [metaItems]);
  const loading = toolsQuery.isPending || metaQuery.isPending;

  const filtered = filterOverridden
    ? items.filter(
        (tool) => deriveSourceState(metaById.get(tool.id) ?? ({} as never)) === "customized",
      )
    : items;

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="mb-6 flex items-start justify-between gap-4">
        <h1 className="text-[22px] font-bold tracking-title-em text-fg-strong">
          {t("tools.list.title", { defaultValue: "Tools" })}
        </h1>
        <label className="flex items-center gap-2 text-sm font-medium text-fg">
          <input
            type="checkbox"
            checked={filterOverridden}
            onChange={(e) => setFilterOverridden(e.target.checked)}
          />
          {t("tools.list.filterOverridden", { defaultValue: "Show only customized" })}
        </label>
      </div>

      {loading ? (
        <p className="text-fg-soft">Loading…</p>
      ) : (
        <div className="overflow-x-auto rounded-sm border border-line bg-surface shadow-card">
          <table className="min-w-full text-sm">
            <thead>
              <tr className="border-b border-line">
                <th className="px-5 py-3 text-left text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
                  ID
                </th>
                <th className="px-5 py-3 text-left text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
                  Name
                </th>
                <th className="px-5 py-3 text-left text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
                  Category
                </th>
                <th className="px-5 py-3 text-left text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
                  Description
                </th>
                <th className="px-5 py-3 text-left text-xs font-medium uppercase tracking-[0.18em] text-fg-faint">
                  State
                </th>
              </tr>
            </thead>
            <tbody>
              {filtered.length === 0 && (
                <tr>
                  <td colSpan={5} className="px-5 py-8 text-center text-sm text-fg-soft">
                    {filterOverridden ? "No customized tools." : "No tools found."}
                  </td>
                </tr>
              )}
              {filtered.map((tool) => {
                const meta = metaById.get(tool.id);
                const state = meta ? deriveSourceState(meta) : "builtin";
                return (
                  <tr key={tool.id} className="border-t border-line text-sm text-fg">
                    <td className="px-5 py-4 font-mono text-fg-strong">
                      <Link to={adminRoutes.tool(tool.id)} className="hover:underline">
                        {tool.id}
                      </Link>
                    </td>
                    <td className="px-5 py-4">{tool.name}</td>
                    <td className="px-5 py-4 text-fg-soft">{tool.category ?? "—"}</td>
                    <td className="max-w-md truncate px-5 py-4 text-fg-soft">{tool.description}</td>
                    <td className="px-5 py-4">
                      {state === "customized" ? (
                        <span className="rounded-pill bg-state-progress px-2 py-0.5 text-[10px] uppercase">
                          customized
                        </span>
                      ) : (
                        <span className="text-[11px] text-fg-faint">builtin</span>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
