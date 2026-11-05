// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Native Session ownership fact; ingress authentication and current PDP admission remain external.
//! Immutable product ownership of a personal Session, outside the Runtime core.

/// Coordinates supplied by trusted application ingress, never editable metadata.
/// Decoding validates shape only; it is not authentication or authorization.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "BindingFields")]
pub struct SessionPrivateBinding {
    org_id: String,
    application_id: String,
    account_id: String,
    workspace_id: String,
    application_binding_revision: u64,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingFields {
    org_id: String,
    application_id: String,
    account_id: String,
    workspace_id: String,
    application_binding_revision: u64,
}

impl SessionPrivateBinding {
    /// Observe this component's immutable fact without expanding the root API.
    #[must_use]
    pub fn of(root: &crate::PersistedSession) -> Option<&Self> {
        root.private_binding.as_ref()
    }

    /// Bind once before the first root insert. The native root CAS separately
    /// refuses changes through decoded or otherwise forged replacements.
    pub fn bind_fresh(
        self,
        mut root: crate::PersistedSession,
    ) -> Result<crate::PersistedSession, crate::SessionMutationValidationError> {
        if root.revision.0 != 0
            || root.private_binding.is_some()
            || root.created_at_unix_ms.is_some()
            || root.updated_at_unix_ms.is_some()
        {
            return Err(crate::SessionMutationValidationError::PrivateBindingChanged);
        }
        root.private_binding = Some(self);
        Ok(root)
    }

    pub fn new(
        org_id: String,
        application_id: String,
        account_id: String,
        workspace_id: String,
        application_binding_revision: u64,
    ) -> Result<Self, &'static str> {
        let exact = |value: &str| {
            !value.is_empty()
                && value.len() <= 256
                && value.trim() == value
                && !value.chars().any(char::is_control)
        };
        if [&org_id, &application_id, &account_id, &workspace_id]
            .into_iter()
            .any(|value| !exact(value))
            || !application_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || application_binding_revision == 0
        {
            return Err("invalid private Session binding coordinates");
        }
        Ok(Self {
            org_id,
            application_id,
            account_id,
            workspace_id,
            application_binding_revision,
        })
    }

    pub fn org_id(&self) -> &str {
        &self.org_id
    }
    pub fn application_id(&self) -> &str {
        &self.application_id
    }
    pub fn account_id(&self) -> &str {
        &self.account_id
    }
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
    pub fn application_binding_revision(&self) -> u64 {
        self.application_binding_revision
    }
}

impl TryFrom<BindingFields> for SessionPrivateBinding {
    type Error = &'static str;
    fn try_from(value: BindingFields) -> Result<Self, Self::Error> {
        Self::new(
            value.org_id,
            value.application_id,
            value.account_id,
            value.workspace_id,
            value.application_binding_revision,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_binding_closed_codec_refuses_unknown_and_malformed_coordinates() {
        let binding = SessionPrivateBinding::new(
            "org".into(),
            "desk".into(),
            "alice".into(),
            "workspace".into(),
            1,
        )
        .unwrap();
        let value = serde_json::to_value(&binding).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionPrivateBinding>(value.clone()).unwrap(),
            binding
        );
        for key in ["org_id", "application_id", "account_id", "workspace_id"] {
            for invalid in ["", "  alice", "alice\n", &"x".repeat(257)] {
                let mut changed = value.clone();
                changed[key] = serde_json::json!(invalid);
                assert!(
                    serde_json::from_value::<SessionPrivateBinding>(changed).is_err(),
                    "{key}"
                );
            }
        }
        for (key, invalid) in [
            ("application_id", serde_json::json!("desk/other")),
            ("application_binding_revision", serde_json::json!(0)),
            ("owner_override", serde_json::json!(true)),
        ] {
            let mut changed = value.clone();
            changed[key] = invalid;
            assert!(
                serde_json::from_value::<SessionPrivateBinding>(changed).is_err(),
                "{key}"
            );
        }
    }
}
