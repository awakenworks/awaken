import { type AgentSpec } from "@/lib/config-api";

export function AdvancedPanel({ spec }: { spec: AgentSpec }) {
  return (
    <section className="rounded-md border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">JSON Preview</h3>
      <p className="mt-2 text-sm text-fg-soft">
        The exact payload that will be PUT to the config API. Useful for sanity checking before
        publish.
      </p>
      <pre className="mt-4 max-h-[36rem] overflow-auto rounded-xl bg-code-bg p-4 text-xs text-code-fg">
        {JSON.stringify(spec, null, 2)}
      </pre>
    </section>
  );
}
