//! Generated JSON Schema registry for the Anthropic-owned DTOs whose 0.117
//! beta and 0.120 GA forms Awaken serves. The Rust DTO derives are the only
//! local schema source; the official SDK declaration fingerprint remains the
//! upstream oracle.

use schemars::schema_for;
use serde_json::{Map, Value};

#[must_use]
pub fn contract_schemas() -> Map<String, Value> {
    let mut schemas = Map::new();
    macro_rules! add {
        ($name:literal, $ty:ty) => {
            schemas.insert(
                $name.into(),
                serde_json::to_value(schema_for!($ty)).expect("schema serializes"),
            );
        };
    }

    add!("FileListParams", crate::types::file::FileListParams);
    add!("BetaFileListParams", crate::types::file::BetaFileListParams);
    add!("FileMetadata", crate::types::file::FileMetadata);
    add!("BetaFileMetadata", crate::types::file::BetaFileMetadata);
    add!("DeletedFile", crate::types::file::DeletedFile);
    add!("SkillListParams", crate::types::skill::SkillListParams);
    add!(
        "BetaSkillListParams",
        crate::types::skill::BetaSkillListParams
    );
    add!(
        "SkillVersionListParams",
        crate::types::skill::SkillVersionListParams
    );
    add!(
        "BetaSkillVersionListParams",
        crate::types::skill::BetaSkillVersionListParams
    );
    add!("Skill", crate::types::skill::Skill);
    add!("BetaSkill", crate::types::skill::BetaSkill);
    add!("SkillVersion", crate::types::skill::SkillVersion);
    add!("BetaSkillVersion", crate::types::skill::BetaSkillVersion);
    add!("DeletedSkill", crate::types::skill::DeletedSkill);
    add!(
        "DeletedSkillVersion",
        crate::types::skill::DeletedSkillVersion
    );
    add!("BetaWorkSecret", crate::types::environment::WorkSecret);
    add!("BetaSelfHostedWork", crate::types::environment::Work);
    add!(
        "BetaSelfHostedWorkHeartbeatResponse",
        crate::types::environment::WorkHeartbeat
    );
    add!(
        "BetaSelfHostedWorkQueueStats",
        crate::types::environment::WorkQueueStats
    );
    add!(
        "BetaSelfHostedWorkStopRequest",
        crate::types::environment::WorkStopParams
    );
    add!(
        "BetaSelfHostedWorkUpdateRequest",
        crate::types::environment::WorkUpdateParams
    );
    add!("BetaManagedAgentsActor", crate::types::memory::MemoryActor);
    add!(
        "BetaManagedAgentsMemoryVersion",
        crate::types::memory::MemoryVersion
    );
    schemas
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn required(schemas: &Map<String, Value>, name: &str) -> BTreeSet<String> {
        schemas[name]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name} must have a required array"))
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .expect("required names are strings")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn every_changed_sdk_family_has_one_generated_rust_schema() {
        // Cause/effect graph: C1 0.120 GA Files/Skills, C2 worker secret/state,
        // C3 Memory actor/version. Effect E1 each public SDK DTO has exactly one
        // Rust-derived schema entry; E2 beta and GA names remain disjoint.
        // Decision table: R1 C1->both beta+GA entries; R2 C2->work+secret entries;
        // R3 C3->actor+version entries. Missing or duplicate ownership fails here.
        let schemas = contract_schemas();
        for name in [
            "FileMetadata",
            "BetaFileMetadata",
            "Skill",
            "BetaSkill",
            "BetaWorkSecret",
            "BetaSelfHostedWork",
            "BetaManagedAgentsActor",
            "BetaManagedAgentsMemoryVersion",
        ] {
            assert!(schemas.contains_key(name), "missing schema {name}");
        }
        assert_ne!(schemas["Skill"], schemas["BetaSkill"], "R1/E2");
        assert_ne!(
            schemas["FileMetadata"], schemas["BetaFileMetadata"],
            "R1/E2"
        );
    }

    #[test]
    fn generated_schema_preserves_sdk_required_and_nullable_semantics() {
        // Cause/effect graph: C1=TS `field?: T`; C2=TS `field: T|null`;
        // C3=TS `field: T`. Effects: E1=property not required; E2=property is
        // required and admits null; E3=property is required and rejects null.
        // Decision rules: File downloadable/expires_at use C1->E1; every Work
        // timestamp/secret and both nullable stats fields use C2->E2; stable
        // identity/state fields use C3->E3. Exact sets make drift visible when
        // either Rust optionality or the upstream 0.120 declarations change.
        let schemas = contract_schemas();
        assert_eq!(
            required(&schemas, "FileMetadata"),
            [
                "created_at",
                "filename",
                "id",
                "mime_type",
                "size_bytes",
                "type"
            ]
            .map(str::to_string)
            .into_iter()
            .collect(),
            "C1/E1"
        );
        assert_eq!(
            required(&schemas, "BetaSelfHostedWork"),
            [
                "acknowledged_at",
                "created_at",
                "data",
                "environment_id",
                "id",
                "latest_heartbeat_at",
                "metadata",
                "secret",
                "started_at",
                "state",
                "stop_requested_at",
                "stopped_at",
                "type",
            ]
            .map(str::to_string)
            .into_iter()
            .collect(),
            "C2+C3/E2+E3"
        );
        assert_eq!(
            required(&schemas, "BetaSelfHostedWorkQueueStats"),
            [
                "depth",
                "oldest_queued_at",
                "pending",
                "type",
                "workers_polling"
            ]
            .map(str::to_string)
            .into_iter()
            .collect(),
            "C2+C3/E2+E3"
        );
        for (name, field) in [
            ("BetaSelfHostedWork", "secret"),
            ("BetaSelfHostedWork", "stopped_at"),
            ("BetaSelfHostedWorkQueueStats", "oldest_queued_at"),
            ("BetaSelfHostedWorkQueueStats", "workers_polling"),
        ] {
            assert!(
                schemas[name]["properties"][field]["type"]
                    .as_array()
                    .is_some_and(|types| types.iter().any(|value| value == "null")),
                "C2/E2 {name}.{field}"
            );
        }
        for (name, literal_type, value) in [
            ("FileMetadata", "FileObjectType", "file"),
            ("Skill", "SkillObjectType", "skill"),
            ("BetaSelfHostedWork", "WorkObjectType", "work"),
            (
                "BetaManagedAgentsMemoryVersion",
                "MemoryVersionObjectType",
                "memory_version",
            ),
        ] {
            assert_eq!(
                schemas[name]["$defs"][literal_type]["enum"],
                serde_json::json!([value]),
                "C3/E3 {name}.type must be a one-value Rust enum"
            );
        }
    }
}
