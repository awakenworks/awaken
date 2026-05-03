import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { en } from "./en";
import { zhCN } from "./zh-CN";

export type Locale = "en" | "zh-CN";

const STORAGE_KEY = "awaken.admin.locale";

function detectInitialLocale(): Locale {
  if (typeof window === "undefined") return "en";
  const stored = window.localStorage.getItem(STORAGE_KEY);
  if (stored === "en" || stored === "zh-CN") return stored;
  const nav = (navigator.languages?.[0] ?? navigator.language ?? "").toLowerCase();
  if (nav.startsWith("zh")) return "zh-CN";
  return "en";
}

void i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    "zh-CN": { translation: zhCN },
  },
  lng: detectInitialLocale(),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
  returnNull: false,
});

export function setLocale(locale: Locale) {
  void i18n.changeLanguage(locale);
  if (typeof window !== "undefined") {
    window.localStorage.setItem(STORAGE_KEY, locale);
    const html = document.documentElement;
    html.lang = locale === "zh-CN" ? "zh-CN" : "en";
    html.dataset.locale = locale;
  }
}

export function currentLocale(): Locale {
  return (i18n.language as Locale) || "en";
}

if (typeof document !== "undefined") {
  document.documentElement.lang = currentLocale() === "zh-CN" ? "zh-CN" : "en";
  document.documentElement.dataset.locale = currentLocale();
}

export default i18n;
