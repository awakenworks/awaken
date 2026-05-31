import { Link } from "react-router";
import { adminRoutes } from "@/lib/routes";

interface ModelProviderSetupBannerProps {
  providerCount: number;
  modelCount: number;
  onCreateProvider?: () => void;
  onCreateModel?: () => void;
}

export function ModelProviderSetupBanner({
  providerCount,
  modelCount,
  onCreateProvider,
  onCreateModel,
}: ModelProviderSetupBannerProps) {
  if (providerCount > 0 && modelCount > 0) return null;

  const providerReady = providerCount > 0;
  const modelReady = modelCount > 0;
  const primary =
    providerReady && !modelReady
      ? {
          label: "Set up model",
          to: adminRoutes.models,
          onClick: onCreateModel,
        }
      : {
          label: "Set up provider",
          to: adminRoutes.providers,
          onClick: onCreateProvider,
        };

  return (
    <section className="mb-5 rounded-sm border border-tone-warn/40 bg-tone-warn/10 px-4 py-3 text-sm text-fg">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0 flex-1">
          <div className="font-semibold text-fg-strong">Connect a model to unlock Admin Assistant</div>
          <p className="mt-1 max-w-3xl text-fg-soft">
            Provider stores endpoint and credentials. Model names the upstream model and capability
            metadata. The built-in Admin Assistant automatically binds to the first configured model;
            MCP-only setups stay in full configuration mode until a model is available.
          </p>
          <div className="mt-3 flex flex-wrap gap-2 text-xs">
            <SetupStep label="Provider" ready={providerReady} detail={`${providerCount} configured`} />
            <SetupStep label="Model" ready={modelReady} detail={`${modelCount} configured`} />
            <SetupStep label="Admin Assistant" ready={modelReady} detail={modelReady ? "enabled" : "locked"} />
          </div>
        </div>
        {primary.onClick ? (
          <button
            type="button"
            onClick={primary.onClick}
            className="rounded-sm border border-tone-warn/40 bg-surface px-3 py-2 text-xs font-medium text-fg hover:bg-soft"
          >
            {primary.label}
          </button>
        ) : (
          <Link
            to={primary.to}
            className="rounded-sm border border-tone-warn/40 bg-surface px-3 py-2 text-xs font-medium text-fg hover:bg-soft"
          >
            {primary.label}
          </Link>
        )}
      </div>
    </section>
  );
}

function SetupStep({ label, ready, detail }: { label: string; ready: boolean; detail: string }) {
  return (
    <span
      className={[
        "inline-flex items-center gap-2 rounded-pill border px-2.5 py-1",
        ready
          ? "border-tone-success/35 bg-tone-success/10 text-tone-success"
          : "border-tone-warn/35 bg-surface text-fg-soft",
      ].join(" ")}
    >
      <span className="font-medium">{label}</span>
      <span className="text-[11px] opacity-75">{detail}</span>
    </span>
  );
}
