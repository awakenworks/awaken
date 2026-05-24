import { useEffect, useMemo } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router";
import type { SkillInfo } from "@/lib/api";
import { adminRoutes } from "@/lib/routes";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";
import { useToast } from "@/components/toast-provider";
import { EmptyState } from "@/components/ui/empty-state";
import { PageHeader } from "@/components/ui/page-header";
import { Pill } from "@/components/ui/pill";
import { SkeletonBlock } from "@/components/ui/skeleton";
import { filterSkills, type ContextFilter, type InvocableFilter } from "@/lib/skills-filter";
import { useSkillsFilterUrlState } from "@/lib/list-url-state";

const INVOCABLE_OPTIONS: Array<{ value: InvocableFilter; label: string }> = [
  { value: "any", label: "Any caller" },
  { value: "user", label: "User callable" },
  { value: "model", label: "Model callable" },
  { value: "internal", label: "Internal only" },
];

const CONTEXT_OPTIONS: Array<{ value: ContextFilter; label: string }> = [
  { value: "any", label: "Any context" },
  { value: "inline", label: "Inline" },
  { value: "fork", label: "Fork" },
];

const EMPTY_SKILLS: SkillInfo[] = [];

export function SkillsPage() {
  const { t } = useTranslation();
  const toast = useToast();
  const capabilitiesQuery = useCapabilitiesQuery();
  const skills = capabilitiesQuery.data?.skills ?? EMPTY_SKILLS;
  const loading = capabilitiesQuery.isPending;

  const { apply: applyFilter, ...filter } = useSkillsFilterUrlState();

  useEffect(() => {
    if (capabilitiesQuery.error) {
      toast.error(
        capabilitiesQuery.error instanceof Error
          ? capabilitiesQuery.error.message
          : String(capabilitiesQuery.error),
      );
    }
  }, [capabilitiesQuery.error, toast]);

  const visibleSkills = useMemo(() => filterSkills(skills, filter), [skills, filter]);

  return (
    <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
      <PageHeader title={t("skills.title")} count={skills.length} />

      <section className="mb-4 flex flex-wrap items-end gap-3 rounded-sm border border-line bg-surface p-4 shadow-card">
        <label className="block w-full max-w-sm">
          <span className="sr-only">Search skills</span>
          <input
            type="search"
            value={filter.search}
            onChange={(event) => applyFilter({ search: event.target.value })}
            placeholder="Search by id, name, description, tool, path…"
            className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
          />
        </label>
        <label className="text-xs text-fg-soft">
          <span className="mr-2">Caller</span>
          <select
            value={filter.invocable}
            onChange={(event) => applyFilter({ invocable: event.target.value as InvocableFilter })}
            className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs text-fg outline-none focus:border-fg"
          >
            {INVOCABLE_OPTIONS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <label className="text-xs text-fg-soft">
          <span className="mr-2">Context</span>
          <select
            value={filter.context}
            onChange={(event) => applyFilter({ context: event.target.value as ContextFilter })}
            className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs text-fg outline-none focus:border-fg"
          >
            {CONTEXT_OPTIONS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
        </label>
        <span className="ml-auto text-xs text-fg-soft">
          {visibleSkills.length} of {skills.length} shown
        </span>
      </section>

      {loading ? (
        <div className="grid gap-4 lg:grid-cols-2">
          <SkillCardSkeleton />
          <SkillCardSkeleton />
        </div>
      ) : skills.length === 0 ? (
        <EmptyState title={t("skills.empty.title")} description={t("skills.empty.desc")} />
      ) : visibleSkills.length === 0 ? (
        <EmptyState title={t("skills.noMatches.title")} description={t("skills.noMatches.desc")} />
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          {visibleSkills.map((skill) => (
            <SkillCard key={skill.id} skill={skill} />
          ))}
        </div>
      )}
    </div>
  );
}

function SkillCard({ skill }: { skill: SkillInfo }) {
  return (
    <article className="rounded-sm border border-line bg-surface p-5 shadow-card">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          {skill.id !== skill.name && (
            <div className="font-mono text-sm text-fg-soft">{skill.id}</div>
          )}
          <h3
            className={
              skill.id !== skill.name
                ? "mt-1 text-xl font-semibold text-fg-strong"
                : "text-xl font-semibold text-fg-strong"
            }
          >
            <Link to={adminRoutes.skill(skill.id)} className="hover:underline">
              {skill.name}
            </Link>
          </h3>
        </div>
        <div className="flex flex-wrap gap-2 text-xs">
          <Pill tone="info" title={`Context: ${skill.context}`}>
            {skill.context}
          </Pill>
          {skill.user_invocable && (
            <Pill tone="neutral" title="Users can invoke this skill from the chat surface">
              user
            </Pill>
          )}
          {skill.model_invocable && (
            <Pill tone="agent" title="Model can autonomously invoke this skill">
              model
            </Pill>
          )}
        </div>
      </div>

      <p className="mt-4 text-sm leading-6 text-fg">{skill.description}</p>

      {skill.when_to_use ? (
        <section className="mt-4">
          <SectionLabel label="When to use (activation hint)" />
          <p className="mt-1 text-sm leading-6 text-fg-soft">{skill.when_to_use}</p>
        </section>
      ) : null}

      <div className="mt-4 grid gap-4 sm:grid-cols-2">
        <section>
          <SectionLabel label="Allowed tools" />
          {skill.allowed_tools.length === 0 ? (
            <p className="mt-1 text-sm text-fg-soft">No explicit tool filter.</p>
          ) : (
            <div className="mt-2 flex flex-wrap gap-1.5">
              {skill.allowed_tools.map((toolId) => (
                <Pill key={toolId} tone="neutral">
                  <span className="font-mono">{toolId}</span>
                </Pill>
              ))}
            </div>
          )}
        </section>

        <section>
          <SectionLabel label="Source paths" />
          {skill.paths.length === 0 ? (
            <p className="mt-1 text-sm text-fg-soft">Unscoped (any path).</p>
          ) : (
            <ul className="mt-2 space-y-0.5 font-mono text-xs text-fg">
              {skill.paths.map((p) => (
                <li key={p}>{p}</li>
              ))}
            </ul>
          )}
        </section>
      </div>

      <section className="mt-4">
        <SectionLabel label="Arguments" />
        {skill.arguments.length === 0 ? (
          <p className="mt-1 text-sm text-fg-soft">No formal arguments declared.</p>
        ) : (
          <ul className="mt-2 space-y-2">
            {skill.arguments.map((argument) => (
              <li key={argument.name} className="rounded-sm border border-line bg-soft px-3 py-2">
                <div className="flex flex-wrap items-center gap-2">
                  <code className="text-sm text-fg-strong">{argument.name}</code>
                  <Pill tone={argument.required ? "warn" : "neutral"}>
                    {argument.required ? "required" : "optional"}
                  </Pill>
                </div>
                {argument.description ? (
                  <p className="mt-1 text-sm text-fg-soft">{argument.description}</p>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="mt-4">
        <SectionLabel label="What the LLM sees (prompt-injection preview)" />
        <pre className="mt-2 max-h-48 overflow-auto rounded-sm border border-line bg-code-bg px-3 py-2 font-mono text-[11px] leading-5 text-code-fg">
          {renderInjectionPreview(skill)}
        </pre>
      </section>
    </article>
  );
}

function renderInjectionPreview(skill: SkillInfo): string {
  const lines: string[] = [];
  lines.push(`# Skill: ${skill.name}`);
  lines.push(`Identifier: ${skill.id}`);
  lines.push(`Context: ${skill.context}`);
  lines.push("");
  lines.push(skill.description);
  if (skill.when_to_use) {
    lines.push("");
    lines.push(`When to use: ${skill.when_to_use}`);
  }
  if (skill.arguments.length > 0) {
    lines.push("");
    lines.push("Arguments:");
    for (const a of skill.arguments) {
      lines.push(
        `  - ${a.name}${a.required ? " (required)" : ""}${a.description ? `: ${a.description}` : ""}`,
      );
    }
  }
  if (skill.allowed_tools.length > 0) {
    lines.push("");
    lines.push(`Allowed tools: ${skill.allowed_tools.join(", ")}`);
  }
  return lines.join("\n");
}

function SectionLabel({ label }: { label: string }) {
  return (
    <div className="text-[11px] font-medium uppercase tracking-[0.18em] text-fg-faint">{label}</div>
  );
}

function SkillCardSkeleton() {
  return (
    <article className="rounded-sm border border-line bg-surface p-5 shadow-card">
      <SkeletonBlock height="14px" width="38%" />
      <div className="mt-2">
        <SkeletonBlock height="22px" width="55%" />
      </div>
      <div className="mt-4 space-y-2">
        <SkeletonBlock height="12px" width="92%" />
        <SkeletonBlock height="12px" width="85%" />
        <SkeletonBlock height="12px" width="60%" />
      </div>
    </article>
  );
}
