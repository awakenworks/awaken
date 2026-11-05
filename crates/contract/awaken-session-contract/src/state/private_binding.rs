// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Native Session ownership fact; ingress authentication and current PDP admission remain external.
//! Immutable product ownership of a personal Session, outside the Runtime core.

/// Coordinates supplied by trusted application ingress, never editable metadata.
/// Decoding validates shape only; it is not authentication or authorization.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SessionPrivateBinding {
    content: awaken_resource_contract::PrivateContentBinding,
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
        awaken_resource_contract::PrivateContentBinding::new(
            org_id,
            application_id,
            account_id,
            workspace_id,
            application_binding_revision,
        )
        .map(|content| Self { content })
        .map_err(|_| "invalid private Session binding coordinates")
    }

    pub fn org_id(&self) -> &str {
        self.content.org_id()
    }
    pub fn application_id(&self) -> &str {
        self.content.application_id()
    }
    pub fn account_id(&self) -> &str {
        self.content.account_id()
    }
    pub fn workspace_id(&self) -> &str {
        self.content.workspace_id()
    }
    pub fn application_binding_revision(&self) -> u64 {
        self.content.application_binding_revision()
    }
}

impl AsRef<awaken_resource_contract::PrivateContentBinding> for SessionPrivateBinding {
    fn as_ref(&self) -> &awaken_resource_contract::PrivateContentBinding {
        &self.content
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
        // Stored roots and original create keys already use this flat codec.
        // A shared coordinate type must preserve these exact original bytes.
        assert_eq!(
            serde_json::to_string(&binding).unwrap(),
            r#"{"org_id":"org","application_id":"desk","account_id":"alice","workspace_id":"workspace","application_binding_revision":1}"#,
        );
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
