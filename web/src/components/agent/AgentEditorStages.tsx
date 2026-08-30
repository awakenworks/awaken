import type {
  AgentConfig,
  AgentToolsetCap,
  CredentialSource,
  InputBinding,
  ManagedToolsetCap,
  PluginCap,
  ResourceInputDefaultMounts,
  RuntimeCap,
  ToolCap,
} from "../../lib/api/types";
import AgentAdvanced from "./AgentAdvanced";
import AgentBuilder from "./AgentBuilder";
import AgentQuickstart from "./AgentQuickstart";
import type { AdvancedSection, AuthorStage, BuilderSection } from "./agent-editor-navigation";

interface Props {
  stage: AuthorStage;
  builderSection: BuilderSection;
  advancedSection: AdvancedSection;
  config: AgentConfig;
  baseline: Partial<AgentConfig>;
  resources: InputBinding[];
  resourceInputDefaults?: ResourceInputDefaultMounts;
  resourcesError?: Error;
  resourceRevision: number;
  isNew: boolean;
  published: boolean;
  canRun: boolean;
  runPending: boolean;
  publishPending: boolean;
  readyModels: string[];
  allModels: string[];
  runtimes: RuntimeCap[];
  tools: ToolCap[];
  toolsets: ManagedToolsetCap[];
  plugins: PluginCap[];
  credentials: CredentialSource[];
  changed: (path: string) => boolean;
  onPatch: (value: Partial<AgentConfig>) => void;
  onApplyControlledModifications: () => void;
  onRawChange: (value: AgentConfig) => void;
  onManageModels: () => void;
  onReviewRun: (environmentId: string, task: string) => void;
  onBuilderSectionChange: (section: BuilderSection) => void;
  onAdvancedSectionChange: (section: AdvancedSection) => void;
  onResourcesChange: (inputs: InputBinding[]) => void;
  onRetryResources: () => void;
  onIntegrationsValidityChange: (valid: boolean) => void;
  onRawValidityChange: (valid: boolean) => void;
  onPublish: () => void;
}

export default function AgentEditorStages(props: Props) {
  const agentToolset = props.toolsets.find(
    (toolset): toolset is AgentToolsetCap => toolset.type === "agent_toolset_20260401",
  );
  if (props.stage === "quickstart") {
    return (
      <AgentQuickstart
        config={props.config}
        readyModels={props.readyModels}
        allModels={props.allModels}
        runtimes={props.runtimes}
        availableTools={props.tools.map((tool) => tool.id)}
        agentToolset={agentToolset}
        availablePlugins={props.plugins.map((plugin) => plugin.id)}
        canRun={props.canRun}
        idEditable={props.isNew}
        published={props.published}
        runPending={props.runPending}
        onPatch={props.onPatch}
        onManageModels={props.onManageModels}
        onReviewRun={props.onReviewRun}
      />
    );
  }
  if (props.stage === "build") {
    return (
      <AgentBuilder
        section={props.builderSection}
        config={props.config}
        isNew={props.isNew}
        readyModels={props.readyModels}
        allModels={props.allModels}
        runtimes={props.runtimes}
        tools={props.tools}
        toolsets={props.toolsets}
        agentToolset={agentToolset}
        plugins={props.plugins}
        credentials={props.credentials}
        resources={props.resources}
        resourceInputDefaults={props.resourceInputDefaults}
        resourcesError={props.resourcesError}
        changed={props.changed}
        onSectionChange={props.onBuilderSectionChange}
        onPatch={props.onPatch}
        onApplyControlledModifications={props.onApplyControlledModifications}
        onManageModels={props.onManageModels}
        onResourcesChange={props.onResourcesChange}
        onRetryResources={props.onRetryResources}
        onValidityChange={props.onIntegrationsValidityChange}
      />
    );
  }
  return (
    <AgentAdvanced
      section={props.advancedSection}
      config={props.config}
      baseline={props.baseline}
      resources={props.resources}
      plugins={props.plugins}
      credentials={props.credentials}
      resourceRevision={props.resourceRevision}
      isNew={props.isNew}
      published={props.published}
      publishPending={props.publishPending}
      changed={props.changed}
      onSectionChange={props.onAdvancedSectionChange}
      onPatch={props.onPatch}
      onRawChange={props.onRawChange}
      onValidityChange={props.onRawValidityChange}
      onPublish={props.onPublish}
    />
  );
}
