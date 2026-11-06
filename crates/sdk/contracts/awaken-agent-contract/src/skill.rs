//! Typed Skill selection shared by authoring, publication, and Session pinning.

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSkillKind {
    Anthropic,
    #[default]
    Custom,
}

/// One Agent-authored Skill reference. Omitted versions normalize to `latest`
/// once at admission, so every downstream snapshot carries an explicit selector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentSkillBinding {
    #[serde(rename = "type")]
    pub kind: AgentSkillKind,
    pub skill_id: String,
    pub version: String,
}

impl AgentSkillBinding {
    #[must_use]
    pub fn custom(skill_id: impl Into<String>) -> Self {
        Self {
            kind: AgentSkillKind::Custom,
            skill_id: skill_id.into(),
            version: "latest".into(),
        }
    }
}

pub const ANTHROPIC_SKILL_IDS: [&str; 4] = ["pptx", "xlsx", "docx", "pdf"];

/// Validate one Agent's selection before publication. Session-level composition
/// applies the same 500-entry cap again after coordinator/subagent expansion.
pub fn validate_agent_skills(skills: &[AgentSkillBinding]) -> Result<(), String> {
    if skills.len() > 500 {
        return Err("skills supports at most 500 entries".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for skill in skills {
        if skill.skill_id.trim().is_empty() {
            return Err("skill ids must be non-empty".into());
        }
        if skill.version != "latest"
            && skill
                .version
                .parse::<u64>()
                .ok()
                .is_none_or(|version| version == 0)
        {
            return Err("skill version must be `latest` or a positive integer".into());
        }
        if skill.kind == AgentSkillKind::Anthropic {
            if !ANTHROPIC_SKILL_IDS.contains(&skill.skill_id.as_str()) {
                return Err(format!(
                    "unknown Anthropic pre-built skill `{}`",
                    skill.skill_id
                ));
            }
            if !matches!(skill.version.as_str(), "latest" | "1") {
                return Err(format!(
                    "Anthropic pre-built skill `{}` has no version `{}`",
                    skill.skill_id, skill.version
                ));
            }
        }
        if !seen.insert((skill.kind, skill.skill_id.as_str())) {
            return Err(format!("skill id {:?} is duplicated", skill.skill_id));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AgentSkillBindingInput {
    Id(String),
    Object {
        #[serde(rename = "type", default)]
        kind: AgentSkillKind,
        #[serde(alias = "id")]
        skill_id: String,
        #[serde(default = "latest")]
        version: String,
    },
}

fn latest() -> String {
    "latest".into()
}

impl<'de> Deserialize<'de> for AgentSkillBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match AgentSkillBindingInput::deserialize(deserializer)? {
            AgentSkillBindingInput::Id(skill_id) => Self::custom(skill_id),
            AgentSkillBindingInput::Object {
                kind,
                skill_id,
                version,
            } => Self {
                kind,
                skill_id,
                version,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_typed_inputs_normalize_to_one_binding() {
        // Causes: legacy string/object id, official custom/prebuilt object, and
        // omitted/explicit version. Constraint: omission means `latest`.
        // Effects: one typed source/version truth serializes in official form.
        // Decision rules: S1 legacy -> custom/latest; S2 typed omitted -> latest;
        // S3 typed pinned -> preserve exact selector.
        for (wire, expected) in [
            (
                r#""skill_1""#,
                (AgentSkillKind::Custom, "skill_1", "latest"),
            ),
            (
                r#"{"id":"skill_1"}"#,
                (AgentSkillKind::Custom, "skill_1", "latest"),
            ),
            (
                r#"{"type":"anthropic","skill_id":"xlsx"}"#,
                (AgentSkillKind::Anthropic, "xlsx", "latest"),
            ),
            (
                r#"{"type":"custom","skill_id":"skill_1","version":"2"}"#,
                (AgentSkillKind::Custom, "skill_1", "2"),
            ),
        ] {
            let binding: AgentSkillBinding = serde_json::from_str(wire).unwrap();
            assert_eq!(
                (
                    binding.kind,
                    binding.skill_id.as_str(),
                    binding.version.as_str()
                ),
                expected
            );
            assert!(
                serde_json::to_value(binding)
                    .unwrap()
                    .get("skill_id")
                    .is_some()
            );
        }
    }

    #[test]
    fn skill_admission_follows_bounds_source_and_version_rules() {
        // Causes: count, source/id, selector, and duplicate identity.
        // Constraints: <=500; prebuilt id is known and locally versioned at 1;
        // custom selectors are latest or positive integers. Effects: valid
        // selections compile; every invalid partition rejects atomically.
        // Decision rules: S4 500 accept; S5 501 reject; S6 known prebuilt
        // latest/1 accept; S7 unknown/version0/non-numeric/duplicate reject.
        assert!(validate_agent_skills(&vec![AgentSkillBinding::custom("x"); 500]).is_err());
        let unique = (0..500)
            .map(|index| AgentSkillBinding::custom(format!("skill-{index}")))
            .collect::<Vec<_>>();
        assert!(validate_agent_skills(&unique).is_ok(), "S4");
        let mut over = unique;
        over.push(AgentSkillBinding::custom("skill-over"));
        assert!(validate_agent_skills(&over).is_err(), "S5");
        for version in ["latest", "1"] {
            assert!(
                validate_agent_skills(&[AgentSkillBinding {
                    kind: AgentSkillKind::Anthropic,
                    skill_id: "xlsx".into(),
                    version: version.into(),
                }])
                .is_ok(),
                "S6"
            );
        }
        for invalid in [
            AgentSkillBinding {
                kind: AgentSkillKind::Anthropic,
                skill_id: "unknown".into(),
                version: "latest".into(),
            },
            AgentSkillBinding {
                kind: AgentSkillKind::Anthropic,
                skill_id: "xlsx".into(),
                version: "2".into(),
            },
            AgentSkillBinding {
                kind: AgentSkillKind::Custom,
                skill_id: "skill_1".into(),
                version: "zero".into(),
            },
        ] {
            assert!(validate_agent_skills(&[invalid]).is_err(), "S7");
        }
    }
}
