//! Content-addressed identity helpers for prompts, tool descriptions, and
//! skills. See ADR-0030 D1.

use sha2::{Digest, Sha256};

/// Length of the hex prefix returned by every identity helper.
///
/// 12 hex characters = 48 bits of entropy, sufficient for the cardinalities
/// expected for distinct prompts / tool descriptions / skills in a single
/// deployment while keeping OTel attribute payloads small.
const ID_HEX_LEN: usize = 12;

/// Stable id derived from an agent's effective system prompt under a
/// specific agent identity.
///
/// Equal `(agent_id, role, content)` triples share an id. Identical prompt
/// **content** under different `agent_id`s intentionally produces different
/// ids — agent identity is part of the attribution so a prompt copied
/// across agents stays distinguishable on the trace stream. Drop the
/// `agent_id` axis at the analysis layer if you need cross-agent
/// equivalence.
pub fn agent_prompt_id(agent_id: &str, role: &str, content: &str) -> String {
    hash3(agent_id, role, content)
}

/// Stable id derived from a tool's effective descriptor metadata.
///
/// Equal `(tool_name, description, schema_json)` triples always yield the
/// same id; any change to the description or schema produces a new id.
pub fn tool_desc_id(tool_name: &str, description: &str, schema_json: &str) -> String {
    hash3(tool_name, description, schema_json)
}

/// Stable id derived from a skill's resolved content.
///
/// Equal `(skill_name, content)` pairs always yield the same id; any
/// content change produces a new id.
pub fn skill_content_id(skill_name: &str, content: &str) -> String {
    hash2(skill_name, content)
}

fn hash3(a: &str, b: &str, c: &str) -> String {
    let mut h = Sha256::new();
    h.update(a.as_bytes());
    h.update(b"\x1f");
    h.update(b.as_bytes());
    h.update(b"\x1f");
    h.update(c.as_bytes());
    let digest = h.finalize();
    hex_prefix(&digest, ID_HEX_LEN)
}

fn hash2(a: &str, b: &str) -> String {
    let mut h = Sha256::new();
    h.update(a.as_bytes());
    h.update(b"\x1f");
    h.update(b.as_bytes());
    let digest = h.finalize();
    hex_prefix(&digest, ID_HEX_LEN)
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    let mut s = String::with_capacity(n);
    for byte in bytes.iter().take(n.div_ceil(2)) {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{byte:02x}");
    }
    s.truncate(n);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_prompt_id_is_stable() {
        let a = agent_prompt_id("weather", "system", "You are a forecaster.");
        let b = agent_prompt_id("weather", "system", "You are a forecaster.");
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn agent_prompt_id_changes_with_content() {
        let a = agent_prompt_id("x", "system", "v1");
        let b = agent_prompt_id("x", "system", "v2");
        assert_ne!(a, b);
    }

    #[test]
    fn agent_prompt_id_changes_with_role() {
        let a = agent_prompt_id("x", "system", "same");
        let b = agent_prompt_id("x", "user", "same");
        assert_ne!(a, b);
    }

    #[test]
    fn tool_desc_id_decouples_from_unrelated_fields() {
        let a = tool_desc_id("get_weather", "Look up forecasts", "{\"type\":\"object\"}");
        let b = tool_desc_id("get_weather", "Look up forecasts", "{\"type\":\"object\"}");
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        let c = tool_desc_id(
            "get_weather",
            "Look up the weather",
            "{\"type\":\"object\"}",
        );
        assert_ne!(a, c);
    }

    #[test]
    fn skill_content_id_two_args_only() {
        let a = skill_content_id("planner", "## Plan steps...");
        assert_eq!(a.len(), 12);
        let b = skill_content_id("planner", "## Plan steps...");
        assert_eq!(a, b);
    }

    #[test]
    fn separator_byte_prevents_collision_across_field_boundaries() {
        // "ab" + "" should not equal "a" + "b" because of the 0x1F separator.
        let a = agent_prompt_id("ab", "", "x");
        let b = agent_prompt_id("a", "b", "x");
        assert_ne!(a, b);
    }
}
