//! The skill authoring aggregate and a minimal `SKILL.md` reader.
//!
//! A [`SkillSpec`] is pure agent-domain data: the model-facing identity of a
//! skill (name/description/when-to-use) plus the instruction `body` a successful
//! activation returns. It carries no executable handle and no per-skill tool —
//! the whole skill set is fronted by the single `Skill` tool ([`crate::tool`]).
//!
//! [`parse_skill_md`] reads the Claude-Code `SKILL.md` shape: an optional YAML
//! frontmatter block delimited by `---`, then the instruction body. The parser is
//! intentionally minimal (scalar `key: value` lines plus comma / inline-`[...]`
//! lists); full YAML and filesystem discovery are a later slice, not Stage 1.

use serde::{Deserialize, Serialize};

/// Cap on a skill name, mirroring the reference implementations (Hermes uses 64).
pub const MAX_NAME_LENGTH: usize = 64;
/// Cap on a skill description shown in the catalog (Hermes uses 1024).
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Truncate on a char boundary, appending an ellipsis when cut. Shared by the
/// parser (source caps) and the catalog renderer (budget caps).
pub(crate) fn truncate_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Where a skill came from — the trust root it was materialized on (ADR-0036 D6).
/// Provenance is derived from location (which root), not authored, so an
/// agent-written skill cannot claim to be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillProvenance {
    /// Control-owned, read-only, trusted (the delivered root).
    #[default]
    Delivered,
    /// Authored by the agent this run in the writable workspace: usable this run
    /// (run-scoped), never pinned, never auto-promoted to the shared store.
    AgentCreated,
}

/// How an activated skill executes: inline in the conversation, or forked into a
/// sub-agent with its own budget (ADR-0036; wired by the host for `Fork`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillContext {
    #[default]
    Inline,
    Fork,
}

/// One skill's model-facing identity and instruction body. Data-only: the
/// runtime never sees a "skill", only the `Skill` / `list_skills` tools that read
/// this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSpec {
    /// Stable id the model passes to the `Skill` tool to activate it.
    pub id: String,
    /// Display name; defaults to `id` when frontmatter omits it.
    pub name: String,
    /// One-line description shown in the catalog.
    pub description: String,
    /// Optional "when to use" hint shown in the catalog.
    pub when_to_use: Option<String>,
    /// Tools this skill is allowed to use once active. Consumed by the permission
    /// layer in a later slice; carried now so the authoring shape is stable.
    pub allowed_tools: Vec<String>,
    /// Whether the model may activate this skill via the `Skill` tool. A `false`
    /// skill is hidden from the catalog and refused at the tool (users only).
    pub model_invocable: bool,
    /// Whether a user may invoke this skill via `/name` (default true).
    pub user_invocable: bool,
    /// Free-text hint about the skill's arguments, shown in the catalog.
    pub argument_hint: Option<String>,
    /// Named argument declarations (informational; substitution is positional).
    pub arguments: Vec<String>,
    /// Optional model override applied when the skill is activated.
    pub model_override: Option<String>,
    /// Execution mode: inline expansion or a forked sub-agent.
    pub context: SkillContext,
    /// Agent type for forked execution (host-resolved).
    pub agent: Option<String>,
    /// Glob patterns for conditional activation; empty means unconditional.
    pub paths: Vec<String>,
    /// Optional grouping category, shown in the catalog and filterable.
    pub category: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// Optional version identifier.
    pub version: Option<String>,
    /// Which trust root this skill came from (ADR-0036 D6). Derived by location.
    pub provenance: SkillProvenance,
    /// Logical directory of the skill under the sandbox root, when discovered from
    /// files. Resolves `${SKILL_DIR}` so the body can point at its references. `None`
    /// for in-memory (config-only) skills.
    pub dir: Option<String>,
    /// The `SKILL.md` instruction body returned to the model on activation.
    pub body: String,
}

impl SkillSpec {
    /// A model-invocable skill with only the required fields; other fields default.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            when_to_use: None,
            allowed_tools: Vec::new(),
            model_invocable: true,
            user_invocable: true,
            argument_hint: None,
            arguments: Vec::new(),
            model_override: None,
            context: SkillContext::Inline,
            agent: None,
            paths: Vec::new(),
            category: None,
            tags: Vec::new(),
            version: None,
            provenance: SkillProvenance::Delivered,
            dir: None,
            body: body.into(),
        }
    }

    /// Mark this skill's provenance (the trust root it was materialized on).
    #[must_use]
    pub fn with_provenance(mut self, provenance: SkillProvenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Set the execution mode (inline or forked sub-agent).
    #[must_use]
    pub fn with_context(mut self, context: SkillContext) -> Self {
        self.context = context;
        self
    }

    /// Set the conditional-activation path globs.
    #[must_use]
    pub fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.paths = paths;
        self
    }

    #[must_use]
    pub fn with_when_to_use(mut self, when_to_use: impl Into<String>) -> Self {
        self.when_to_use = Some(when_to_use.into());
        self
    }

    #[must_use]
    pub fn with_allowed_tools(mut self, allowed_tools: Vec<String>) -> Self {
        self.allowed_tools = allowed_tools;
        self
    }
}

/// Read a `SKILL.md` document into a [`SkillSpec`]. Recognizes an optional
/// frontmatter block (`---` … `---`) with these keys (hyphen or underscore):
/// `name`, `description`, `when-to-use`, `allowed-tools`, `disable-model-invocation`,
/// `user-invocable`, `argument-hint`, `arguments`, `model`/`model-override`,
/// `context` (`inline`|`fork`), `agent`, `paths`, `category`, `tags`, `version`.
/// Everything after the frontmatter is the instruction body; an absent
/// frontmatter treats the whole input as body. Unknown keys are ignored
/// (forward-compatible).
pub fn parse_skill_md(id: impl Into<String>, content: &str) -> SkillSpec {
    let id = id.into();
    let (frontmatter, body) = split_frontmatter(content);

    let mut spec = SkillSpec::new(id.clone(), id, String::new(), body);
    for (key, value) in frontmatter {
        match key.replace('_', "-").as_str() {
            "name" => spec.name = value,
            "description" => spec.description = value,
            "when-to-use" => spec.when_to_use = Some(value),
            "allowed-tools" => spec.allowed_tools = parse_list(&value),
            "disable-model-invocation" => spec.model_invocable = !parse_bool(&value),
            "user-invocable" => spec.user_invocable = parse_bool(&value),
            "argument-hint" => spec.argument_hint = Some(value),
            "arguments" => spec.arguments = parse_list(&value),
            "model" | "model-override" => spec.model_override = Some(value),
            "context" => {
                spec.context = if value.eq_ignore_ascii_case("fork") {
                    SkillContext::Fork
                } else {
                    SkillContext::Inline
                }
            }
            "agent" => spec.agent = Some(value),
            "paths" => spec.paths = parse_list(&value),
            "category" => spec.category = Some(value),
            "tags" => spec.tags = parse_list(&value),
            "version" => spec.version = Some(value),
            _ => {}
        }
    }
    // Bound the model-facing metadata so a large or malformed skill cannot blow
    // out the catalog (ADR-0036: size limits).
    spec.name = truncate_chars(&spec.name, MAX_NAME_LENGTH);
    spec.description = truncate_chars(&spec.description, MAX_DESCRIPTION_LENGTH);
    spec
}

/// Split a leading `---`-delimited frontmatter block from the body. Returns the
/// parsed `key: value` pairs and the remaining body (trimmed of a leading
/// newline). With no frontmatter, the pairs are empty and the body is the input.
fn split_frontmatter(content: &str) -> (Vec<(String, String)>, String) {
    let trimmed = content.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (Vec::new(), content.to_string());
    };
    // The opening fence must be its own line.
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return (Vec::new(), content.to_string());
    };
    let Some(end) = find_closing_fence(rest) else {
        return (Vec::new(), content.to_string());
    };
    let (yaml, after) = rest.split_at(end);
    let body = after
        .trim_start_matches("---")
        .trim_start_matches("\r\n")
        .trim_start_matches('\n')
        .to_string();

    let pairs = yaml
        .lines()
        .filter_map(parse_scalar_line)
        .collect::<Vec<_>>();
    (pairs, body)
}

/// Find the byte offset of the closing `---` fence (a line that is exactly `---`).
fn find_closing_fence(rest: &str) -> Option<usize> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Parse one `key: value` frontmatter line, skipping blanks and comments.
fn parse_scalar_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once(':')?;
    let key = key.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    Some((key, unquote(value.trim())))
}

/// Strip a single pair of surrounding single or double quotes.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if value.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Parse a comma-separated or inline-`[a, b]` list into trimmed, non-empty items.
fn parse_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

/// Parse a YAML-ish boolean; anything but a truthy token is `false`.
fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "1"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let md = "---\nname: Commit\ndescription: Make a git commit\nwhen-to-use: recording changes\nallowed-tools: read, bash\n---\nRun the steps below.\n";
        let spec = parse_skill_md("commit", md);
        assert_eq!(spec.id, "commit");
        assert_eq!(spec.name, "Commit");
        assert_eq!(spec.description, "Make a git commit");
        assert_eq!(spec.when_to_use.as_deref(), Some("recording changes"));
        assert_eq!(spec.allowed_tools, vec!["read", "bash"]);
        assert!(spec.model_invocable);
        assert_eq!(spec.body, "Run the steps below.\n");
    }

    #[test]
    fn disable_model_invocation_flips_the_flag() {
        let md = "---\ndisable-model-invocation: true\n---\nbody";
        let spec = parse_skill_md("x", md);
        assert!(!spec.model_invocable);
    }

    #[test]
    fn no_frontmatter_is_all_body_and_id_defaults() {
        let spec = parse_skill_md("plain", "just instructions");
        assert_eq!(spec.name, "plain");
        assert_eq!(spec.description, "");
        assert_eq!(spec.body, "just instructions");
        assert!(spec.allowed_tools.is_empty());
    }

    #[test]
    fn parses_extended_frontmatter_keys() {
        let md = "---\nname: PR\ndescription: review a PR\ncontext: fork\nagent: general-purpose\nuser-invocable: false\nargument-hint: <pr-number>\narguments: number, base\nmodel: opus\npaths: [\"src/**\", \"*.rs\"]\ncategory: dev\ntags: git, review\nversion: 1.2\n---\nsteps";
        let spec = parse_skill_md("review-pr", md);
        assert_eq!(spec.context, SkillContext::Fork);
        assert_eq!(spec.agent.as_deref(), Some("general-purpose"));
        assert!(!spec.user_invocable);
        assert_eq!(spec.argument_hint.as_deref(), Some("<pr-number>"));
        assert_eq!(spec.arguments, vec!["number", "base"]);
        assert_eq!(spec.model_override.as_deref(), Some("opus"));
        assert_eq!(spec.paths, vec!["src/**", "*.rs"]);
        assert_eq!(spec.category.as_deref(), Some("dev"));
        assert_eq!(spec.tags, vec!["git", "review"]);
        assert_eq!(spec.version.as_deref(), Some("1.2"));
    }

    #[test]
    fn extended_keys_default_when_absent() {
        let spec = parse_skill_md("x", "no frontmatter");
        assert_eq!(spec.context, SkillContext::Inline);
        assert!(spec.user_invocable);
        assert!(spec.paths.is_empty());
        assert!(spec.model_override.is_none());
    }

    #[test]
    fn over_long_name_and_description_are_capped() {
        let long_name = "n".repeat(200);
        let long_desc = "d".repeat(4000);
        let md = format!("---\nname: {long_name}\ndescription: {long_desc}\n---\nbody");
        let spec = parse_skill_md("x", &md);
        assert!(spec.name.chars().count() <= MAX_NAME_LENGTH);
        assert!(spec.description.chars().count() <= MAX_DESCRIPTION_LENGTH);
        assert!(spec.name.ends_with('…'));
        assert!(spec.description.ends_with('…'));
    }

    #[test]
    fn inline_list_and_quotes() {
        let md = "---\nname: \"PDF\"\nallowed-tools: [read, \"bash\"]\n---\nx";
        let spec = parse_skill_md("pdf", md);
        assert_eq!(spec.name, "PDF");
        assert_eq!(spec.allowed_tools, vec!["read", "bash"]);
    }

    #[test]
    fn a_name_at_exactly_the_cap_is_left_untouched_but_one_over_is_ellipsized() {
        // Boundary: length == cap keeps the value verbatim (no truncation marker);
        // cap+1 truncates and appends the ellipsis. (Complements the over-cap test.)
        let at_cap = "n".repeat(MAX_NAME_LENGTH);
        let spec = parse_skill_md("x", &format!("---\nname: {at_cap}\n---\nb"));
        assert_eq!(
            spec.name, at_cap,
            "a name exactly at the cap is not truncated"
        );
        assert!(!spec.name.ends_with('…'));

        let over = "n".repeat(MAX_NAME_LENGTH + 1);
        let spec = parse_skill_md("x", &format!("---\nname: {over}\n---\nb"));
        assert_eq!(spec.name.chars().count(), MAX_NAME_LENGTH);
        assert!(spec.name.ends_with('…'));
    }

    #[test]
    fn truncation_respects_char_boundaries_for_multibyte_names() {
        // A CJK name over the cap must truncate on a char boundary (never mid-byte) —
        // `chars().count()`, not byte length, bounds it.
        let wide = "字".repeat(MAX_NAME_LENGTH + 5);
        let spec = parse_skill_md("x", &format!("---\nname: {wide}\n---\nb"));
        assert_eq!(spec.name.chars().count(), MAX_NAME_LENGTH);
        assert!(spec.name.ends_with('…'));
        // Round-trips as valid UTF-8 (would panic on a mid-byte cut).
        assert!(std::str::from_utf8(spec.name.as_bytes()).is_ok());
    }

    #[test]
    fn an_unterminated_frontmatter_fence_is_treated_as_all_body() {
        // Malformed: an opening `---` with no closing fence must not eat the content —
        // it falls through to "no frontmatter", so the whole input is the body and the
        // name defaults to the id (lenient parse, never a hard failure).
        let spec = parse_skill_md("id7", "---\nname: X\nno closing fence here\nstill body");
        assert_eq!(
            spec.name, "id7",
            "unterminated frontmatter → name defaults to id"
        );
        assert!(spec.body.contains("still body"));
    }
}
