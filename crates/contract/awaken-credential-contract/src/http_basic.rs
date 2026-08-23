use std::collections::BTreeMap;

use awaken_agent_contract::{RedactedString, StructuredCredentialMaterial};

pub const HTTP_BASIC_MATERIAL_TYPE: &str = "awaken.http-basic/v1";

/// Build the canonical typed material consumed by HTTP Basic authentication.
/// Protocol owners still decide the username and password semantics.
#[must_use]
pub fn http_basic_material(
    username: RedactedString,
    password: RedactedString,
) -> StructuredCredentialMaterial {
    StructuredCredentialMaterial {
        type_id: HTTP_BASIC_MATERIAL_TYPE.to_owned(),
        fields: BTreeMap::from([("username".into(), username), ("password".into(), password)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause/effect rule H1: supplied username + password produces the one
    /// canonical type with exactly those two named fields.
    #[test]
    fn construction_has_one_closed_shape() {
        let material =
            http_basic_material(RedactedString::new("user"), RedactedString::new("password"));
        assert_eq!(material.type_id, HTTP_BASIC_MATERIAL_TYPE, "H1");
        assert_eq!(material.fields.len(), 2, "H1");
        assert_eq!(material.fields["username"].expose_secret(), "user", "H1");
        assert_eq!(
            material.fields["password"].expose_secret(),
            "password",
            "H1"
        );
    }
}
