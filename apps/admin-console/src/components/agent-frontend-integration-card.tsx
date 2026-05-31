const DOCS_BASE = "https://awakenworks.github.io/awaken";

export function AgentFrontendIntegrationCard({ agentId }: { agentId: string | undefined }) {
  const id = agentId?.trim();
  if (!id) return null;

  const encoded = encodeURIComponent(id);
  const aiSdkEndpoint = `/v1/ai-sdk/agents/${encoded}/runs`;
  const agUiEndpoint = `/v1/ag-ui/agents/${encoded}/runs`;

  return (
    <section
      className="rounded-sm border border-line bg-surface p-4 text-sm shadow-sm"
      data-testid="agent-frontend-integration-card"
    >
      <div className="text-xs font-medium uppercase tracking-eyebrow text-fg-soft">
        Frontend integration
      </div>
      <h2 className="mt-2 text-base font-semibold text-fg-strong">Connect this agent</h2>
      <p className="mt-2 text-sm leading-6 text-fg-soft">
        After the sandbox conversation behaves as expected, call the saved agent through these
        protocol routes. Use the agent-scoped route when the UI or API client should always talk to{" "}
        <span className="font-mono text-fg">{id}</span>.
      </p>
      <div className="mt-3 space-y-2">
        <EndpointHint label="AI SDK v6" value={aiSdkEndpoint} />
        <EndpointHint label="AG-UI / CopilotKit" value={agUiEndpoint} />
      </div>
      <div className="mt-4 flex flex-wrap gap-2">
        <a
          href={`${DOCS_BASE}/how-to/integrate-ai-sdk-frontend/`}
          target="_blank"
          rel="noreferrer"
          className="rounded-sm border border-line bg-soft px-3 py-1.5 text-xs font-medium text-fg hover:bg-muted"
        >
          AI SDK guide
        </a>
        <a
          href={`${DOCS_BASE}/how-to/integrate-copilotkit-ag-ui/`}
          target="_blank"
          rel="noreferrer"
          className="rounded-sm border border-line bg-soft px-3 py-1.5 text-xs font-medium text-fg hover:bg-muted"
        >
          AG-UI guide
        </a>
        <a
          href={`${DOCS_BASE}/reference/protocols/ai-sdk-v6/`}
          target="_blank"
          rel="noreferrer"
          className="rounded-sm border border-line bg-soft px-3 py-1.5 text-xs font-medium text-fg hover:bg-muted"
        >
          Endpoint reference
        </a>
        <a
          href={`${DOCS_BASE}/reference/http-api/`}
          target="_blank"
          rel="noreferrer"
          className="rounded-sm border border-line bg-soft px-3 py-1.5 text-xs font-medium text-fg hover:bg-muted"
        >
          HTTP API
        </a>
      </div>
    </section>
  );
}

function EndpointHint({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-sm border border-line bg-soft px-3 py-2">
      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
        {label}
      </div>
      <code className="mt-1 block break-all text-xs text-code-fg">{value}</code>
    </div>
  );
}
