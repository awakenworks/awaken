//! Field-level override for [`SkillSpec`](crate::skill_spec::SkillSpec).
//!
//! Stored as JSON inside `RecordMeta::user_overrides` for built-in skill
//! records. Missing fields inherit from the built-in skill; JSON `null` clears
//! nullable metadata fields.

use serde::{Deserialize, Serialize};

use crate::skill_spec::{SkillArgumentSpec, SkillSpec, SkillSpecContext};

pub type NullablePatch<T> = Option<Option<T>>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SkillSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions_md: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub when_to_use: NullablePatch<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<SkillArgumentSpec>>,
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub argument_hint: NullablePatch<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_invocable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_invocable: Option<bool>,
    #[serde(
        default,
        deserialize_with = "nullable_patch::deserialize",
        serialize_with = "nullable_patch::serialize",
        skip_serializing_if = "nullable_patch::is_missing"
    )]
    pub model_override: NullablePatch<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<SkillSpecContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

impl SkillSpecPatch {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.description.is_none()
            && self.instructions_md.is_none()
            && self.allowed_tools.is_none()
            && self.when_to_use.is_none()
            && self.arguments.is_none()
            && self.argument_hint.is_none()
            && self.user_invocable.is_none()
            && self.model_invocable.is_none()
            && self.model_override.is_none()
            && self.context.is_none()
            && self.paths.is_none()
    }
}

pub fn merge_skill_spec(base: SkillSpec, patch: SkillSpecPatch) -> SkillSpec {
    SkillSpec {
        id: base.id,
        name: patch.name.unwrap_or(base.name),
        description: patch.description.unwrap_or(base.description),
        instructions_md: patch.instructions_md.unwrap_or(base.instructions_md),
        allowed_tools: patch.allowed_tools.unwrap_or(base.allowed_tools),
        when_to_use: merge_nullable(base.when_to_use, patch.when_to_use),
        arguments: patch.arguments.unwrap_or(base.arguments),
        argument_hint: merge_nullable(base.argument_hint, patch.argument_hint),
        user_invocable: patch.user_invocable.unwrap_or(base.user_invocable),
        model_invocable: patch.model_invocable.unwrap_or(base.model_invocable),
        model_override: merge_nullable(base.model_override, patch.model_override),
        context: patch.context.unwrap_or(base.context),
        paths: patch.paths.unwrap_or(base.paths),
    }
}

fn merge_nullable<T>(base: Option<T>, patch: NullablePatch<T>) -> Option<T> {
    patch.unwrap_or(base)
}

mod nullable_patch {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, T>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match value {
            None => serializer.serialize_none(),
            Some(inner) => inner.serialize(serializer),
        }
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Some)
    }

    pub fn is_missing<T>(value: &Option<Option<T>>) -> bool {
        value.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> SkillSpec {
        SkillSpec {
            id: "db-management".into(),
            name: "Database Management".into(),
            description: "stock".into(),
            instructions_md: "Use stock instructions.".into(),
            when_to_use: Some("stock hint".into()),
            argument_hint: Some("dialect=postgres".into()),
            model_override: Some("fast".into()),
            ..Default::default()
        }
    }

    #[test]
    fn empty_patch_keeps_base() {
        assert_eq!(merge_skill_spec(base(), SkillSpecPatch::default()), base());
    }

    #[test]
    fn scalar_patch_replaces_base() {
        let merged = merge_skill_spec(
            base(),
            SkillSpecPatch {
                description: Some("custom".into()),
                instructions_md: Some("Custom.".into()),
                model_invocable: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(merged.description, "custom");
        assert_eq!(merged.instructions_md, "Custom.");
        assert!(!merged.model_invocable);
    }

    #[test]
    fn nullable_patch_clears_values() {
        let patch: SkillSpecPatch = serde_json::from_value(json!({
            "when_to_use": null,
            "argument_hint": null,
            "model_override": null
        }))
        .unwrap();
        let merged = merge_skill_spec(base(), patch);
        assert_eq!(merged.when_to_use, None);
        assert_eq!(merged.argument_hint, None);
        assert_eq!(merged.model_override, None);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let bad = json!({"description": "x", "id": "renamed"});
        assert!(serde_json::from_value::<SkillSpecPatch>(bad).is_err());
    }
}
