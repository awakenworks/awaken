import type { ReactNode } from "react";
import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import type { AgentEditorTabId } from "@/lib/editor-tabs";
import {
  readSkillAllowlist,
  SKILLS_ACTIVE_PLUGIN_ID,
  SKILLS_DISCOVERY_PLUGIN_ID,
  withSkillAllowlist,
} from "@/lib/agent-resource-references";
import { isToolAllowed, type AgentSpecCatalog } from "@/lib/tool-catalog";

const SKILL_TOOL_ID = "skill";
const LOAD_SKILL_RESOURCE_TOOL_ID = "load_skill_resource";
const SKILL_SCRIPT_TOOL_ID = "skill_script";

export function SkillsPanel({
  spec,
  capabilities,
  updateField,
  onNavigate,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  onNavigate?: (tab: AgentEditorTabId) => void;
}) {
  if (!capabilities) {
    return (
      <div className="rounded-sm border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        Loading published skills...
      </div>
    );
  }

  const allowlist = readSkillAllowlist(spec);
  const selectedOnly = allowlist !== null;
  const selectedSet = new Set(allowlist ?? []);
  const pluginIds = spec.plugin_ids ?? [];
  const hookFilter = spec.active_hook_filter ?? [];
  const skills = capabilities.skills ?? [];
  const defaultAllowAll =
    spec.allowed_tools === undefined && spec.allowed_tool_patterns === undefined;
  const catalog: AgentSpecCatalog = {
    allowed_tools: spec.allowed_tools ?? undefined,
    allowed_tool_patterns: defaultAllowAll ? ["*"] : (spec.allowed_tool_patterns ?? undefined),
    excluded_tools: spec.excluded_tools ?? undefined,
    excluded_tool_patterns: spec.excluded_tool_patterns ?? undefined,
  };
  const discoveryEnabled = pluginIds.includes(SKILLS_DISCOVERY_PLUGIN_ID);
  const activeInstructionsEnabled = pluginIds.includes(SKILLS_ACTIVE_PLUGIN_ID);
  const discoveryHookActive = hookFilter.length === 0 || hookFilter.includes(SKILLS_DISCOVERY_PLUGIN_ID);
  const activeInstructionsHookActive =
    hookFilter.length === 0 || hookFilter.includes(SKILLS_ACTIVE_PLUGIN_ID);
  const skillToolAllowed = isToolAllowed(catalog, SKILL_TOOL_ID);
  const loadResourceAllowed = isToolAllowed(catalog, LOAD_SKILL_RESOURCE_TOOL_ID);
  const scriptToolAllowed = isToolAllowed(catalog, SKILL_SCRIPT_TOOL_ID);
  const hasRemoteEndpoint = Boolean(spec.endpoint || spec.registry);

  function setPlugin(pluginId: string, checked: boolean) {
    const next = checked
      ? Array.from(new Set([...pluginIds, pluginId]))
      : pluginIds.filter((id) => id !== pluginId);
    updateField("plugin_ids", next);
  }

  function setMode(mode: "all" | "selected") {
    if (mode === "all") {
      updateField("sections", withSkillAllowlist(spec, null));
    } else if (!selectedOnly) {
      updateField("sections", withSkillAllowlist(spec, []));
    }
  }

  function toggleSkill(skillId: string, checked: boolean) {
    const next = checked
      ? Array.from(new Set([...(allowlist ?? []), skillId]))
      : (allowlist ?? []).filter((id) => id !== skillId);
    updateField("sections", withSkillAllowlist(spec, next));
  }

  function addPluginToHookFilter(pluginId: string) {
    if (hookFilter.length === 0 || hookFilter.includes(pluginId)) return;
    updateField("active_hook_filter", [...hookFilter, pluginId]);
  }

  function allowToolIds(toolIds: string[]) {
    updateField("allowed_tools", Array.from(new Set([...(spec.allowed_tools ?? []), ...toolIds])));
    updateField(
      "excluded_tools",
      (spec.excluded_tools ?? []).filter((toolId) => !toolIds.includes(toolId)),
    );
  }

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">Skills</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Select the skill catalog this agent can see, then enable the runtime plugins that
            expose skill discovery and active skill instructions during runs.
          </p>
        </div>
        <div className="rounded-sm border border-line bg-soft px-3 py-2 text-right">
          <div className="font-mono text-lg font-semibold text-fg-strong">
            {selectedOnly ? selectedSet.size : skills.length}
          </div>
          <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
            visible skills
          </div>
        </div>
      </div>

      <div className="mt-4 grid gap-3 md:grid-cols-2">
        <SkillPluginToggle
          label="Skill discovery"
          pluginId={SKILLS_DISCOVERY_PLUGIN_ID}
          checked={pluginIds.includes(SKILLS_DISCOVERY_PLUGIN_ID)}
          onChange={(checked) => setPlugin(SKILLS_DISCOVERY_PLUGIN_ID, checked)}
        />
        <SkillPluginToggle
          label="Active skill instructions"
          pluginId={SKILLS_ACTIVE_PLUGIN_ID}
          checked={pluginIds.includes(SKILLS_ACTIVE_PLUGIN_ID)}
          onChange={(checked) => setPlugin(SKILLS_ACTIVE_PLUGIN_ID, checked)}
        />
      </div>

      <div className="mt-4 space-y-2">
        {!discoveryEnabled ? (
          <SkillWarningCallout
            message="Skills are inactive until `skills-discovery` is enabled. That plugin registers `skill`, `load_skill_resource`, and `skill_script`."
            actions={
              <>
                <WarningButton onClick={() => setPlugin(SKILLS_DISCOVERY_PLUGIN_ID, true)}>
                  Enable discovery
                </WarningButton>
                <WarningButton onClick={() => onNavigate?.("plugins")}>Open Plugins</WarningButton>
              </>
            }
          />
        ) : null}
        {discoveryEnabled && !discoveryHookActive ? (
          <SkillWarningCallout
            message="`skills-discovery` is loaded but excluded by active_hook_filter, so its tools and catalog hook are filtered out."
            actions={
              <>
                <WarningButton onClick={() => addPluginToHookFilter(SKILLS_DISCOVERY_PLUGIN_ID)}>
                  Add to hook filter
                </WarningButton>
                <WarningButton onClick={() => onNavigate?.("plugins")}>Open Plugins</WarningButton>
              </>
            }
          />
        ) : null}
        {discoveryEnabled && !skillToolAllowed ? (
          <SkillWarningCallout
            message="The current tool allow/exclude rules block `skill`; the model cannot activate selected skills."
            actions={
              <>
                <WarningButton onClick={() => allowToolIds([SKILL_TOOL_ID])}>
                  Allow `skill`
                </WarningButton>
                <WarningButton onClick={() => onNavigate?.("tools")}>Open Tools</WarningButton>
              </>
            }
          />
        ) : null}
        {discoveryEnabled && !loadResourceAllowed ? (
          <SkillWarningCallout
            message="`load_skill_resource` is blocked by tool filters; selected skills cannot load references or assets."
            actions={
              <>
                <WarningButton onClick={() => allowToolIds([LOAD_SKILL_RESOURCE_TOOL_ID])}>
                  Allow resources
                </WarningButton>
                <WarningButton onClick={() => onNavigate?.("tools")}>Open Tools</WarningButton>
              </>
            }
          />
        ) : null}
        {!activeInstructionsEnabled ? (
          <SkillWarningCallout
            message="`skills-active-instructions` is disabled. Skill activation can be recorded when discovery is active, but hidden skill instructions will not be injected on later turns."
            actions={
              <>
                <WarningButton onClick={() => setPlugin(SKILLS_ACTIVE_PLUGIN_ID, true)}>
                  Enable instructions
                </WarningButton>
                <WarningButton onClick={() => onNavigate?.("plugins")}>Open Plugins</WarningButton>
              </>
            }
          />
        ) : null}
        {activeInstructionsEnabled && !activeInstructionsHookActive ? (
          <SkillWarningCallout
            message="`skills-active-instructions` is excluded by active_hook_filter, so activated skills will not inject hidden instructions."
            actions={
              <>
                <WarningButton onClick={() => addPluginToHookFilter(SKILLS_ACTIVE_PLUGIN_ID)}>
                  Add to hook filter
                </WarningButton>
                <WarningButton onClick={() => onNavigate?.("plugins")}>Open Plugins</WarningButton>
              </>
            }
          />
        ) : null}
        <SkillWarningCallout
          message={
            hasRemoteEndpoint
              ? "This agent is remote/cloud-backed. Local skill scripts are not executed by this console/runtime; configure executable skills on the remote runtime. Treat this panel as catalog/prompt configuration."
              : "Only filesystem-backed skills in a trusted local runtime can run `skill_script`. Config-managed, embedded, and MCP-backed skills are prompt/resource surfaces and return unsupported for scripts."
          }
          actions={
            hasRemoteEndpoint ? (
              <WarningButton onClick={() => onNavigate?.("advanced")}>
                Inspect endpoint
              </WarningButton>
            ) : undefined
          }
        />
        {scriptToolAllowed && hasRemoteEndpoint ? (
          <SkillWarningCallout
            message="`skill_script` is currently allowed, but remote/cloud-backed agents should not rely on local script execution."
            actions={<WarningButton onClick={() => onNavigate?.("tools")}>Open Tools</WarningButton>}
          />
        ) : null}
      </div>

      <fieldset aria-label="Skill catalog mode" className="mt-5 flex flex-wrap gap-2">
        <label
          className={[
            "min-w-[10rem] cursor-pointer rounded-sm border px-3 py-2 text-sm transition",
            !selectedOnly
              ? "border-accent bg-accent text-accent-text shadow-sm"
              : "border-line bg-surface text-fg hover:border-line-strong hover:bg-soft",
          ].join(" ")}
        >
          <input
            type="radio"
            className="sr-only"
            checked={!selectedOnly}
            onChange={() => setMode("all")}
          />
          <div className="font-semibold">All skills</div>
          <div className={!selectedOnly ? "mt-0.5 text-xs text-fg-faint" : "mt-0.5 text-xs text-fg-soft"}>
            Follow the published registry.
          </div>
        </label>
        <label
          className={[
            "min-w-[10rem] cursor-pointer rounded-sm border px-3 py-2 text-sm transition",
            selectedOnly
              ? "border-accent bg-accent text-accent-text shadow-sm"
              : "border-line bg-surface text-fg hover:border-line-strong hover:bg-soft",
          ].join(" ")}
        >
          <input
            type="radio"
            className="sr-only"
            checked={selectedOnly}
            onChange={() => setMode("selected")}
          />
          <div className="font-semibold">Selected skills</div>
          <div className={selectedOnly ? "mt-0.5 text-xs text-fg-faint" : "mt-0.5 text-xs text-fg-soft"}>
            Store an explicit allowlist.
          </div>
        </label>
      </fieldset>

      {skills.length === 0 ? (
        <div className="mt-4 rounded-sm border border-dashed border-line bg-soft px-4 py-3 text-sm text-fg-soft">
          No skills are registered in the current runtime capabilities.
        </div>
      ) : selectedOnly ? (
        <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
          {skills.map((skill) => (
            <label
              key={skill.id}
              className={[
                "rounded-sm border px-4 py-3 text-sm transition-colors",
                selectedSet.has(skill.id)
                  ? "border-agent-stripe/40 bg-agent-tint text-agent-fg"
                  : "border-line bg-soft text-fg hover:border-line-strong",
              ].join(" ")}
            >
              <div className="flex items-start gap-3">
                <input
                  type="checkbox"
                  checked={selectedSet.has(skill.id)}
                  onChange={(event) => toggleSkill(skill.id, event.target.checked)}
                />
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="font-mono text-xs text-fg-strong">{skill.id}</span>
                    <span className="rounded-pill bg-muted px-2 py-0.5 text-[10px] font-medium text-fg-soft">
                      {skill.context}
                    </span>
                    {skill.model_invocable ? (
                      <span className="rounded-pill bg-agent-tint px-2 py-0.5 text-[10px] font-medium text-agent-fg">
                        model
                      </span>
                    ) : null}
                  </div>
                  <div className="mt-1 truncate text-xs text-fg-soft">{skill.name}</div>
                </div>
              </div>
            </label>
          ))}
        </div>
      ) : (
        <div className="mt-4 rounded-sm border border-line bg-soft px-4 py-3 text-sm text-fg-soft">
          The agent will use every model-invocable skill published by the runtime.
        </div>
      )}
    </section>
  );
}

function SkillWarningCallout({
  message,
  actions,
}: {
  message: string;
  actions?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-center gap-3 rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-4 py-3 text-sm text-tone-warn">
      <div className="min-w-0 flex-1">{message}</div>
      {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
    </div>
  );
}

function WarningButton({
  children,
  onClick,
}: {
  children: ReactNode;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="rounded-sm border border-tone-warn/40 bg-surface px-2 py-1 text-xs font-medium text-tone-warn hover:bg-soft"
    >
      {children}
    </button>
  );
}

function SkillPluginToggle({
  label,
  pluginId,
  checked,
  onChange,
}: {
  label: string;
  pluginId: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <label className="rounded-sm border border-line bg-soft px-4 py-3 text-sm text-fg">
      <div className="flex items-start gap-3">
        <input
          type="checkbox"
          checked={checked}
          onChange={(event) => onChange(event.target.checked)}
        />
        <div>
          <div className="font-medium text-fg-strong">{label}</div>
          <div className="mt-1 font-mono text-xs text-fg-soft">{pluginId}</div>
        </div>
      </div>
    </label>
  );
}
