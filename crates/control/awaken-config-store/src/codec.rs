use awaken_agent_config::StoredPublication;
use awaken_tenancy::ScopeId;

fn bind_legacy_execution_workspace(
    value: &mut serde_json::Value,
    configuration_scope: &ScopeId,
) -> Result<(), serde_json::Error> {
    let object = value.as_object_mut().ok_or_else(|| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stored publication must be a JSON object",
        ))
    })?;
    object
        .entry("execution_workspace")
        .or_insert_with(|| serde_json::json!(configuration_scope.as_str()));
    Ok(())
}

pub(crate) fn decode_publication_value(
    mut value: serde_json::Value,
    configuration_scope: &ScopeId,
) -> Result<StoredPublication, serde_json::Error> {
    bind_legacy_execution_workspace(&mut value, configuration_scope)?;
    serde_json::from_value(value)
}

pub(crate) fn decode_publication(
    record: &str,
    configuration_scope: &ScopeId,
) -> Result<StoredPublication, serde_json::Error> {
    decode_publication_value(serde_json::from_str(record)?, configuration_scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_workspace_upgrade_is_scope_bound_and_non_overwriting() {
        // Causes: C1 historical object omits the coordinate; C2 current object
        // carries an explicit target; C3 malformed non-object. Effects: E1 bind
        // the row's configuration scope; E2 preserve the exact target; E3 fail
        // before domain decoding. Rules: C1=>E1; C2=>E2; C3=>E3.
        let scope = ScopeId::from("workspace-a");
        let mut historical = serde_json::json!({"fingerprint": "legacy"});
        bind_legacy_execution_workspace(&mut historical, &scope).expect("C1");
        assert_eq!(historical["execution_workspace"], "workspace-a", "E1");

        let mut current = serde_json::json!({"execution_workspace": "workspace-b"});
        bind_legacy_execution_workspace(&mut current, &scope).expect("C2");
        assert_eq!(current["execution_workspace"], "workspace-b", "E2");

        assert!(
            bind_legacy_execution_workspace(&mut serde_json::json!([]), &scope).is_err(),
            "E3"
        );
    }
}
