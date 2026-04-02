use crate::error::SkillError;
use crate::skill::{
    LoadedAsset, LoadedReference, ScriptResult, Skill, SkillMeta, SkillResource, SkillResourceKind,
};
use crate::skill_md::{parse_allowed_tools, parse_skill_md};
use async_trait::async_trait;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

/// Static data for an embedded skill, designed for `include_str!`.
pub struct EmbeddedSkillData {
    /// Raw `SKILL.md` content (YAML frontmatter + markdown body).
    pub skill_md: &'static str,
    /// Reference files as `(relative_path, content)` pairs.
    pub references: &'static [(&'static str, &'static str)],
    /// Asset files as `(relative_path, content_base64, media_type)` tuples.
    pub assets: &'static [(&'static str, &'static str, Option<&'static str>)],
}

/// An in-memory skill built from static content.
///
/// Implements [`Skill`] directly. Constructed via [`EmbeddedSkill::new`] which
/// parses and validates the SKILL.md at construction time.
#[derive(Debug, Clone)]
pub struct EmbeddedSkill {
    meta: SkillMeta,
    skill_md: String,
    references: HashMap<String, LoadedReference>,
    assets: HashMap<String, LoadedAsset>,
}

impl EmbeddedSkill {
    /// Build an embedded skill from static data.
    pub fn new(data: &EmbeddedSkillData) -> Result<Self, SkillError> {
        let doc =
            parse_skill_md(data.skill_md).map_err(|e| SkillError::InvalidSkillMd(e.to_string()))?;

        let fm = &doc.frontmatter;
        let id = fm.name.clone();

        let allowed_tools = fm
            .allowed_tools
            .as_deref()
            .map(parse_allowed_tools)
            .transpose()
            .map_err(|e| SkillError::InvalidSkillMd(e.to_string()))?
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.raw)
            .collect::<Vec<_>>();

        let meta = SkillMeta::new(
            id.clone(),
            id.clone(),
            fm.description.clone(),
            allowed_tools,
        );

        let mut references = HashMap::new();
        for &(path, content) in data.references {
            let sha = format!("{:x}", Sha256::digest(content.as_bytes()));
            references.insert(
                path.to_string(),
                LoadedReference {
                    skill: id.clone(),
                    path: path.to_string(),
                    sha256: sha,
                    truncated: false,
                    content: content.to_string(),
                    bytes: content.len() as u64,
                },
            );
        }

        let mut assets = HashMap::new();
        for &(path, content_base64, media_type) in data.assets {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(content_base64.as_bytes())
                .map_err(|e| {
                    SkillError::InvalidSkillMd(format!(
                        "invalid base64 asset '{path}' for skill '{id}': {e}"
                    ))
                })?;
            let sha = format!("{:x}", Sha256::digest(&decoded));
            assets.insert(
                path.to_string(),
                LoadedAsset {
                    skill: id.clone(),
                    path: path.to_string(),
                    sha256: sha,
                    truncated: false,
                    bytes: decoded.len() as u64,
                    media_type: media_type.map(|m| m.to_string()),
                    encoding: "base64".to_string(),
                    content: content_base64.to_string(),
                },
            );
        }

        Ok(Self {
            meta,
            skill_md: data.skill_md.to_string(),
            references,
            assets,
        })
    }

    /// Construct multiple embedded skills from static data, collecting into trait objects.
    pub fn from_static_slice(
        data: &[EmbeddedSkillData],
    ) -> Result<Vec<Arc<dyn Skill>>, SkillError> {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut skills: Vec<Arc<dyn Skill>> = Vec::new();
        for d in data {
            let skill = Self::new(d)?;
            if !seen.insert(skill.meta.id.clone()) {
                return Err(SkillError::DuplicateSkillId(skill.meta.id));
            }
            skills.push(Arc::new(skill));
        }
        Ok(skills)
    }
}

#[async_trait]
impl Skill for EmbeddedSkill {
    fn meta(&self) -> &SkillMeta {
        &self.meta
    }

    async fn read_instructions(&self) -> Result<String, SkillError> {
        Ok(self.skill_md.clone())
    }

    async fn load_resource(
        &self,
        kind: SkillResourceKind,
        path: &str,
    ) -> Result<SkillResource, SkillError> {
        match kind {
            SkillResourceKind::Reference => self
                .references
                .get(path)
                .cloned()
                .map(SkillResource::Reference)
                .ok_or_else(|| SkillError::Unsupported(format!("reference not available: {path}"))),
            SkillResourceKind::Asset => self
                .assets
                .get(path)
                .cloned()
                .map(SkillResource::Asset)
                .ok_or_else(|| SkillError::Unsupported(format!("asset not available: {path}"))),
        }
    }

    async fn run_script(&self, script: &str, _args: &[String]) -> Result<ScriptResult, SkillError> {
        Err(SkillError::Unsupported(format!(
            "embedded skills do not support script execution: {script}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SKILL_MD: &str = "\
---
name: test-skill
description: A test skill for unit testing.
allowed-tools: read_file write_file
---
# Test Skill

Follow these instructions to do testing.
";

    const VALID_SKILL_MD_2: &str = "\
---
name: another-skill
description: Another test skill.
---
# Another

More instructions.
";

    const REFERENCE_CONTENT: &str = "# Guide\n\nSome reference material.\n";

    #[test]
    fn new_parses_valid_skill() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[("references/guide.md", REFERENCE_CONTENT)],
            assets: &[],
        };

        let skill = EmbeddedSkill::new(&data).unwrap();
        assert_eq!(skill.meta.id, "test-skill");
        assert_eq!(skill.meta.name, "test-skill");
        assert_eq!(skill.meta.description, "A test skill for unit testing.");
        assert_eq!(
            skill.meta.allowed_tools,
            vec!["read_file".to_string(), "write_file".to_string()]
        );
    }

    #[test]
    fn new_rejects_invalid_skill_md() {
        let data = EmbeddedSkillData {
            skill_md: "not valid frontmatter",
            references: &[],
            assets: &[],
        };

        let err = EmbeddedSkill::new(&data).unwrap_err();
        assert!(matches!(err, SkillError::InvalidSkillMd(_)));
    }

    #[test]
    fn from_static_slice_rejects_duplicate_ids() {
        let data = &[
            EmbeddedSkillData {
                skill_md: VALID_SKILL_MD,
                references: &[],
                assets: &[],
            },
            EmbeddedSkillData {
                skill_md: VALID_SKILL_MD,
                references: &[],
                assets: &[],
            },
        ];

        let err = EmbeddedSkill::from_static_slice(data).unwrap_err();
        assert!(matches!(err, SkillError::DuplicateSkillId(ref id) if id == "test-skill"));
    }

    #[test]
    fn from_static_slice_accepts_unique_ids() {
        let data = &[
            EmbeddedSkillData {
                skill_md: VALID_SKILL_MD,
                references: &[],
                assets: &[],
            },
            EmbeddedSkillData {
                skill_md: VALID_SKILL_MD_2,
                references: &[],
                assets: &[],
            },
        ];

        let skills = EmbeddedSkill::from_static_slice(data).unwrap();
        assert_eq!(skills.len(), 2);
    }

    #[tokio::test]
    async fn read_instructions_returns_raw_content() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();

        let md = skill.read_instructions().await.unwrap();
        assert!(md.contains("# Test Skill"));
        assert!(md.contains("name: test-skill"));
    }

    #[tokio::test]
    async fn load_reference_returns_content() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[("references/guide.md", REFERENCE_CONTENT)],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();

        let r = skill
            .load_resource(SkillResourceKind::Reference, "references/guide.md")
            .await
            .unwrap();
        let SkillResource::Reference(r) = r else {
            panic!("expected reference resource");
        };
        assert_eq!(r.skill, "test-skill");
        assert_eq!(r.path, "references/guide.md");
        assert_eq!(r.content, REFERENCE_CONTENT);
        assert_eq!(r.bytes, REFERENCE_CONTENT.len() as u64);
        assert!(!r.truncated);
        assert!(!r.sha256.is_empty());
    }

    #[tokio::test]
    async fn load_reference_unknown_returns_error() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Reference, "references/missing.md")
            .await
            .unwrap_err();
        assert!(matches!(err, SkillError::Unsupported(_)));
    }

    #[tokio::test]
    async fn run_script_returns_unsupported() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();

        let err = skill.run_script("scripts/run.sh", &[]).await.unwrap_err();
        assert!(matches!(err, SkillError::Unsupported(_)));
        assert!(err.to_string().contains("embedded skills do not support"));
    }

    #[test]
    fn skill_without_allowed_tools_has_empty_vec() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD_2,
            references: &[],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();
        assert!(skill.meta.allowed_tools.is_empty());
    }

    static MULTI_REF_DATA: EmbeddedSkillData = EmbeddedSkillData {
        skill_md: VALID_SKILL_MD,
        references: &[
            ("references/a.md", "Content A"),
            ("references/b.md", "Content B"),
        ],
        assets: &[],
    };

    #[tokio::test]
    async fn multiple_references_per_skill() {
        let skill = EmbeddedSkill::new(&MULTI_REF_DATA).unwrap();

        let a = skill
            .load_resource(SkillResourceKind::Reference, "references/a.md")
            .await
            .unwrap();
        let SkillResource::Reference(a) = a else {
            panic!("expected reference resource");
        };
        assert_eq!(a.content, "Content A");

        let b = skill
            .load_resource(SkillResourceKind::Reference, "references/b.md")
            .await
            .unwrap();
        let SkillResource::Reference(b) = b else {
            panic!("expected reference resource");
        };
        assert_eq!(b.content, "Content B");
    }

    #[test]
    fn reference_sha256_is_deterministic() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[("references/guide.md", REFERENCE_CONTENT)],
            assets: &[],
        };
        let skill1 = EmbeddedSkill::new(&data).unwrap();
        let skill2 = EmbeddedSkill::new(&data).unwrap();

        let hash1 = &skill1.references.get("references/guide.md").unwrap().sha256;
        let hash2 = &skill2.references.get("references/guide.md").unwrap().sha256;
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64);
    }

    #[test]
    fn clone_produces_equal_skill() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[("references/guide.md", REFERENCE_CONTENT)],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();
        let cloned = skill.clone();

        assert_eq!(skill.meta.id, cloned.meta.id);
    }

    #[test]
    fn new_with_valid_base64_asset() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[("assets/image.png", "aGVsbG8=", Some("image/png"))],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();
        assert!(skill.assets.contains_key("assets/image.png"));
        let asset = &skill.assets["assets/image.png"];
        assert_eq!(asset.skill, "test-skill");
        assert_eq!(asset.media_type.as_deref(), Some("image/png"));
        assert_eq!(asset.encoding, "base64");
        assert_eq!(asset.content, "aGVsbG8=");
        assert_eq!(asset.bytes, 5); // "hello" = 5 bytes
        assert!(!asset.truncated);
        assert!(!asset.sha256.is_empty());
    }

    #[test]
    fn new_rejects_invalid_base64_asset() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[("assets/bad.png", "not-valid-base64!!!", Some("image/png"))],
        };
        let err = EmbeddedSkill::new(&data).unwrap_err();
        assert!(matches!(err, SkillError::InvalidSkillMd(_)));
        assert!(err.to_string().contains("invalid base64"));
    }

    #[test]
    fn new_with_asset_no_media_type() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[("assets/data.bin", "AQID", None)],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();
        let asset = &skill.assets["assets/data.bin"];
        assert!(asset.media_type.is_none());
    }

    #[tokio::test]
    async fn load_asset_returns_content() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[("assets/image.png", "aGVsbG8=", Some("image/png"))],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();

        let r = skill
            .load_resource(SkillResourceKind::Asset, "assets/image.png")
            .await
            .unwrap();
        let SkillResource::Asset(a) = r else {
            panic!("expected asset resource");
        };
        assert_eq!(a.skill, "test-skill");
        assert_eq!(a.content, "aGVsbG8=");
        assert_eq!(a.media_type.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn load_asset_unknown_returns_error() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Asset, "assets/missing.png")
            .await
            .unwrap_err();
        assert!(matches!(err, SkillError::Unsupported(_)));
        assert!(err.to_string().contains("asset not available"));
    }

    #[test]
    fn asset_sha256_is_deterministic() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[],
            assets: &[("assets/img.png", "aGVsbG8=", Some("image/png"))],
        };
        let s1 = EmbeddedSkill::new(&data).unwrap();
        let s2 = EmbeddedSkill::new(&data).unwrap();
        let h1 = &s1.assets["assets/img.png"].sha256;
        let h2 = &s2.assets["assets/img.png"].sha256;
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn from_static_slice_empty_succeeds() {
        let data: &[EmbeddedSkillData] = &[];
        let skills = EmbeddedSkill::from_static_slice(data).unwrap();
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn load_reference_for_unknown_path_returns_error() {
        let data = EmbeddedSkillData {
            skill_md: VALID_SKILL_MD,
            references: &[("references/guide.md", REFERENCE_CONTENT)],
            assets: &[],
        };
        let skill = EmbeddedSkill::new(&data).unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Reference, "references/nonexistent.md")
            .await
            .unwrap_err();
        assert!(matches!(err, SkillError::Unsupported(_)));
    }
}
