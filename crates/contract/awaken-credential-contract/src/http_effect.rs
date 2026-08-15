use serde::{Deserialize, Serialize};

use crate::CredentialUsage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpEffectMaterialShape {
    Secret,
    Structured,
}

/// Exact material-shape kernel shared by the concrete material validator and
/// its bounded proof. A scalar secret can bind one and only one declared field;
/// structured material must have the complete, equal key set. Empty effects are
/// rejected here as well as by [`CredentialUsage::validate`], keeping the kernel
/// fail closed when reused independently.
#[must_use]
pub(crate) const fn http_effect_material_shape_is_exact(
    shape: HttpEffectMaterialShape,
    declared_fields: usize,
    structured_keys_exact: bool,
) -> bool {
    declared_fields != 0
        && match shape {
            HttpEffectMaterialShape::Secret => declared_fields == 1,
            HttpEffectMaterialShape::Structured => structured_keys_exact,
        }
}

/// One exact destination at which a hosted HTTP effect may render a material
/// field. JSON pointers are rooted at the effect's `json` or `body` value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HttpEffectPlacement {
    Header { name: String },
    Query { name: String },
    JsonPointer { pointer: String },
}

impl CredentialUsage {
    /// Validate canonical, secret-free usage facts before material is opened.
    pub fn validate(&self) -> Result<(), CredentialUsageError> {
        let Self::HttpEffect { fields } = self else {
            return Ok(());
        };
        if fields.is_empty() {
            return Err(CredentialUsageError::EmptyHttpEffectFields);
        }
        for (field, placements) in fields {
            if field.trim().is_empty() || field.trim() != field {
                return Err(CredentialUsageError::InvalidMaterialField);
            }
            if placements.is_empty() {
                return Err(CredentialUsageError::EmptyHttpEffectPlacements);
            }
            for placement in placements {
                match placement {
                    HttpEffectPlacement::Header { name } if !is_canonical_header_name(name) => {
                        return Err(CredentialUsageError::InvalidHeaderName);
                    }
                    HttpEffectPlacement::Query { name } if !is_valid_query_name(name) => {
                        return Err(CredentialUsageError::InvalidQueryName);
                    }
                    HttpEffectPlacement::JsonPointer { pointer }
                        if !is_valid_json_pointer(pointer) =>
                    {
                        return Err(CredentialUsageError::InvalidJsonPointer);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Return the sole declared material field for a built-in HTTP effect.
    /// Gateways use this to bind an opaque [`crate::CredentialMaterial::Secret`]
    /// to a route projection's `sole_field` without reimplementing map rules.
    pub fn http_effect_single_field(&self) -> Result<Option<&str>, CredentialUsageError> {
        self.validate()?;
        let Self::HttpEffect { fields } = self else {
            return Ok(None);
        };
        Ok((fields.len() == 1).then(|| {
            fields
                .first_key_value()
                .expect("validated HTTP effect has at least one field")
                .0
                .as_str()
        }))
    }
}

fn is_canonical_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_valid_query_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'&' | b'=' | b'#'))
}

fn is_valid_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') || pointer.chars().any(char::is_control) {
        return false;
    }
    let mut bytes = pointer.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~' && !matches!(bytes.next(), Some(b'0' | b'1')) {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CredentialUsageError {
    #[error("HTTP-effect credential usage declares no material fields")]
    EmptyHttpEffectFields,
    #[error("HTTP-effect credential usage has an invalid material field")]
    InvalidMaterialField,
    #[error("HTTP-effect credential usage declares a field with no placements")]
    EmptyHttpEffectPlacements,
    #[error("HTTP-effect credential usage has a non-canonical header name")]
    InvalidHeaderName,
    #[error("HTTP-effect credential usage has an invalid query name")]
    InvalidQueryName,
    #[error("HTTP-effect credential usage has an invalid RFC 6901 JSON pointer")]
    InvalidJsonPointer,
}

#[cfg(kani)]
mod verification {
    use super::{HttpEffectMaterialShape, http_effect_material_shape_is_exact};

    /// Exhaustively proves the HTTP effect material shape cannot silently drop,
    /// add, or merge fields: scalar material binds exactly one declared field,
    /// while structured material requires non-empty exact key equality.
    #[kani::proof]
    fn http_effect_material_shape_is_exact_and_non_widening() {
        let declared_fields = kani::any::<usize>();
        let structured_keys_exact = kani::any::<bool>();
        let shape = if kani::any::<bool>() {
            HttpEffectMaterialShape::Secret
        } else {
            HttpEffectMaterialShape::Structured
        };
        let admitted =
            http_effect_material_shape_is_exact(shape, declared_fields, structured_keys_exact);

        match shape {
            HttpEffectMaterialShape::Secret => {
                assert_eq!(admitted, declared_fields == 1);
            }
            HttpEffectMaterialShape::Structured => {
                assert_eq!(admitted, declared_fields != 0 && structured_keys_exact);
            }
        }
        if admitted {
            assert_ne!(declared_fields, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use awaken_agent_contract::{RedactedString, StructuredCredentialMaterial};

    use super::*;
    use crate::{
        CredentialMaterial, CredentialMaterialBinding, CredentialMaterialError,
        OAuthCredentialMaterial,
    };

    fn placements(
        field: &str,
        placements: impl IntoIterator<Item = HttpEffectPlacement>,
    ) -> (String, BTreeSet<HttpEffectPlacement>) {
        (field.to_owned(), placements.into_iter().collect())
    }

    fn exact_usage() -> CredentialUsage {
        CredentialUsage::HttpEffect {
            fields: BTreeMap::from([
                placements(
                    "account",
                    [HttpEffectPlacement::Query {
                        name: "account".into(),
                    }],
                ),
                placements(
                    "token",
                    [
                        HttpEffectPlacement::Header {
                            name: "authorization".into(),
                        },
                        HttpEffectPlacement::JsonPointer {
                            pointer: "/nested/0".into(),
                        },
                    ],
                ),
            ]),
        }
    }

    /// HTTP-effect cause/effect graph: C1 usage fields/placements are
    /// well-formed; C2 opaque material has one sole field; C3 structured
    /// material fields exactly equal the declared fields; C4 material is OAuth.
    /// E1 admits only the exact single/structured shapes; E2 rejects malformed,
    /// broader, narrower, and OAuth shapes before any last-mile effect.
    ///
    /// | Rule | C1 | Material | Field relation | Effect |
    /// |---|---|---|---|---|
    /// | H1 | T | Secret | one sole field | admit |
    /// | H2 | T | Secret | two fields | reject |
    /// | H3 | T | Structured | exact | admit |
    /// | H4 | T | Structured | extra/missing | reject |
    /// | H5 | T | OAuth | - | reject |
    /// | H6 | F | any | malformed pointer | reject before material |
    #[test]
    fn usage_freezes_exact_fields_and_placements() {
        let exact_usage = exact_usage();
        assert_eq!(exact_usage.validate(), Ok(()));
        let sole_usage = CredentialUsage::HttpEffect {
            fields: BTreeMap::from([placements(
                "token",
                [HttpEffectPlacement::Header {
                    name: "authorization".into(),
                }],
            )]),
        };
        assert_eq!(sole_usage.http_effect_single_field(), Ok(Some("token")));
        assert_eq!(exact_usage.http_effect_single_field(), Ok(None));
        let secret = CredentialMaterial::secret(RedactedString::new("secret"));
        assert_eq!(secret.validate_usage(&sole_usage), Ok(()), "H1");
        assert_eq!(
            secret.validate_usage(&exact_usage),
            Err(CredentialMaterialError::MaterialKindMismatch),
            "H2"
        );

        let structured = |fields: &[&str]| {
            CredentialMaterial::Structured(StructuredCredentialMaterial {
                type_id: "example.connector/v1".into(),
                fields: fields
                    .iter()
                    .map(|field| ((*field).to_owned(), RedactedString::new("secret")))
                    .collect(),
            })
        };
        assert_eq!(
            structured(&["account", "token"]).validate_usage(&exact_usage),
            Ok(()),
            "H3"
        );
        for fields in [&["token"][..], &["account", "token", "unused"][..]] {
            assert_eq!(
                structured(fields).validate_usage(&exact_usage),
                Err(CredentialMaterialError::MaterialKindMismatch),
                "H4"
            );
        }
        let oauth = CredentialMaterial::OAuth(OAuthCredentialMaterial {
            access_token: RedactedString::new("access"),
            refresh_token: RedactedString::new("refresh"),
            expires_at_unix_ms: None,
            account_id: None,
            account_plan: None,
        });
        assert_eq!(
            oauth.validate_usage(&sole_usage),
            Err(CredentialMaterialError::MaterialKindMismatch),
            "H5"
        );
        let malformed = CredentialUsage::HttpEffect {
            fields: BTreeMap::from([placements(
                "token",
                [HttpEffectPlacement::JsonPointer {
                    pointer: "/bad~2pointer".into(),
                }],
            )]),
        };
        assert_eq!(
            malformed.validate(),
            Err(CredentialUsageError::InvalidJsonPointer),
            "H6"
        );
    }

    /// Target-binding rationale: destinations are authorization facts, so two
    /// otherwise identical effects that place one field differently must never
    /// share a target/use fingerprint. The serde assertion freezes the neutral
    /// cross-repository wire names.
    #[test]
    fn placement_is_part_of_the_target_use_fingerprint() {
        let usage = |placement| CredentialUsage::HttpEffect {
            fields: BTreeMap::from([("token".into(), BTreeSet::from([placement]))]),
        };
        let header = usage(HttpEffectPlacement::Header {
            name: "authorization".into(),
        });
        let query = usage(HttpEffectPlacement::Query {
            name: "token".into(),
        });
        assert_ne!(
            CredentialMaterialBinding::for_target("workspace", &"route", &header)
                .target_use_fingerprint,
            CredentialMaterialBinding::for_target("workspace", &"route", &query)
                .target_use_fingerprint
        );
        assert_eq!(
            serde_json::to_value(&header).unwrap(),
            serde_json::json!({
                "type": "http_effect",
                "fields": {
                    "token": [{"type": "header", "name": "authorization"}]
                }
            })
        );
    }
}
