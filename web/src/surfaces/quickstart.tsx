// Workspace · Quickstart: the landing step of the four-step arc. It owns only
// the choice of starting point — describe the Agent in plain English, pick a
// Starter, or start blank — and then hands over to the existing Agent editor,
// which remains the single owner of draft state, publication review, and the
// first run. Nothing is created here, so leaving the page loses nothing.

import { Button, Card } from "@awaken/ui";
import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import QuickstartProgress from "../components/agent/QuickstartProgress";
import { Pill, TextAreaField } from "../components/ui";
import { STARTERS } from "../lib/agent-starters";
import { useApp } from "../lib/app-state";

/** Describing an Agent is the Assistant's existing job: it drafts and persists
 * an unpublished config Agent, then hands the operator to that draft. Seeding
 * its first message keeps one authoring loop instead of a second drafting path
 * that would have to reimplement the same tools. */
export function assistantSeedPath(workspaceId: string, description: string): string {
  const ws = encodeURIComponent(workspaceId || "default");
  return `/w/${ws}/assistant?seed=${encodeURIComponent(description.trim())}`;
}

/** A Starter is carried to the editor by route, not by shared state, so a
 * reload or a shared link reproduces the same seeded draft. */
export function starterDraftPath(workspaceId: string, starterId?: string): string {
  const ws = encodeURIComponent(workspaceId || "default");
  return starterId
    ? `/w/${ws}/agents/new?starter=${encodeURIComponent(starterId)}`
    : `/w/${ws}/agents/new`;
}

export default function QuickstartSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const [description, setDescription] = useState("");

  const submit = () => {
    const described = description.trim();
    nav(described ? assistantSeedPath(wsId, described) : starterDraftPath(wsId));
  };

  return (
    <div className="quickstart-landing">
      <QuickstartProgress current="agent" />
      <div className="quickstart-landing-intro">
        <h2 className="section-title">{app.t("What do you want to build?", "你想构建什么？")}</h2>
        <p className="hint">{app.t(
          "Describe the Agent, or pick a starting point. Either way you land in the editor with a draft you can change before anything is published.",
          "描述你的 Agent，或选择一个起点。两种方式都会进入编辑器，并得到一份可修改的草稿；发布前不会生效。",
        )}</p>
      </div>
      <Card className="quickstart-step">
        <form
          className="quickstart-landing-form"
          onSubmit={(event) => { event.preventDefault(); submit(); }}
        >
          <TextAreaField
            label={app.t("Describe your Agent", "描述你的 Agent")}
            rows={3}
            value={description}
            placeholder={app.t(
              "A support agent that answers questions from our docs and escalates to the on-call channel when it is stuck…",
              "一个从我们的文档回答问题、卡住时升级到值班频道的客服 Agent…",
            )}
            onChange={(event) => setDescription(event.target.value)}
          />
          <div className="row">
            <Button type="submit" variant="primary">
              {app.t("Create agent", "创建 Agent")}
            </Button>
          </div>
          <span className="mut">{app.t(
            "A description opens the Console Assistant, which drafts the Agent and keeps every change under your review.",
            "填写描述会打开 Console 助手，由它起草 Agent；所有改动仍由你审阅。",
          )}</span>
        </form>
      </Card>
      <div className="template-grid">
        {STARTERS.map((starter) => (
          <button
            key={starter.id}
            className="template-card"
            onClick={() => nav(starterDraftPath(wsId, starter.id))}
          >
            <strong>{app.t(starter.title, starter.titleZh)}</strong>
            <span>{app.t(starter.description, starter.descriptionZh)}</span>
            <small className="mut">{app.t(starter.requirement, starter.requirementZh)}</small>
            {starter.outcome && (
              <Pill tone="info">{app.t("Outcome-checked", "结果自评")}</Pill>
            )}
          </button>
        ))}
      </div>
      <p className="hint">
        {app.t("Or ", "或者 ")}
        <Link to={starterDraftPath(wsId)}>
          {app.t("start from a blank Agent", "从空白 Agent 开始")}
        </Link>
        {app.t(".", "。")}
      </p>
    </div>
  );
}
