// @formal surface exclude=contract_vocabulary
// @formal owner crates/server/awaken-session-application/src/file_content_access.rs
// @formal reason: Closed ownership coordinates only; decoding does not authenticate or authorize a caller.
//! Shared coordinates for original private Session and File records.

/// A data value supplied by trusted ingress, never an authorization proof.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "BindingFields")]
pub struct PrivateContentBinding {
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

impl PrivateContentBinding {
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
            return Err("invalid private content binding coordinates");
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

impl TryFrom<BindingFields> for PrivateContentBinding {
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
