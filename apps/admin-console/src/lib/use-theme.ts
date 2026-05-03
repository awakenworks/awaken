import { useCallback, useEffect, useState } from "react";

export type ThemeChoice = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

const STORAGE_KEY = "awaken.admin.theme";

function readStored(): ThemeChoice {
  if (typeof window === "undefined") return "system";
  const raw = window.localStorage.getItem(STORAGE_KEY);
  if (raw === "light" || raw === "dark" || raw === "system") return raw;
  return "system";
}

function systemPrefersDark(): boolean {
  if (typeof window === "undefined" || !window.matchMedia) return false;
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

function apply(choice: ThemeChoice): ResolvedTheme {
  const resolved: ResolvedTheme =
    choice === "system" ? (systemPrefersDark() ? "dark" : "light") : choice;
  const root = document.documentElement;
  if (choice === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", choice);
  }
  return resolved;
}

export function useTheme() {
  const [choice, setChoice] = useState<ThemeChoice>(() => readStored());
  const [resolved, setResolved] = useState<ResolvedTheme>(() =>
    typeof window === "undefined" ? "light" : (readStored() === "system" ? (systemPrefersDark() ? "dark" : "light") : readStored() as ResolvedTheme),
  );

  useEffect(() => {
    setResolved(apply(choice));
    window.localStorage.setItem(STORAGE_KEY, choice);
  }, [choice]);

  useEffect(() => {
    if (choice !== "system" || typeof window === "undefined" || !window.matchMedia) return;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = () => setResolved(apply("system"));
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, [choice]);

  const cycle = useCallback(() => {
    setChoice((prev) => (prev === "light" ? "dark" : prev === "dark" ? "system" : "light"));
  }, []);

  return { choice, resolved, setChoice, cycle };
}
