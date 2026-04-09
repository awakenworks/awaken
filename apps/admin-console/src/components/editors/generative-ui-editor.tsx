import type { GenerativeUiConfig } from "@/lib/plugin-config";
import {
  normalizeGenerativeUiConfig,
  serializeGenerativeUiConfig,
} from "@/lib/plugin-config";
import { Field, Hint } from "@/components/form-components";

export function GenerativeUiConfigEditor({
  value,
  onChange,
}: {
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const config = normalizeGenerativeUiConfig(value);

  function update(nextConfig: GenerativeUiConfig) {
    onChange(serializeGenerativeUiConfig(nextConfig));
  }

  return (
    <div className="space-y-4">
      <Field label="Catalog ID">
        <input
          type="text"
          value={config.catalog_id}
          onChange={(event) =>
            update({ ...config, catalog_id: event.target.value })
          }
          placeholder="https://a2ui.org/specification/v0_8/standard_catalog_definition.json"
          className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
        />
      </Field>
      <Field label="Examples">
        <textarea
          value={config.examples}
          onChange={(event) => update({ ...config, examples: event.target.value })}
          rows={6}
          className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
        />
      </Field>
      <Field label="Instruction override">
        <textarea
          value={config.instructions}
          onChange={(event) =>
            update({ ...config, instructions: event.target.value })
          }
          rows={8}
          className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
        />
      </Field>
      <Hint>
        Leave fields empty to keep the plugin defaults. Setting an instruction
        override takes precedence over catalog and examples.
      </Hint>
    </div>
  );
}
