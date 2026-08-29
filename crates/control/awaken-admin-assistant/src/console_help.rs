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
        what: "The rail follows the operator journey: Control plane, Build, Resources, Run, Connect, and Govern.",
        why: "Each configuration has one clear owner, while the Agent remains the publishable unit that composes capabilities and resources.",
        location: "The left navigation rail; Overview shows the Connect → Build → Run → Observe journey.",
        how: "Connect a runnable model → create and publish an Agent → run a Session in an Environment → inspect its Trace and Artifacts.",
        gotcha: "Environment is selected per run. MCP is configured inside an Agent. Files and Artifacts are different projections of one file catalog.",
    },
    Topic {
        key: "agent",
        title: "What an agent is",
        what: "A declarative configuration (system prompt, model, tools, plugins, policy, resources) that compiles to a content-addressed executable snapshot.",
        why: "Agents are configured, not coded — you change behavior by editing config and re-publishing, never by shipping code.",
        location: "Build ▸ Agents. Open one to move through Quickstart / Build / Advanced.",
        how: "Quickstart reaches a first real run; Build owns prompt, tools, Skills, Memory, MCP, and permissions; Advanced owns orchestration, plugin details, raw config, and release diff.",
        gotcha: "Draft (Saved) ≠ Published. A session runs the PUBLISHED config; Save alone doesn't affect running sessions.",
    },
    Topic {
        key: "connect-model",
        title: "Connecting a model",
        what: "A runnable model = a provider + its endpoint + an offering (the model id) + a credential.",
        why: "Model config is data the platform owns — declared here, never in environment variables or code.",
        location: "Connect ▸ Models & providers. Provider connections own endpoint, dialect, credential, and discovered model offerings.",
        how: "Choose a provider or compatible protocol → enter a human-readable credential name and key → discover offerings → use Test on a runnable model.",
        gotcha: "Provider keys are write-only and separate from Runtime secrets. A compatible endpoint may implement OpenAI or Anthropic protocol even when the provider brand differs.",
    },
    Topic {
        key: "building-blocks",
        title: "Building blocks an agent uses",
        what: "Reusable resources an agent references: Environments (where it runs), MCP servers (external tools), A2A (other agents), Skills, Memory stores.",
        why: "They're authored once and bound to many agents, keeping agents small and composable.",
        location: "Build, Resources, Run, and Connect surfaces; bindings are authored inside Agent ▸ Build.",
        how: "Create the reusable resource on its owning page, then bind it under Agent ▸ Build ▸ Memory & resources or Skills & MCP.",
        gotcha: "A memory_store binds at agent config (mounts into every session it runs); it can't be attached to an already-running session.",
    },
    Topic {
        key: "tools",
        title: "Tools and tool presentation",
        what: "The tools an agent may call (host built-ins + MCP tools), plus exact appearance overrides and eager/on-demand schema exposure.",
        why: "Presentation shapes how the model sees a tool without changing what it does — clearer names, tighter descriptions, and smaller context through on-demand discovery.",
        location: "Agent editor ▸ Build ▸ Tools & permissions.",
        how: "Pick tools from the catalog (or add an MCP tool id), then add an exact override or an exact/prefix exposure rule.",
        gotcha: "An override's target is the tool's canonical id (a catalog id or mcp__server__tool) — it applies to static and MCP tools uniformly.",
    },
    Topic {
        key: "permissions",
        title: "The permission gate",
        what: "A default decision (Allow / Ask / Deny) plus an ordered rule table (glob pattern → behavior), enforced at runtime on every tool call.",
        why: "It's how you make an agent safe: require human approval for risky calls, or hard-deny some outright.",
        location: "Agent editor ▸ Build ▸ Tools & permissions.",
        how: "Set the default, then add rules like Bash(*rm*) → Deny.",
        gotcha: "Deny always wins; otherwise the most specific match decides. Ask surfaces an approval in the session (human-in-the-loop).",
    },
    Topic {
        key: "plugins",
        title: "Behaviors (plugins): compaction, memory, state machine",
        what: "Named runtime behaviors composed onto an agent: auto-compaction (summarize old Steps), memory recall, and a state machine (constrain tool-call order + emit system reminders).",
        why: "They add durable, background behavior without touching the prompt — e.g. keep a long agent within its context budget, or force read-before-write.",
        location: "Common behaviors live in Build by intent; state machine and generic plugin configuration live in Advanced.",
        how: "Configure compaction with Instructions, Memory with Memory & resources, and state-machine transitions under Advanced ▸ Orchestration.",
        gotcha: "The state machine's emit entries ARE the 'system reminder' mechanism — a reminder is a transition emit, not a separate plugin.",
    },
    Topic {
        key: "sessions-deployments",
        title: "Sessions vs deployments",
        what: "A Session is one run/conversation instance. A Deployment is a standing rule (agent × environment × trigger/schedule) that PRODUCES sessions.",
        why: "You test ad-hoc in sessions; you put an agent into ongoing operation with a deployment.",
        location: "Run ▸ Sessions (instances) and Run ▸ Deployments (rules).",
        how: "Create a deployment by picking a published agent + an environment + a schedule; each firing produces a session.",
        gotcha: "A deployment is not a single run — it's the recurring binding; the runs it makes show up under Sessions.",
    },
    Topic {
        key: "resources",
        title: "Session resources and artifacts",
        what: "A session's mounted resources (the memory stores / files / repos it was given) and its output artifacts (files it wrote under the runtime-owned /mnt/session/outputs/ directory).",
        why: "It's how you see what an agent had access to and what it produced.",
        location: "A Session detail page ▸ Inputs / Artifacts, plus the Workspace Files and Artifacts projections.",
        how: "Open a Session → inspect Inputs for mounted files and Artifacts for outputs → use Trace for execution evidence.",
        gotcha: "Output artifacts only appear after a run writes to the outputs mount; a chat-only session has none.",
    },
    Topic {
        key: "author-agent",
        title: "Authoring an agent (start to publish)",
        what: "Create an agent from empty or by describing it to the assistant, then review and publish it.",
        why: "Publishing compiles the config to a reproducible fingerprint that sessions run.",
        location: "Build ▸ Agents ▸ New agent, or the ✦ Assistant entry / FAB.",
        how: "Quickstart: task template + runnable model + Environment + first task → review the diff → Publish & run. Continue in Build or Advanced.",
        gotcha: "The assistant can draft/patch everything but never publishes — publishing is your explicit action.",
    },
    Topic {
        key: "bind-resource",
        title: "Binding a memory store / file / repo / skill",
        what: "Attach a data-plane resource to an agent so it's mounted into every session the agent runs.",
        why: "It gives the agent durable memory, reference files, a code checkout, or a skill bundle.",
        location: "Agent editor ▸ Build ▸ Memory & resources, or ask the assistant ('bind the project-notes memory store').",
        how: "Add a binding (kind + which resource + mount path + access) and Save resources.",
        gotcha: "The resource must already exist (create the memory store / upload the file first); memory + repo bindings can be read_write and write back on harvest.",
    },
    Topic {
        key: "test-agent",
        title: "Testing an agent live",
        what: "Run the agent in an in-editor Sandbox against its real model.",
        why: "Prove the behavior before you rely on it — no separate deploy needed.",
        location: "Agent editor ▸ ▷ Try draft (opens an isolated preview drawer). Quickstart creates the durable first Session.",
        how: "Try draft → Start preview for an unsaved snapshot, or Quickstart → Review & run for a published durable Session.",
        gotcha: "It needs a credentialed model; if none is configured the sandbox says so instead of faking a reply.",
    },
    Topic {
        key: "inspect-run",
        title: "Inspecting a run (Trace)",
        what: "A session rendered as spans — the run's inference calls, tool calls, and status transitions.",
        why: "Transparency: see exactly what the agent did and why, step by step.",
        location: "A session's detail page ▸ Trace view.",
        how: "Open a session → Trace → expand spans (each carries its JSON).",
        gotcha: "Durable-op detail requires typed durable ingress; the base trace is always available.",
    },
    Topic {
        key: "skills",
        title: "Creating and using Skills",
        what: "A Skill is a versioned bundle of reusable instructions and optional supporting files that an Agent loads for a task.",
        why: "Skills keep specialized procedures reusable without copying them into every Agent prompt.",
        location: "Build ▸ Skills to create or import; Agent ▸ Build ▸ Skills & MCP to attach.",
        how: "Create instruction content online or import a Skill folder → declare whether execution needs a Sandbox → publish a version → select it in the Agent.",
        gotcha: "Instruction-only Skills do not need a Sandbox. A Skill that runs commands or reads a filesystem needs a compatible Environment and permissions.",
    },
    Topic {
        key: "files-artifacts",
        title: "Files, folders, and Artifacts",
        what: "Files are reusable Workspace inputs; Artifacts are read-only outputs harvested from Sessions. Both are projections of one file catalog.",
        why: "Keeping provenance lets an operator distinguish supplied inputs from generated results without duplicating storage.",
        location: "Resources ▸ Files, Run ▸ Artifacts, and a Session's Inputs / Artifacts tabs.",
        how: "Upload a File → bind it to an Agent under Build ▸ Memory & resources. Open an Artifact's producing Session for its conversation and Trace.",
        gotcha: "A logical path may contain separators and is rendered as folders; the stored filename and provenance remain unchanged.",
    },
    Topic {
        key: "memory-dreams",
        title: "Memory Stores and Dreams",
        what: "A Memory Store is editable durable content mounted into Agent Sessions. A Dream is a separate consolidation run whose output must be reviewed.",
        why: "Stores preserve working memory while Dreams turn selected evidence into curated, auditable output.",
        location: "Resources ▸ Memory. Open a Store to browse/edit content; use Dreams for consolidation history and policies.",
        how: "Create a named Store → inspect or edit its files → bind it to an Agent. For a Dream, choose sources and policy → run → review output → explicitly use it.",
        gotcha: "Dream output does not silently overwrite source memory, and a store binding applies to new Sessions rather than an already-running Session.",
    },
    Topic {
        key: "environments",
        title: "Environments, packages, and Sandbox",
        what: "An Environment defines where a Session runs plus packages, environment variables, networking, resource limits, and Sandbox timing.",
        why: "Runtime placement is run-scoped and reusable; it should not be hidden inside an Agent definition.",
        location: "Run ▸ Environments; select one in Quickstart or when creating a Session.",
        how: "Create or edit an Environment → choose placement → configure supported packages/network/limits → select it for a real Session.",
        gotcha: "Cloud and self-hosted variants support different fields. Runtime secrets are selected from Vaults and are never stored as plain Environment values.",
    },
    Topic {
        key: "mcp",
        title: "MCP servers and remote Skills",
        what: "MCP connects an Agent to external tools or prompts through HTTP or Sandbox stdio transports.",
        why: "The Agent owns the binding and permissions, while MCP Overview provides a Workspace-wide read-only projection.",
        location: "Agent ▸ Build ▸ Skills & MCP to configure; Connect ▸ MCP overview to inspect usage; Session ▸ Integrations to see successful connections.",
        how: "Add a server to the owning Agent → choose transport and a type-compatible active Vault revision → optionally expose prompts as Skills → publish and start a Session.",
        gotcha: "Configured does not mean connected. Only a successful Session projection proves the MCP server was active, and model-provider credentials are not valid MCP Vault choices.",
    },
    Topic {
        key: "api-access",
        title: "API access and protocols",
        what: "Published Agents can be called through supported application APIs and protocols using Workspace-scoped service credentials.",
        why: "A trusted backend needs a revocable identity and the narrowest role; browser code must not hold a service secret.",
        location: "Connect ▸ API & protocols for integration patterns; Govern ▸ Access to create or revoke service API keys.",
        how: "Prove the Agent in a Session → choose the protocol for the client → create a least-privileged API key → copy it once into the trusted backend's secret store.",
        gotcha: "API keys are shown only once. Creating, copying, rotating, and revoking them remains an explicit operator action.",
    },
    Topic {
        key: "runtime-secrets",
        title: "Runtime Secret Vaults",
        what: "Vaults hold versioned secret material used by MCP servers, tools, and Sandbox processes, classified by consumer-compatible type.",
        why: "Human-readable names and typed selectors prevent a model key, SSH key, token, or structured credential from being bound to the wrong consumer.",
        location: "Govern ▸ Runtime secrets; compatible selectors also appear in Agent MCP and Session runtime overrides.",
        how: "Choose a category/type → give the Vault a descriptive name → enter write-only material → validate it when supported → select only matching active revisions.",
        gotcha: "Provider model credentials belong under Models & providers. Archived, worker-local, provider-bound, or type-mismatched Vaults must not appear in unrelated selectors.",
    },
    Topic {
        key: "a2a",
        title: "A2A federation",
        what: "A2A exposes published Agent Cards and lets compatible remote agents discover callable endpoints.",
        why: "It is an interoperability boundary between deployments, distinct from an auxiliary Agent inside one primary Agent's roster.",
        location: "Connect ▸ A2A federation.",
        how: "Open A2A federation → inspect this deployment's published Agent Card and endpoints → configure the remote A2A client with the required access token. Configure outbound peers in the calling Agent's Collaboration settings.",
        gotcha: "A2A does not make a draft callable and does not replace the parent-owned auxiliary Agent relationship used for local delegation.",
    },
    Topic {
        key: "settings",
        title: "Workspace settings and ownership",
        what: "Settings summarizes the current Workspace scope and links to the surfaces that own each configuration.",
        why: "Configuration stays editable at one source of truth instead of being duplicated in a universal settings form.",
        location: "Govern ▸ Settings.",
        how: "Use the ownership links to open Models, Environments, Access, or Runtime secrets and make the change on its canonical page.",
        gotcha: "Settings is a directory, not a second persistence surface; a value should never be edited in two places.",
    },
    Topic {
        key: "assistant",
        title: "The AI assistant (this copilot)",
        what: "A page-aware Console copilot that answers workflow questions, diagnoses prerequisites, drafts or refines Agents, and creates Environments from plain language.",
        why: "It gives every page one conversational entry while keeping configuration ownership, validation, and activation explicit.",
        location: "The ✦ button on every Workspace page or Build ▸ Agents ▸ Ask Assistant. On an Agent editor it targets that exact draft.",
        how: "Ask a question or state an outcome → the Assistant uses current page context → it explains or performs a supported draft action → open the result → review, test, and publish when appropriate.",
        gotcha: "It cannot publish, reveal stored secrets, create service keys, or pretend an unsupported operation succeeded; it gives exact operator steps for those boundaries.",
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

    #[test]
    fn current_console_surfaces_have_help_topics() {
        for key in [
            "overview",
            "agent",
            "skills",
            "files-artifacts",
            "memory-dreams",
            "sessions-deployments",
            "environments",
            "connect-model",
            "mcp",
            "api-access",
            "a2a",
            "runtime-secrets",
            "settings",
            "assistant",
        ] {
            assert_eq!(explain(Some(key))["topic"], key, "missing help for {key}");
        }
    }

    // Task 1: corpus determinism — EVERY curated topic is a complete 5-section
    // explanation (plus a non-empty key + title), so `explain(Some(key))` can never
    // return a blank section for any topic the index advertises.
    #[test]
    fn every_topic_has_five_nonempty_sections() {
        assert!(!TOPICS.is_empty(), "the curated corpus must not be empty");
        for t in TOPICS {
            // The struct-level invariant: key + title + all 5 sections are non-empty.
            for (field, value) in [
                ("key", t.key),
                ("title", t.title),
                ("what", t.what),
                ("why", t.why),
                ("location", t.location),
                ("how", t.how),
                ("gotcha", t.gotcha),
            ] {
                assert!(
                    !value.trim().is_empty(),
                    "topic `{}` has an empty `{field}` field",
                    t.key
                );
            }
            // The projected payload for every topic carries all 5 rendered sections.
            let v = explain(Some(t.key));
            assert_eq!(v["topic"], t.key);
            for k in ["what", "why", "where", "how", "gotchas"] {
                assert!(
                    v[k].as_str().is_some_and(|s| !s.trim().is_empty()),
                    "topic `{}` renders an empty `{k}` section",
                    t.key
                );
            }
        }
    }

    // Task 1: lookup is case-insensitive AND whitespace-trimmed, so an operator's loose
    // topic string ("  Connect-Model  ") still resolves to the exact curated topic.
    #[test]
    fn lookup_is_case_insensitive_and_whitespace_trimmed() {
        for probe in [
            "connect-model",
            "CONNECT-MODEL",
            "Connect-Model",
            "  connect-model  ",
            "\tConnect-Model\n",
        ] {
            let v = explain(Some(probe));
            assert_eq!(
                v["topic"], "connect-model",
                "probe {probe:?} should resolve to the connect-model topic"
            );
            // A resolved topic never carries the unknown-topic note.
            assert!(v.get("unknown_topic").is_none(), "probe {probe:?} misfired");
        }
    }

    // Task 1: a whitespace-only / empty topic is treated as "no topic" → the index,
    // not an unknown-topic miss (the `filter(|t| !t.is_empty())` after trim).
    #[test]
    fn blank_topic_is_treated_as_the_index() {
        for probe in ["", "   ", "\t\n"] {
            let v = explain(Some(probe));
            assert_eq!(
                v["topics"].as_array().map(Vec::len),
                Some(TOPICS.len()),
                "blank probe {probe:?} should return the full index"
            );
            assert!(
                v.get("unknown_topic").is_none(),
                "blank probe {probe:?} must not be an unknown-topic miss"
            );
        }
    }
}
