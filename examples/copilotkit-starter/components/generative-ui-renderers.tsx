"use client";

import { useEffect, useRef } from "react";
import {
  Renderer as JsonRenderRenderer,
  JSONUIProvider,
  type Spec,
  type ComponentRegistry,
} from "@json-render/react";
import { Renderer as OpenUIRenderer } from "@openuidev/react-lang";
import { openuiLibrary } from "@openuidev/react-ui";

// ---------------------------------------------------------------------------
// 1. JSON Render Panel
// ---------------------------------------------------------------------------

/**
 * A generic component registry that renders every component type as a simple
 * <div> with the element's children. This is sufficient for previewing any
 * json-render spec without needing a project-specific catalog.
 */
const fallbackRegistry: ComponentRegistry = new Proxy(
  {} as ComponentRegistry,
  {
    get(_target, _prop) {
      return ({
        element,
        children,
      }: {
        element: { type: string; props: Record<string, unknown> };
        children?: React.ReactNode;
      }) => (
        <div
          data-jr-type={element.type}
          className="rounded-lg border border-blue-200 bg-blue-50/50 p-2 my-1"
        >
          <div className="text-xs font-semibold text-blue-600 mb-1">
            {element.type}
          </div>
          {element.props &&
            Object.entries(element.props).map(([k, v]) =>
              typeof v === "string" || typeof v === "number" ? (
                <div key={k} className="text-sm text-slate-700">
                  {String(v)}
                </div>
              ) : null,
            )}
          {children}
        </div>
      );
    },
  },
);

interface JsonRenderPanelProps {
  /** The activity content — expected to be a json-render Spec ({ root, elements }). */
  data: unknown;
}

export function JsonRenderPanel({ data }: JsonRenderPanelProps) {
  const spec = data as Spec | null;
  if (!spec || typeof spec !== "object") return null;

  return (
    <div data-testid="json-render-panel" className="my-2">
      <JSONUIProvider registry={fallbackRegistry}>
        <JsonRenderRenderer
          spec={spec}
          registry={fallbackRegistry}
          loading={false}
        />
      </JSONUIProvider>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 2. OpenUI Panel
// ---------------------------------------------------------------------------

interface OpenUIPanelProps {
  /** Raw openui-lang response text. */
  response: string;
  /** Whether the LLM is still streaming. */
  isStreaming?: boolean;
}

export function OpenUIPanel({
  response,
  isStreaming = false,
}: OpenUIPanelProps) {
  if (!response) return null;

  return (
    <div data-testid="openui-panel" className="my-2">
      <OpenUIRenderer
        response={response}
        library={openuiLibrary}
        isStreaming={isStreaming}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// 3. A2UI Panel (Lit Web Component wrapper)
// ---------------------------------------------------------------------------

interface A2UIPanelProps {
  /** Array of A2UI protocol messages from the render_a2ui tool output. */
  messages: unknown[];
}

/**
 * Wraps the `<a2ui-surface>` Lit web component in React.
 *
 * Lazily imports the side-effectful UI registration module which defines
 * `<a2ui-surface>` as a custom element. Then feeds messages into an
 * A2uiMessageProcessor and passes the resulting surface + processor to the
 * element via DOM property assignment (React doesn't set complex properties
 * on custom elements through JSX attributes).
 */
export function A2UIPanel({ messages }: A2UIPanelProps) {
  const ref = useRef<HTMLElement | null>(null);
  const processorRef = useRef<{
    processor: unknown;
    initialized: boolean;
  }>({ processor: null, initialized: false });

  // Lazily import the Lit UI bundle (registers <a2ui-surface> custom element)
  useEffect(() => {
    import("@a2ui/lit/ui").catch(() => {
      // UI module may already be registered; ignore duplicate registration errors
    });
  }, []);

  // Process messages and set properties on the custom element
  useEffect(() => {
    if (!ref.current || !Array.isArray(messages) || messages.length === 0)
      return;

    let cancelled = false;

    (async () => {
      try {
        const { v0_8 } = await import("@a2ui/lit");
        if (cancelled) return;

        if (!processorRef.current.initialized) {
          processorRef.current.processor =
            v0_8.Data.createSignalA2uiMessageProcessor();
          processorRef.current.initialized = true;
        }

        const processor = processorRef.current
          .processor as import("@a2ui/web_core/data/model-processor").A2uiMessageProcessor;

        processor.processMessages(messages as never[]);

        const surfaces = processor.getSurfaces();
        const firstEntry = surfaces.entries().next();
        if (firstEntry.done) return;

        const [surfaceId, surface] = firstEntry.value;
        const el = ref.current;
        if (!el) return;

        (el as unknown as Record<string, unknown>).surfaceId = surfaceId;
        (el as unknown as Record<string, unknown>).surface = surface;
        (el as unknown as Record<string, unknown>).processor = processor;
      } catch {
        // Silently handle import/processing errors in development
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [messages]);

  return (
    <div data-testid="a2ui-lit-panel" className="my-2">
      {/* @ts-ignore — custom element not known to React's JSX types */}
      <a2ui-surface ref={ref} />
    </div>
  );
}
