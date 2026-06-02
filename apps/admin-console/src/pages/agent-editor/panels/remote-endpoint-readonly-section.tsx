import { type AgentSpec } from "@/lib/config-api";
import { redactEndpointForDisplay } from "@/lib/agent-editor-helpers";

export function RemoteEndpointReadonlySection({
  endpoint,
}: {
  endpoint: NonNullable<AgentSpec["endpoint"]>;
}) {
  const redacted = redactEndpointForDisplay(endpoint);
  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Remote endpoint</h3>
      <p className="mt-2 max-w-xl text-sm text-fg-soft">
        This agent is configured to run on a remote backend. Endpoint configuration is preserved
        across edits but not editable from this form.
      </p>
      <p
        className="mt-2 max-w-xl text-xs text-fg-soft"
        data-testid="remote-endpoint-redaction-notice"
      >
        Credential fields (bearer tokens, API keys, etc.) are masked as
        <span className="mx-1 font-mono">***</span>
        below. Manage the real credential in the remote backend record.
      </p>
      <pre
        className="mt-3 overflow-auto rounded-sm bg-code-bg p-3 text-xs text-code-fg"
        data-testid="remote-endpoint-readonly"
      >
        {JSON.stringify(redacted, null, 2)}
      </pre>
    </section>
  );
}
