//! The console help corpus behind `admin_explain_console` (Q4 option b): a BOUNDED,
//! curated set of topics, each a fixed 5-section explanation (what / why / where / how /
//! gotchas). Chosen over a retrieval corpus because the console's help domain is small +
//! stable, accuracy matters more than open-ended coverage, and the assistant runs on a
//! tight context budget (weak models) — so the size stays controlled and the answers are
//! exact, never a retrieval guess. `explain(None)` returns the index so the model knows
//! what it can ask about; `explain(Some(topic))` returns one topic (or the index + an
//! `unknown` note). Read-only, secret-free.

use serde_json::{Value, json};

/// One curated help topic. `key` is stable + matches a real surface/concept.
struct Topic {
    key: &'static str,
    title: &'static str,
    what: &'static str,
    why: &'static str,
    location: &'static str,
    how: &'static str,
    gotcha: &'static str,
}

const TOPICS: &[Topic] = &[
    Topic {
        key: "overview",
        title: "How the console is organized",
        what: "The console is grouped by what you're doing: Author (Agents), Building blocks, Operate, Supply, Observe, Govern.",
        why: "An agent is the hero object; everything else is a block it composes from, the infra it runs on, or how you watch it run.",
        location: "The left rail groups.",
        how: "Author an agent → assemble it from building blocks → give it a model (Supply) → put it into operation (Operate) → watch it (Observe).",
        gotcha: "Models/Credentials are inference SUPPLY, not agent building blocks — they live under Supply, not with Environments/MCP/Memory.",
    },
    Topic {
        key: "agent",
        title: "What an agent is",
        what: "A declarative configuration (system prompt, model, tools, plugins, policy, resources) that compiles to a content-addressed runnable config.",
        why: "Agents are configured, not coded — you change behavior by editing config and re-publishing, never by shipping code.",
        location: "Author ▸ Agents. Open one to edit; the left rail sections are Overview / Behavior / Tools / Resources.",
        how: "New agent (or Draft with AI) → set model + system prompt + tools/plugins → Save → Publish (review the diff).",
        gotcha: "Draft (Saved) ≠ Published. A session runs the PUBLISHED config; Save alone doesn't affect running sessions.",
    },
    Topic {
        key: "connect-model",
        title: "Connecting a model",
        what: "A runnable model = a provider + its endpoint + an offering (the model id) + a credential.",
        why: "Model config is data the platform owns — declared here, never in environment variables or code.",
        location: "Supply ▸ Models (author provider/endpoint/offering) and Supply ▸ Inference credentials (enter the key).",
        how: "In Models, author the provider, its endpoint (dialect + base URL), and the offering (model id + context window) → in Credentials, seal the API key for that provider.",
        gotcha: "The secret is write-only — sealed on entry and never shown again. There is no env-var path (an ironclad rule).",
    },
    Topic {
        key: "building-blocks",
        title: "Building blocks an agent uses",
        what: "Reusable resources an agent references: Environments (where it runs), MCP servers (external tools), A2A (other agents), Skills, Memory stores.",
        why: "They're authored once and bound to many agents, keeping agents small and composable.",
        location: "The Building blocks rail group; bindings are set per agent in the agent editor's Resources tab.",
        how: "Create the block in its surface, then bind it on the agent (Resources tab, or ask the assistant to bind it).",
        gotcha: "A memory_store binds at agent config (mounts into every session it runs); it can't be attached to an already-running session.",
    },
    Topic {
        key: "tools",
        title: "Tools and tool presentation",
        what: "The tools an agent may call (host built-ins + MCP tools), plus per-tool presentation: alias (rename for the model), description override, and defer (send its schema only when opened).",
        why: "Presentation shapes how the model sees a tool without changing what it does — clearer names, tighter descriptions, cheaper context via defer.",
        location: "Agent editor ▸ Tools.",
        how: "Pick tools from the catalog (or add an MCP tool id), then add an override to rename/redescribe/defer a specific tool.",
        gotcha: "An override's target is the tool's canonical id (a catalog id or mcp__server__tool) — it applies to static and MCP tools uniformly.",
    },
    Topic {
        key: "permissions",
        title: "The permission gate",
        what: "A default decision (Allow / Ask / Deny) plus an ordered rule table (glob pattern → behavior), enforced at runtime on every tool call.",
        why: "It's how you make an agent safe: require human approval for risky calls, or hard-deny some outright.",
        location: "Agent editor ▸ Tools ▸ Permissions.",
        how: "Set the default, then add rules like Bash(*rm*) → Deny.",
        gotcha: "Deny always wins; otherwise the most specific match decides. Ask surfaces an approval in the session (human-in-the-loop).",
    },
    Topic {
        key: "plugins",
        title: "Behaviors (plugins): compaction, memory, state machine",
        what: "Named runtime behaviors composed onto an agent: auto-compaction (summarize old turns), memory recall, and a state machine (constrain tool-call order + emit system reminders).",
        why: "They add durable, background behavior without touching the prompt — e.g. keep a long agent within its context budget, or force read-before-write.",
        location: "Agent editor ▸ Behavior (each a card with its own form).",
        how: "Toggle a behavior on and fill its config; the compaction prompt and the state-machine transitions are authored here.",
        gotcha: "The state machine's emit entries ARE the 'system reminder' mechanism — a reminder is a transition emit, not a separate plugin.",
    },
    Topic {
        key: "sessions-vs-deployments",
        title: "Sessions vs deployments",
        what: "A Session is one run/conversation instance. A Deployment is a standing rule (agent × environment × trigger/schedule) that PRODUCES sessions.",
        why: "You test ad-hoc in sessions; you put an agent into ongoing operation with a deployment.",
        location: "Operate ▸ Sessions (instances) and Operate ▸ Deployments (rules).",
        how: "Create a deployment by picking a published agent + an environment + a schedule; each firing produces a session.",
        gotcha: "A deployment is not a single run — it's the recurring binding; the runs it makes show up under Sessions.",
    },
    Topic {
        key: "resources",
        title: "Session resources and artifacts",
        what: "A session's mounted resources (the memory stores / files / repos it was given) and its output artifacts (files it wrote under outputs/).",
        why: "It's how you see what an agent had access to and what it produced.",
        location: "A session's detail page ▸ Files view (alongside Chat and Trace).",
        how: "Open a session → Files → see mounted resources + download artifacts.",
        gotcha: "Output artifacts only appear after a run writes to the outputs mount; a chat-only session has none.",
    },
    Topic {
        key: "author-agent",
        title: "Authoring an agent (start to publish)",
        what: "Create an agent from empty or by describing it to the assistant, then review and publish it.",
        why: "Publishing compiles the config to a reproducible fingerprint that sessions run.",
        location: "Author ▸ Agents ▸ New agent, or the ✦ Draft with AI entry / FAB.",
        how: "Set id + model + system prompt → add tools/plugins/permissions/resources → Save → Publish (a diff preview shows exactly what changes).",
        gotcha: "The assistant can draft/patch everything but never publishes — publishing is your explicit action.",
    },
    Topic {
        key: "bind-resource",
        title: "Binding a memory store / file / repo / skill",
        what: "Attach a data-plane resource to an agent so it's mounted into every session the agent runs.",
        why: "It gives the agent durable memory, reference files, a code checkout, or a skill bundle.",
        location: "Agent editor ▸ Resources, or ask the assistant ('bind the project-notes memory store').",
        how: "Add a binding (kind + which resource + mount path + access) and Save resources.",
        gotcha: "The resource must already exist (create the memory store / upload the file first); memory + repo bindings can be read_write and write back on harvest.",
    },
    Topic {
        key: "test-agent",
        title: "Testing an agent live",
        what: "Run the agent in an in-editor Sandbox against its real model.",
        why: "Prove the behavior before you rely on it — no separate deploy needed.",
        location: "Agent editor ▸ ▷ Try it (opens a live session drawer).",
        how: "Try it → Start session → send a message → watch the real reply.",
        gotcha: "It needs a credentialed model; if none is configured the sandbox says so instead of faking a reply.",
    },
    Topic {
        key: "inspect-run",
        title: "Inspecting a run (Trace)",
        what: "A session rendered as spans — the run's inference calls, tool calls, and status transitions.",
        why: "Transparency: see exactly what the agent did and why, step by step.",
        location: "A session's detail page ▸ Trace view.",
        how: "Open a session → Trace → expand spans (each carries its JSON).",
        gotcha: "Durable-op detail requires AWAKEN_INGRESS=durable; the base trace is always available.",
    },
    Topic {
        key: "assistant",
        title: "The AI assistant (this copilot)",
        what: "An in-console copilot that authors agents in plain English: it drafts a full config (tools, plugins, permissions, resource bindings) and saves it as an unpublished draft.",
        why: "Fastest way to go from intent to a reviewable agent; it reads the live platform so it only proposes real models/tools/resources.",
        location: "The ✦ FAB (any page) or Author ▸ Agents ▸ Draft with AI. Opened over an agent editor, it refines THAT agent.",
        how: "Describe the agent (or a change) → it drafts/patches → open the draft in the editor → review → publish.",
        gotcha: "It never publishes and never reads a stored secret — those stay your responsibility.",
    },
    Topic {
        key: "guardrails",
        title: "Platform guardrails",
        what: "Invariants the platform enforces: model config only via the console (no env), secrets write-only, publishing is human-only, tenants are isolated.",
        why: "They make the platform safe to operate and auditable.",
        location: "Enforced everywhere; secrets via the sealed credential entry, publish via the agent editor.",
        how: "Follow the flows — enter keys in Credentials, publish from the editor.",
        gotcha: "There is deliberately no way to set a model key by environment variable or to have the assistant publish for you.",
    },
];

/// The help payload for `admin_explain_console`. `None` (or an unknown topic) returns the
/// index of topic keys + titles; a known topic returns its full 5-section explanation.
#[must_use]
pub fn explain(topic: Option<&str>) -> Value {
    let index = || -> Value {
        json!({
            "topics": TOPICS.iter().map(|t| json!({ "topic": t.key, "title": t.title }))
                .collect::<Vec<_>>()
        })
    };
    match topic.map(str::trim).filter(|t| !t.is_empty()) {
        None => index(),
        Some(key) => match TOPICS.iter().find(|t| t.key.eq_ignore_ascii_case(key)) {
            Some(t) => json!({
                "topic": t.key,
                "title": t.title,
                "what": t.what,
                "why": t.why,
                "where": t.location,
                "how": t.how,
                "gotchas": t.gotcha,
            }),
            None => {
                let mut idx = index();
                idx["unknown_topic"] = json!(key);
                idx["note"] = json!("No such topic; pick one of `topics` above.");
                idx
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_lists_all_topics_when_no_topic() {
        let v = explain(None);
        assert_eq!(v["topics"].as_array().unwrap().len(), TOPICS.len());
    }

    #[test]
    fn known_topic_returns_the_five_sections() {
        let v = explain(Some("connect-model"));
        assert_eq!(v["topic"], "connect-model");
        for k in ["what", "why", "where", "how", "gotchas"] {
            assert!(v[k].as_str().is_some_and(|s| !s.is_empty()), "missing {k}");
        }
    }

    #[test]
    fn unknown_topic_falls_back_to_the_index_with_a_note() {
        let v = explain(Some("does-not-exist"));
        assert_eq!(v["unknown_topic"], "does-not-exist");
        assert!(v["topics"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn topic_keys_are_unique() {
        let mut keys: Vec<_> = TOPICS.iter().map(|t| t.key).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate topic key");
    }
}
