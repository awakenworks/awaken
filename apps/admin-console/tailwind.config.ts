import type { Config } from "tailwindcss";

const config: Config = {
  content: ["./src/**/*.{ts,tsx}", "./index.html"],
  theme: {
    extend: {
      fontFamily: {
        sans: "var(--aw-font-sans)",
        mono: "var(--aw-font-mono)",
      },
      colors: {
        bg: "var(--aw-bg)",
        canvas: "var(--aw-bg-canvas)",
        surface: "var(--aw-bg-elevated)",
        soft: "var(--aw-bg-soft)",
        muted: "var(--aw-bg-muted)",

        fg: "var(--aw-text)",
        "fg-strong": "var(--aw-text-strong)",
        "fg-soft": "var(--aw-text-soft)",
        "fg-faint": "var(--aw-text-faint)",

        line: "var(--aw-border)",
        "line-strong": "var(--aw-border-strong)",

        accent: "var(--aw-accent)",
        "accent-soft": "var(--aw-accent-soft)",
        "accent-text": "var(--aw-accent-text)",

        link: "var(--aw-link)",
        focus: "var(--aw-focus)",

        agent: "var(--aw-agent)",
        "agent-tint": "var(--aw-agent-tint)",
        "agent-stripe": "var(--aw-agent-stripe)",
        "agent-fg": "var(--aw-agent-fg)",

        "state-backlog": "var(--aw-state-backlog)",
        "state-progress": "var(--aw-state-progress)",
        "state-review": "var(--aw-state-review)",
        "state-done": "var(--aw-state-done)",
        "state-blocked": "var(--aw-state-blocked)",
        "state-paused": "var(--aw-state-paused)",

        "tone-info": "var(--aw-tone-info)",
        "tone-warn": "var(--aw-tone-warn)",
        "tone-error": "var(--aw-tone-error)",
        "tone-success": "var(--aw-tone-success)",

        "phase-resolve": "var(--aw-phase-resolve)",
        "phase-prepare": "var(--aw-phase-prepare)",
        "phase-prompt": "var(--aw-phase-prompt)",
        "phase-stream": "var(--aw-phase-stream)",
        "phase-gate": "var(--aw-phase-gate)",
        "phase-tool": "var(--aw-phase-tool)",
        "phase-commit": "var(--aw-phase-commit)",
        "phase-events": "var(--aw-phase-events)",
        "phase-finalize": "var(--aw-phase-finalize)",

        "chrome-bg": "var(--aw-chrome-bg)",
        "chrome-bg-2": "var(--aw-chrome-bg-2)",
        "chrome-line": "var(--aw-chrome-line)",
        "chrome-fg": "var(--aw-chrome-fg)",
        "chrome-fg-muted": "var(--aw-chrome-fg-muted)",
        "chrome-eyebrow": "var(--aw-chrome-eyebrow)",
      },
      borderRadius: {
        sm: "var(--aw-radius-sm)",
        DEFAULT: "var(--aw-radius-md)",
        md: "var(--aw-radius-md)",
        lg: "var(--aw-radius-lg)",
        xl: "var(--aw-radius-xl)",
        "2xl": "var(--aw-radius-2xl)",
        pill: "var(--aw-radius-pill)",
      },
      boxShadow: {
        card: "var(--aw-shadow-card)",
        "card-lift": "var(--aw-shadow-card-lift)",
        overlay: "var(--aw-shadow-overlay)",
        pop: "var(--aw-shadow-pop)",
        focus: "var(--aw-focus-ring)",
      },
      transitionDuration: {
        instant: "var(--aw-duration-instant)",
        fast: "var(--aw-duration-fast)",
        base: "var(--aw-duration-base)",
        slow: "var(--aw-duration-slow)",
      },
      transitionTimingFunction: {
        ease: "var(--aw-ease)",
        "ease-out": "var(--aw-ease-out)",
      },
    },
  },
  plugins: [],
};

export default config;
