import { useEffect, useState } from "react";
import { type SkillInfo, configApi } from "@/lib/config-api";

export function SkillsPage() {
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const capabilities = await configApi.capabilities();
        if (!cancelled) {
          setSkills(capabilities.skills);
          setError(null);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
          setSkills([]);
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

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <header className="mb-6">
        <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
          Runtime Catalog
        </p>
        <h2 className="mt-2 text-3xl font-semibold text-slate-950">
          Skill Registry
        </h2>
        <p className="mt-2 max-w-3xl text-sm text-slate-600">
          This registry is a live snapshot of the skills currently attached to
          the runtime. It is read-only here because skills are not stored in the
          managed config namespaces.
        </p>
      </header>

      {error ? (
        <div className="mb-4 rounded-2xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700">
          {error}
        </div>
      ) : null}

      {loading ? (
        <div className="rounded-2xl border border-slate-200 bg-white p-6 text-sm text-slate-500 shadow-sm">
          Loading skill registry...
        </div>
      ) : skills.length === 0 ? (
        <div className="rounded-2xl border border-slate-200 bg-white p-6 text-sm text-slate-500 shadow-sm">
          No skills are currently registered.
        </div>
      ) : (
        <div className="grid gap-4 lg:grid-cols-2">
          {skills.map((skill) => (
            <article
              key={skill.id}
              className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm"
            >
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div>
                  <div className="font-mono text-sm text-slate-500">{skill.id}</div>
                  <h3 className="mt-1 text-xl font-semibold text-slate-950">
                    {skill.name}
                  </h3>
                </div>
                <div className="flex flex-wrap gap-2 text-xs font-medium">
                  <Badge label={skill.context} />
                  <Badge
                    label={skill.user_invocable ? "user callable" : "user hidden"}
                  />
                  <Badge
                    label={skill.model_invocable ? "model callable" : "model hidden"}
                  />
                </div>
              </div>

              <p className="mt-4 text-sm leading-6 text-slate-700">
                {skill.description}
              </p>

              {skill.when_to_use ? (
                <section className="mt-4">
                  <SectionLabel label="When To Use" />
                  <p className="mt-1 text-sm leading-6 text-slate-600">
                    {skill.when_to_use}
                  </p>
                </section>
              ) : null}

              <div className="mt-4 grid gap-4 sm:grid-cols-2">
                <section>
                  <SectionLabel label="Allowed Tools" />
                  {skill.allowed_tools.length === 0 ? (
                    <p className="mt-1 text-sm text-slate-500">No explicit tool filter.</p>
                  ) : (
                    <div className="mt-2 flex flex-wrap gap-2">
                      {skill.allowed_tools.map((toolId) => (
                        <code
                          key={toolId}
                          className="rounded-full bg-slate-100 px-2.5 py-1 text-xs text-slate-700"
                        >
                          {toolId}
                        </code>
                      ))}
                    </div>
                  )}
                </section>

                <section>
                  <SectionLabel label="Activation" />
                  <dl className="mt-1 space-y-1 text-sm text-slate-600">
                    <div>
                      <dt className="inline font-medium text-slate-700">Hint:</dt>{" "}
                      <dd className="inline">
                        {skill.argument_hint?.trim() || "None"}
                      </dd>
                    </div>
                    <div>
                      <dt className="inline font-medium text-slate-700">Model override:</dt>{" "}
                      <dd className="inline">
                        {skill.model_override?.trim() || "None"}
                      </dd>
                    </div>
                    <div>
                      <dt className="inline font-medium text-slate-700">Paths:</dt>{" "}
                      <dd className="inline">
                        {skill.paths.length > 0 ? skill.paths.join(", ") : "Unscoped"}
                      </dd>
                    </div>
                  </dl>
                </section>
              </div>

              <section className="mt-4">
                <SectionLabel label="Arguments" />
                {skill.arguments.length === 0 ? (
                  <p className="mt-1 text-sm text-slate-500">
                    No formal arguments declared.
                  </p>
                ) : (
                  <ul className="mt-2 space-y-2">
                    {skill.arguments.map((argument) => (
                      <li
                        key={argument.name}
                        className="rounded-xl border border-slate-200 bg-slate-50 px-3 py-2"
                      >
                        <div className="flex flex-wrap items-center gap-2">
                          <code className="text-sm text-slate-900">{argument.name}</code>
                          {argument.required ? (
                            <Badge label="required" subtle />
                          ) : (
                            <Badge label="optional" subtle />
                          )}
                        </div>
                        {argument.description ? (
                          <p className="mt-1 text-sm text-slate-600">
                            {argument.description}
                          </p>
                        ) : null}
                      </li>
                    ))}
                  </ul>
                )}
              </section>
            </article>
          ))}
        </div>
      )}
    </div>
  );
}

function SectionLabel({ label }: { label: string }) {
  return (
    <div className="text-xs font-semibold uppercase tracking-[0.18em] text-slate-500">
      {label}
    </div>
  );
}

function Badge({ label, subtle = false }: { label: string; subtle?: boolean }) {
  return (
    <span
      className={[
        "rounded-full px-2.5 py-1",
        subtle
          ? "bg-slate-100 text-slate-600"
          : "bg-[#f4efe6] text-slate-700",
      ].join(" ")}
    >
      {label}
    </span>
  );
}
