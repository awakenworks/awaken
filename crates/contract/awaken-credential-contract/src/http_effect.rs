use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};

use crate::CredentialUsage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedMaterialShape {
    Secret,
    Structured,
}

/// Exact named-material kernel shared by HTTP effects, signature verification,
/// the concrete material validator, and its bounded proof. A scalar secret can
/// bind one and only one declared field; structured material must have the
/// complete, equal key set. Empty declarations remain fail closed when this
/// kernel is reused independently.
#[must_use]
pub(crate) const fn named_material_shape_is_exact(
    shape: NamedMaterialShape,
    declared_fields: usize,
    structured_keys_exact: bool,
) -> bool {
    declared_fields != 0
        && match shape {
            NamedMaterialShape::Secret => declared_fields == 1,
            NamedMaterialShape::Structured => structured_keys_exact,
        }
}

/// One exact destination at which a hosted HTTP effect may render a material
/// field. JSON pointers are rooted at the effect's `json` or `body` value.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HttpEffectPlacement {
    Header { name: String },
    Query { name: String },
    JsonPointer { pointer: String },
}

/// One exact secret-free material reference embedded in an HTTP effect.
///
/// Product-owned effect DTOs keep their own request shape. This neutral value
/// owns only the shared reference wire and its deterministic string rendering,
/// so authoring and execution cannot disagree about what counts as a material
/// reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpEffectMaterialReference {
    #[serde(deserialize_with = "deserialize_material_coordinate")]
    credential: String,
    #[serde(deserialize_with = "deserialize_material_coordinate")]
    field: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
}

impl HttpEffectMaterialReference {
    /// Parse a header or query value. Strings are literals; every other value
    /// is a reference candidate and therefore must match the exact wire.
    pub fn from_scalar_value(
        value: &serde_json::Value,
    ) -> Result<Option<Self>, HttpEffectMaterialReferenceError> {
        if value.is_string() {
            return Ok(None);
        }
        if !value.is_object() {
            return Err(HttpEffectMaterialReferenceError::InvalidScalarValue);
        }
        Self::parse_candidate(value, true)
    }

    /// Parse one value while recursively walking a JSON body. An object that
    /// contains the `credential` discriminator is a reference candidate and
    /// must match the exact wire; all other JSON values remain literal body
    /// content.
    pub fn from_json_value(
        value: &serde_json::Value,
    ) -> Result<Option<Self>, HttpEffectMaterialReferenceError> {
        let candidate = value
            .as_object()
            .is_some_and(|object| object.contains_key("credential"));
        Self::parse_candidate(value, candidate)
    }

    fn parse_candidate(
        value: &serde_json::Value,
        candidate: bool,
    ) -> Result<Option<Self>, HttpEffectMaterialReferenceError> {
        if !candidate {
            return Ok(None);
        }
        serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| HttpEffectMaterialReferenceError::InvalidReference)
    }

    #[must_use]
    pub fn credential(&self) -> &str {
        &self.credential
    }

    #[must_use]
    pub fn field(&self) -> &str {
        &self.field
    }

    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    #[must_use]
    pub fn suffix(&self) -> &str {
        &self.suffix
    }

    /// Render already-authorized material without performing lookup or
    /// widening the reference. Callers resolve `credential` + `field`, then
    /// pass that exact value here at the final plaintext boundary.
    #[must_use]
    pub fn render(&self, material: &str) -> String {
        format!("{}{material}{}", self.prefix, self.suffix)
    }
}

fn deserialize_material_coordinate<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(serde::de::Error::custom(
            "HTTP-effect material coordinate must be nonempty, unpadded, and control-free",
        ));
    }
    Ok(value)
}

/// Exact field-to-destination map for one credential role.
pub type HttpEffectFieldPlacements = BTreeMap<String, BTreeSet<HttpEffectPlacement>>;

/// Canonical role-to-field-to-destination projection of one HTTP effect.
///
/// The map is intentionally wrapped so consumers compare or transform the
/// complete projection instead of constructing a second interpretation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpEffectMaterialPlacements(BTreeMap<String, HttpEffectFieldPlacements>);

impl HttpEffectMaterialPlacements {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn for_credential(&self, credential: &str) -> Option<&HttpEffectFieldPlacements> {
        self.0.get(credential)
    }

    #[must_use]
    pub fn as_map(&self) -> &BTreeMap<String, HttpEffectFieldPlacements> {
        &self.0
    }

    #[must_use]
    pub fn into_map(self) -> BTreeMap<String, HttpEffectFieldPlacements> {
        self.0
    }

    fn record(
        &mut self,
        reference: &HttpEffectMaterialReference,
        placement: HttpEffectPlacement,
    ) -> Result<(), HttpEffectMaterialReferenceError> {
        validate_http_effect_placement(&placement)?;
        self.0
            .entry(reference.credential.clone())
            .or_default()
            .entry(reference.field.clone())
            .or_default()
            .insert(placement);
        Ok(())
    }
}

/// Project the exact references embedded in a product-owned HTTP effect.
///
/// Header names are normalized exactly once to their lowercase authorization
/// destination. Query names remain exact, and JSON pointers are canonical RFC
/// 6901 paths rooted at the supplied body value. Malformed candidates fail the
/// whole projection; they never degrade into literals.
pub fn project_http_effect_material_placements(
    headers: &BTreeMap<String, serde_json::Value>,
    query: &BTreeMap<String, serde_json::Value>,
    json_body: Option<&serde_json::Value>,
) -> Result<HttpEffectMaterialPlacements, HttpEffectMaterialReferenceError> {
    let mut placements = HttpEffectMaterialPlacements::default();
    for (name, value) in headers {
        if let Some(reference) = HttpEffectMaterialReference::from_scalar_value(value)? {
            placements.record(
                &reference,
                HttpEffectPlacement::Header {
                    name: name.trim().to_ascii_lowercase(),
                },
            )?;
        }
    }
    for (name, value) in query {
        if let Some(reference) = HttpEffectMaterialReference::from_scalar_value(value)? {
            placements.record(
                &reference,
                HttpEffectPlacement::Query { name: name.clone() },
            )?;
        }
    }
    if let Some(value) = json_body {
        collect_json_material_placements(value, "", &mut placements)?;
    }
    Ok(placements)
}

fn collect_json_material_placements(
    value: &serde_json::Value,
    pointer: &str,
    placements: &mut HttpEffectMaterialPlacements,
) -> Result<(), HttpEffectMaterialReferenceError> {
    if let Some(reference) = HttpEffectMaterialReference::from_json_value(value)? {
        return placements.record(
            &reference,
            HttpEffectPlacement::JsonPointer {
                pointer: pointer.to_owned(),
            },
        );
    }
    match value {
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_json_material_placements(value, &format!("{pointer}/{index}"), placements)?;
            }
        }
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                collect_json_material_placements(
                    value,
                    &format!("{pointer}/{escaped}"),
                    placements,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HttpEffectMaterialReferenceError {
    #[error("HTTP-effect scalar must be a string or exact material reference")]
    InvalidScalarValue,
    #[error("HTTP-effect material reference is malformed or has unknown fields")]
    InvalidReference,
    #[error(transparent)]
    InvalidPlacement(#[from] CredentialUsageError),
}

impl CredentialUsage {
    /// Validate canonical, secret-free usage facts before material is opened.
    pub fn validate(&self) -> Result<(), CredentialUsageError> {
        match self {
            Self::HttpEffect { fields } => {
                if fields.is_empty() {
                    return Err(CredentialUsageError::EmptyHttpEffectFields);
                }
                for (field, placements) in fields {
                    validate_material_field(field)?;
                    if placements.is_empty() {
                        return Err(CredentialUsageError::EmptyHttpEffectPlacements);
                    }
                    for placement in placements {
                        validate_http_effect_placement(placement)?;
                    }
                }
            }
            Self::SignatureVerification { fields } => {
                if fields.is_empty() {
                    return Err(CredentialUsageError::EmptySignatureFields);
                }
                for (field, algorithms) in fields {
                    validate_material_field(field)?;
                    if algorithms.is_empty() {
                        return Err(CredentialUsageError::EmptySignatureAlgorithms);
                    }
                }
            }
            _ => {}
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

fn validate_material_field(field: &str) -> Result<(), CredentialUsageError> {
    if field.trim().is_empty() || field.trim() != field || field.chars().any(char::is_control) {
        return Err(CredentialUsageError::InvalidMaterialField);
    }
    Ok(())
}

fn validate_http_effect_placement(
    placement: &HttpEffectPlacement,
) -> Result<(), CredentialUsageError> {
    match placement {
        HttpEffectPlacement::Header { name } if !is_canonical_header_name(name) => {
            Err(CredentialUsageError::InvalidHeaderName)
        }
        HttpEffectPlacement::Query { name } if !is_valid_query_name(name) => {
            Err(CredentialUsageError::InvalidQueryName)
        }
        HttpEffectPlacement::JsonPointer { pointer } if !is_valid_json_pointer(pointer) => {
            Err(CredentialUsageError::InvalidJsonPointer)
        }
        _ => Ok(()),
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
    #[error("named credential usage has an invalid material field")]
    InvalidMaterialField,
    #[error("HTTP-effect credential usage declares a field with no placements")]
    EmptyHttpEffectPlacements,
    #[error("signature-verification credential usage declares no material fields")]
    EmptySignatureFields,
    #[error("signature-verification credential usage declares a field with no algorithms")]
    EmptySignatureAlgorithms,
    #[error("HTTP-effect credential usage has a non-canonical header name")]
    InvalidHeaderName,
    #[error("HTTP-effect credential usage has an invalid query name")]
    InvalidQueryName,
    #[error("HTTP-effect credential usage has an invalid RFC 6901 JSON pointer")]
    InvalidJsonPointer,
}

#[cfg(kani)]
mod verification {
    use super::{NamedMaterialShape, named_material_shape_is_exact};

    /// Exhaustively proves a named-material usage cannot silently drop, add, or
    /// merge fields: scalar material binds exactly one declared field, while
    /// structured material requires non-empty exact key equality.
    #[kani::proof]
    fn named_material_shape_is_exact_and_non_widening() {
        let declared_fields = kani::any::<usize>();
        let structured_keys_exact = kani::any::<bool>();
        let shape = if kani::any::<bool>() {
            NamedMaterialShape::Secret
        } else {
            NamedMaterialShape::Structured
        };
        let admitted = named_material_shape_is_exact(shape, declared_fields, structured_keys_exact);

        match shape {
            NamedMaterialShape::Secret => {
                assert_eq!(admitted, declared_fields == 1);
            }
            NamedMaterialShape::Structured => {
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

    /// Material-reference cause/effect graph: C1=context is scalar or JSON;
    /// C2=value is a literal or a reference candidate; C3=the candidate has the
    /// complete known wire; C4=coordinates are canonical. E1 preserves
    /// literals, E2 returns one typed reference/rendering, and E3 rejects the
    /// complete projection before material lookup.
    ///
    /// | Rule | Context | Candidate | Exact wire | Coordinates | Effect |
    /// |---|---|---|---|---|---|
    /// | R1 | scalar | literal string | - | - | no reference |
    /// | R2 | scalar/JSON | yes | yes | yes | typed reference + exact render |
    /// | R3 | scalar | non-string | no | - | reject malformed scalar |
    /// | R4 | JSON | ordinary object | - | - | literal body object |
    /// | R5 | JSON | `credential` object | missing/unknown member | - | reject candidate |
    /// | R6 | scalar/JSON | yes | yes | empty/padded/control | reject candidate |
    #[test]
    fn material_reference_candidate_rules_are_shared_and_fail_closed() {
        let wire = serde_json::json!({
            "credential": "api",
            "field": "token",
            "prefix": "Bearer ",
            "suffix": "!"
        });
        let scalar = HttpEffectMaterialReference::from_scalar_value(&wire)
            .unwrap()
            .expect("R2 scalar reference");
        let json = HttpEffectMaterialReference::from_json_value(&wire)
            .unwrap()
            .expect("R2 JSON reference");
        assert_eq!(scalar, json, "R2 shared parser");
        assert_eq!(scalar.credential(), "api", "R2");
        assert_eq!(scalar.field(), "token", "R2");
        assert_eq!(scalar.prefix(), "Bearer ", "R2");
        assert_eq!(scalar.suffix(), "!", "R2");
        assert_eq!(scalar.render("secret"), "Bearer secret!", "R2");
        assert_eq!(serde_json::to_value(&scalar).unwrap(), wire, "R2 wire");

        assert_eq!(
            HttpEffectMaterialReference::from_scalar_value(&serde_json::json!("literal")),
            Ok(None),
            "R1"
        );
        assert_eq!(
            HttpEffectMaterialReference::from_json_value(&serde_json::json!({
                "field": "business-field",
                "nested": true
            })),
            Ok(None),
            "R4"
        );
        assert_eq!(
            HttpEffectMaterialReference::from_scalar_value(&serde_json::json!(7)),
            Err(HttpEffectMaterialReferenceError::InvalidScalarValue),
            "R3"
        );
        for malformed in [
            serde_json::json!({"credential":"api"}),
            serde_json::json!({"credential":"api","field":"token","unknown":true}),
            serde_json::json!({"credential":"api","field":" token"}),
            serde_json::json!({"credential":"api","field":""}),
        ] {
            assert_eq!(
                HttpEffectMaterialReference::from_json_value(&malformed),
                Err(HttpEffectMaterialReferenceError::InvalidReference),
                "R5/R6"
            );
        }
    }

    /// Placement-projection cause/effect graph: C1=valid scalar references;
    /// C2=literal scalar/body values; C3=nested JSON reference; C4=JSON member
    /// requires RFC 6901 escaping; C5=malformed scalar or JSON candidate;
    /// C6=destination itself is invalid. E1 records the exact normalized
    /// destinations, E2 records nothing for literals, and E3 rejects the whole
    /// projection.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
    /// |---|---|---|---|---|---|---|---|
    /// | P1 | T | T | T | T | F | F | header/query plus `/a~1b~0c/0` |
    /// | P2 | F | T | F | - | F | F | empty projection |
    /// | P3 | - | - | - | - | scalar | - | reject |
    /// | P4 | - | - | - | - | JSON candidate | - | reject |
    /// | P5 | - | - | root reference | - | F | F | empty JSON pointer |
    /// | P6 | T | - | T | - | F | header/query/pointer | reject |
    #[test]
    fn projection_is_exact_literal_safe_and_pointer_canonical() {
        let reference = |field: &str| {
            serde_json::json!({
                "credential": "api",
                "field": field,
                "prefix": "Bearer "
            })
        };
        let headers = BTreeMap::from([
            (" Authorization ".to_owned(), reference("token")),
            ("x-literal".to_owned(), serde_json::json!("public")),
        ]);
        let query = BTreeMap::from([("account".to_owned(), reference("account"))]);
        let body = serde_json::json!({
            "literal": {"field": "business-field"},
            "a/b~c": [reference("token")]
        });
        let projected =
            project_http_effect_material_placements(&headers, &query, Some(&body)).unwrap();
        let fields = projected.for_credential("api").expect("P1 role");
        assert_eq!(
            fields["account"],
            BTreeSet::from([HttpEffectPlacement::Query {
                name: "account".into()
            }]),
            "P1"
        );
        assert_eq!(
            fields["token"],
            BTreeSet::from([
                HttpEffectPlacement::Header {
                    name: "authorization".into()
                },
                HttpEffectPlacement::JsonPointer {
                    pointer: "/a~1b~0c/0".into()
                }
            ]),
            "P1"
        );
        assert_eq!(projected.as_map().len(), 1, "P1/P2");
        assert_eq!(
            projected.clone().into_map(),
            *projected.as_map(),
            "map view"
        );
        assert!(
            project_http_effect_material_placements(
                &BTreeMap::from([("x".into(), serde_json::json!("literal"))]),
                &BTreeMap::new(),
                Some(&serde_json::json!({"field":"business"})),
            )
            .unwrap()
            .is_empty(),
            "P2"
        );
        assert_eq!(
            project_http_effect_material_placements(
                &BTreeMap::from([("x".into(), serde_json::json!(false))]),
                &BTreeMap::new(),
                None,
            ),
            Err(HttpEffectMaterialReferenceError::InvalidScalarValue),
            "P3"
        );
        assert_eq!(
            project_http_effect_material_placements(
                &BTreeMap::new(),
                &BTreeMap::new(),
                Some(&serde_json::json!({
                    "nested": {"credential":"api","field":"token","unknown":true}
                })),
            ),
            Err(HttpEffectMaterialReferenceError::InvalidReference),
            "P4"
        );
        let root = project_http_effect_material_placements(
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some(&reference("token")),
        )
        .unwrap();
        assert_eq!(
            root.for_credential("api").unwrap()["token"],
            BTreeSet::from([HttpEffectPlacement::JsonPointer {
                pointer: String::new()
            }]),
            "P5"
        );
        assert_eq!(
            project_http_effect_material_placements(
                &BTreeMap::from([(" ".into(), reference("token"))]),
                &BTreeMap::new(),
                None,
            ),
            Err(HttpEffectMaterialReferenceError::InvalidPlacement(
                CredentialUsageError::InvalidHeaderName
            )),
            "P6 header"
        );
        assert_eq!(
            project_http_effect_material_placements(
                &BTreeMap::new(),
                &BTreeMap::from([("bad=name".into(), reference("token"))]),
                None,
            ),
            Err(HttpEffectMaterialReferenceError::InvalidPlacement(
                CredentialUsageError::InvalidQueryName
            )),
            "P6 query"
        );
        assert_eq!(
            project_http_effect_material_placements(
                &BTreeMap::new(),
                &BTreeMap::new(),
                Some(&serde_json::json!({"bad\nkey": reference("token")})),
            ),
            Err(HttpEffectMaterialReferenceError::InvalidPlacement(
                CredentialUsageError::InvalidJsonPointer
            )),
            "P6 pointer"
        );
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

    /// Signature-verification cause/effect decision table:
    /// S1 one scalar field + at least one exact algorithm => admit;
    /// S2 more than one scalar field => material mismatch;
    /// S3 structured material with the exact complete field set => admit;
    /// S4 empty fields/algorithm set or malformed field => reject before open;
    /// S5 OAuth material => reject. No algorithm or field fallback is allowed.
    #[test]
    fn signature_usage_freezes_exact_fields_and_algorithms() {
        use crate::SignatureVerificationAlgorithm::{ConstantTime, HmacSha256};

        let sole = CredentialUsage::SignatureVerification {
            fields: BTreeMap::from([("webhook_secret".into(), BTreeSet::from([HmacSha256]))]),
        };
        let multiple = CredentialUsage::SignatureVerification {
            fields: BTreeMap::from([
                ("token".into(), BTreeSet::from([ConstantTime])),
                ("webhook_secret".into(), BTreeSet::from([HmacSha256])),
            ]),
        };
        assert_eq!(sole.validate(), Ok(()), "S1");
        assert_eq!(
            CredentialMaterial::secret(RedactedString::new("secret")).validate_usage(&sole),
            Ok(()),
            "S1"
        );
        assert_eq!(
            CredentialMaterial::secret(RedactedString::new("secret")).validate_usage(&multiple),
            Err(CredentialMaterialError::MaterialKindMismatch),
            "S2"
        );
        let structured = CredentialMaterial::Structured(StructuredCredentialMaterial {
            type_id: "example.signature/v1".into(),
            fields: BTreeMap::from([
                ("token".into(), RedactedString::new("token")),
                ("webhook_secret".into(), RedactedString::new("secret")),
            ]),
        });
        assert_eq!(structured.validate_usage(&multiple), Ok(()), "S3");
        for malformed in [
            CredentialUsage::SignatureVerification {
                fields: BTreeMap::new(),
            },
            CredentialUsage::SignatureVerification {
                fields: BTreeMap::from([("webhook_secret".into(), BTreeSet::new())]),
            },
            CredentialUsage::SignatureVerification {
                fields: BTreeMap::from([(" webhook_secret".into(), BTreeSet::from([HmacSha256]))]),
            },
        ] {
            assert!(malformed.validate().is_err(), "S4: {malformed:?}");
        }
        let oauth = CredentialMaterial::OAuth(OAuthCredentialMaterial {
            access_token: RedactedString::new("access"),
            refresh_token: RedactedString::new("refresh"),
            expires_at_unix_ms: None,
            account_id: None,
            account_plan: None,
        });
        assert_eq!(
            oauth.validate_usage(&sole),
            Err(CredentialMaterialError::MaterialKindMismatch),
            "S5"
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
