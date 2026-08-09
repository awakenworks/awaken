import { useLocation } from "react-router";
import { useApp } from "../../lib/app-state";
import { pageIntentForPath } from "../../lib/page-intents";

export default function PageIntentHeader() {
  const app = useApp();
  const location = useLocation();
  const intent = pageIntentForPath(location.pathname);
  if (!intent) return null;
  return (
    <header className="page-purpose">
      <div>
        <h1>{app.t(intent.title, intent.titleZh)}</h1>
        <p>{app.t(intent.description, intent.descriptionZh)}</p>
      </div>
      <aside>
        <small>{app.t("Recommended next step", "建议下一步")}</small>
        <strong>{app.t(intent.outcome, intent.outcomeZh)}</strong>
      </aside>
    </header>
  );
}
