import { useApp } from "../../lib/app-state";

/** Capability-gated placeholder: the IA keeps the page; the backend face is
 * not mounted yet (design/web-ui.md §7). */
export default function GatedPage({
  title,
  endpoint,
  note,
}: {
  title: string;
  endpoint: string;
  note?: string;
}) {
  const app = useApp();
  return (
    <div className="card">
      <h2>{title}</h2>
      <div className="banner gate">
        <span>◌</span>
        <span>
          {app.t("This surface is designed but its backend face is not mounted yet: ", "该界面已设计,后端面尚未就绪:")}
          <code>{endpoint}</code>
          {note ? ` — ${note}` : null}{" "}
          {app.t("See design/web-ui.md §7 for the roadmap.", "路线见 design/web-ui.md §7。")}
        </span>
      </div>
    </div>
  );
}
