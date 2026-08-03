import { titleForPath } from "./navigation/paths";

export interface AssistantSurfaceContext {
  label: string;
  topic: string;
  path: string;
  suggestions: string[];
}

const DEFAULT_SUGGESTIONS = [
  "What should I do next in this Workspace?",
  "Explain how Agents, Sessions, Environments, and models fit together.",
  "Help me create an Agent from my goal.",
];

const SURFACES: Array<{
  pattern: RegExp;
  topic: string;
  suggestions: string[];
}> = [
  { pattern: /\/agents\/new$/, topic: "author-agent", suggestions: ["Help me choose the right starting template.", "What must be ready before my first real run?", "Draft this Agent from my requirements."] },
  { pattern: /\/agents\/[^/]+$/, topic: "agent", suggestions: ["Explain this Agent's current configuration.", "Help me improve this Agent safely.", "Why can't I publish or run this Agent?"] },
  { pattern: /\/agents$/, topic: "agent", suggestions: ["Create an Agent from my goal.", "Explain primary and auxiliary Agents.", "Which Agent should I open next?"] },
  { pattern: /\/skills$/, topic: "skills", suggestions: ["When should I create a Skill?", "Does this Skill need a Sandbox?", "How do I attach a Skill to an Agent?"] },
  { pattern: /\/files$/, topic: "files-artifacts", suggestions: ["How do I attach a file to an Agent?", "How are folders represented?", "What is the difference between Files and Artifacts?"] },
  { pattern: /\/artifacts$/, topic: "files-artifacts", suggestions: ["Where did this Artifact come from?", "How do I inspect its producing Session?", "Can an Artifact become a reusable input?"] },
  { pattern: /\/memory\/dreams\//, topic: "memory-dreams", suggestions: ["Explain this Dream result.", "What should I review before using it?", "How do Dreams differ from Memory Stores?"] },
  { pattern: /\/memory$/, topic: "memory-dreams", suggestions: ["How do I create and edit a Memory Store?", "When should I run a Dream?", "How do I bind memory to an Agent?"] },
  { pattern: /\/sessions\/[^/]+$/, topic: "inspect-run", suggestions: ["Help me diagnose this Session.", "Explain Child runs and Trace.", "Where are this Session's inputs and outputs?"] },
  { pattern: /\/sessions$/, topic: "sessions-deployments", suggestions: ["How do I start a real Session?", "Why is an Agent unavailable here?", "When should I use a Deployment instead?"] },
  { pattern: /\/deployments$/, topic: "sessions-deployments", suggestions: ["Help me configure a safe schedule.", "What does Run once verify?", "How do Deployment runs appear in Sessions?"] },
  { pattern: /\/environments$/, topic: "environments", suggestions: ["Create an Environment for my workload.", "Which packages and network policy do I need?", "When is a Sandbox required?"] },
  { pattern: /\/models$/, topic: "connect-model", suggestions: ["Help me connect a model provider.", "Why is this model not runnable?", "How do I test a compatible endpoint?"] },
  { pattern: /\/mcp$/, topic: "mcp", suggestions: ["How do I add an MCP server?", "Why is an MCP server not active in a Session?", "When should prompts become Skills?"] },
  { pattern: /\/protocols$/, topic: "api-access", suggestions: ["How do I call a published Agent from my app?", "Which API or protocol should I use?", "Where do I create an API key?"] },
  { pattern: /\/a2a-servers$/, topic: "a2a", suggestions: ["Explain A2A federation.", "How do I verify an Agent Card?", "How is A2A different from an auxiliary Agent?"] },
  { pattern: /\/access$/, topic: "api-access", suggestions: ["Create the least-privileged API key plan.", "Which role should this service use?", "How do I revoke a key safely?"] },
  { pattern: /\/vaults$/, topic: "runtime-secrets", suggestions: ["Which Vault type should I use?", "How should I name and classify this Vault?", "Why is a Vault unavailable in this selector?"] },
  { pattern: /\/settings$/, topic: "settings", suggestions: ["Where is this setting owned?", "Explain the Workspace configuration boundaries.", "What should I configure next?"] },
  { pattern: /\/overview$/, topic: "overview", suggestions: DEFAULT_SUGGESTIONS },
  { pattern: /\/assistant$/, topic: "assistant", suggestions: DEFAULT_SUGGESTIONS },
];

export function assistantContextForLocation(pathname: string, search = ""): AssistantSurfaceContext {
  const surface = SURFACES.find((candidate) => candidate.pattern.test(pathname));
  const title = titleForPath(pathname).title || "Console";
  return {
    label: title,
    topic: surface?.topic ?? "overview",
    path: `${pathname}${search}`,
    suggestions: surface?.suggestions ?? DEFAULT_SUGGESTIONS,
  };
}
