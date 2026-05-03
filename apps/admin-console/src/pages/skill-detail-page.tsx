import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link, useParams } from "react-router";
import {
  type AgentSpec,
  type SkillInfo,
  configApi,
} from "@/lib/config-api";
import { Pill } from "@/components/ui/pill";
import { adminRoutes } from "@/lib/routes";

export function SkillDetailPage() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const [skill, setSkill] = useState<SkillInfo | null | undefined>(undefined);
  const [agents, setAgents] = useState<AgentSpec[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    setError(null);
    void Promise.all([
      configApi.capabilities(),
      configApi.list<AgentSpec>("agents").catch(() => ({ items: [] as AgentSpec[] })),
    ])
      .then(([caps, ag]) => {
        if (cancelled) return;
        setSkill(caps.skills.find((s) => s.id === id) ?? null);
        setAgents(ag.items);
      })
      .catch((err) => {
        if (!cancelled) setError(err instanceof Error ? err.message : String(err));
      });
    return () => {
      cancelled = true;
    };
  }, [id]);

  const usedByAgents = useMemo(() => {
    if (!id) return [] as AgentSpec[];
    return agents.filter((a) => mentionsSkill(a, id));
  }, [agents, id]);

  if (!id) return <Shell><p className="text-sm text-fg-soft">Missing skill id.</p></Shell>;
  if (error) return <Shell><div className="rounded-md border border-tone-error/30 bg-tone-error/10 p-4 text-sm text-tone-error">{error}</div></Shell>;
  if (skill === undefined) return <Shell><p className="text-sm text-fg-soft">{t("common.loading")}</p></Shell>;
  if (skill === null) return <Shell><p className="text-sm text-fg-soft">{t("trace.notFound")}</p></Shell>;

  return (
    <Shell>
      <header className="mb-4">
        <div className="mb-2 text-xs">
          <Link to={adminRoutes.skills} className="text-fg-soft hover:text-fg">
            ← {t("nav.items.skills")}
          </Link>
        </div>
        <div className="flex items-baseline justify-between gap-4">
          <div>
            {skill.id !== skill.name && (
              <div className="font-mono text-xs text-fg-soft">{skill.id}</div>
            )}
            <h2 className="mt-1 text-2xl font-semibold tracking-title-em text-fg-strong">
              {skill.name}
            </h2>
          </div>
          <div className="flex items-center gap-2 text-xs">
            <Pill tone="info" title={`Context: ${skill.context}`}>{t(`skills.${skill.context}` as const) || skill.context}</Pill>
            {skill.user_invocable && <Pill tone="neutral">{t("skills.user")}</Pill>}
            {skill.model_invocable && <Pill tone="agent">{t("skills.model")}</Pill>}
          </div>
        </div>
        {skill.description && (
          <p className="mt-2 max-w-3xl text-sm leading-6 text-fg-soft">{skill.description}</p>
        )}
      </header>

      {skill.when_to_use && (
        <section className="mt-4 rounded-md border border-line bg-surface p-4 shadow-card">
          <h3 className="text-sm font-semibold text-fg-strong">When to use</h3>
          <p className="mt-2 text-sm leading-6 text-fg-soft">{skill.when_to_use}</p>
        </section>
      )}

      <section className="mt-4 grid gap-4 lg:grid-cols-2">
        <Card title={t("skills.allowedTools")}>
          {skill.allowed_tools.length === 0 ? (
            <p className="text-sm text-fg-soft">{t("skills.noToolFilter")}</p>
          ) : (
            <ul className="flex flex-wrap gap-1.5">
              {skill.allowed_tools.map((tool) => (
                <li key={tool}>
                  <Pill tone="neutral">
                    <span className="font-mono">{tool}</span>
                  </Pill>
                </li>
              ))}
            </ul>
          )}
        </Card>

        <Card title={t("skills.sourcePaths")}>
          {skill.paths.length === 0 ? (
            <p className="text-sm text-fg-soft">{t("skills.unscopedPath")}</p>
          ) : (
            <ul className="space-y-0.5 font-mono text-xs text-fg">
              {skill.paths.map((p) => <li key={p}>{p}</li>)}
            </ul>
          )}
        </Card>
      </section>

      <section className="mt-4 rounded-md border border-line bg-surface p-4 shadow-card">
        <h3 className="text-sm font-semibold text-fg-strong">{t("skills.arguments")}</h3>
        {skill.arguments.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">{t("skills.noArguments")}</p>
        ) : (
          <ul className="mt-3 space-y-2">
            {skill.arguments.map((a) => (
              <li key={a.name} className="rounded-md border border-line bg-soft px-3 py-2">
                <div className="flex flex-wrap items-center gap-2">
                  <code className="font-mono text-sm text-fg-strong">{a.name}</code>
                  <Pill tone={a.required ? "warn" : "neutral"}>
                    {a.required ? t("skills.required") : t("skills.optional")}
                  </Pill>
                </div>
                {a.description && (
                  <p className="mt-1 text-sm text-fg-soft">{a.description}</p>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="mt-4 rounded-md border border-line bg-surface p-4 shadow-card">
        <h3 className="text-sm font-semibold text-fg-strong">Used by</h3>
        {usedByAgents.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">No agents reference this skill.</p>
        ) : (
          <ul className="mt-3 space-y-1.5">
            {usedByAgents.map((a) => (
              <li key={a.id} className="flex items-center justify-between gap-3 rounded-md border border-line bg-soft px-3 py-2">
                <Link to={adminRoutes.agent(a.id)} className="font-mono text-sm text-fg-strong hover:underline">
                  {a.id}
                </Link>
                <span className="font-mono text-xs text-fg-soft">{a.model_id}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="mt-4 rounded-md border border-line bg-surface p-4 shadow-card">
        <h3 className="text-sm font-semibold text-fg-strong">{t("skills.llmPreview")}</h3>
        <pre className="mt-2 max-h-72 overflow-auto rounded-md border border-line bg-code-bg px-3 py-2 font-mono text-[11px] leading-5 text-code-fg">
          {renderInjectionPreview(skill)}
        </pre>
      </section>
    </Shell>
  );
}

function Shell({ children }: { children: React.ReactNode }) {
  return <div className="mx-auto max-w-5xl p-6 md:p-8">{children}</div>;
}

function Card({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-md border border-line bg-surface p-4 shadow-card">
      <h3 className="text-sm font-semibold text-fg-strong">{title}</h3>
      <div className="mt-2">{children}</div>
    </div>
  );
}

/**
 * Same logic as SkillsPage's renderInjectionPreview — duplicated locally to
 * keep the SkillsPage import surface small. Both produce the same output.
 */
function renderInjectionPreview(skill: SkillInfo): string {
  const lines: string[] = [];
  lines.push(`# Skill: ${skill.name}`);
  lines.push(`Identifier: ${skill.id}`);
  lines.push(`Context: ${skill.context}`);
  if (skill.when_to_use) lines.push(`When to use: ${skill.when_to_use}`);
  lines.push("");
  lines.push(skill.description);
  if (skill.arguments.length > 0) {
    lines.push("");
    lines.push("Arguments:");
    for (const a of skill.arguments) {
      lines.push(`  - ${a.name} (${a.required ? "required" : "optional"})${a.description ? ": " + a.description : ""}`);
    }
  }
  return lines.join("\n");
}

function mentionsSkill(agent: AgentSpec, skillId: string): boolean {
  // Skills can be referenced via plugin sections or directly in `skills` field
  // depending on the runtime build. Check both common shapes.
  if ((agent.plugin_ids ?? []).includes(skillId)) return true;
  const sections = (agent as { sections?: Record<string, unknown> }).sections ?? {};
  const skills = (sections.skills ?? sections["skills-discovery"]) as
    | { allowlist?: string[]; ids?: string[] }
    | undefined;
  if (skills?.allowlist?.includes(skillId)) return true;
  if (skills?.ids?.includes(skillId)) return true;
  return false;
}
