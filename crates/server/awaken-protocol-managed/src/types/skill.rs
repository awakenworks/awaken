//! Strong beta and GA Managed Skills DTOs over one durable Skill aggregate.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SkillObjectType {
    #[serde(rename = "skill")]
    Skill,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SkillVersionObjectType {
    #[serde(rename = "skill_version")]
    SkillVersion,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeletedSkillObjectType {
    #[serde(rename = "skill_deleted")]
    SkillDeleted,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeletedSkillVersionObjectType {
    #[serde(rename = "skill_version_deleted")]
    SkillVersionDeleted,
}

/// GA `SkillListParams`.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SkillListParams {
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Beta `SkillListParams` including the generated `beta=true` selector.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BetaSkillListParams {
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub source: Option<String>,
}

/// GA `VersionListParams`; versions accept only the shared cursor fields.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SkillVersionListParams {
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
}

/// Beta `VersionListParams` plus the SDK's generated path selector.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BetaSkillVersionListParams {
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
}

/// GA `SkillSource` object.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillSource {
    Custom,
    Anthropic,
    AnthropicExample,
    Plugin,
}

/// GA `Skill`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Skill {
    pub id: String,
    pub created_at: String,
    pub display_name: String,
    pub latest_version_id: String,
    pub source: SkillSource,
    #[serde(rename = "type")]
    pub kind: SkillObjectType,
    pub updated_at: String,
}

/// Beta `BetaSkill` retained for SDK 0.117 compatibility.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BetaSkill {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: SkillObjectType,
    pub created_at: String,
    pub updated_at: String,
    pub display_title: Option<String>,
    pub latest_version: Option<String>,
    pub source: &'static str,
}

/// GA `SkillVersion`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SkillVersion {
    pub id: String,
    pub created_at: String,
    pub description: String,
    pub name: String,
    pub skill_id: String,
    #[serde(rename = "type")]
    pub kind: SkillVersionObjectType,
}

/// Beta `BetaSkillVersion` retained for the beta versions API.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BetaSkillVersion {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: SkillVersionObjectType,
    pub created_at: String,
    pub description: String,
    pub directory: String,
    pub name: String,
    pub skill_id: String,
    pub version: String,
}

/// Internal protocol union selected before projection. It is not another
/// resource model: each variant is one official SDK response type.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum SkillWire {
    Beta(BetaSkill),
    Ga(Skill),
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum SkillVersionWire {
    Beta(BetaSkillVersion),
    Ga(SkillVersion),
}

/// `DeletedSkill`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeletedSkill {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: DeletedSkillObjectType,
}

/// `DeletedSkillVersion`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeletedSkillVersion {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: DeletedSkillVersionObjectType,
}
