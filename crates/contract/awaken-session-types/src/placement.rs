//! Frozen execution placement shared by Session commands and observations.

/// Environment Work selection cannot reinterpret this frozen Runtime owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimePlacement {
    Local,
    Worker,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_runtime_placement_retains_its_durable_wire_values() {
        for (placement, wire) in [
            (SessionRuntimePlacement::Local, "\"local\""),
            (SessionRuntimePlacement::Worker, "\"worker\""),
        ] {
            assert_eq!(serde_json::to_string(&placement).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<SessionRuntimePlacement>(wire).unwrap(),
                placement
            );
        }
        for invalid in ["\"registered_worker\"", "null", "{}"] {
            assert!(serde_json::from_str::<SessionRuntimePlacement>(invalid).is_err());
        }
    }
}
