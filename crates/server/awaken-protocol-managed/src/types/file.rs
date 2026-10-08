// @formal surface requirement=product.public_dto.anti_corruption
// @formal reason: Static owner: Managed Files wire DTOs and request-value validation. Dynamic path: SDK
// @formal reason: query, metadata, scope, deletion, and expiry shapes are decoded or projected around the
// @formal reason: canonical File authority without creating another File lifecycle. Evidence:
// @formal reason: product.public_dto.anti_corruption and SDK surface conformance; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
//! Strong Managed Files DTOs. The beta and GA shapes project the same
//! `FileRecord`; their protocol differences never create another File authority.

use serde::{Deserialize, Serialize};

/// SDK: inline — `BetaFileMetadata.type` (also `FileMetadata.type`).
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FileObjectType {
    #[serde(rename = "file")]
    File,
}

/// SDK: inline — `BetaDeletedFile.type` (also `DeletedFile.type`).
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeletedFileObjectType {
    #[serde(rename = "file_deleted")]
    FileDeleted,
}

/// SDK: inline — `BetaFileScope.type`.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FileScopeObjectType {
    #[serde(rename = "session")]
    Session,
}

/// `FileListParams` from the GA `client.files.list` surface.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct FileListParams {
    #[serde(default)]
    pub page: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default, rename = "ids[]")]
    pub ids: Option<Vec<String>>,
}

#[derive(Debug, Default)]
struct ParsedCursorFileList {
    page: Option<String>,
    limit: Option<u16>,
    ids: Vec<String>,
    scope_id: Option<String>,
}

fn parse_cursor_file_list(raw: &str, allow_scope: bool) -> Result<ParsedCursorFileList, String> {
    let query = super::page::split_array_query(raw, "ids")?;
    let mut params = ParsedCursorFileList {
        ids: query
            .values
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect(),
        ..Default::default()
    };
    for (key, value) in query.parameters {
        match key.as_ref() {
            "page" if value.is_empty() => params.page = None,
            "page" => params.page = Some(value),
            "limit" => {
                params.limit = Some(
                    value
                        .parse()
                        .map_err(|_| "limit must be an unsigned integer".to_string())?,
                );
            }
            "scope_id" if allow_scope => {
                params.scope_id = (!value.is_empty()).then_some(value);
            }
            unknown => return Err(format!("unknown Files list parameter `{unknown}`")),
        }
    }
    Ok(params)
}

impl FileListParams {
    /// Decode the SDK's repeated `ids[]` form keys. `serde_urlencoded` treats a
    /// single repeated-form value as a scalar and therefore cannot faithfully
    /// decode the generated SDK request on its own.
    pub fn from_query(raw: &str) -> Result<Self, String> {
        let parsed = parse_cursor_file_list(raw, false)?;
        Ok(Self {
            page: parsed.page,
            limit: parsed.limit,
            ids: (!parsed.ids.is_empty()).then_some(parsed.ids),
        })
    }
}

/// Post-GA `client.beta.files.list` keeps the Beta namespace's `scope_id` but
/// adopts the GA cursor and `ids[]` pagination vocabulary. A distinct DTO keeps
/// both it and top-level GA fail-closed without selecting behavior by SDK
/// version.
///
/// `FileListParams` from `client.beta.files.list` (`page`, `limit`, `ids[]`, `scope_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BetaFileCursorListParams {
    pub page: Option<String>,
    pub limit: Option<u16>,
    pub ids: Option<Vec<String>>,
    pub scope_id: Option<String>,
}

impl BetaFileCursorListParams {
    pub fn from_query(raw: &str) -> Result<Self, String> {
        let parsed = parse_cursor_file_list(raw, true)?;
        Ok(Self {
            page: parsed.page,
            limit: parsed.limit,
            ids: (!parsed.ids.is_empty()).then_some(parsed.ids),
            scope_id: parsed.scope_id,
        })
    }
}

/// `BetaFileListParams` from `client.beta.files.list`.
///
/// SDK: none — the pre-cursor beta Files list query (`before_id`/`after_id`); SDK 0.125.0 no longer declares it, so it is kept only for older clients.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BetaFileListParams {
    #[serde(default)]
    pub before_id: Option<String>,
    #[serde(default)]
    pub after_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u16>,
    #[serde(default)]
    pub scope_id: Option<String>,
}

/// `FileMetadata` from the GA Files resource.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FileMetadata {
    pub id: String,
    pub created_at: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    #[serde(rename = "type")]
    pub kind: FileObjectType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloadable: Option<bool>,
    pub expires_at: Option<String>,
}

/// `BetaFileMetadata` from the beta Files resource.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BetaFileMetadata {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: FileObjectType,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloadable: Option<bool>,
    pub scope: Option<BetaFileScope>,
}

/// Post-GA Beta Files response. It deliberately combines the Beta-only scope
/// with the GA expiry field exactly as the official SDK does; neither the
/// legacy capability projection nor the top-level GA projection is widened.
///
/// `BetaFileMetadata`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BetaFileCursorMetadata {
    pub id: String,
    pub created_at: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    #[serde(rename = "type")]
    pub kind: FileObjectType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloadable: Option<bool>,
    pub expires_at: Option<String>,
    pub scope: Option<BetaFileScope>,
}

/// `BetaFileScope`.
///
/// The beta-only Session scope projection.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BetaFileScope {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: FileScopeObjectType,
}

/// `DeletedFile` shared by beta and GA.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeletedFile {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: DeletedFileObjectType,
}

/// Parsed GA multipart expiry input. Multipart itself is decoded by axum, but
/// this strong value preserves the SDK's exact numeric range before mutation.
///
/// SDK: inline — `FileUploadParams.expires_in_seconds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileExpirySeconds(u64);

impl FileExpirySeconds {
    pub const MIN: u64 = 3_600;
    pub const MAX: u64 = 7_776_000;

    pub fn new(value: u64) -> Result<Self, String> {
        if (Self::MIN..=Self::MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(format!(
                "expires_in_seconds must be between {} and {}",
                Self::MIN,
                Self::MAX
            ))
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{BetaFileCursorListParams, FileListParams};

    #[test]
    fn ga_file_list_query_preserves_repeated_sdk_ids() {
        // Cause/effect graph: C1=no ids/page (Python null); C2=empty scalar
        // ids + page (TS null); C3=one/repeated encoded ids[]; C4=non-empty
        // scalar or unknown key. Effects: E1/E2=the same None state; E3=all
        // ordered IDs; E4=fail closed. The empty scalar exception is exact and
        // cannot widen the declared array into an arbitrary scalar parameter.
        // Consolidation constraint: the shared SDK decoder retains member order
        // and nullable-scalar semantics; Files alone still ignores empty IDs.
        assert_eq!(FileListParams::from_query("").unwrap().ids, None, "R1/E1");
        let typescript_null = FileListParams::from_query("ids=&page=").unwrap();
        assert_eq!(typescript_null.ids, None, "R2 nullable array");
        assert_eq!(typescript_null.page, None, "R2 nullable cursor");
        assert_eq!(
            FileListParams::from_query("ids%5B%5D=file_1").unwrap().ids,
            Some(vec!["file_1".into()]),
            "R2/E2"
        );
        assert_eq!(
            FileListParams::from_query("ids%5B%5D=file_1&ids%5B%5D=file_2")
                .unwrap()
                .ids,
            Some(vec!["file_1".into(), "file_2".into()]),
            "R3/E3"
        );
        assert!(
            FileListParams::from_query("ids=file_1").is_err(),
            "R4 scalar"
        );
        assert!(FileListParams::from_query("unknown=x").is_err(), "R4/E4");
        let mixed =
            FileListParams::from_query("ids=&ids[]=&ids[]=file_1&ids[]=file_1&page=p&limit=2")
                .unwrap();
        assert_eq!(mixed.ids, Some(vec!["file_1".into(), "file_1".into()]));
        assert_eq!(mixed.page.as_deref(), Some("p"));
        assert_eq!(mixed.limit, Some(2));
    }

    #[test]
    fn beta_cursor_file_query_owns_scope_without_widening_ga() {
        // Change-point decision table: the post-GA Beta namespace owns
        // scope_id + ids[] + cursor; top-level GA owns ids[] + cursor only.
        // This syntactic request edge is what distinguishes the two official
        // SDK surfaces without consulting x-stainless version metadata.
        let beta = BetaFileCursorListParams::from_query(
            "scope_id=sesn_1&ids%5B%5D=file_1&ids%5B%5D=file_2",
        )
        .unwrap();
        assert_eq!(beta.scope_id.as_deref(), Some("sesn_1"));
        assert_eq!(beta.ids.unwrap(), ["file_1", "file_2"]);
        let beta_null = BetaFileCursorListParams::from_query("ids=&page=").unwrap();
        assert_eq!(beta_null.ids, None);
        assert_eq!(beta_null.page, None);
        assert!(FileListParams::from_query("scope_id=sesn_1").is_err());
        assert!(BetaFileCursorListParams::from_query("before_id=file_1").is_err());

        let typescript_empty =
            BetaFileCursorListParams::from_query("scope_id=&ids%5B%5D=&page=").unwrap();
        assert_eq!(typescript_empty, BetaFileCursorListParams::default());
    }
}
