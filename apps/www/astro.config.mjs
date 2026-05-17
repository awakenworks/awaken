import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import sitemap from "@astrojs/sitemap";
import mdx from "@astrojs/mdx";
import mermaid from "astro-mermaid";
import { readFileSync } from "node:fs";

/* Hand-maintained Starlight sidebar. Edit sections, ordering, or labels in
 * this JSON; Starlight rebuilds nav from it on every site build. */
const sidebar = JSON.parse(
  readFileSync(new URL("./sidebar.generated.json", import.meta.url), "utf8"),
);

/* Awaken Shiki theme — colours driven by --aw-syntax-* + --aw-code-* tokens.
 * Edit packages/design-tokens/tokens/semantic/colors-{light,dark}.json then
 * resync apps/www/src/styles/shiki/awaken.json. */
const awakenShikiTheme = JSON.parse(
  readFileSync(new URL("./src/styles/shiki/awaken.json", import.meta.url), "utf8"),
);

export default defineConfig({
  site: "https://awaken.dev",
  trailingSlash: "ignore",
  integrations: [
    mermaid({
      theme: "base",
      /* autoTheme would re-init mermaid with theme:'dark' on theme change,
       * which we don't need — themeCSS uses var(--aw-*) tokens that auto-flip
       * via data-theme, so a single render covers both modes. */
      autoTheme: false,
      mermaidConfig: {
        /* themeVariables = raw color anchors that themeCSS can't override
         * (mermaid uses these for derived colors: gradients, alpha mixes,
         * sequence-diagram fills not covered by selectors). The previous
         * defaults were a light lavender/indigo palette that biased the
         * derived chrome toward light mode. Switch to neutral mid-greys so
         * mermaid produces achromatic derivations that look reasonable in
         * BOTH light and dark. The visible foreground/background is still
         * fully token-driven via themeCSS below. Font set to mono to match
         * the Awaken brand (`Type IS the brand`). */
        themeVariables: {
          fontFamily:
            "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
          fontSize: "13px",
          primaryColor:       "#cccccc",
          primaryBorderColor: "#888888",
          primaryTextColor:   "#444444",
          lineColor:          "#999999",
        },
        /* themeCSS is injected as <style> inside the generated SVG, so
         * var(--aw-*) resolves at paint time against the document root
         * — automatic light/dark adaptation without re-rendering. */
        themeCSS: `
          .node rect, .node circle, .node ellipse,
          .node polygon, .node path {
            fill: var(--aw-accent-soft) !important;
            stroke: var(--aw-accent) !important;
            stroke-width: 1px !important;
          }
          .cluster rect {
            fill: var(--aw-bg-canvas) !important;
            stroke: var(--aw-border) !important;
            stroke-width: 1px !important;
          }
          .edgePath .path,
          .flowchart-link {
            stroke: var(--aw-border-strong) !important;
            stroke-width: 1.5px !important;
          }
          .arrowheadPath,
          marker path {
            fill: var(--aw-border-strong) !important;
            stroke: var(--aw-border-strong) !important;
          }
          /* Text — broad sweep. Mermaid uses both SVG <text> and
           * <foreignObject><span> depending on diagram type / browser. */
          text { fill: var(--aw-text) !important; }
          foreignObject *, foreignObject span, foreignObject p,
          foreignObject div, foreignObject {
            color: var(--aw-text) !important;
          }
          .nodeLabel, .edgeLabel, .cluster-label .nodeLabel, .label {
            fill: var(--aw-text) !important;
            color: var(--aw-text) !important;
          }
          .edgeLabel rect, .edgeLabel foreignObject {
            background-color: var(--aw-bg-elevated) !important;
          }
          /* Sequence diagrams */
          .actor {
            fill: var(--aw-accent-soft) !important;
            stroke: var(--aw-accent) !important;
          }
          .actor-line { stroke: var(--aw-border-strong) !important; }
          text.actor, text.actor > tspan,
          .actor-man, .actor-man text {
            fill: var(--aw-text) !important;
          }
          .messageLine0, .messageLine1 {
            stroke: var(--aw-text-soft) !important;
          }
          .messageText {
            fill: var(--aw-text) !important;
            stroke: none !important;
          }
          .labelBox {
            fill: var(--aw-bg-canvas) !important;
            stroke: var(--aw-border) !important;
          }
          .labelText, .labelText > tspan {
            fill: var(--aw-text) !important;
          }
          .loopText, .loopText > tspan,
          .loopLine { stroke: var(--aw-border-strong) !important; }
          .loopText, .loopText > tspan { fill: var(--aw-text) !important; stroke: none !important; }
          .note {
            fill: var(--aw-bg-canvas) !important;
            stroke: var(--aw-border) !important;
          }
          .noteText, .noteText > tspan { fill: var(--aw-text) !important; }
          /* Sequence phase/section bar */
          rect.rect {
            fill: var(--aw-bg-muted) !important;
            stroke: var(--aw-border) !important;
          }
        `,
      },
    }),
    starlight({
      title: "Awaken",
      description: "Production AI agent runtime for Rust.",
      logo: { src: "./src/assets/awaken-mark.svg", replacesTitle: false },
      social: [
        { icon: "github", label: "GitHub", href: "https://github.com/AwakenWorks/awaken" },
      ],
      defaultLocale: "root",
      locales: {
        root: { label: "English", lang: "en" },
        "zh-cn": { label: "简体中文", lang: "zh-CN" },
      },
      customCss: [
        "./src/styles/awaken.css",
        "./src/styles/landing.css",
        "./src/styles/docs.css",
      ],
      editLink: {
        baseUrl: "https://github.com/AwakenWorks/awaken/edit/main/apps/www/",
      },
      lastUpdated: true,
      sidebar,
      components: {
        Footer: "./src/components/CustomFooter.astro",
      },
      expressiveCode: {
        themes: [awakenShikiTheme],
        styleOverrides: {
          borderColor: "var(--aw-border)",
          borderRadius: "var(--aw-radius-md)",
          codeBackground: "var(--aw-code-bg)",
        },
      },
    }),
    mdx(),
    sitemap({
      i18n: {
        defaultLocale: "en",
        locales: { en: "en", "zh-cn": "zh-CN" },
      },
    }),
  ],
});
