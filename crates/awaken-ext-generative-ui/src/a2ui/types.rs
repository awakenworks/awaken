use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single A2UI v0.9 message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiMessage {
    /// Protocol version, must be "v0.9".
    pub version: String,
    /// Create a new surface.
    #[serde(
        rename = "createSurface",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub create_surface: Option<A2uiCreateSurface>,
    /// Update components on a surface.
    #[serde(
        rename = "updateComponents",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub update_components: Option<A2uiUpdateComponents>,
    /// Update the data model of a surface.
    #[serde(
        rename = "updateDataModel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub update_data_model: Option<A2uiUpdateDataModel>,
    /// Delete a surface.
    #[serde(
        rename = "deleteSurface",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub delete_surface: Option<A2uiDeleteSurface>,
}

/// Parameters for creating a new A2UI surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiCreateSurface {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    #[serde(rename = "catalogId")]
    pub catalog_id: String,
}

/// Parameters for updating components on a surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiUpdateComponents {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    pub components: Vec<A2uiComponent>,
}

/// A single A2UI component definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiComponent {
    pub id: String,
    pub component: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// Parameters for updating the data model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiUpdateDataModel {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    pub path: String,
    pub value: Value,
}

/// Parameters for deleting a surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2uiDeleteSurface {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
}
