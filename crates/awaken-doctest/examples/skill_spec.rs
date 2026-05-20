//! `SkillSpec` + `SkillArgumentSpec` round-trip — pins the surface
//! `how-to/use-skills-subsystem.md` cites.

use awaken::{SkillArgumentSpec, SkillSpec};

fn main() {
    let arg = SkillArgumentSpec {
        name: "query".into(),
        description: Some("search query".into()),
        required: true,
    };

    let spec = SkillSpec {
        id: "search".into(),
        name: "web-search".into(),
        description: "Search the web.".into(),
        instructions_md: "Use keywords, not full sentences.".into(),
        allowed_tools: vec!["http_get".into()],
        arguments: vec![arg],
        ..Default::default()
    };

    let json = serde_json::to_value(&spec).expect("encode");
    let parsed: SkillSpec = serde_json::from_value(json).expect("decode");
    assert_eq!(parsed.id, "search");
    assert_eq!(parsed.allowed_tools, vec!["http_get".to_string()]);
    assert_eq!(parsed.arguments.len(), 1);
}
