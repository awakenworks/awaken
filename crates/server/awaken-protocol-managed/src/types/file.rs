//! Strong Managed Files DTOs. The beta and GA shapes project the same
//! `FileRecord`; their protocol differences never create another File authority.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum FileObjectType {
    #[serde(rename = "file")]
    File,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum DeletedFileObjectType {
    #[serde(rename = "file_deleted")]
    FileDeleted,
}

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

impl FileListParams {
    /// Decode the SDK's repeated `ids[]` form keys. `serde_urlencoded` treats a
    /// single repeated-form value as a scalar and therefore cannot faithfully
    /// decode the generated SDK request on its own.
    pub fn from_query(raw: &str) -> Result<Self, String> {
        let mut params = Self::default();
        let mut ids = Vec::new();
        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            match key.as_ref() {
                "page" => params.page = Some(value.into_owned()),
                "limit" => {
                    params.limit = Some(
                        value
                            .parse()
                            .map_err(|_| "limit must be an unsigned integer".to_string())?,
                    );
                }
                "ids[]" => ids.push(value.into_owned()),
                unknown => return Err(format!("unknown Files list parameter `{unknown}`")),
            }
        }
        if !ids.is_empty() {
            params.ids = Some(ids);
        }
        Ok(params)
    }
}

/// `BetaFileListParams` from `client.beta.files.list`.
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
    use super::FileListParams;

    #[test]
    fn ga_file_list_query_preserves_repeated_sdk_ids() {
        // Cause/effect graph: C1=no ids; C2=one encoded ids[]; C3=repeated
        // ids[]; C4=unknown key. Effects: E1=None; E2/E3=all ordered IDs;
        // E4=fail closed. Decision rules R1=C1->E1, R2=C2->E2,
        // R3=C3->E3, R4=C4->E4. This targets the generated SDK's form encoding.
        assert_eq!(FileListParams::from_query("").unwrap().ids, None, "R1/E1");
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
        assert!(FileListParams::from_query("unknown=x").is_err(), "R4/E4");
    }
}
